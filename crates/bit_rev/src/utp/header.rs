//! BEP-0029 version-1 packet header and extension 1 (selective ACK).

use std::fmt;

pub const HEADER_LEN: usize = 20;
pub const VERSION: u8 = 1;
pub const EXT_SELECTIVE_ACK: u8 = 1;
pub const MAX_EXTENSION_BYTES: usize = 64;
pub const MAX_PACKET: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Data = 0,
    Fin = 1,
    State = 2,
    Reset = 3,
    Syn = 4,
}

impl PacketType {
    pub fn from_u4(n: u8) -> Option<Self> {
        match n {
            0 => Some(Self::Data),
            1 => Some(Self::Fin),
            2 => Some(Self::State),
            3 => Some(Self::Reset),
            4 => Some(Self::Syn),
            _ => None,
        }
    }

    pub fn consumes_seq(self) -> bool {
        matches!(self, Self::Data | Self::Fin | Self::Syn)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extension {
    pub ty: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub ty: PacketType,
    pub version: u8,
    pub connection_id: u16,
    pub timestamp: u32,
    pub timestamp_diff: u32,
    pub wnd_size: u32,
    pub seq_nr: u16,
    pub ack_nr: u16,
    pub extensions: Vec<Extension>,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    TooShort,
    BadVersion,
    BadType,
    BadExtension,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => write!(f, "uTP packet shorter than 20-byte header"),
            Self::BadVersion => write!(f, "uTP version is not 1"),
            Self::BadType => write!(f, "unknown uTP packet type"),
            Self::BadExtension => write!(f, "truncated or oversized uTP extension"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl Packet {
    pub fn new(ty: PacketType, connection_id: u16, seq_nr: u16, ack_nr: u16) -> Self {
        Self {
            ty,
            version: VERSION,
            connection_id,
            timestamp: 0,
            timestamp_diff: 0,
            wnd_size: 0,
            seq_nr,
            ack_nr,
            extensions: Vec::new(),
            payload: Vec::new(),
        }
    }

    pub fn selective_ack(&self) -> Option<&[u8]> {
        self.extensions
            .iter()
            .find(|ext| ext.ty == EXT_SELECTIVE_ACK)
            .map(|ext| ext.data.as_slice())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(HEADER_LEN + self.payload.len() + 16);
        self.encode_into(&mut buf);
        buf
    }

    pub fn encode_into(&self, buf: &mut Vec<u8>) {
        let type_ver = ((self.ty as u8) << 4) | (self.version & 0x0f);
        buf.push(type_ver);
        buf.push(self.extensions.first().map(|e| e.ty).unwrap_or(0));
        buf.extend_from_slice(&self.connection_id.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.extend_from_slice(&self.timestamp_diff.to_be_bytes());
        buf.extend_from_slice(&self.wnd_size.to_be_bytes());
        buf.extend_from_slice(&self.seq_nr.to_be_bytes());
        buf.extend_from_slice(&self.ack_nr.to_be_bytes());
        for (i, ext) in self.extensions.iter().enumerate() {
            let next = self.extensions.get(i + 1).map(|e| e.ty).unwrap_or(0);
            buf.push(next);
            buf.push(ext.data.len() as u8);
            buf.extend_from_slice(&ext.data);
        }
        buf.extend_from_slice(&self.payload);
    }

    pub fn decode(buf: &[u8]) -> Result<Self, DecodeError> {
        if buf.len() < HEADER_LEN {
            return Err(DecodeError::TooShort);
        }
        let type_ver = buf[0];
        let version = type_ver & 0x0f;
        if version != VERSION {
            return Err(DecodeError::BadVersion);
        }
        let ty = PacketType::from_u4(type_ver >> 4).ok_or(DecodeError::BadType)?;
        let mut ext_ty = buf[1];
        let connection_id = u16::from_be_bytes([buf[2], buf[3]]);
        let timestamp = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
        let timestamp_diff = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
        let wnd_size = u32::from_be_bytes([buf[12], buf[13], buf[14], buf[15]]);
        let seq_nr = u16::from_be_bytes([buf[16], buf[17]]);
        let ack_nr = u16::from_be_bytes([buf[18], buf[19]]);

        let mut offset = HEADER_LEN;
        let mut extensions = Vec::new();
        let mut ext_bytes = 0usize;
        while ext_ty != 0 {
            if offset + 2 > buf.len() {
                return Err(DecodeError::BadExtension);
            }
            let next = buf[offset];
            let len = buf[offset + 1] as usize;
            offset += 2;
            if offset + len > buf.len() {
                return Err(DecodeError::BadExtension);
            }
            ext_bytes = ext_bytes.saturating_add(2 + len);
            if ext_bytes > MAX_EXTENSION_BYTES {
                return Err(DecodeError::BadExtension);
            }
            extensions.push(Extension {
                ty: ext_ty,
                data: buf[offset..offset + len].to_vec(),
            });
            offset += len;
            ext_ty = next;
        }

        Ok(Self {
            ty,
            version,
            connection_id,
            timestamp,
            timestamp_diff,
            wnd_size,
            seq_nr,
            ack_nr,
            extensions,
            payload: buf[offset..].to_vec(),
        })
    }
}

/// Selective ACK bit 0 is `ack_nr + 2`.
pub fn sack_contains(mask: &[u8], ack_nr: u16, seq: u16) -> bool {
    let dist = seq.wrapping_sub(ack_nr.wrapping_add(2));
    let byte = dist as usize / 8;
    let bit = dist % 8;
    mask.get(byte)
        .is_some_and(|b| dist < (mask.len() as u16).saturating_mul(8) && b & (1 << bit) != 0)
}

pub fn sack_set(mask: &mut [u8], ack_nr: u16, seq: u16) {
    let dist = seq.wrapping_sub(ack_nr.wrapping_add(2));
    let byte = dist as usize / 8;
    let bit = dist % 8;
    let bits = (mask.len() as u16).saturating_mul(8);
    if dist < bits {
        if let Some(slot) = mask.get_mut(byte) {
            *slot |= 1 << bit;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // type=ST_SYN (4), ver=1 => 0x41. Big-endian multi-byte fields.
    const SYN_FIXTURE: [u8; 20] = [
        0x41, 0x00, 0x12, 0x34, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x00, 0x10, 0x00,
        0x00, 0x00, 0x01, 0x00, 0x00,
    ];

    // ST_STATE + extension 1, 4-byte sack with bit 0 set (ack_nr+2).
    const STATE_SACK_FIXTURE: [u8; 26] = [
        0x21, 0x01, 0x00, 0x10, 0xaa, 0xbb, 0xcc, 0xdd, 0x00, 0x00, 0x00, 0x64, 0x00, 0x01, 0x00,
        0x00, 0x00, 0x05, 0x00, 0x03, 0x00, 0x04, 0x01, 0x00, 0x00, 0x00,
    ];

    #[test]
    fn syn_header_fixture_round_trip() {
        let pkt = Packet::decode(&SYN_FIXTURE).unwrap();
        assert_eq!(pkt.ty, PacketType::Syn);
        assert_eq!(pkt.version, 1);
        assert_eq!(pkt.connection_id, 0x1234);
        assert_eq!(pkt.timestamp, 0x0102_0304);
        assert_eq!(pkt.timestamp_diff, 0x0506_0708);
        assert_eq!(pkt.wnd_size, 0x0010_0000);
        assert_eq!(pkt.seq_nr, 1);
        assert_eq!(pkt.ack_nr, 0);
        assert!(pkt.extensions.is_empty());
        assert!(pkt.payload.is_empty());
        assert_eq!(pkt.encode(), SYN_FIXTURE);
    }

    #[test]
    fn state_selective_ack_fixture_round_trip() {
        let pkt = Packet::decode(&STATE_SACK_FIXTURE).unwrap();
        assert_eq!(pkt.ty, PacketType::State);
        assert_eq!(pkt.connection_id, 0x0010);
        assert_eq!(pkt.seq_nr, 5);
        assert_eq!(pkt.ack_nr, 3);
        assert_eq!(pkt.timestamp_diff, 100);
        let sack = pkt.selective_ack().unwrap();
        assert_eq!(sack, &[0x01, 0x00, 0x00, 0x00]);
        assert!(sack_contains(sack, 3, 5));
        assert!(!sack_contains(sack, 3, 6));
        assert_eq!(pkt.encode(), STATE_SACK_FIXTURE);
    }

    #[test]
    fn data_payload_follows_header() {
        let mut raw = SYN_FIXTURE.to_vec();
        raw[0] = 0x01;
        raw.extend_from_slice(b"abc");
        let pkt = Packet::decode(&raw).unwrap();
        assert_eq!(pkt.ty, PacketType::Data);
        assert_eq!(pkt.payload, b"abc");
        assert_eq!(pkt.encode(), raw);
    }

    #[test]
    fn decode_rejects_short_and_bad_version() {
        assert_eq!(
            Packet::decode(&SYN_FIXTURE[..19]),
            Err(DecodeError::TooShort)
        );
        let mut bad_ver = SYN_FIXTURE;
        bad_ver[0] = 0x42;
        assert_eq!(Packet::decode(&bad_ver), Err(DecodeError::BadVersion));
        let mut bad_ty = SYN_FIXTURE;
        bad_ty[0] = 0x51;
        assert_eq!(Packet::decode(&bad_ty), Err(DecodeError::BadType));
    }

    #[test]
    fn decode_rejects_truncated_extension() {
        let mut raw = STATE_SACK_FIXTURE.to_vec();
        raw.truncate(23);
        assert_eq!(Packet::decode(&raw), Err(DecodeError::BadExtension));
    }

    #[test]
    fn sack_bit_layout_ack_plus_two_is_bit_zero() {
        let mut mask = [0u8; 4];
        sack_set(&mut mask, 10, 12);
        sack_set(&mut mask, 10, 13);
        sack_set(&mut mask, 10, 20);
        assert_eq!(mask[0], 0b0000_0011);
        assert!(sack_contains(&mask, 10, 20));
        assert!(!sack_contains(&mask, 10, 11));
    }
}
