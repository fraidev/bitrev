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
        let piece_hashes: Vec<[u8; 20]> = torrent_file
            .info
            .pieces
            .chunks_exact(20)
            .map(|chunk| {
                let mut array = [0u8; 20];
                array.copy_from_slice(chunk);
                array
            })
            .collect();

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
    if torrent_file.info.pieces.len() % 20 != 0 {
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
}
