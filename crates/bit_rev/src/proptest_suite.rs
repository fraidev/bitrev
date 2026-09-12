use proptest::prelude::*;
use serde_bytes::ByteBuf;

use crate::bitfield::Bitfield;
use crate::file::url_encode_bytes;
use crate::handshake::Handshake;
use crate::message::{self, Message, PieceChunk};
use crate::resume::{self, ResumeData, ResumeFile, RESUME_VERSION};
use crate::torrent::{Torrent, TorrentFileInfo};
use crate::utils::{calculate_bounds_for_piece, calculate_piece_size, map_piece_to_files};

fn config() -> ProptestConfig {
    ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    }
}

fn arb_message() -> impl Strategy<Value = Message> {
    prop_oneof![
        Just(Message::Choke),
        Just(Message::Unchoke),
        Just(Message::Interested),
        Just(Message::NotInterested),
        Just(Message::HaveAll),
        Just(Message::HaveNone),
        Just(Message::HashRequest),
        Just(Message::HashReject),
        Just(Message::KeepAlive),
        any::<u32>().prop_map(Message::Have),
        any::<u32>().prop_map(Message::SuggestPiece),
        any::<u32>().prop_map(Message::AllowedFast),
        prop::collection::vec(any::<u8>(), 0..64).prop_map(Message::Bitfield),
        prop::collection::vec(any::<u8>(), 12).prop_map(Message::Request),
        prop::collection::vec(any::<u8>(), 12).prop_map(Message::Cancel),
        prop::collection::vec(any::<u8>(), 0..32).prop_map(Message::Hashes),
        (any::<u8>(), prop::collection::vec(any::<u8>(), 0..32))
            .prop_map(|(ext_id, payload)| { Message::Extended { ext_id, payload } }),
        (any::<u32>(), any::<u32>(), any::<u32>()).prop_map(|(index, begin, length)| {
            Message::RejectRequest {
                index,
                begin,
                length,
            }
        }),
        (
            any::<u32>(),
            any::<u32>(),
            prop::collection::vec(any::<u8>(), 0..48)
        )
            .prop_map(|(index, start, data)| {
                Message::Piece(PieceChunk {
                    index,
                    start,
                    length: data.len() as u32,
                    data,
                })
            }),
    ]
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn message_serialize_parse_round_trip(msg in arb_message()) {
        let bytes = message::serialize(Some(msg.clone()));
        prop_assert!(bytes.len() >= 4);
        let parsed = message::read(&bytes[..4], &bytes[4..]).expect("round-trip must parse");
        prop_assert_eq!(parsed, msg);
    }

    #[test]
    fn handshake_serialize_from_bytes_round_trip(
        info_hash in any::<[u8; 20]>(),
        peer_id in any::<[u8; 20]>(),
        reserved in any::<[u8; 8]>(),
    ) {
        let handshake = Handshake {
            pstr: "BitTorrent protocol".into(),
            reserved,
            info_hash,
            peer_id,
        };
        let bytes = handshake.serialize();
        prop_assert_eq!(bytes.len(), 68);
        prop_assert_eq!(Handshake::from_bytes(&bytes).unwrap(), handshake);
    }

    #[test]
    fn handshake_reserved_bits_stay_in_bounds(index in 0usize..80, info_hash in any::<[u8; 20]>()) {
        let mut handshake = Handshake::new(info_hash, [0; 20]);
        let before = handshake.reserved;
        handshake.set_reserved_bit(index);
        if index < 64 {
            prop_assert!(handshake.reserved_bit(index));
        } else {
            prop_assert_eq!(handshake.reserved, before);
            prop_assert!(!handshake.reserved_bit(index));
        }
    }

    #[test]
    fn bitfield_has_set_and_out_of_range(
        piece_count in 0usize..=256,
        sets in prop::collection::vec(0usize..320, 0..32),
    ) {
        let mut bitfield = Bitfield::with_piece_count(piece_count);
        let nbytes = bitfield.as_bytes().len();
        for index in &sets {
            bitfield.set_piece(*index);
            if *index / 8 < nbytes {
                prop_assert!(bitfield.has_piece(*index));
            } else {
                prop_assert!(!bitfield.has_piece(*index));
            }
        }
        prop_assert!(!bitfield.has_piece(nbytes.saturating_mul(8).saturating_add(1)));
        let mut again = Bitfield::new(bitfield.as_bytes().to_vec());
        again.set_piece(usize::MAX);
        prop_assert!(!again.has_piece(usize::MAX));
    }

    #[test]
    fn map_piece_to_files_ranges_cover_the_piece(
        lengths in prop::collection::vec(0i64..=200, 1..8),
        piece_length in 1i64..=64,
        piece_pick in 0u32..16,
    ) {
        let mut offset = 0i64;
        let files = lengths
            .iter()
            .enumerate()
            .map(|(i, &length)| {
                let file = TorrentFileInfo {
                    path: vec![format!("f{i}")],
                    length,
                    offset,
                };
                offset += length;
                file
            })
            .collect();
        let torrent = Torrent {
            info_hash: [0; 20],
            piece_hashes: vec![],
            piece_length,
            length: offset,
            files,
            name: "t".into(),
            private: false,
        };
        prop_assume!(torrent.length > 0);
        let n_pieces = (torrent.length as u64).div_ceil(piece_length as u64) as usize;
        let piece_index = piece_pick as usize % n_pieces;
        let mappings = map_piece_to_files(&torrent, piece_index);
        let expected = calculate_piece_size(&torrent, piece_index);
        let sum: usize = mappings.iter().map(|m| m.length).sum();
        prop_assert_eq!(sum, expected);

        let (piece_start, piece_end) = calculate_bounds_for_piece(&torrent, piece_index);
        let mut cursor = piece_start;
        for mapping in &mappings {
            prop_assert!(mapping.file_index < torrent.files.len());
            let file = &torrent.files[mapping.file_index];
            let file_len = file.length as usize;
            prop_assert!(mapping.file_offset <= file_len);
            prop_assert!(mapping.file_offset + mapping.length <= file_len);
            let torrent_off = file.offset as usize + mapping.file_offset;
            prop_assert_eq!(torrent_off, cursor);
            cursor += mapping.length;
        }
        prop_assert_eq!(cursor, piece_end);
    }

    #[test]
    fn resume_encode_decode_round_trip(
        info_hash in any::<[u8; 20]>(),
        bitfield in prop::collection::vec(any::<u8>(), 0..16),
        output_dir in "[a-zA-Z0-9/._-]{0,32}",
        torrent_path in "[a-zA-Z0-9/._-]{0,32}",
        uploaded in any::<i64>(),
        downloaded in any::<i64>(),
        paused in 0i64..=1,
        added_at in any::<i64>(),
        completed_at in any::<i64>(),
        file_name in "[a-zA-Z0-9._-]{1,16}",
        file_length in any::<i64>(),
        mtime in any::<i64>(),
    ) {
        let data = ResumeData {
            version: RESUME_VERSION,
            info_hash: ByteBuf::from(info_hash.to_vec()),
            bitfield: ByteBuf::from(bitfield),
            output_dir,
            files: vec![ResumeFile {
                path: vec![file_name],
                length: file_length,
                mtime,
            }],
            uploaded,
            downloaded,
            torrent_path,
            paused,
            added_at,
            completed_at,
        };
        let bytes = resume::encode(&data).expect("encode");
        let loaded = resume::decode(&bytes).expect("decode");
        prop_assert_eq!(loaded, data);
    }

    #[test]
    fn url_encode_bytes_is_invertible(bytes in prop::collection::vec(any::<u8>(), 0..64)) {
        let encoded = url_encode_bytes(&bytes);
        let decoded = decode_percent_encoding(&encoded).expect("valid percent-encoding");
        prop_assert_eq!(decoded, bytes);
        for ch in encoded.chars() {
            prop_assert!(
                ch.is_ascii_alphanumeric()
                    || matches!(ch, '-' | '.' | '_' | '~' | '%')
                    || ch.is_ascii_hexdigit()
            );
        }
    }
}

fn decode_percent_encoding(input: &str) -> Result<Vec<u8>, ()> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len() {
                return Err(());
            }
            let hi = from_hex(bytes[i + 1])?;
            let lo = from_hex(bytes[i + 2])?;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Ok(out)
}

fn from_hex(b: u8) -> Result<u8, ()> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'A'..=b'F' => Ok(b - b'A' + 10),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(()),
    }
}
