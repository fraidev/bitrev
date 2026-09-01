use std::fmt::Display;

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum MessageId {
    MsgChoke = 0,
    MsgUnchoke = 1,
    MsgInterested = 2,
    MsgNotInterested = 3,
    MsgHave = 4,
    MsgBitfield = 5,
    MsgRequest = 6,
    MsgPiece = 7,
    MsgCancel = 8,
    MsgReject = 16,
    MsgHashRequest = 21,
    MsgHashes = 22,
    MsgHashReject = 23,
}

#[derive(Debug)]
pub enum WriterRequest {
    Message(Message),
    //ReadChunkRequest(ChunkInfo),
    //Disconnect(anyhow::Result<()>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request(Vec<u8>),
    Piece(PieceChunk),
    Cancel(Vec<u8>),
    Reject,
    HashRequest,
    Hashes(Vec<u8>),
    HashReject,
    KeepAlive,
}

impl From<MessageInner> for Message {
    fn from(inner: MessageInner) -> Self {
        match inner.id {
            MessageId::MsgChoke => Message::Choke,
            MessageId::MsgUnchoke => Message::Unchoke,
            MessageId::MsgInterested => Message::Interested,
            MessageId::MsgNotInterested => Message::NotInterested,
            MessageId::MsgHave => {
                Message::Have(u32::from_be_bytes(inner.payload[0..4].try_into().unwrap()))
            }
            MessageId::MsgBitfield => Message::Bitfield(inner.payload),
            MessageId::MsgRequest => Message::Request(inner.payload[0..12].to_vec()),
            MessageId::MsgPiece => {
                let index = u32::from_be_bytes(inner.payload[0..4].try_into().unwrap());
                let start = u32::from_be_bytes(inner.payload[4..8].try_into().unwrap()) as usize;
                let data = inner.payload[8..].to_vec();
                Message::Piece(PieceChunk {
                    index,
                    start: start as u32,
                    length: data.len() as u32,
                    data,
                })
            }
            MessageId::MsgCancel => Message::Cancel(inner.payload[0..12].to_vec()),
            MessageId::MsgReject => Message::Reject,
            MessageId::MsgHashRequest => Message::HashRequest,
            MessageId::MsgHashes => Message::Hashes(inner.payload),
            MessageId::MsgHashReject => Message::HashReject,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MessageInner {
    pub id: MessageId,
    pub payload: Vec<u8>,
}

impl Display for MessageInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = match self.id {
            MessageId::MsgChoke => "CHOKE",
            MessageId::MsgUnchoke => "UNCHOKE",
            MessageId::MsgInterested => "INTERESTED",
            MessageId::MsgNotInterested => "NOT_INTERESTED",
            MessageId::MsgHave => "HAVE",
            MessageId::MsgBitfield => "BITFIELD",
            MessageId::MsgRequest => "REQUEST",
            MessageId::MsgPiece => "PIECE",
            MessageId::MsgCancel => "CANCEL",
            MessageId::MsgReject => "REJECT",
            MessageId::MsgHashRequest => "HASH_REQUEST",
            MessageId::MsgHashes => "HASHES",
            MessageId::MsgHashReject => "HASH_REJECT",
        };
        write!(f, "{}", id)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageError {
    InvalidMessageId(String),
    InvalidPayload(String),
}

pub const MAX_INCOMING_REQUEST_LENGTH: u32 = 128 * 1024;
pub const MAX_UPLOAD_QUEUE: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockRequest {
    pub index: u32,
    pub begin: u32,
    pub length: u32,
}

impl BlockRequest {
    pub fn from_payload(payload: &[u8]) -> Option<Self> {
        if payload.len() < 12 {
            return None;
        }
        Some(Self {
            index: u32::from_be_bytes(payload[0..4].try_into().ok()?),
            begin: u32::from_be_bytes(payload[4..8].try_into().ok()?),
            length: u32::from_be_bytes(payload[8..12].try_into().ok()?),
        })
    }

    pub fn to_payload(self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(12);
        payload.extend_from_slice(&self.index.to_be_bytes());
        payload.extend_from_slice(&self.begin.to_be_bytes());
        payload.extend_from_slice(&self.length.to_be_bytes());
        payload
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    MissingPiece,
    InvalidLength,
    OutOfBounds,
}

pub fn validate_request(
    req: &BlockRequest,
    piece_length: u32,
    have_piece: bool,
) -> Result<(), RequestError> {
    if !have_piece {
        return Err(RequestError::MissingPiece);
    }
    if req.length == 0 || req.length > MAX_INCOMING_REQUEST_LENGTH {
        return Err(RequestError::InvalidLength);
    }
    let end = req.begin.checked_add(req.length);
    match end {
        Some(end) if end <= piece_length => Ok(()),
        _ => Err(RequestError::OutOfBounds),
    }
}

pub fn format_request(index: u32, start: u32, length: u32) -> Message {
    Message::Request(
        BlockRequest {
            index,
            begin: start,
            length,
        }
        .to_payload(),
    )
}

pub fn format_piece(index: u32, begin: u32, data: Vec<u8>) -> Message {
    Message::Piece(PieceChunk {
        index,
        start: begin,
        length: data.len() as u32,
        data,
    })
}

pub fn format_have(index: u32) -> Message {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&index.to_be_bytes());
    Message::Have(index)
}

//pub fn parse_piece(buf: &mut [u8]) -> Result<Piece, MessageError> {
//    //match msg {
//    //    Message::Piece(msg) => {
//    //        if msg.start.len() < 8 {
//    //            return Err(MessageError::InvalidPayload(format!(
//    //                "Payload too short. {} < 8",
//    //                msg.payload.len()
//    //            )));
//    //        }
//    //        let index = u32::from_be_bytes(msg.payload[0..4].try_into().unwrap());
//    //        let start = u32::from_be_bytes(msg.payload[4..8].try_into().unwrap()) as usize;
//    //        if start > (buf.len()) {
//    //            return Err(MessageError::InvalidPayload(format!(
//    //                "Start offset too high. {} >= {}",
//    //                start,
//    //                buf.len()
//    //            )));
//    //        }
//    //        let data = msg.payload[8..].to_vec();
//    //        if start + (data.len()) > (buf.len()) {
//    //            return Err(MessageError::InvalidPayload(format!(
//    //                "Data too long. {} + {} > {}",
//    //                start,
//    //                data.len(),
//    //                buf.len()
//    //            )));
//    //        }
//    //
//    //        buf[start..(start + data.len())].copy_from_slice(data.as_slice());
//    //        //Ok(data.len())
//    //        Ok(Piece {
//    //            index,
//    //            start: start as u32,
//    //            length: data.len() as u32,
//    //        })
//    //    }
//    //}
//    match m
//}

#[derive(Debug, Clone, PartialEq)]
pub struct PieceChunk {
    pub index: u32,
    pub start: u32,
    pub length: u32,
    pub data: Vec<u8>,
}

pub struct PieceFull {
    pub index: u32,
    pub start: u32,
    pub length: u32,
    pub data: Vec<u8>,
}

//pub fn parse_have(msg: Message) -> Result<u32, MessageError> {
//    match msg.id {
//        MessageId::MsgHave => {
//            if msg.payload.len() != 4 {
//                return Err(MessageError::InvalidPayload(format!(
//                    "Expected payload length 4, got length {}",
//                    msg.payload.len()
//                )));
//            }
//            let index = u32::from_be_bytes(msg.payload[0..4].try_into().unwrap());
//            Ok(index)
//        }
//        _ => Err(MessageError::InvalidMessageId(format!(
//            "Expected HAVE (ID {}), got ID {}",
//            MessageId::MsgHave as u8,
//            msg.id as u8
//        ))),
//    }
//}
//
//
pub fn serialize(msg: Option<Message>) -> Vec<u8> {
    match msg {
        None => Vec::with_capacity(4),
        Some(m) => {
            let (id, payload) = match m {
                Message::Choke => (MessageId::MsgChoke, vec![]),
                Message::Unchoke => (MessageId::MsgUnchoke, vec![]),
                Message::Interested => (MessageId::MsgInterested, vec![]),
                Message::NotInterested => (MessageId::MsgNotInterested, vec![]),
                Message::Have(index) => (MessageId::MsgHave, index.to_be_bytes().to_vec()),
                Message::Bitfield(payload) => (MessageId::MsgBitfield, payload),
                Message::Request(payload) => (MessageId::MsgRequest, payload),
                Message::Piece(piece) => {
                    let mut payload = Vec::with_capacity(8 + piece.data.len());
                    payload.extend_from_slice(&piece.index.to_be_bytes());
                    payload.extend_from_slice(&piece.start.to_be_bytes());
                    payload.extend_from_slice(&piece.data);
                    (MessageId::MsgPiece, payload)
                }
                Message::Cancel(payload) => (MessageId::MsgCancel, payload),
                Message::Reject => (MessageId::MsgReject, vec![]),
                Message::HashRequest => (MessageId::MsgHashRequest, vec![]),
                Message::Hashes(payload) => (MessageId::MsgHashes, payload),
                Message::HashReject => (MessageId::MsgHashReject, vec![]),
                Message::KeepAlive => return vec![0, 0, 0, 0],
            };

            let length = (payload.len() + 1) as u32;
            let mut buf = Vec::with_capacity(4 + length as usize);
            buf.extend_from_slice(&length.to_be_bytes());
            buf.push(id as u8);
            buf.extend_from_slice(&payload);
            buf
        }
    }
}

pub fn read(length_buf: &[u8], message_buf: &[u8]) -> Option<Message> {
    let length_buf: [u8; 4] = length_buf.try_into().ok()?;
    let length = u32::from_be_bytes(length_buf);
    if length == 0 {
        return None;
    }
    let length = length as usize;
    if message_buf.is_empty() || message_buf.len() < length {
        return None;
    }

    let id = message_buf[0];
    let payload = message_buf[1..length].to_vec();

    let message_id = match id {
        0 => MessageId::MsgChoke,
        1 => MessageId::MsgUnchoke,
        2 => MessageId::MsgInterested,
        3 => MessageId::MsgNotInterested,
        4 => MessageId::MsgHave,
        5 => MessageId::MsgBitfield,
        6 => MessageId::MsgRequest,
        7 => MessageId::MsgPiece,
        8 => MessageId::MsgCancel,
        16 => MessageId::MsgReject,
        21 => MessageId::MsgHashRequest,
        22 => MessageId::MsgHashes,
        23 => MessageId::MsgHashReject,
        _ => return None,
    };

    match message_id {
        MessageId::MsgHave if payload.len() < 4 => return None,
        MessageId::MsgRequest | MessageId::MsgCancel if payload.len() < 12 => return None,
        MessageId::MsgPiece if payload.len() < 8 => return None,
        _ => {}
    }

    Some(
        (MessageInner {
            id: message_id,
            payload,
        })
        .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_request_test() {
        let expected = vec![
            0x00, 0x00, 0x00, 0x04, // Index
            0x00, 0x00, 0x02, 0x37, // Begin
            0x00, 0x00, 0x10, 0xe1, // Length
        ];
        let index = 4;
        let start = 567;
        let length = 4321;
        let msg = format_request(index, start, length);

        assert!(matches!(msg, Message::Request(payload) if payload == expected));
    }

    #[test]
    fn block_request_round_trip() {
        let req = BlockRequest {
            index: 4,
            begin: 567,
            length: 4321,
        };
        assert_eq!(BlockRequest::from_payload(&req.to_payload()), Some(req));
        assert_eq!(BlockRequest::from_payload(&[0u8; 11]), None);
    }

    #[test]
    fn validate_request_accepts_in_range() {
        let req = BlockRequest {
            index: 0,
            begin: 0,
            length: 16 * 1024,
        };
        assert_eq!(validate_request(&req, 32 * 1024, true), Ok(()));
        let last = BlockRequest {
            index: 0,
            begin: 16 * 1024,
            length: 16 * 1024,
        };
        assert_eq!(validate_request(&last, 32 * 1024, true), Ok(()));
    }

    #[test]
    fn validate_request_rejects_oversized_and_out_of_bounds() {
        let oversized = BlockRequest {
            index: 0,
            begin: 0,
            length: MAX_INCOMING_REQUEST_LENGTH + 1,
        };
        assert_eq!(
            validate_request(&oversized, 1024 * 1024, true),
            Err(RequestError::InvalidLength)
        );

        let zero = BlockRequest {
            index: 0,
            begin: 0,
            length: 0,
        };
        assert_eq!(
            validate_request(&zero, 16 * 1024, true),
            Err(RequestError::InvalidLength)
        );

        let past_end = BlockRequest {
            index: 0,
            begin: 16 * 1024,
            length: 1,
        };
        assert_eq!(
            validate_request(&past_end, 16 * 1024, true),
            Err(RequestError::OutOfBounds)
        );

        let overflow = BlockRequest {
            index: 0,
            begin: u32::MAX,
            length: 1,
        };
        assert_eq!(
            validate_request(&overflow, u32::MAX, true),
            Err(RequestError::OutOfBounds)
        );

        let missing = BlockRequest {
            index: 3,
            begin: 0,
            length: 16,
        };
        assert_eq!(
            validate_request(&missing, 16 * 1024, false),
            Err(RequestError::MissingPiece)
        );
    }

    #[test]
    fn format_have_test() {
        let index = 4;
        let msg = format_have(index);

        assert!(matches!(msg, Message::Have(index) if index == 4));
    }

    // #[test]
    // fn parse_piece_test() {
    //     let buf = &mut [0u8; 10];
    //     let msg = Message {
    //         id: MessageId::MsgPiece,
    //         payload: vec![
    //             0x00, 0x00, 0x00, 0x04, // Index
    //             0x00, 0x00, 0x00, 0x02, // Begin
    //             0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, // Length
    //         ],
    //     };
    //
    //     let expected_buf = vec![0x00, 0x00, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x00];
    //
    //     let expected_result = parse_piece(buf, msg);
    //
    //     assert_eq!(
    //         expected_result,
    //         Ok(Piece {
    //             index: 4,
    //             start: 2,
    //             length: 6,
    //         })
    //     );
    //     assert_eq!(buf, expected_buf.as_slice());
    // }

    // #[test]
    // fn parse_have_test() {
    //     let msg = Message::Have(4);
    //     let expected_result = parse_have(msg);
    //     assert_eq!(expected_result, Ok(4));
    // }

    #[test]
    fn serialize_test() {
        let msg = Message::Piece(PieceChunk {
            index: 4,
            start: 0,
            length: 4,
            data: vec![0x00, 0x00, 0x00, 0x04],
        });
        let expected = vec![
            0x00, 0x00, 0x00, 0x0d, 0x07, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x04,
        ];
        let result = serialize(Some(msg));
        assert_eq!(result, expected);
    }

    #[test]
    fn read_test() {
        let length_buf = vec![0x00, 0x00, 0x00, 0x05];
        let message_buf = vec![0x04, 0x00, 0x00, 0x00, 0x04];
        let expected = Message::Have(4);

        let result = read(&length_buf, &message_buf);
        assert_eq!(result, Some(expected));
    }

    fn round_trip(msg: Message) -> Option<Message> {
        let bytes = serialize(Some(msg));
        read(&bytes[0..4], &bytes[4..])
    }

    #[test]
    fn serialize_read_round_trip_all_variants() {
        let cases = [
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(42),
            Message::Bitfield(vec![0b1010_0001, 0b0000_1111]),
            Message::Request(vec![
                0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x02, 0x37, 0x00, 0x00, 0x10, 0xe1,
            ]),
            Message::Piece(PieceChunk {
                index: 7,
                start: 16384,
                length: 4,
                data: vec![0xaa, 0xbb, 0xcc, 0xdd],
            }),
            Message::Cancel(vec![
                0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00,
            ]),
            Message::Reject,
            Message::HashRequest,
            Message::Hashes(vec![0x01, 0x02, 0x03]),
            Message::HashReject,
        ];

        for msg in cases {
            let expected = msg.clone();
            assert_eq!(round_trip(msg), Some(expected));
        }

        // Keep-alive serializes to a zero length prefix. read maps length 0 to None.
        assert_eq!(serialize(Some(Message::KeepAlive)), vec![0, 0, 0, 0]);
        assert_eq!(round_trip(Message::KeepAlive), None);
    }

    #[test]
    fn read_keep_alive_length_zero() {
        assert_eq!(read(&[0, 0, 0, 0], &[]), None);
    }

    #[test]
    fn read_truncated_payloads() {
        let cases: &[(&[u8], &[u8])] = &[
            // Have claims length 5 (id + 4) but only the id byte is present
            (&[0, 0, 0, 5], &[4]),
            // Have with a present but too-short payload
            (&[0, 0, 0, 2], &[4, 0]),
            // Piece payload shorter than 8 bytes
            (&[0, 0, 0, 5], &[7, 0, 0, 0, 1]),
            (&[0, 0, 0, 8], &[7, 0, 0, 0, 1, 0, 0, 0]),
            // Request payload shorter than 12 bytes
            (&[0, 0, 0, 9], &[6, 0, 0, 0, 1, 0, 0, 0, 0]),
            (&[0, 0, 0, 12], &[6, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]),
        ];

        for (length_buf, message_buf) in cases {
            assert_eq!(read(length_buf, message_buf), None);
        }
    }

    #[test]
    fn read_unknown_id() {
        let cases: &[(&[u8], &[u8])] = &[
            (&[0, 0, 0, 1], &[99]),
            (&[0, 0, 0, 3], &[9, 0, 1]),
            (&[0, 0, 0, 1], &[255]),
        ];

        for (length_buf, message_buf) in cases {
            assert_eq!(read(length_buf, message_buf), None);
        }
    }

    #[test]
    fn read_empty_buffer() {
        assert_eq!(read(&[], &[]), None);
        assert_eq!(read(&[0, 0, 0, 1], &[]), None);
    }

    #[test]
    fn piece_framing_payload_sizes() {
        use rand::{Rng, RngCore};

        let mut rng = rand::thread_rng();
        let mut sizes = vec![0usize, 1, 16, 16383, 16384, 40000];
        for _ in 0..5 {
            sizes.push(rng.gen_range(2..4096));
        }

        for size in sizes {
            let mut data = vec![0u8; size];
            rng.fill_bytes(&mut data);

            let index = 3u32;
            let start = 16u32;
            let msg = Message::Piece(PieceChunk {
                index,
                start,
                length: size as u32,
                data: data.clone(),
            });

            let bytes = serialize(Some(msg));
            let length = u32::from_be_bytes(bytes[0..4].try_into().unwrap());
            assert_eq!(length, 1 + 8 + size as u32);

            let parsed = read(&bytes[0..4], &bytes[4..]).expect("piece should parse");
            assert_eq!(
                parsed,
                Message::Piece(PieceChunk {
                    index,
                    start,
                    length: size as u32,
                    data,
                })
            );
        }
    }
}
