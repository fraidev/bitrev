use std::{
    sync::{
        atomic::{AtomicBool, AtomicU32},
        Arc, Mutex,
    },
    time::Duration,
};

use tokio::{
    io::AsyncWriteExt,
    net::TcpStream,
    sync::{Notify, Semaphore},
    time::timeout,
};
use tracing::{debug, error, trace};

use crate::{
    bitfield::Bitfield,
    message::{self, Message, WriterRequest},
    peer::PeerAddr,
    peer_state::{PeerState, PeerStates},
    protocol::{Protocol, ProtocolError},
    session::PieceWork,
    utils,
};

pub struct TorrentDownloadedState {
    pub semaphore: Semaphore,
    pub pieces: Vec<PieceWorkState>,
}

impl TorrentDownloadedState {
    pub fn is_complete(&self) -> bool {
        self.pieces
            .iter()
            .all(|pw| pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
    }

    pub fn missing_pieces(&self) -> Vec<u32> {
        self.pieces
            .iter()
            .enumerate()
            .filter(|(_, pw)| !pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
            .map(|(i, _)| i as u32)
            .collect()
    }

    pub fn reserved_and_not_downloaded(&self) -> Vec<u32> {
        self.pieces
            .iter()
            .enumerate()
            .filter(|(_, pw)| {
                pw.reserved.lock().unwrap().is_none()
                    && !pw.downloaded.load(std::sync::atomic::Ordering::Relaxed)
            })
            .map(|(i, _)| i as u32)
            .collect()
    }

    pub async fn get_and_reserve_piece(&self, peer: PeerAddr) -> Option<&PieceWorkState> {
        //loop {
        //    if let Ok(acq) = self.semaphore.try_acquire() {
        //        break acq.forget();
        //    } else {
        //        sleep(Duration::from_secs(1)).await;
        //    }
        //}

        for pw in self.pieces.iter() {
            if pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            //if pw
            //    .reserverd
            //    .swap(true, std::sync::atomic::Ordering::Relaxed)
            //{
            //    continue;
            //}

            let mut reserved = pw.reserved.lock().unwrap();

            //if let Some(p) = reserved.as_ref() {
            //    if *p == peer {
            //        self.semaphore.add_permits(1);
            //        return Some(pw);
            //    }
            //}

            if reserved.is_some() {
                continue;
            }

            //pw.reserverd
            //    .store(true, std::sync::atomic::Ordering::Relaxed);
            reserved.replace(peer);
            drop(reserved);
            self.semaphore.add_permits(1);

            return Some(pw);
        }

        for pw in self.pieces.iter() {
            if pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }

            return Some(pw);
        }

        None
    }
    pub fn remove_downloaded(&self, index: u32) {
        for pw in self.pieces.iter() {
            if pw.piece_work.index == index {
                pw.chuncks.lock().unwrap().clear();
                pw.downloaded
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    pub fn remove_reserved(&self, peer: PeerAddr) {
        for pw in self.pieces.iter() {
            //if pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
            //    continue;
            //}

            let mut reserved = pw.reserved.lock().unwrap();
            if let Some(p) = reserved.as_ref() {
                if *p == peer {
                    reserved.take();
                    //self.semaphore.add_permits(1);
                }
            }
        }
    }

    pub fn set_chuncks(&self, index: u32, start: u32, buf: Vec<u8>) {
        //let mut chuncks = self.pieces[index as usize].chuncks.lock().unwrap();
        let mut chuncks = self
            .pieces
            .iter()
            .find(|pw| pw.piece_work.index == index)
            .unwrap()
            .chuncks
            .lock()
            .unwrap();
        chuncks.push(Chunk {
            index,
            start,
            length: buf.len() as u32,
            buf,
        });
    }

    pub fn set_downloaded_if_all_chunks(&self, index: u32) -> Option<&PieceWorkState> {
        // check if all chuncks are downloaded
        if self.pieces[index as usize]
            .chuncks
            .lock()
            .unwrap()
            .iter()
            .fold(0, |acc, c| acc + c.length as usize)
            == self.pieces[index as usize].piece_work.length as usize
        {
            self.pieces[index as usize]
                .downloaded
                .store(true, std::sync::atomic::Ordering::Relaxed);
            return Some(&self.pieces[index as usize]);
        }
        None
    }
}

pub struct PieceWorkState {
    pub piece_work: PieceWork,
    pub chuncks: Mutex<Vec<Chunk>>,
    pub downloaded: AtomicBool,
    pub reserved: Mutex<Option<PeerAddr>>,
}

impl PieceWorkState {
    pub fn chunk_to_buf(&self) -> Vec<u8> {
        let mut chuncks = self.chuncks.lock().unwrap();
        let mut buf = vec![];
        // sort by start
        chuncks.sort_by(|a, b| a.start.cmp(&b.start));
        for chunk in chuncks.iter() {
            buf.extend(chunk.buf.iter());
        }
        buf
    }
}

pub struct Chunk {
    pub index: u32,
    pub start: u32,
    pub length: u32,
    pub buf: Vec<u8>,
}

pub struct FullPiece {
    pub index: u32,
    pub length: u32,
    pub buf: Vec<u8>,
}

impl PieceWorkState {
    pub fn set_downloaded(&self) {
        if self.chuncks.lock().unwrap().len() == self.piece_work.length as usize {
            self.downloaded
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

pub struct PeerHandler {
    unchoke_notify: Notify,
    on_bitfield_notify: Notify,
    chocked: AtomicBool,
    downloaded: AtomicU32,
    peers_state: Arc<PeerStates>,
    piece_tx: flume::Sender<FullPiece>,
    peer_writer_tx: flume::Sender<WriterRequest>,
    requests_sem: Semaphore,
    peer: PeerAddr,
    torrent_downloaded_state: Arc<TorrentDownloadedState>,
}

impl PeerHandler {
    pub fn new(
        peer: PeerAddr,
        unchoked_notify: Notify,
        piece_tx: flume::Sender<FullPiece>,
        peer_writer_tx: flume::Sender<WriterRequest>,
        peers_state: Arc<PeerStates>,
        //pieces: Vec<PieceWork>,
        torrent_downloaded_state: Arc<TorrentDownloadedState>,
    ) -> Self {
        Self {
            unchoke_notify: unchoked_notify,
            on_bitfield_notify: Notify::new(),
            downloaded: AtomicU32::new(0),
            chocked: AtomicBool::new(true),
            peers_state,
            requests_sem: Semaphore::new(0),
            piece_tx,
            peer_writer_tx,
            peer,
            torrent_downloaded_state,
            //torrent_downloaded_state: Arc::new(TorrentDownloadedState {
            //
            //    semaphore: Semaphore::new(1),
            //    pieces: pieces
            //        .into_iter()
            //        .map(|pw| PieceWorkState {
            //            piece_work: pw,
            //            chuncks: Mutex::new(vec![]),
            //            downloaded: AtomicBool::new(false),
            //            reserverd: AtomicBool::new(false),
            //        })
            //        .collect(),
            //}),
        }
    }

    pub fn on_peer_died(&self) {
        self.peers_state.states.remove(&self.peer);
        self.torrent_downloaded_state.remove_reserved(self.peer);
    }

    pub fn should_transmit_have(&self, id: u32) -> bool {
        if let Some(state) = self.peers_state.states.get(&self.peer) {
            !state.bitfield.has_piece(id as usize)
        } else {
            false
        }
    }

    // The job of this is to request chunks and also to keep peer alive.
    // The moment this ends, the peer is disconnected.
    pub async fn task_peer_chunk_requester(&self) -> Result<(), anyhow::Error> {
        let notfied = self.on_bitfield_notify.notified();
        let user_state = self.peers_state.states.get(&self.peer);
        if let Some(state) = user_state {
            if state.bitfield.is_empty() {
                notfied.await;
            }
        }

        let mut update_interest = {
            let mut current = false;
            move |h: &PeerHandler, new_value: bool| -> anyhow::Result<()> {
                if new_value != current {
                    h.peer_writer_tx.send(if new_value {
                        trace!("sending interested");
                        WriterRequest::Message(Message::Interested)
                    } else {
                        trace!("sending not interested");
                        WriterRequest::Message(Message::NotInterested)
                    })?;
                    current = new_value;
                }
                Ok(())
            }
        };

        loop {
            update_interest(self, true)?;

            trace!("waiting for unchoke");

            if self.chocked.load(std::sync::atomic::Ordering::Relaxed) {
                self.unchoke_notify.notified().await;
            }
            trace!("unchoke received");

            if self.torrent_downloaded_state.is_complete() {
                trace!("TORRENT IS COMPLETE");
                return Ok(());
            }

            let piece = self
                .torrent_downloaded_state
                .get_and_reserve_piece(self.peer)
                .await;

            if piece.is_none() {
                trace!("no more pieces to download");
                return Ok(());
            }

            let piece = piece.unwrap().piece_work;

            let mut offset: u32 = 0;
            while offset < piece.length {
                loop {
                    match (tokio::time::timeout(
                        Duration::from_secs(5),
                        self.requests_sem.acquire(),
                    ))
                    .await
                    {
                        Ok(acq) => break acq?.forget(),
                        Err(_) => continue,
                    };
                }
                let block_size = utils::calculate_block_size(piece.length, offset);

                let r = message::format_request(piece.index, offset, block_size);

                debug!(
                    "requesting piece index {} start {} length {}",
                    piece.index, offset, block_size
                );
                if self.peer_writer_tx.send(WriterRequest::Message(r)).is_err() {
                    error!("error sending request to peer");
                    return Ok(());
                }
                offset += block_size;
            }
        }
    }

    fn on_received_message(&self, message: crate::message::Message) -> Result<(), anyhow::Error> {
        match message {
            Message::Choke => {
                debug!("peer choked us");
                self.chocked
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Message::Unchoke => {
                debug!("peer unchoked us");
                self.chocked
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                self.unchoke_notify.notify_waiters();
                self.requests_sem.add_permits(128);
            }
            Message::Interested => {
                debug!("peer is interested");
            }
            Message::NotInterested => {
                debug!("peer is not interested");
            }
            Message::Have(h) => {
                let p_state = self.peers_state.states.get_mut(&self.peer);
                if let Some(mut p_state) = p_state {
                    p_state.bitfield.set_piece(h as usize)
                }

                self.on_bitfield_notify.notify_waiters();
            }
            Message::Bitfield(vec) => {
                debug!("peer sent bitfield");
                let p_state = self.peers_state.states.get_mut(&self.peer);

                if let Some(mut ps) = p_state {
                    ps.bitfield = Bitfield::new(vec);
                } else {
                    self.peers_state.states.insert(
                        self.peer,
                        PeerState {
                            bitfield: Bitfield::new(vec),
                            peer_interested: false,
                        },
                    );
                }

                self.on_bitfield_notify.notify_waiters();
            }
            Message::Request(_) => {
                debug!("peer requested piece, not implemented");
            }
            Message::Piece(piece_chunk) => {
                self.downloaded
                    .fetch_add(piece_chunk.length, std::sync::atomic::Ordering::Relaxed);
                self.requests_sem.add_permits(1);
                self.torrent_downloaded_state.set_chuncks(
                    piece_chunk.index,
                    piece_chunk.start,
                    piece_chunk.data,
                );
                if let Some(full_piece) = self
                    .torrent_downloaded_state
                    .set_downloaded_if_all_chunks(piece_chunk.index)
                {
                    let buf = full_piece.chunk_to_buf();

                    if utils::check_integrity(full_piece.piece_work.hash.as_ref(), &buf) {
                        trace!("piece index {} is correct", piece_chunk.index);
                        let full_piece = FullPiece {
                            index: piece_chunk.index,
                            length: full_piece.piece_work.length,
                            buf,
                        };

                        self.piece_tx.send(full_piece).unwrap();
                    } else {
                        trace!("piece index {} is corrupted", piece_chunk.index);
                        self.torrent_downloaded_state
                            .remove_downloaded(piece_chunk.index);
                        //self.torrent_downloaded_state.remove_reserved(self.peer);
                        //return Ok(());
                    }
                }

                //self.piece_tx.send(piece.clone()).unwrap();
                trace!(
                    "peer received piece index {} start {} length {}",
                    piece_chunk.index,
                    piece_chunk.start,
                    piece_chunk.length
                );
            }
            Message::Cancel(_) => {
                debug!("peer canceled request");
                //trace!("peer canceled request");
            }
            message => {
                debug!("received unsupported message {:?}, ignoring", message);
            }
        }

        Ok(())
    }
}

pub struct PeerConnection {
    pub handler: Arc<PeerHandler>,
    pub bitfield: Bitfield,
    pub peer: PeerAddr,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

impl PeerConnection {
    pub fn new(
        peer: PeerAddr,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
        handler: Arc<PeerHandler>,
    ) -> Self {
        Self {
            handler,
            bitfield: Bitfield::new(vec![]),
            peer,
            info_hash,
            peer_id,
        }
    }

    pub async fn manage_peer_incoming(
        &self,
        peer_writer_rx: flume::Receiver<WriterRequest>,
        mut have_broadcast: tokio::sync::broadcast::Receiver<u32>,
    ) -> anyhow::Result<()> {
        let connect = async {
            TcpStream::connect(self.peer)
                .await
                .map_err(ProtocolError::Io)
        };
        let mut stream = match tokio::time::timeout(Duration::from_secs(6), connect).await {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(ProtocolError::Timeout(e)),
        }?;

        let protocol = Arc::new(Protocol::connect(self.peer, self.info_hash, self.peer_id).await?);
        let _handshake = protocol.complete_handshake(&mut stream).await?;
        protocol.send_unchoke(&mut stream).await?;
        protocol.send_interested(&mut stream).await?;

        // manage peer
        let (mut read, mut write) = stream.split();

        let writer = {
            async move {
                let mut broadcast_closed = false;
                loop {
                    let req = loop {
                        break tokio::select! {
                            r = have_broadcast.recv(), if !broadcast_closed => match r {
                                Ok(id) => {
                                    if self.handler.should_transmit_have(id) {
                                         WriterRequest::Message(Message::Have(id))
                                    } else {
                                        continue
                                    }
                                },
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    broadcast_closed = true;
                                    debug!("broadcast channel closed, will not poll it anymore");
                                    continue
                                },
                                _ => continue
                            },
                            r = timeout(Duration::from_secs(120), peer_writer_rx.recv_async()) => match r {
                                Ok(Ok(msg)) =>{
                                    msg
                                },
                                Ok(Err(_)) => {
                                    error!("closing writer, channel closed");
                                    anyhow::bail!("closing writer, channel closed");
                                }
                                Err(_) => {
                                    debug!("timeout reading, let's keep alive");
                                    WriterRequest::Message(Message::KeepAlive)
                                },
                            }
                        };
                    };

                    let buf = match req {
                        WriterRequest::Message(msg) => message::serialize(Some(msg)),
                    };

                    match timeout(Duration::from_secs(10), write.write_all(&buf)).await {
                        Ok(Ok(_)) => {
                            //debug!("sent message");
                        }
                        Ok(Err(e)) => {
                            debug!("error writing to peer: {:?}", e);
                            break;
                        }
                        Err(e) => {
                            debug!("timeout writing to peer: {:?}", e);
                            break;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
        };

        let reader = async move {
            loop {
                let message =
                    tokio::time::timeout(Duration::from_secs(10), protocol.read(&mut read)).await;

                match message {
                    Ok(Ok(None)) => {
                        debug!("peer disconnected");
                        break;
                    }
                    Ok(Ok(Some(msg))) => match self.handler.on_received_message(msg) {
                        Ok(_) => {}
                        Err(e) => {
                            debug!("error processing message: {:?}", e);
                            break;
                        }
                    },
                    Ok(Err(e)) => {
                        debug!("error reading from peer: {:?}", e);
                        break;
                    }
                    Err(e) => {
                        debug!("timeout reading from peer: {:?}", e);
                        break;
                    }
                }
            }

            Ok::<_, anyhow::Error>(())
        };

        tokio::select! {
            r = reader => {
                trace!(result=?r, "reader is done, exiting");
                r
            }
            r = writer => {
                trace!(result=?r, "writer is done, exiting");
                r
            }
        }
    }
}
