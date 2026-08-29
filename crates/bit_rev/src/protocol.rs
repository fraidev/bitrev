use crate::handshake::{Handshake, HandshakeError};
use crate::message;
use crate::message::Message;
use crate::peer::PeerAddr;
use byteorder::{BigEndian, ByteOrder};
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::error::Elapsed;

const HANDSHAKE_TIMEOUT: u64 = 3;

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
    #[error("Expected bitfield id")]
    ExpectedBitfieldId,
    #[error("Message is none")]
    MessageIsNone,
}

#[derive(Debug, Clone)]
pub struct Protocol {
    pub peer: PeerAddr,
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
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
        })
    }

    pub async fn read(
        &self,
        mut stream: impl AsyncReadExt + Unpin,
    ) -> Result<Option<Message>, ProtocolError> {
        // Read exactly 4 bytes for the length
        let mut length_buf = [0u8; 4];
        stream
            .read_exact(&mut length_buf)
            .await
            .map_err(ProtocolError::Io)?;

        let length = BigEndian::read_u32(&length_buf) as usize;

        // Check if length is zero (keep-alive in BT),
        // or is otherwise "invalid" (too large, etc.)
        if length == 0 {
            // Possibly treat as keep-alive or return Ok(None):
            return Ok(None);
        }

        // Read exactly `length` bytes for the payload
        let mut msg_bytes = vec![0u8; length];
        stream
            .read_exact(&mut msg_bytes)
            .await
            .map_err(ProtocolError::Io)?;

        // Delegate to your parser
        Ok(message::read(&length_buf, &msg_bytes))
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
        let timeout = tokio::time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT), async {
            let handshake = Handshake::new(self.info_hash, self.peer_id);
            let handshake_bytes = handshake.serialize();
            stream
                .write_all(&handshake_bytes)
                .await
                .map_err(ProtocolError::Io)?;

            let protocol_str_len_buf = &mut [0u8; 1];
            stream
                .read_exact(protocol_str_len_buf)
                .await
                .map_err(ProtocolError::Io)?;
            let protocol_str_len = protocol_str_len_buf[0] as usize;
            let handshake_bytes = &mut vec![0u8; protocol_str_len + 48];
            stream
                .read_exact(handshake_bytes)
                .await
                .map_err(ProtocolError::Io)?;

            Handshake::read(protocol_str_len, handshake_bytes.to_vec())
                .map_err(ProtocolError::Handshake)
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

    pub async fn recv_bitfield(
        &self,
        stream: &mut (impl AsyncReadExt + Unpin),
    ) -> Result<Vec<u8>, ProtocolError> {
        let func = async {
            match self.read(stream).await? {
                None => Err(ProtocolError::MessageIsNone),
                Some(msg) => match msg {
                    Message::Bitfield(b) => Ok(b),
                    _ => Err(ProtocolError::ExpectedBitfieldId),
                },
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
    async fn read_keep_alive_returns_none() {
        let proto = protocol().await;
        let (mut writer, reader) = tokio::io::duplex(64);

        writer.write_all(&[0, 0, 0, 0]).await.unwrap();
        let result = proto.read(reader).await.unwrap();
        assert_eq!(result, None);
    }
}
