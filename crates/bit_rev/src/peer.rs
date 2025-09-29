use std::net::SocketAddr;

use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;

pub type PeerAddr = SocketAddr;

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone)]
pub struct BencodeResponse {
    pub peers: ByteBuf,
    pub peers6: ByteBuf,
    pub interval: u64,
}

impl BencodeResponse {
    pub fn get_peers(self) -> anyhow::Result<Vec<PeerAddr>> {
        // TODO: This is a bit of a mess
        let peers_bin = self.peers;
        let peer_size = 6;
        let peers_bin_length = peers_bin.len();
        let num_peers = peers_bin_length / peer_size;
        if !peers_bin_length.is_multiple_of(peer_size) {
            anyhow::bail!("Received empty peers");
        }

        let peers_bin6 = self.peers6;
        let peer_size6 = 18;
        let peers_bin_length6 = peers_bin6.len();
        let num_peers6 = peers_bin_length6 / peer_size6;
        if !peers_bin_length6.is_multiple_of(peer_size6) {
            anyhow::bail!("Received empty peers");
        }

        let mut peers = Vec::new();
        for i in 0..num_peers {
            let ip_size = 4;
            let offset = i * peer_size;
            let ip_bin = &peers_bin[offset..offset + ip_size];
            let port =
                u16::from_be_bytes([peers_bin[offset + ip_size], peers_bin[offset + ip_size + 1]]);
            let ip_array = [ip_bin[0], ip_bin[1], ip_bin[2], ip_bin[3]];
            let ip = std::net::Ipv4Addr::new(ip_array[0], ip_array[1], ip_array[2], ip_array[3]);
            let peer = SocketAddr::new(ip.into(), port);
            peers.push(peer);
        }

        for i in 0..num_peers6 {
            let ip_size = 16;
            let offset = i * peer_size6;
            let ip_bin = &peers_bin6[offset..offset + ip_size];
            let port = u16::from_be_bytes([
                peers_bin6[offset + ip_size],
                peers_bin6[offset + ip_size + 1],
            ]);
            let ip_array = [
                ip_bin[0], ip_bin[1], ip_bin[2], ip_bin[3], ip_bin[4], ip_bin[5], ip_bin[6],
                ip_bin[7], ip_bin[8], ip_bin[9], ip_bin[10], ip_bin[11], ip_bin[12], ip_bin[13],
                ip_bin[14], ip_bin[15],
            ];
            let ip = std::net::Ipv6Addr::new(
                u16::from_be_bytes([ip_array[0], ip_array[1]]),
                u16::from_be_bytes([ip_array[2], ip_array[3]]),
                u16::from_be_bytes([ip_array[4], ip_array[5]]),
                u16::from_be_bytes([ip_array[6], ip_array[7]]),
                u16::from_be_bytes([ip_array[8], ip_array[9]]),
                u16::from_be_bytes([ip_array[10], ip_array[11]]),
                u16::from_be_bytes([ip_array[12], ip_array[13]]),
                u16::from_be_bytes([ip_array[14], ip_array[15]]),
            );
            let peer = SocketAddr::new(ip.into(), port);
            peers.push(peer);
        }

        Ok(peers)
    }
}
