use crate::handshake::{Handshake, HandshakeError, MAX_PSTR_LEN};
use crate::message;
use crate::message::{Message, WriterRequest};
use crate::peer::PeerAddr;
use byteorder::{BigEndian, ByteOrder};
use std::io::ErrorKind;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::error::Elapsed;

/// Upper bound on a peer-supplied length prefix. Covers a 16 KiB block and a
/// bitfield for very large torrents. Never allocate before this check.
pub const MAX_MESSAGE_LEN: u32 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerTimeouts {
    pub connect: Duration,
    pub handshake: Duration,
    pub read_step: Duration,
    pub idle: Duration,
    pub handshake_to_first: Duration,
    pub keep_alive: Duration,
}

impl Default for PeerTimeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(6),
            handshake: Duration::from_secs(3),
            read_step: Duration::from_secs(10),
            idle: Duration::from_secs(180),
            handshake_to_first: Duration::from_secs(20),
            keep_alive: Duration::from_secs(120),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    KeepAlive,
    Message(Message),
    Unknown { id: u8 },
    Eof,
}

#[derive(Error, Debug)]
pub enum ProtocolError {
    #[error("Handshake error: {0}")]
    Handshake(HandshakeError),
    #[error("Timeout: {0}")]
    Timeout(Elapsed),
    #[error("IO error: {0}")]
    Io(std::io::Error),
    #[error("Info hash is not equal")]
    InfoHashIsNotEqual,
    #[error("message too large: {length} bytes")]
    MessageTooLarge { length: u32 },
    #[error("truncated message")]
    Truncated,
    #[error("peer idle timeout")]
    Idle,
    #[error("no message after handshake")]
    HandshakeIdle,
}

#[derive(Debug, Clone)]
pub struct Protocol {
    pub peer: PeerAddr,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
    pub piece_count: Option<usize>,
    pub timeouts: PeerTimeouts,
}

impl Protocol {
    pub async fn connect(
        peer: PeerAddr,
        info_hash: [u8; 20],
        peer_id: [u8; 20],
    ) -> Result<Self, ProtocolError> {
        Ok(Self {
            peer,
            info_hash,
            peer_id,
            piece_count: None,
            timeouts: PeerTimeouts::default(),
        })
    }

    pub fn with_piece_count(mut self, count: usize) -> Self {
        self.piece_count = Some(count);
        self
    }

    pub fn with_timeouts(mut self, timeouts: PeerTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub async fn read(
        &self,
        mut stream: impl AsyncReadExt + Unpin,
    ) -> Result<Frame, ProtocolError> {
        let mut length_buf = [0u8; 4];
        match stream.read_exact(&mut length_buf).await {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(Frame::Eof),
            Err(e) => return Err(ProtocolError::Io(e)),
        }

        let length = BigEndian::read_u32(&length_buf);
        if length == 0 {
            return Ok(Frame::KeepAlive);
        }
        if length > MAX_MESSAGE_LEN {
            return Err(ProtocolError::MessageTooLarge { length });
        }

        let mut id_buf = [0u8; 1];
        match tokio::time::timeout(self.timeouts.read_step, stream.read_exact(&mut id_buf)).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                return Err(ProtocolError::Truncated);
            }
            Ok(Err(e)) => return Err(ProtocolError::Io(e)),
            Err(e) => return Err(ProtocolError::Timeout(e)),
        }

        let remaining = (length as usize) - 1;
        if id_buf[0] == message::MessageId::MsgBitfield as u8 {
            if let Some(count) = self.piece_count {
                if remaining > count.div_ceil(8) {
                    return Err(ProtocolError::MessageTooLarge { length });
                }
            }
        }

        let mut msg_bytes = vec![0u8; length as usize];
        msg_bytes[0] = id_buf[0];
        if remaining > 0 {
            match tokio::time::timeout(
                self.timeouts.read_step,
                stream.read_exact(&mut msg_bytes[1..]),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) if e.kind() == ErrorKind::UnexpectedEof => {
                    return Err(ProtocolError::Truncated);
                }
                Ok(Err(e)) => return Err(ProtocolError::Io(e)),
                Err(e) => return Err(ProtocolError::Timeout(e)),
            }
        }

        match message::read(&length_buf, &msg_bytes) {
            Ok(Message::KeepAlive) => Ok(Frame::KeepAlive),
            Ok(msg) => Ok(Frame::Message(msg)),
            Err(message::DecodeError::UnknownId(id)) => Ok(Frame::Unknown { id }),
            Err(message::DecodeError::Truncated | message::DecodeError::InvalidLengthPrefix) => {
                Err(ProtocolError::Truncated)
            }
        }
    }

    pub async fn read_with_idle(
        &self,
        stream: &mut (impl AsyncReadExt + Unpin),
        last_inbound: tokio::time::Instant,
        seen_first: bool,
        handshake_at: tokio::time::Instant,
    ) -> Result<Frame, ProtocolError> {
        let idle_at = last_inbound + self.timeouts.idle;
        let first_at = handshake_at + self.timeouts.handshake_to_first;
        let deadline = if seen_first {
            idle_at
        } else {
            first_at.min(idle_at)
        };
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                if !seen_first && tokio::time::Instant::now() >= first_at {
                    Err(ProtocolError::HandshakeIdle)
                } else {
                    Err(ProtocolError::Idle)
                }
            }
            result = self.read(stream) => result,
        }
    }

    pub async fn write_with_keepalives(
        write: &mut (impl AsyncWriteExt + Unpin),
        rx: flume::Receiver<WriterRequest>,
        timeouts: &PeerTimeouts,
    ) -> Result<(), ProtocolError> {
        loop {
            let req = match tokio::time::timeout(timeouts.keep_alive, rx.recv_async()).await {
                Ok(Ok(req)) => req,
                Ok(Err(_)) => break,
                Err(_) => WriterRequest::Message(Message::KeepAlive),
            };
            let buf = match req {
                WriterRequest::Disconnect => break,
                WriterRequest::Message(msg) => message::serialize(Some(msg)),
            };
            match tokio::time::timeout(timeouts.read_step, write.write_all(&buf)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(ProtocolError::Io(e)),
                Err(e) => return Err(ProtocolError::Timeout(e)),
            }
        }
        Ok(())
    }

    pub async fn send_request(
        &self,
        mut stream: impl AsyncWriteExt + Unpin,
        index: u32,
        start: u32,
        length: u32,
    ) -> Result<(), ProtocolError> {
        let msg = message::format_request(index, start, length);
        let msg_bytes = message::serialize(Some(msg));
        stream
            .write_all(&msg_bytes)
            .await
            .map_err(ProtocolError::Io)
    }

    pub async fn send_interested(
        &self,
        mut stream: impl AsyncWriteExt + Unpin,
    ) -> Result<(), ProtocolError> {
        let msg = message::Message::Interested;
        let msg_bytes = message::serialize(Some(msg));
        stream
            .write_all(&msg_bytes)
            .await
            .map_err(ProtocolError::Io)
    }

    pub async fn send_not_interested(
        &self,
        mut stream: impl AsyncWriteExt + Unpin,
    ) -> Result<(), ProtocolError> {
        let msg = message::Message::NotInterested;
        let msg_bytes = message::serialize(Some(msg));
        stream
            .write_all(&msg_bytes)
            .await
            .map_err(ProtocolError::Io)
    }

    pub async fn send_unchoke(
        &self,
        mut stream: impl AsyncWriteExt + Unpin,
    ) -> Result<(), ProtocolError> {
        let msg = message::Message::Unchoke;
        let msg_bytes = message::serialize(Some(msg));
        stream
            .write_all(&msg_bytes)
            .await
            .map_err(ProtocolError::Io)
    }

    pub async fn send_choke(
        &self,
        mut stream: impl AsyncWriteExt + Unpin,
    ) -> Result<(), ProtocolError> {
        let msg = message::Message::Choke;
        let msg_bytes = message::serialize(Some(msg));
        stream
            .write_all(&msg_bytes)
            .await
            .map_err(ProtocolError::Io)
    }

    pub async fn send_bitfield(
        &self,
        mut stream: impl AsyncWriteExt + Unpin,
        bitfield: &[u8],
    ) -> Result<(), ProtocolError> {
        let msg = message::Message::Bitfield(bitfield.to_vec());
        let msg_bytes = message::serialize(Some(msg));
        stream
            .write_all(&msg_bytes)
            .await
            .map_err(ProtocolError::Io)
    }

    async fn read_handshake_body(
        stream: &mut (impl AsyncReadExt + Unpin),
    ) -> Result<Handshake, ProtocolError> {
        let protocol_str_len_buf = &mut [0u8; 1];
        stream
            .read_exact(protocol_str_len_buf)
            .await
            .map_err(ProtocolError::Io)?;
        let protocol_str_len = protocol_str_len_buf[0] as usize;
        if protocol_str_len == 0 {
            return Err(ProtocolError::Handshake(
                HandshakeError::ProtocolLengthCantBeZero,
            ));
        }
        if protocol_str_len > MAX_PSTR_LEN {
            return Err(ProtocolError::Handshake(
                HandshakeError::InvalidProtocolLength,
            ));
        }
        let mut handshake_bytes = vec![0u8; protocol_str_len + 48];
        stream
            .read_exact(&mut handshake_bytes)
            .await
            .map_err(ProtocolError::Io)?;
        Handshake::read(protocol_str_len, handshake_bytes).map_err(ProtocolError::Handshake)
    }

    pub async fn read_handshake(
        stream: &mut (impl AsyncReadExt + Unpin),
    ) -> Result<Handshake, ProtocolError> {
        let timeout = tokio::time::timeout(
            PeerTimeouts::default().handshake,
            Self::read_handshake_body(stream),
        )
        .await;

        match timeout {
            Ok(Ok(h)) => Ok(h),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(ProtocolError::Timeout(e)),
        }
    }

    pub async fn write_handshake(
        stream: &mut (impl AsyncWriteExt + Unpin),
        handshake: &Handshake,
    ) -> Result<(), ProtocolError> {
        stream
            .write_all(&handshake.serialize())
            .await
            .map_err(ProtocolError::Io)
    }

    pub async fn send_have(
        &self,
        mut stream: impl AsyncWriteExt + Unpin,
        index: u32,
    ) -> Result<(), ProtocolError> {
        let msg = message::format_have(index);
        let msg_bytes = message::serialize(Some(msg));
        stream
            .write_all(&msg_bytes)
            .await
            .map_err(ProtocolError::Io)
    }

    pub async fn complete_handshake(
        &self,
        stream: &mut (impl AsyncReadExt + AsyncWriteExt + Unpin),
    ) -> Result<Handshake, ProtocolError> {
        let timeout = tokio::time::timeout(self.timeouts.handshake, async {
            let handshake = Handshake::outgoing(self.info_hash, self.peer_id);
            let handshake_bytes = handshake.serialize();
            stream
                .write_all(&handshake_bytes)
                .await
                .map_err(ProtocolError::Io)?;
            Self::read_handshake_body(stream).await
        })
        .await;

        match timeout {
            Ok(Ok(h)) => {
                if h.info_hash != self.info_hash {
                    return Err(ProtocolError::InfoHashIsNotEqual);
                }
                Ok(h)
            }
            Ok(Err(e)) => Err(e),
            Err(e) => Err(ProtocolError::Timeout(e)),
        }
    }

    pub async fn send_message(
        &self,
        mut stream: impl AsyncWriteExt + Unpin,
        msg: Message,
    ) -> Result<(), ProtocolError> {
        let msg_bytes = message::serialize(Some(msg));
        stream
            .write_all(&msg_bytes)
            .await
            .map_err(ProtocolError::Io)
    }

    pub async fn recv_bitfield(
        &self,
        stream: &mut (impl AsyncReadExt + Unpin),
    ) -> Result<Vec<u8>, ProtocolError> {
        let func = async {
            match self.read(stream).await? {
                Frame::Message(Message::Bitfield(b)) => Ok(b),
                // BEP-0003: a peer may omit the bitfield, meaning it has no pieces.
                _ => Ok(Vec::new()),
            }
        };
        match tokio::time::timeout(Duration::from_secs(6), func).await {
            Ok(Ok(b)) => Ok(b),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(ProtocolError::Timeout(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const INFO_HASH: [u8; 20] = [
        134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16, 44, 247, 23, 128, 49, 0, 116,
    ];
    const LOCAL_PEER_ID: [u8; 20] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
    ];
    const REMOTE_PEER_ID: [u8; 20] = [
        20, 19, 18, 17, 16, 15, 14, 13, 12, 11, 10, 9, 8, 7, 6, 5, 4, 3, 2, 1,
    ];

    fn peer_addr() -> PeerAddr {
        "127.0.0.1:6881".parse().unwrap()
    }

    async fn protocol() -> Protocol {
        Protocol::connect(peer_addr(), INFO_HASH, LOCAL_PEER_ID)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn complete_handshake_success_round_trip() {
        let proto = protocol().await;
        let (mut client, mut server) = tokio::io::duplex(256);

        let server_task = tokio::spawn(async move {
            let mut client_hs = [0u8; 68];
            server.read_exact(&mut client_hs).await.unwrap();
            let reply = Handshake::new(INFO_HASH, REMOTE_PEER_ID).serialize();
            server.write_all(&reply).await.unwrap();
        });

        let handshake = proto.complete_handshake(&mut client).await.unwrap();
        assert_eq!(handshake.info_hash, INFO_HASH);
        assert_eq!(handshake.peer_id, REMOTE_PEER_ID);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn complete_handshake_advertises_fast_extension() {
        let proto = protocol().await;
        let (mut client, mut server) = tokio::io::duplex(256);

        let server_task = tokio::spawn(async move {
            let mut client_hs = [0u8; 68];
            server.read_exact(&mut client_hs).await.unwrap();
            assert_eq!(client_hs[27] & crate::handshake::FAST_EXTENSION_FLAG, 0x04);
            assert_eq!(
                client_hs[25] & crate::handshake::EXTENSION_PROTOCOL_FLAG,
                0x10
            );
            assert_eq!(&client_hs[20..25], &[0u8; 5]);
            assert_eq!(client_hs[26], 0);
            let reply = Handshake::new(INFO_HASH, REMOTE_PEER_ID).serialize();
            server.write_all(&reply).await.unwrap();
        });

        proto.complete_handshake(&mut client).await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn recv_bitfield_allows_omitted_or_non_bitfield_first() {
        let proto = protocol().await;

        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&[0, 0, 0, 0]).await.unwrap();
        drop(writer);
        let empty = proto.recv_bitfield(&mut reader).await.unwrap();
        assert!(empty.is_empty());

        let (mut writer, mut reader) = tokio::io::duplex(64);
        let interested = message::serialize(Some(Message::Interested));
        writer.write_all(&interested).await.unwrap();
        drop(writer);
        let empty = proto.recv_bitfield(&mut reader).await.unwrap();
        assert!(empty.is_empty());

        let (mut writer, mut reader) = tokio::io::duplex(64);
        let bitfield = message::serialize(Some(Message::Bitfield(vec![0b1010_0000])));
        writer.write_all(&bitfield).await.unwrap();
        drop(writer);
        let got = proto.recv_bitfield(&mut reader).await.unwrap();
        assert_eq!(got, vec![0b1010_0000]);
    }

    #[tokio::test]
    async fn complete_handshake_info_hash_mismatch() {
        let proto = protocol().await;
        let (mut client, mut server) = tokio::io::duplex(256);
        let other_hash = [0xABu8; 20];

        let server_task = tokio::spawn(async move {
            let mut client_hs = [0u8; 68];
            server.read_exact(&mut client_hs).await.unwrap();
            let reply = Handshake::new(other_hash, REMOTE_PEER_ID).serialize();
            server.write_all(&reply).await.unwrap();
        });

        let err = proto.complete_handshake(&mut client).await.unwrap_err();
        assert!(matches!(err, ProtocolError::InfoHashIsNotEqual));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_not_interested_writes_not_interested_frame() {
        let proto = protocol().await;
        let (mut writer, mut reader) = tokio::io::duplex(64);

        proto.send_not_interested(&mut writer).await.unwrap();

        let expected = message::serialize(Some(Message::NotInterested));
        let interested = message::serialize(Some(Message::Interested));
        let mut buf = vec![0u8; expected.len()];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, expected);
        assert_ne!(buf, interested);
        assert_eq!(buf[4], message::MessageId::MsgNotInterested as u8);
    }

    #[tokio::test]
    async fn send_interested_writes_interested_frame() {
        let proto = protocol().await;
        let (mut writer, mut reader) = tokio::io::duplex(64);

        proto.send_interested(&mut writer).await.unwrap();

        let expected = message::serialize(Some(Message::Interested));
        let mut buf = vec![0u8; expected.len()];
        reader.read_exact(&mut buf).await.unwrap();
        assert_eq!(buf, expected);
        assert_eq!(buf[4], message::MessageId::MsgInterested as u8);
    }

    #[tokio::test]
    async fn read_write_handshake_round_trip() {
        let (mut client, mut server) = tokio::io::duplex(128);
        let expected = Handshake::new(INFO_HASH, REMOTE_PEER_ID);
        let reply = expected.clone();

        let server_task = tokio::spawn(async move {
            Protocol::write_handshake(&mut server, &reply)
                .await
                .unwrap();
        });

        let got = Protocol::read_handshake(&mut client).await.unwrap();
        assert_eq!(got, expected);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn read_keep_alive_returns_keep_alive() {
        let proto = protocol().await;
        let (mut writer, reader) = tokio::io::duplex(64);

        writer.write_all(&[0, 0, 0, 0]).await.unwrap();
        let result = proto.read(reader).await.unwrap();
        assert_eq!(result, Frame::KeepAlive);
    }

    #[tokio::test]
    async fn read_rejects_length_bomb_without_allocating() {
        let proto = protocol().await;
        let (mut writer, reader) = tokio::io::duplex(16);
        writer.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).await.unwrap();
        let err = proto.read(reader).await.unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::MessageTooLarge { length: u32::MAX }
        ));
    }

    #[tokio::test]
    async fn read_truncated_payload_is_error() {
        let proto = protocol().await;
        let (mut writer, reader) = tokio::io::duplex(16);
        writer.write_all(&[0, 0, 0, 5, 4]).await.unwrap();
        drop(writer);
        let err = proto.read(reader).await.unwrap_err();
        assert!(matches!(err, ProtocolError::Truncated));
    }

    #[tokio::test]
    async fn read_unknown_id_is_skipped_then_message() {
        let proto = protocol().await;
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&[0, 0, 0, 1, 99]).await.unwrap();
        writer
            .write_all(&message::serialize(Some(Message::Have(4))))
            .await
            .unwrap();
        assert_eq!(
            proto.read(&mut reader).await.unwrap(),
            Frame::Unknown { id: 99 }
        );
        assert_eq!(
            proto.read(&mut reader).await.unwrap(),
            Frame::Message(Message::Have(4))
        );
    }

    #[tokio::test]
    async fn read_eof_on_closed_stream() {
        let proto = protocol().await;
        let (writer, reader) = tokio::io::duplex(16);
        drop(writer);
        assert_eq!(proto.read(reader).await.unwrap(), Frame::Eof);
    }

    #[tokio::test]
    async fn bitfield_length_is_tightened_when_piece_count_known() {
        let proto = protocol().await.with_piece_count(8);
        let (mut writer, reader) = tokio::io::duplex(32);
        // id=5 bitfield, claims 4 payload bytes; 8 pieces need only 1.
        writer
            .write_all(&[0, 0, 0, 5, 5, 0xFF, 0xFF, 0xFF, 0xFF])
            .await
            .unwrap();
        let err = proto.read(reader).await.unwrap_err();
        assert!(matches!(err, ProtocolError::MessageTooLarge { length: 5 }));
    }

    #[tokio::test]
    async fn idle_peer_dropped_at_three_minutes() {
        tokio::time::pause();
        tokio::spawn(std::future::pending::<()>());
        let proto = protocol().await;
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(&[0, 0, 0, 0]).await.unwrap();
        let handshake_at = tokio::time::Instant::now();
        let frame = proto
            .read_with_idle(&mut reader, handshake_at, false, handshake_at)
            .await
            .unwrap();
        assert_eq!(frame, Frame::KeepAlive);
        let last = tokio::time::Instant::now();

        let proto_idle = proto.clone();
        let handle = tokio::spawn(async move {
            proto_idle
                .read_with_idle(&mut reader, last, true, handshake_at)
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(180)).await;
        let result = handle.await.unwrap();
        assert!(matches!(result, Err(ProtocolError::Idle)));
    }

    #[tokio::test]
    async fn keep_alive_sending_peer_is_kept() {
        tokio::time::pause();
        tokio::spawn(std::future::pending::<()>());
        let proto = protocol().await;
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let handshake_at = tokio::time::Instant::now();
        writer.write_all(&[0, 0, 0, 0]).await.unwrap();
        assert_eq!(
            proto
                .read_with_idle(&mut reader, handshake_at, false, handshake_at)
                .await
                .unwrap(),
            Frame::KeepAlive
        );

        let mut last = tokio::time::Instant::now();
        for _ in 0..3 {
            tokio::time::advance(Duration::from_secs(120)).await;
            writer.write_all(&[0, 0, 0, 0]).await.unwrap();
            let frame = proto
                .read_with_idle(&mut reader, last, true, handshake_at)
                .await
                .expect("keep-alive should keep the connection");
            assert_eq!(frame, Frame::KeepAlive);
            last = tokio::time::Instant::now();
        }
    }

    #[tokio::test]
    async fn writer_sends_keep_alive_at_two_minutes() {
        tokio::time::pause();
        tokio::spawn(std::future::pending::<()>());
        let (mut client, mut server) = tokio::io::duplex(64);
        let (_tx, rx) = flume::unbounded::<WriterRequest>();
        let timeouts = PeerTimeouts::default();
        let writer = tokio::spawn(async move {
            Protocol::write_with_keepalives(&mut client, rx, &timeouts).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(120)).await;

        let mut buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(1), server.read_exact(&mut buf))
            .await
            .expect("keep-alive should arrive")
            .unwrap();
        assert_eq!(buf, [0, 0, 0, 0]);
        writer.abort();
    }

    #[tokio::test]
    async fn no_first_message_dropped_at_twenty_seconds() {
        tokio::time::pause();
        tokio::spawn(std::future::pending::<()>());
        let proto = protocol().await;
        let (_writer, mut reader) = tokio::io::duplex(16);
        let handshake_at = tokio::time::Instant::now();
        let handle = tokio::spawn(async move {
            proto
                .read_with_idle(&mut reader, handshake_at, false, handshake_at)
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(20)).await;
        let result = handle.await.unwrap();
        assert!(matches!(result, Err(ProtocolError::HandshakeIdle)));
    }

    #[tokio::test]
    async fn read_handshake_rejects_oversized_pstrlen() {
        let (mut writer, mut reader) = tokio::io::duplex(32);
        writer.write_all(&[255]).await.unwrap();
        let err = Protocol::read_handshake(&mut reader).await.unwrap_err();
        assert!(matches!(
            err,
            ProtocolError::Handshake(HandshakeError::InvalidProtocolLength)
        ));
    }
}
