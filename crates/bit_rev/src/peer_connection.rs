use std::{
    collections::VecDeque,
    future,
    sync::{
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
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
    message::{self, validate_request, BlockRequest, Message, WriterRequest, MAX_UPLOAD_QUEUE},
    peer::PeerAddr,
    peer_state::PeerStates,
    protocol::{Protocol, ProtocolError},
    session::{DownloadState, PieceWork},
    storage::Storage,
    torrent::Torrent,
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

    pub fn downloaded_bytes(&self) -> u64 {
        self.pieces
            .iter()
            .filter(|pw| pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
            .map(|pw| u64::from(pw.piece_work.length))
            .sum()
    }

    pub fn left_bytes(&self) -> u64 {
        self.pieces
            .iter()
            .filter(|pw| !pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
            .map(|pw| u64::from(pw.piece_work.length))
            .sum()
    }

    pub fn has_piece(&self, index: u32) -> bool {
        self.pieces
            .get(index as usize)
            .map(|pw| pw.downloaded.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    }

    pub fn piece_length(&self, index: u32) -> Option<u32> {
        self.pieces
            .get(index as usize)
            .map(|pw| pw.piece_work.length)
    }

    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    pub fn mark_all_downloaded(&self) {
        for pw in &self.pieces {
            pw.downloaded
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn mark_downloaded(&self, index: u32) {
        if let Some(pw) = self.pieces.get(index as usize) {
            pw.downloaded
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn our_bitfield(&self) -> Bitfield {
        let mut bitfield = Bitfield::with_piece_count(self.pieces.len());
        for (i, pw) in self.pieces.iter().enumerate() {
            if pw.downloaded.load(std::sync::atomic::Ordering::Relaxed) {
                bitfield.set_piece(i);
            }
        }
        bitfield
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
        chuncks.sort_by_key(|a| a.start);
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

pub struct PeerHandlerConfig {
    pub peer: PeerAddr,
    pub piece_tx: flume::Sender<FullPiece>,
    pub peer_writer_tx: flume::Sender<WriterRequest>,
    pub peers_state: Arc<PeerStates>,
    pub torrent_downloaded_state: Arc<TorrentDownloadedState>,
    pub download_state: Arc<Mutex<DownloadState>>,
    pub storage: Arc<Storage>,
    pub uploaded: Arc<AtomicU64>,
    pub torrent: Arc<Torrent>,
    pub choke_notify: Arc<Notify>,
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
    download_state: Arc<Mutex<DownloadState>>,
    storage: Arc<Storage>,
    uploaded: Arc<AtomicU64>,
    _torrent: Arc<Torrent>,
    choke_notify: Arc<Notify>,
    upload_queue: Mutex<VecDeque<BlockRequest>>,
}

impl PeerHandler {
    pub fn from_config(config: PeerHandlerConfig) -> Self {
        Self {
            unchoke_notify: Notify::new(),
            on_bitfield_notify: Notify::new(),
            downloaded: AtomicU32::new(0),
            chocked: AtomicBool::new(true),
            peers_state: config.peers_state,
            requests_sem: Semaphore::new(0),
            piece_tx: config.piece_tx,
            peer_writer_tx: config.peer_writer_tx,
            peer: config.peer,
            torrent_downloaded_state: config.torrent_downloaded_state,
            download_state: config.download_state,
            storage: config.storage,
            uploaded: config.uploaded,
            _torrent: config.torrent,
            choke_notify: config.choke_notify,
            upload_queue: Mutex::new(VecDeque::new()),
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

    pub fn get_download_state(&self) -> DownloadState {
        *self.download_state.lock().unwrap()
    }

    pub fn is_downloading(&self) -> bool {
        self.get_download_state() == DownloadState::Downloading
    }

    fn peer_has_needed_piece(&self) -> bool {
        let Some(state) = self.peers_state.states.get(&self.peer) else {
            return false;
        };
        self.torrent_downloaded_state
            .pieces
            .iter()
            .enumerate()
            .any(|(i, pw)| !pw.downloaded.load(Ordering::Relaxed) && state.bitfield.has_piece(i))
    }

    fn am_choking(&self) -> bool {
        self.peers_state
            .states
            .get(&self.peer)
            .map(|s| s.stats.am_choking.load(Ordering::Relaxed))
            .unwrap_or(true)
    }

    fn on_incoming_request(&self, payload: Vec<u8>) -> Result<(), anyhow::Error> {
        let req = BlockRequest::from_payload(&payload)
            .ok_or_else(|| anyhow::anyhow!("truncated request from peer"))?;
        let piece_length = self
            .torrent_downloaded_state
            .piece_length(req.index)
            .ok_or_else(|| anyhow::anyhow!("request for unknown piece {}", req.index))?;
        let have_piece = self.torrent_downloaded_state.has_piece(req.index);
        validate_request(&req, piece_length, have_piece)
            .map_err(|e| anyhow::anyhow!("invalid request {:?}: {:?}", req, e))?;

        if self.am_choking() {
            debug!("ignoring request while choking {:?}", req);
            return Ok(());
        }

        let mut queue = self.upload_queue.lock().unwrap();
        if queue.len() >= MAX_UPLOAD_QUEUE {
            debug!("upload queue full, dropping request {:?}", req);
            return Ok(());
        }
        queue.push_back(req);
        drop(queue);
        if let Some(state) = self.peers_state.states.get(&self.peer) {
            state.stats.upload_notify.notify_waiters();
        }
        Ok(())
    }

    pub async fn task_peer_uploader(&self) -> Result<(), anyhow::Error> {
        loop {
            let stats = self
                .peers_state
                .states
                .get(&self.peer)
                .map(|s| s.stats.clone());
            if let Some(stats) = stats {
                stats.upload_notify.notified().await;
            } else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }

            loop {
                if self.am_choking() {
                    self.upload_queue.lock().unwrap().clear();
                    break;
                }
                let req = self.upload_queue.lock().unwrap().pop_front();
                let Some(req) = req else {
                    break;
                };
                let data = self
                    .storage
                    .read_block(req.index, req.begin, req.length)
                    .await?;
                let length = data.len() as u64;
                if self
                    .peer_writer_tx
                    .send(WriterRequest::Message(message::format_piece(
                        req.index, req.begin, data,
                    )))
                    .is_err()
                {
                    return Ok(());
                }
                self.uploaded.fetch_add(length, Ordering::Relaxed);
                if let Some(state) = self.peers_state.states.get(&self.peer) {
                    state
                        .stats
                        .bytes_uploaded
                        .fetch_add(length, Ordering::Relaxed);
                }
            }
        }
    }

    // The job of this is to request chunks and also to keep peer alive.
    // The moment this ends, the peer is disconnected.
    pub async fn task_peer_chunk_requester(&self) -> Result<(), anyhow::Error> {
        let needs_bitfield = self
            .peers_state
            .states
            .get(&self.peer)
            .map(|state| state.bitfield.is_empty())
            .unwrap_or(true);
        if needs_bitfield {
            self.on_bitfield_notify.notified().await;
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
                    if let Some(mut state) = h.peers_state.states.get_mut(&h.peer) {
                        state.set_am_interested(new_value);
                    }
                    current = new_value;
                }
                Ok(())
            }
        };

        loop {
            while !self.is_downloading() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }

            if self.torrent_downloaded_state.is_complete() || !self.peer_has_needed_piece() {
                update_interest(self, false)?;
                if self.torrent_downloaded_state.is_complete() {
                    trace!("torrent complete, staying connected to seed");
                    future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }

            update_interest(self, true)?;

            trace!("waiting for unchoke");

            if self.chocked.load(std::sync::atomic::Ordering::Relaxed) {
                self.unchoke_notify.notified().await;
            }
            trace!("unchoke received");

            if self.torrent_downloaded_state.is_complete() {
                update_interest(self, false)?;
                trace!("torrent complete, staying connected to seed");
                future::pending::<()>().await;
            }

            let piece = self
                .torrent_downloaded_state
                .get_and_reserve_piece(self.peer)
                .await;

            if piece.is_none() {
                update_interest(self, false)?;
                if self.torrent_downloaded_state.is_complete() {
                    future::pending::<()>().await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }

            let piece = piece.unwrap().piece_work;

            let mut offset: u32 = 0;
            while offset < piece.length {
                // Check download state before requesting each block
                if !self.is_downloading() {
                    // Wait while not downloading
                    while !self.is_downloading() {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }

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
                self.chocked.store(true, Ordering::Relaxed);
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.set_peer_choking(true);
                }
            }
            Message::Unchoke => {
                debug!("peer unchoked us");
                self.chocked.store(false, Ordering::Relaxed);
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.set_peer_choking(false);
                }
                self.unchoke_notify.notify_waiters();
                self.requests_sem.add_permits(128);
            }
            Message::Interested => {
                debug!("peer is interested");
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.set_peer_interested(true);
                }
                self.choke_notify.notify_waiters();
            }
            Message::NotInterested => {
                debug!("peer is not interested");
                if let Some(mut state) = self.peers_state.states.get_mut(&self.peer) {
                    state.set_peer_interested(false);
                }
                self.choke_notify.notify_waiters();
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
                if let Some(mut ps) = self.peers_state.states.get_mut(&self.peer) {
                    ps.bitfield = Bitfield::new(vec);
                }

                self.on_bitfield_notify.notify_waiters();
            }
            Message::Request(payload) => {
                self.on_incoming_request(payload)?;
            }
            Message::Piece(piece_chunk) => {
                self.downloaded
                    .fetch_add(piece_chunk.length, Ordering::Relaxed);
                if let Some(state) = self.peers_state.states.get(&self.peer) {
                    state
                        .stats
                        .bytes_downloaded
                        .fetch_add(piece_chunk.length as u64, Ordering::Relaxed);
                }
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
            Message::Cancel(payload) => {
                if let Some(req) = BlockRequest::from_payload(&payload) {
                    self.upload_queue
                        .lock()
                        .unwrap()
                        .retain(|queued| *queued != req);
                    debug!("peer canceled request {:?}", req);
                }
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
        have_broadcast: tokio::sync::broadcast::Receiver<u32>,
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
        self.send_initial_bitfield(&protocol, &mut stream).await?;
        self.manage_established(stream, protocol, peer_writer_rx, have_broadcast)
            .await
    }

    pub async fn manage_incoming_stream(
        &self,
        mut stream: TcpStream,
        peer_writer_rx: flume::Receiver<WriterRequest>,
        have_broadcast: tokio::sync::broadcast::Receiver<u32>,
    ) -> anyhow::Result<()> {
        let protocol = Arc::new(Protocol::connect(self.peer, self.info_hash, self.peer_id).await?);
        self.send_initial_bitfield(&protocol, &mut stream).await?;
        self.manage_established(stream, protocol, peer_writer_rx, have_broadcast)
            .await
    }

    async fn send_initial_bitfield(
        &self,
        protocol: &Protocol,
        stream: &mut TcpStream,
    ) -> anyhow::Result<()> {
        let bitfield = self.handler.torrent_downloaded_state.our_bitfield();
        if !bitfield.is_empty() {
            protocol.send_bitfield(stream, bitfield.as_bytes()).await?;
        }
        Ok(())
    }

    async fn manage_established(
        &self,
        mut stream: TcpStream,
        protocol: Arc<Protocol>,
        peer_writer_rx: flume::Receiver<WriterRequest>,
        mut have_broadcast: tokio::sync::broadcast::Receiver<u32>,
    ) -> anyhow::Result<()> {
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

pub struct SpawnPeerParams {
    pub peer: PeerAddr,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub piece_tx: flume::Sender<FullPiece>,
    pub have_broadcast: Arc<tokio::sync::broadcast::Sender<u32>>,
    pub torrent_downloaded_state: Arc<TorrentDownloadedState>,
    pub peer_states: Arc<PeerStates>,
    pub download_state: Arc<Mutex<DownloadState>>,
    pub storage: Arc<Storage>,
    pub uploaded: Arc<AtomicU64>,
    pub torrent: Arc<Torrent>,
    pub choke_notify: Arc<Notify>,
    pub incoming: Option<TcpStream>,
    pub global_peers: Arc<std::sync::atomic::AtomicUsize>,
    pub max_peers_per_torrent: usize,
    pub max_peers_global: usize,
}

pub fn try_spawn_peer(params: SpawnPeerParams) -> bool {
    let global = params.global_peers.load(Ordering::Relaxed);
    if global >= params.max_peers_global {
        debug!(peer = %params.peer, "global connection cap reached");
        return false;
    }
    if params.peer_states.len() >= params.max_peers_per_torrent {
        debug!(peer = %params.peer, "per-torrent connection cap reached");
        return false;
    }

    let (peer_writer_tx, peer_writer_rx) = flume::unbounded();
    if !params
        .peer_states
        .insert_live(params.peer, peer_writer_tx.clone())
    {
        return false;
    }
    params.global_peers.fetch_add(1, Ordering::Relaxed);

    tokio::spawn(async move {
        let handler = Arc::new(PeerHandler::from_config(PeerHandlerConfig {
            peer: params.peer,
            piece_tx: params.piece_tx,
            peer_writer_tx,
            peers_state: params.peer_states.clone(),
            torrent_downloaded_state: params.torrent_downloaded_state,
            download_state: params.download_state,
            storage: params.storage,
            uploaded: params.uploaded,
            torrent: params.torrent,
            choke_notify: params.choke_notify,
        }));
        let connection = PeerConnection::new(
            params.peer,
            params.info_hash,
            params.peer_id,
            handler.clone(),
        );
        let requester = handler.task_peer_chunk_requester();
        let uploader = handler.task_peer_uploader();
        let have_rx = params.have_broadcast.subscribe();
        let result = match params.incoming {
            Some(stream) => {
                tokio::select! {
                    r = connection.manage_incoming_stream(stream, peer_writer_rx, have_rx) => r,
                    r = requester => r,
                    r = uploader => r,
                }
            }
            None => {
                tokio::select! {
                    r = connection.manage_peer_incoming(peer_writer_rx, have_rx) => r,
                    r = requester => r,
                    r = uploader => r,
                }
            }
        };
        if let Err(e) = result {
            debug!("error managing peer: {:#}", e);
        }
        handler.on_peer_died();
        params.global_peers.fetch_sub(1, Ordering::Relaxed);
    });
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn piece(index: u32, length: u32) -> PieceWorkState {
        PieceWorkState {
            piece_work: PieceWork {
                index,
                length,
                hash: [0; 20],
            },
            chuncks: Mutex::new(vec![]),
            downloaded: AtomicBool::new(false),
            reserved: Mutex::new(None),
        }
    }

    fn state(n: u32, piece_len: u32) -> TorrentDownloadedState {
        TorrentDownloadedState {
            semaphore: Semaphore::new(0),
            pieces: (0..n).map(|i| piece(i, piece_len)).collect(),
        }
    }

    fn peer(port: u16) -> PeerAddr {
        (std::net::Ipv4Addr::LOCALHOST, port).into()
    }

    #[tokio::test]
    async fn get_and_reserve_piece_assigns_distinct_peers_then_steals_without_overwrite() {
        let s = state(3, 16);
        let p1 = peer(6881);
        let p2 = peer(6882);
        let p3 = peer(6883);
        let p4 = peer(6884);

        let a = s.get_and_reserve_piece(p1).await.unwrap();
        assert_eq!(a.piece_work.index, 0);
        assert_eq!(*a.reserved.lock().unwrap(), Some(p1));

        let b = s.get_and_reserve_piece(p2).await.unwrap();
        assert_eq!(b.piece_work.index, 1);
        assert_eq!(*b.reserved.lock().unwrap(), Some(p2));

        let c = s.get_and_reserve_piece(p3).await.unwrap();
        assert_eq!(c.piece_work.index, 2);
        assert_eq!(*c.reserved.lock().unwrap(), Some(p3));

        let stolen = s.get_and_reserve_piece(p4).await.unwrap();
        assert_eq!(stolen.piece_work.index, 0);
        assert_eq!(*stolen.reserved.lock().unwrap(), Some(p1));
        assert_eq!(*s.pieces[0].reserved.lock().unwrap(), Some(p1));
        assert_eq!(*s.pieces[1].reserved.lock().unwrap(), Some(p2));
        assert_eq!(*s.pieces[2].reserved.lock().unwrap(), Some(p3));
    }

    #[tokio::test]
    async fn remove_reserved_clears_only_that_peer() {
        let s = state(3, 16);
        let p1 = peer(6881);
        let p2 = peer(6882);
        let p3 = peer(6883);

        s.get_and_reserve_piece(p1).await.unwrap();
        s.get_and_reserve_piece(p2).await.unwrap();
        s.get_and_reserve_piece(p3).await.unwrap();

        s.remove_reserved(p2);

        assert_eq!(*s.pieces[0].reserved.lock().unwrap(), Some(p1));
        assert!(s.pieces[1].reserved.lock().unwrap().is_none());
        assert_eq!(*s.pieces[2].reserved.lock().unwrap(), Some(p3));
    }

    #[test]
    fn set_chuncks_then_downloaded_when_lengths_sum() {
        let s = state(2, 16);

        s.set_chuncks(0, 0, vec![0u8; 8]);
        assert!(s.set_downloaded_if_all_chunks(0).is_none());
        assert!(!s.pieces[0].downloaded.load(Ordering::Relaxed));
        assert!(!s.is_complete());

        s.set_chuncks(0, 8, vec![1u8; 8]);
        let done = s.set_downloaded_if_all_chunks(0);
        assert!(done.is_some());
        assert_eq!(done.unwrap().piece_work.index, 0);
        assert!(s.pieces[0].downloaded.load(Ordering::Relaxed));
        assert!(!s.is_complete());

        s.set_chuncks(1, 0, vec![2u8; 16]);
        assert!(s.set_downloaded_if_all_chunks(1).is_some());
        assert!(s.pieces[1].downloaded.load(Ordering::Relaxed));
        assert!(s.is_complete());
    }

    #[test]
    fn has_piece_and_bitfield_follow_downloaded_flags() {
        let s = state(3, 16);
        assert!(!s.has_piece(0));
        assert_eq!(s.piece_length(1), Some(16));
        assert_eq!(s.piece_count(), 3);
        s.mark_downloaded(1);
        assert!(s.has_piece(1));
        let bf = s.our_bitfield();
        assert!(!bf.has_piece(0));
        assert!(bf.has_piece(1));
        s.mark_all_downloaded();
        assert!(s.is_complete());
        assert!(s.has_piece(0) && s.has_piece(2));
    }

    #[test]
    fn remove_downloaded_clears_chunks_and_flag() {
        let s = state(1, 16);
        s.set_chuncks(0, 0, vec![7u8; 16]);
        assert!(s.set_downloaded_if_all_chunks(0).is_some());
        assert!(s.pieces[0].downloaded.load(Ordering::Relaxed));
        assert_eq!(s.pieces[0].chuncks.lock().unwrap().len(), 1);

        s.remove_downloaded(0);
        assert!(!s.pieces[0].downloaded.load(Ordering::Relaxed));
        assert!(s.pieces[0].chuncks.lock().unwrap().is_empty());
        assert!(!s.is_complete());
    }

    #[tokio::test]
    async fn concurrent_reservation_completes_each_piece_once() {
        const N: u16 = 16;
        const M: u32 = 8;
        const PIECE_LEN: u32 = 16;

        let s = Arc::new(state(M, PIECE_LEN));
        let mut set = tokio::task::JoinSet::new();

        for i in 0..N {
            let s = Arc::clone(&s);
            let peer = peer(6881 + i);
            set.spawn(async move {
                let mut completed = 0u32;
                while let Some(pw) = s.get_and_reserve_piece(peer).await {
                    let reserved_by_me = pw.reserved.lock().unwrap().as_ref() == Some(&peer);
                    if reserved_by_me {
                        let index = pw.piece_work.index;
                        s.set_chuncks(index, 0, vec![0u8; PIECE_LEN as usize]);
                        s.set_downloaded_if_all_chunks(index);
                        completed += 1;
                    } else {
                        tokio::task::yield_now().await;
                    }
                }
                completed
            });
        }

        let mut total_completed = 0u32;
        while let Some(res) = set.join_next().await {
            total_completed += res.unwrap();
        }

        assert_eq!(total_completed, M);
        assert!(s.is_complete());
        for pw in s.pieces.iter() {
            assert!(pw.downloaded.load(Ordering::Relaxed));
            let reserved = pw.reserved.lock().unwrap();
            assert!(reserved.is_some());
        }
    }
}
