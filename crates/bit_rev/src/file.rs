use serde::Deserialize;
use serde::Serialize;
use serde_bencode::de;
use serde_bencode::ser;
use serde_bytes::ByteBuf;
use std::io::Read;

use anyhow::Result;

const URL_UNRESERVED_HEX: &[u8; 16] = b"0123456789ABCDEF";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Node(String, i64);

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct File {
    pub path: Vec<String>,
    pub length: i64,
    #[serde(default)]
    pub md5sum: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Info {
    pub name: String,
    pub pieces: ByteBuf,
    #[serde(rename = "piece length")]
    pub piece_length: i64,
    #[serde(default)]
    pub md5sum: Option<String>,
    #[serde(default)]
    pub length: Option<i64>,
    #[serde(default)]
    pub files: Option<Vec<File>>,
    #[serde(default)]
    pub private: Option<u8>,
    #[serde(default)]
    pub path: Option<Vec<String>>,
    #[serde(default)]
    #[serde(rename = "root hash")]
    pub root_hash: Option<String>,
}

impl Info {
    /// BEP-0027: `private=1` is private; missing or `0` is public.
    pub fn is_private(&self) -> bool {
        matches!(self.private, Some(flag) if flag != 0)
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TorrentFile {
    pub info: Info,
    #[serde(default)]
    pub announce: Option<String>,
    #[serde(default)]
    pub nodes: Option<Vec<Node>>,
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub httpseeds: Option<Vec<String>>,
    #[serde(default)]
    #[serde(rename = "announce-list")]
    pub announce_list: Option<Vec<Vec<String>>>,
    #[serde(default)]
    #[serde(rename = "creation date")]
    pub creation_date: Option<i64>,
    #[serde(rename = "comment")]
    pub comment: Option<String>,
    #[serde(default)]
    #[serde(rename = "created by")]
    pub created_by: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct TorrentMeta {
    pub torrent_file: TorrentFile,
    pub info_hash: [u8; 20],
    pub piece_hashes: Vec<[u8; 20]>,
}

impl TorrentMeta {
    pub fn new(torrent_file: TorrentFile) -> Result<Self> {
        validate_torrent_file(&torrent_file)?;
        let file_info_bencode = ser::to_bytes(&torrent_file.info)?;
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(&file_info_bencode);
        let info_hash = hasher.digest().bytes();
        Ok(Self::from_validated(torrent_file, info_hash))
    }

    fn from_validated(torrent_file: TorrentFile, info_hash: [u8; 20]) -> Self {
        let piece_hashes = torrent_file.info.pieces.as_chunks::<20>().0.to_vec();

        Self {
            torrent_file,
            info_hash,
            piece_hashes,
        }
    }
}

pub fn from_bytes(content: &[u8]) -> Result<TorrentMeta> {
    let torrent = de::from_bytes::<TorrentFile>(content)?;
    validate_torrent_file(&torrent)?;
    let info_bytes = raw_info_dict(content)?;
    let mut hasher = sha1_smol::Sha1::new();
    hasher.update(info_bytes);
    let info_hash = hasher.digest().bytes();
    Ok(TorrentMeta::from_validated(torrent, info_hash))
}

pub fn from_filename(filename: &str) -> Result<TorrentMeta> {
    let mut file = std::fs::File::open(filename)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;
    from_bytes(&content)
}

fn validate_torrent_file(torrent_file: &TorrentFile) -> Result<()> {
    if torrent_file.info.piece_length <= 0 {
        anyhow::bail!("piece length must be positive");
    }
    if !torrent_file.info.pieces.len().is_multiple_of(20) {
        anyhow::bail!("pieces length must be a multiple of 20");
    }

    match (
        torrent_file.info.length.is_some(),
        torrent_file.info.files.as_ref(),
    ) {
        (true, None) => {}
        (false, Some(files)) => {
            for file in files {
                validate_file_path(&file.path)?;
            }
        }
        _ => anyhow::bail!("exactly one of length or files must be present"),
    }

    Ok(())
}

fn validate_file_path(path: &[String]) -> Result<()> {
    if path.is_empty() {
        anyhow::bail!("file path must not be empty");
    }
    for component in path {
        if !path_component_is_safe(component) {
            anyhow::bail!("unsafe file path component: {component:?}");
        }
    }
    Ok(())
}

fn path_component_is_safe(component: &str) -> bool {
    if component.is_empty() || component == ".." {
        return false;
    }
    if component.starts_with('/') || component.contains('/') || component.contains('\\') {
        return false;
    }
    if component.len() >= 2 && component.as_bytes()[1] == b':' {
        return false;
    }
    true
}

fn parse_bencode_byte_string(data: &[u8], i: usize) -> Result<(&[u8], usize)> {
    let colon = data[i..]
        .iter()
        .position(|&b| b == b':')
        .map(|p| i + p)
        .ok_or_else(|| anyhow::anyhow!("truncated bencode string"))?;
    let len: usize = std::str::from_utf8(&data[i..colon])
        .ok()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("invalid bencode string length"))?;
    let start = colon + 1;
    let end = start
        .checked_add(len)
        .ok_or_else(|| anyhow::anyhow!("bencode string length overflow"))?;
    if end > data.len() {
        anyhow::bail!("truncated bencode string");
    }
    Ok((&data[start..end], end))
}

fn skip_bencode(data: &[u8], i: usize) -> Result<usize> {
    if i >= data.len() {
        anyhow::bail!("truncated bencode");
    }
    match data[i] {
        b'i' => {
            let end = data[i + 1..]
                .iter()
                .position(|&b| b == b'e')
                .map(|p| i + 1 + p)
                .ok_or_else(|| anyhow::anyhow!("truncated bencode integer"))?;
            Ok(end + 1)
        }
        b'l' | b'd' => {
            let mut j = i + 1;
            while j < data.len() && data[j] != b'e' {
                j = skip_bencode(data, j)?;
            }
            if j >= data.len() {
                anyhow::bail!("truncated bencode list or dict");
            }
            Ok(j + 1)
        }
        b'0'..=b'9' => {
            let (_, end) = parse_bencode_byte_string(data, i)?;
            Ok(end)
        }
        _ => anyhow::bail!("invalid bencode"),
    }
}

fn raw_info_dict(data: &[u8]) -> Result<&[u8]> {
    if data.first() != Some(&b'd') {
        anyhow::bail!("torrent metainfo must be a dict");
    }
    let mut i = 1;
    while i < data.len() && data[i] != b'e' {
        let (key, after_key) = parse_bencode_byte_string(data, i)?;
        let value_end = skip_bencode(data, after_key)?;
        if key == b"info" {
            return Ok(&data[after_key..value_end]);
        }
        i = value_end;
    }
    anyhow::bail!("missing info dictionary")
}

/// RFC 3986 unreserved set `A-Z a-z 0-9 - . _ ~` stays literal; every other byte is `%XX`.
/// Encodes raw bytes; never interprets the input as UTF-8.
pub fn url_encode_bytes(content: &[u8]) -> String {
    let mut out = String::with_capacity(content.len() * 3);

    for &byte in content {
        match byte {
            b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(URL_UNRESERVED_HEX[(byte >> 4) as usize] as char);
                out.push(URL_UNRESERVED_HEX[(byte & 0x0f) as usize] as char);
            }
        }
    }

    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnounceEvent {
    Started,
    Stopped,
    Completed,
}

impl AnnounceEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Stopped => "stopped",
            Self::Completed => "completed",
        }
    }

    pub fn as_udp(self) -> u32 {
        match self {
            Self::Completed => 1,
            Self::Started => 2,
            Self::Stopped => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnounceParams {
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub port: u16,
    pub event: Option<AnnounceEvent>,
    pub numwant: u32,
    pub key: u32,
    pub tracker_id: Option<Vec<u8>>,
}

impl AnnounceParams {
    pub const DEFAULT_NUMWANT: u32 = 50;
}

fn push_encoded_pair(url: &mut String, key: &str, value: &[u8]) {
    if !url.ends_with('?') && !url.ends_with('&') {
        let separator = if url.contains('?') { '&' } else { '?' };
        url.push(separator);
    }
    url.push_str(key);
    url.push('=');
    url.push_str(&url_encode_bytes(value));
}

pub fn build_tracker_url(
    torrent_meta: &TorrentMeta,
    peer_id: &[u8],
    tracker_url: &str,
    params: &AnnounceParams,
) -> String {
    let mut url = String::from(tracker_url);

    push_encoded_pair(&mut url, "info_hash", torrent_meta.info_hash.as_ref());
    push_encoded_pair(&mut url, "peer_id", peer_id);
    push_encoded_pair(&mut url, "port", params.port.to_string().as_bytes());
    push_encoded_pair(&mut url, "uploaded", params.uploaded.to_string().as_bytes());
    push_encoded_pair(
        &mut url,
        "downloaded",
        params.downloaded.to_string().as_bytes(),
    );
    push_encoded_pair(&mut url, "left", params.left.to_string().as_bytes());
    push_encoded_pair(&mut url, "compact", b"1");
    if let Some(event) = params.event {
        push_encoded_pair(&mut url, "event", event.as_str().as_bytes());
    }
    push_encoded_pair(&mut url, "numwant", params.numwant.to_string().as_bytes());
    push_encoded_pair(&mut url, "key", params.key.to_string().as_bytes());
    if let Some(tracker_id) = params.tracker_id.as_deref() {
        push_encoded_pair(&mut url, "trackerid", tracker_id);
    }

    url
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO_HASH_FIXTURE: [u8; 20] = [
        0x00, 0x20, 0xFF, b'A', b'z', b'0', b'-', b'.', b'_', b'~', 0x01, 0x7F, 0x80, b'/', b'?',
        b'&', b'=', b'+', 0x0A, 0x0D,
    ];
    const INFO_HASH_ENCODED: &str = "%00%20%FFAz0-._~%01%7F%80%2F%3F%26%3D%2B%0A%0D";

    const PEER_ID: [u8; 20] = [
        b'-', b'B', b'R', 0x00, 0xFF, b' ', b'~', b'_', 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
    ];
    const PEER_ID_ENCODED: &str = "-BR%00%FF%20~_%01%02%03%04%05%06%07%08%09%0A%0B%0C";

    fn test_meta(info_hash: [u8; 20]) -> TorrentMeta {
        TorrentMeta {
            torrent_file: TorrentFile {
                info: Info {
                    name: "test".into(),
                    pieces: ByteBuf::from(info_hash.to_vec()),
                    piece_length: 16384,
                    md5sum: None,
                    length: Some(1_000_000),
                    files: None,
                    private: None,
                    path: None,
                    root_hash: None,
                },
                announce: Some("http://tracker.example/announce".into()),
                nodes: None,
                encoding: None,
                httpseeds: None,
                announce_list: None,
                creation_date: None,
                comment: None,
                created_by: None,
            },
            info_hash,
            piece_hashes: vec![info_hash],
        }
    }

    #[test]
    fn announce_event_udp_codes() {
        assert_eq!(AnnounceEvent::Completed.as_udp(), 1);
        assert_eq!(AnnounceEvent::Started.as_udp(), 2);
        assert_eq!(AnnounceEvent::Stopped.as_udp(), 3);
    }

    fn sample_params(event: Option<AnnounceEvent>) -> AnnounceParams {
        AnnounceParams {
            uploaded: 100,
            downloaded: 200,
            left: 300,
            port: 6881,
            event,
            numwant: AnnounceParams::DEFAULT_NUMWANT,
            key: 0xDF45_C574,
            tracker_id: None,
        }
    }

    fn decode_info_hash(hex: &str) -> [u8; 20] {
        assert_eq!(hex.len(), 40, "info hash hex must be 40 chars");
        let mut out = [0u8; 20];
        for (i, slot) in out.iter_mut().enumerate() {
            *slot = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex digit");
        }
        out
    }

    fn torrent_file_entry(path: &[&str], length: i64) -> File {
        File {
            path: path
                .iter()
                .map(|component| (*component).to_string())
                .collect(),
            length,
            md5sum: None,
        }
    }

    fn torrent_file(
        name: &str,
        piece_length: i64,
        pieces: Vec<u8>,
        length: Option<i64>,
        files: Option<Vec<File>>,
    ) -> TorrentFile {
        TorrentFile {
            info: Info {
                name: name.into(),
                pieces: ByteBuf::from(pieces),
                piece_length,
                md5sum: None,
                length,
                files,
                private: None,
                path: None,
                root_hash: None,
            },
            announce: Some("http://tracker.example/announce".into()),
            nodes: None,
            encoding: None,
            httpseeds: None,
            announce_list: None,
            creation_date: None,
            comment: None,
            created_by: None,
        }
    }

    fn encode_torrent(
        name: &str,
        piece_length: i64,
        pieces: Vec<u8>,
        length: Option<i64>,
        files: Option<Vec<File>>,
    ) -> Vec<u8> {
        ser::to_bytes(&torrent_file(name, piece_length, pieces, length, files))
            .expect("serialize torrent")
    }

    #[test]
    fn url_encode_bytes_encodes_binary_info_hash() {
        assert_eq!(url_encode_bytes(&INFO_HASH_FIXTURE), INFO_HASH_ENCODED);
    }

    #[test]
    fn url_encode_bytes_keeps_unreserved_set() {
        assert_eq!(
            url_encode_bytes(b"ABCXYZabcxyz0123456789-._~"),
            "ABCXYZabcxyz0123456789-._~"
        );
    }

    #[test]
    fn build_tracker_url_appends_question_mark_when_no_query() {
        let url = build_tracker_url(
            &test_meta(INFO_HASH_FIXTURE),
            &PEER_ID,
            "http://tracker.example/announce",
            &sample_params(Some(AnnounceEvent::Started)),
        );
        assert!(url.starts_with("http://tracker.example/announce?info_hash="));
    }

    #[test]
    fn build_tracker_url_appends_ampersand_when_query_present() {
        let url = build_tracker_url(
            &test_meta(INFO_HASH_FIXTURE),
            &PEER_ID,
            "http://tracker.example/announce?passkey=x",
            &sample_params(Some(AnnounceEvent::Started)),
        );
        assert!(url.starts_with("http://tracker.example/announce?passkey=x&info_hash="));
        assert_eq!(url.chars().filter(|&c| c == '?').count(), 1);
    }

    #[test]
    fn build_tracker_url_includes_all_encoded_params() {
        let url = build_tracker_url(
            &test_meta(INFO_HASH_FIXTURE),
            &PEER_ID,
            "http://tracker.example/announce",
            &sample_params(Some(AnnounceEvent::Started)),
        );
        let expected = format!(
            "http://tracker.example/announce?info_hash={INFO_HASH_ENCODED}&peer_id={PEER_ID_ENCODED}&port=6881&uploaded=100&downloaded=200&left=300&compact=1&event=started&numwant=50&key=3745891700"
        );
        assert_eq!(url, expected);
    }

    #[test]
    fn build_tracker_url_omits_event_when_none() {
        let url = build_tracker_url(
            &test_meta(INFO_HASH_FIXTURE),
            &PEER_ID,
            "http://tracker.example/announce",
            &sample_params(None),
        );
        assert!(!url.contains("event="));
        assert!(url.contains("&compact=1&numwant=50&key=3745891700"));
    }

    #[test]
    fn build_tracker_url_encodes_completed_and_stopped_events() {
        let completed = build_tracker_url(
            &test_meta(INFO_HASH_FIXTURE),
            &PEER_ID,
            "http://tracker.example/announce",
            &sample_params(Some(AnnounceEvent::Completed)),
        );
        assert!(completed.contains("&event=completed&"));

        let stopped = build_tracker_url(
            &test_meta(INFO_HASH_FIXTURE),
            &PEER_ID,
            "http://tracker.example/announce",
            &sample_params(Some(AnnounceEvent::Stopped)),
        );
        assert!(stopped.contains("&event=stopped&"));
    }

    #[test]
    fn build_tracker_url_echoes_trackerid() {
        let mut params = sample_params(None);
        params.tracker_id = Some(b"abc".to_vec());
        let url = build_tracker_url(
            &test_meta(INFO_HASH_FIXTURE),
            &PEER_ID,
            "http://tracker.example/announce",
            &params,
        );
        assert!(url.ends_with("&trackerid=abc"));
    }

    #[test]
    fn from_filename_parses_debian_sample() {
        const DEBIAN_SAMPLE: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../samples/debian-13.0.0-amd64-netinst.iso.torrent"
        );
        const DEBIAN_INFO_HASH_HEX: &str = "155a51b44b337d3b147c8d93d9764df48705ff89";

        let meta = from_filename(DEBIAN_SAMPLE).expect("parse debian sample");
        let info = &meta.torrent_file.info;

        assert_eq!(info.name, "debian-13.0.0-amd64-netinst.iso");
        assert_eq!(info.piece_length, 262144);
        assert_eq!(meta.piece_hashes.len(), 3016);
        assert_eq!(info.pieces.len(), 3016 * 20);
        assert_eq!(info.length, Some(790_626_304));
        assert!(info.files.is_none());
        assert_eq!(meta.info_hash, decode_info_hash(DEBIAN_INFO_HASH_HEX));
        assert_eq!(
            meta.torrent_file.announce.as_deref(),
            Some("http://bttracker.debian.org:6969/announce")
        );
        assert!(!info.is_private());
    }

    #[test]
    fn from_bytes_parses_multi_file_fixture_and_offsets() {
        let files = vec![
            torrent_file_entry(
                &["images", "LOC_Main_Reading_Room_Highsmith.jpg"],
                17_614_527,
            ),
            torrent_file_entry(&["images", "melk-abbey-library.jpg"], 1_682_177),
            torrent_file_entry(&["README"], 20),
        ];
        let encoded = encode_torrent("test_folder", 32768, vec![0u8; 3 * 20], None, Some(files));
        let meta = from_bytes(&encoded).expect("parse multi-file fixture");
        let info = &meta.torrent_file.info;

        assert_eq!(info.name, "test_folder");
        assert_eq!(info.piece_length, 32768);
        assert_eq!(meta.piece_hashes.len(), 3);

        let files = info.files.as_ref().expect("multi-file layout");
        assert_eq!(
            files[0].path,
            ["images", "LOC_Main_Reading_Room_Highsmith.jpg"]
        );
        assert_eq!(files[0].length, 17_614_527);
        assert_eq!(files[1].path, ["images", "melk-abbey-library.jpg"]);
        assert_eq!(files[1].length, 1_682_177);
        assert_eq!(files[2].path, ["README"]);
        assert_eq!(files[2].length, 20);

        let torrent = crate::torrent::Torrent::new(&meta).expect("torrent layout");
        assert_eq!(torrent.files[0].offset, 0);
        assert_eq!(torrent.files[1].offset, 17_614_527);
        assert_eq!(torrent.files[2].offset, 17_614_527 + 1_682_177);
        assert_eq!(torrent.length, 17_614_527 + 1_682_177 + 20);
    }

    #[test]
    fn from_bytes_hashes_raw_info_dict() {
        // SHA-1 of the info dict bytes, not a re-encoded struct.
        const RAW: &[u8] = b"d8:announce31:http://tracker.example/announce4:infod6:lengthi4e4:name8:tiny.bin12:piece lengthi16384e6:pieces20:01234567890123456789ee";
        const INFO_HASH_HEX: &str = "fea8fe53535a24b45effde348c0daab127924ff2";

        let meta = from_bytes(RAW).expect("parse tiny fixture");
        assert_eq!(meta.torrent_file.info.name, "tiny.bin");
        assert_eq!(meta.torrent_file.info.piece_length, 16384);
        assert_eq!(meta.torrent_file.info.length, Some(4));
        assert_eq!(meta.piece_hashes.len(), 1);
        assert_eq!(meta.info_hash, decode_info_hash(INFO_HASH_HEX));
    }

    #[test]
    fn from_bytes_rejects_malformed_inputs() {
        let valid = encode_torrent("test", 16384, vec![0u8; 20], Some(4), None);
        let cases: &[(&str, Vec<u8>)] = &[
            ("empty", b"".to_vec()),
            ("truncated dict", b"d4:infod".to_vec()),
            ("truncated valid torrent", valid[..valid.len() / 2].to_vec()),
            (
                "pieces not multiple of 20",
                encode_torrent("test", 16384, vec![0u8; 21], Some(4), None),
            ),
            (
                "both length and files",
                encode_torrent(
                    "test",
                    16384,
                    vec![0u8; 20],
                    Some(4),
                    Some(vec![torrent_file_entry(&["a"], 4)]),
                ),
            ),
            (
                "neither length nor files",
                encode_torrent("test", 16384, vec![0u8; 20], None, None),
            ),
            (
                "piece_length zero",
                encode_torrent("test", 0, vec![0u8; 20], Some(4), None),
            ),
            (
                "piece_length negative",
                encode_torrent("test", -1, vec![0u8; 20], Some(4), None),
            ),
        ];

        for (label, bytes) in cases {
            assert!(
                from_bytes(bytes).is_err(),
                "{label} should be rejected without panicking"
            );
        }
    }

    #[test]
    fn from_bytes_rejects_unsafe_file_paths() {
        let cases: &[&[&str]] = &[
            &["..", "secret"],
            &["dir", ""],
            &["/etc", "passwd"],
            &["foo/bar"],
            &["foo\\bar"],
            &["C:windows"],
        ];

        for path in cases {
            let files = vec![torrent_file_entry(path, 4)];
            let encoded = encode_torrent("test", 16384, vec![0u8; 20], None, Some(files));
            assert!(
                from_bytes(&encoded).is_err(),
                "path {path:?} should be rejected"
            );
        }

        let empty_path = encode_torrent(
            "test",
            16384,
            vec![0u8; 20],
            None,
            Some(vec![torrent_file_entry(&[], 4)]),
        );
        assert!(from_bytes(&empty_path).is_err());
    }

    #[test]
    fn from_bytes_ignores_unknown_keys() {
        let raw = b"d8:announce31:http://tracker.example/announce7:unknown7:ignored4:infod6:lengthi4e4:name4:test12:piece lengthi16384e6:pieces20:012345678901234567896:unused3:baree";
        let meta = from_bytes(raw).expect("unknown keys should be ignored");
        assert_eq!(meta.torrent_file.info.name, "test");
        assert_eq!(meta.torrent_file.info.piece_length, 16384);
        assert_eq!(meta.torrent_file.info.length, Some(4));
        assert_eq!(meta.piece_hashes.len(), 1);
        assert_eq!(
            meta.torrent_file.announce.as_deref(),
            Some("http://tracker.example/announce")
        );
    }

    fn fixture_path(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn from_filename_parses_private_fixture() {
        const PRIVATE_INFO_HASH_HEX: &str = "f40c245c537eb8d8f3519fe1993632a806b6b571";

        let meta = from_filename(&fixture_path("private.torrent")).expect("parse private fixture");
        assert_eq!(meta.torrent_file.info.name, "private.bin");
        assert_eq!(meta.torrent_file.info.private, Some(1));
        assert!(meta.torrent_file.info.is_private());
        assert_eq!(meta.info_hash, decode_info_hash(PRIVATE_INFO_HASH_HEX));

        let torrent = crate::torrent::Torrent::new(&meta).expect("torrent");
        assert!(torrent.is_private());
        assert!(!torrent.allows_dht());
        assert!(!torrent.allows_pex());
        assert!(!torrent.allows_lsd());
    }

    #[test]
    fn from_filename_private_zero_is_public() {
        let meta =
            from_filename(&fixture_path("private-zero.torrent")).expect("parse private=0 fixture");
        assert_eq!(meta.torrent_file.info.private, Some(0));
        assert!(!meta.torrent_file.info.is_private());
        let torrent = crate::torrent::Torrent::new(&meta).expect("torrent");
        assert!(!torrent.is_private());
        assert!(torrent.allows_dht());
    }

    #[test]
    fn from_filename_info_hash_includes_unknown_info_keys() {
        // extra-field and unknown-key live inside info and are not on Info.
        // Hashing a re-encoded struct would drop them and change the info hash.
        const EXTRA_INFO_HASH_HEX: &str = "6a90441e947dcc1910191a8a5f710399dbe90d6f";

        let path = fixture_path("extra-info-keys.torrent");
        let bytes = std::fs::read(&path).expect("read extra-info-keys fixture");
        let meta = from_bytes(&bytes).expect("parse extra-info-keys fixture");

        assert_eq!(meta.torrent_file.info.name, "tiny.bin");
        assert!(meta.torrent_file.info.private.is_none());
        assert!(!meta.torrent_file.info.is_private());
        assert_eq!(meta.info_hash, decode_info_hash(EXTRA_INFO_HASH_HEX));

        let raw_info = raw_info_dict(&bytes).expect("raw info");
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(raw_info);
        assert_eq!(meta.info_hash, hasher.digest().bytes());

        let reencoded = ser::to_bytes(&meta.torrent_file.info).expect("re-encode info");
        assert_ne!(
            reencoded.as_slice(),
            raw_info,
            "re-encoding must drop unknown info keys, proving from_bytes hashes the original bytes"
        );
    }
}
