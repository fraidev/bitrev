use serde::Deserialize;
use serde::Serialize;
use serde_bencode::de;
use serde_bencode::ser;
use serde_bytes::ByteBuf;
use std::fmt::Write;
use std::{error::Error, io::Read};

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
    pub fn new(torrent_file: TorrentFile) -> Self {
        let file_info_beaconde = &ser::to_bytes(&torrent_file.info).unwrap();
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(file_info_beaconde);
        let info_hash = hasher.digest().bytes();

        let piece_hashes: Vec<[u8; 20]> = torrent_file
            .info
            .pieces
            .chunks(20)
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

pub fn from_filename(filename: &str) -> Result<TorrentMeta, Box<dyn Error>> {
    let mut file = std::fs::File::open(filename)?;
    let mut content = Vec::new();
    file.read_to_end(&mut content)?;
    let torrent = de::from_bytes::<TorrentFile>(&content)?;
    Ok(TorrentMeta::new(torrent))
}

pub fn url_encode_bytes(content: &[u8]) -> Result<String, Box<dyn Error>> {
    let mut out: String = String::new();

    for byte in content.iter() {
        match *byte as char {
            '0'..='9' | 'a'..='z' | 'A'..='Z' | '.' | '-' | '_' | '~' => out.push(*byte as char),
            _ => write!(&mut out, "%{:02X}", byte)?,
        };
    }

    Ok(out)
}

pub fn build_tracker_url(
    torrent_meta: &TorrentMeta,
    peer_id: &[u8],
    port: u16,
    tracker_url: &str,
) -> String {
    // let announce_url = torrent_meta.torrent_file.announce.as_ref().unwrap();
    let info_hash_encoded = url_encode_bytes(torrent_meta.info_hash.as_ref()).unwrap();
    let peer_id_encoded = url_encode_bytes(peer_id).unwrap();
    // let info_hash_encoded = urlencoding::encode_binary(&torrent_meta.info_hash);
    // let peer_id_encoded = urlencoding::encode_binary(&peer_id);

    format!(
        // "{}?info_hash={}&peer_id={}&port={}&uploaded=0&downloaded=0&compact=1&left={}&event=started?supportcrypto=1&numwant=80&key=DF45C574",
        "{}?info_hash={}&peer_id={}&port={}&uploaded=0&downloaded=0&compact=1&left={}",
        tracker_url,
        info_hash_encoded,
        peer_id_encoded,
        port,
        torrent_meta.torrent_file.info.length.as_ref().unwrap()
    )
    .to_string()
}
