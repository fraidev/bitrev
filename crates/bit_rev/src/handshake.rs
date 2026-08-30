use thiserror::Error;

#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Handshake {
    pub pstr: String,
    pub reserved: [u8; 8],
    pub info_hash: [u8; 20],
    pub peer_id: [u8; 20],
}

#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum HandshakeError {
    #[error("Protocol length can't be zero")]
    ProtocolLengthCantBeZero,
    #[error("Handshake buffer is too short")]
    BufferTooShort,
}

impl Handshake {
    pub fn new(info_hash: [u8; 20], peer_id: [u8; 20]) -> Self {
        Self {
            pstr: "BitTorrent protocol".to_string(),
            reserved: [0u8; 8],
            info_hash,
            peer_id,
        }
    }

    pub fn reserved_bit(&self, index: usize) -> bool {
        if index >= 64 {
            return false;
        }
        let byte_index = index / 8;
        let offset = index % 8;
        (self.reserved[byte_index] >> (7 - offset)) & 1 != 0
    }

    pub fn set_reserved_bit(&mut self, index: usize) {
        if index >= 64 {
            return;
        }
        let byte_index = index / 8;
        let offset = index % 8;
        self.reserved[byte_index] |= 1 << (7 - offset);
    }

    pub fn serialize(&self) -> Vec<u8> {
        let mut handshake = Vec::new();
        handshake.push(self.pstr.len() as u8);
        handshake.extend(self.pstr.as_bytes());
        handshake.extend(self.reserved);
        handshake.extend(self.info_hash);
        handshake.extend(self.peer_id);
        handshake
    }
    pub fn read(
        protocol_str_len: usize,
        handshake_buf: Vec<u8>,
    ) -> Result<Handshake, HandshakeError> {
        if protocol_str_len == 0 {
            return Err(HandshakeError::ProtocolLengthCantBeZero);
        }
        if handshake_buf.len() < protocol_str_len + 48 {
            return Err(HandshakeError::BufferTooShort);
        }
        let pstr = String::from_utf8_lossy(&handshake_buf[..protocol_str_len]).into_owned();
        let reserved_start = protocol_str_len;
        let info_start = reserved_start + 8;
        let peer_start = info_start + 20;
        let reserved = handshake_buf[reserved_start..info_start]
            .try_into()
            .map_err(|_| HandshakeError::BufferTooShort)?;
        let info_hash = handshake_buf[info_start..peer_start]
            .try_into()
            .map_err(|_| HandshakeError::BufferTooShort)?;
        let peer_id = handshake_buf[peer_start..peer_start + 20]
            .try_into()
            .map_err(|_| HandshakeError::BufferTooShort)?;
        Ok(Handshake {
            pstr,
            reserved,
            info_hash,
            peer_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const HASH_INFO: [u8; 20] = [
        134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16, 44, 247, 23, 128, 49, 0, 116,
    ];
    const PEER_ID: [u8; 20] = [
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20,
    ];

    #[test]
    fn serialize_handshake() {
        let expected = vec![
            19, 66, 105, 116, 84, 111, 114, 114, 101, 110, 116, 32, 112, 114, 111, 116, 111, 99,
            111, 108, 0, 0, 0, 0, 0, 0, 0, 0, 134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90,
            16, 44, 247, 23, 128, 49, 0, 116, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
            16, 17, 18, 19, 20,
        ];
        let handshake = Handshake::new(HASH_INFO, PEER_ID);
        let result = handshake.serialize();

        assert_eq!(result, expected);
    }

    #[test]
    fn sucefull_reading_handshake() {
        let protocol_str_len = 19;
        let handshake_bytes = vec![
            66, 105, 116, 84, 111, 114, 114, 101, 110, 116, 32, 112, 114, 111, 116, 111, 99, 111,
            108, 0, 0, 0, 0, 0, 0, 0, 0, 134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16,
            44, 247, 23, 128, 49, 0, 116, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
            17, 18, 19, 20,
        ];
        let result = Handshake::read(protocol_str_len, handshake_bytes).unwrap();
        let expected = Handshake::new(HASH_INFO, PEER_ID);

        assert_eq!(result, expected);
    }

    #[test]
    fn failure_reading_handshake_when_pstrlen_is_zero() {
        let protocol_str_len = 0;
        let handshake_bytes = vec![
            66, 105, 116, 84, 111, 114, 114, 101, 110, 116, 32, 112, 114, 111, 116, 111, 99, 111,
            108, 0, 0, 0, 0, 0, 0, 0, 0, 134, 212, 200, 0, 36, 164, 105, 190, 76, 80, 188, 90, 16,
            44, 247, 23, 128, 49, 0, 116, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16,
            17, 18, 19, 20,
        ];
        let result = Handshake::read(protocol_str_len, handshake_bytes);

        assert_eq!(result, Err(HandshakeError::ProtocolLengthCantBeZero));
    }

    #[test]
    fn serialize_read_round_trip() {
        let handshake = Handshake::new(HASH_INFO, PEER_ID);
        let serialized = handshake.serialize();
        let protocol_str_len = serialized[0] as usize;
        let rest = serialized[1..].to_vec();
        let read = Handshake::read(protocol_str_len, rest).unwrap();
        assert_eq!(read, handshake);
    }

    #[test]
    fn exact_68_byte_layout_for_standard_pstr() {
        let handshake = Handshake::new(HASH_INFO, PEER_ID);
        let serialized = handshake.serialize();
        assert_eq!(serialized.len(), 68);
        assert_eq!(serialized[0], 19);
        assert_eq!(&serialized[1..20], b"BitTorrent protocol");
        assert_eq!(&serialized[20..28], &[0u8; 8]);
        assert_eq!(&serialized[28..48], &HASH_INFO);
        assert_eq!(&serialized[48..68], &PEER_ID);

        let mut with_reserved = Handshake::new(HASH_INFO, PEER_ID);
        with_reserved.set_reserved_bit(0);
        let serialized = with_reserved.serialize();
        assert_eq!(serialized.len(), 68);
        assert_eq!(serialized[20], 0b1000_0000);
        assert_eq!(&serialized[21..28], &[0u8; 7]);
    }

    #[test]
    fn wrong_length_pstr() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"hello");
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&HASH_INFO);
        buf.extend_from_slice(&PEER_ID);
        assert_eq!(buf.len(), 5 + 48);
        let result = Handshake::read(5, buf).unwrap();
        assert_eq!(result.pstr, "hello");
        assert_eq!(result.pstr.len(), 5);
        assert_eq!(result.info_hash, HASH_INFO);
        assert_eq!(result.peer_id, PEER_ID);

        let short = vec![0u8; 10];
        let result = Handshake::read(19, short);
        assert_eq!(result, Err(HandshakeError::BufferTooShort));
    }

    #[test]
    fn reserved_bit_get_set_across_byte_boundaries() {
        let mut handshake = Handshake::new(HASH_INFO, PEER_ID);
        for index in [0, 7, 8, 63] {
            assert!(!handshake.reserved_bit(index));
        }

        handshake.set_reserved_bit(0);
        handshake.set_reserved_bit(7);
        handshake.set_reserved_bit(8);
        handshake.set_reserved_bit(63);

        assert!(handshake.reserved_bit(0));
        assert!(handshake.reserved_bit(7));
        assert!(handshake.reserved_bit(8));
        assert!(handshake.reserved_bit(63));
        assert!(!handshake.reserved_bit(1));
        assert!(!handshake.reserved_bit(6));
        assert!(!handshake.reserved_bit(9));
        assert!(!handshake.reserved_bit(62));

        handshake.set_reserved_bit(64);
        assert!(!handshake.reserved_bit(64));

        assert_eq!(handshake.reserved[0], 0b1000_0001);
        assert_eq!(handshake.reserved[1], 0b1000_0000);
        assert_eq!(handshake.reserved[7], 0b0000_0001);
    }
}
