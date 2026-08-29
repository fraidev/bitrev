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
}
