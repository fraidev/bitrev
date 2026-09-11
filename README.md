# BitRev

BitRev is a BitTorrent client written entirely in Rust.
This Project it's a rewrite of [tornado](https://github.com/fraidev/tornado) in Rust.

## Setup

Exemple of how to download a debian iso:

```bash
cargo run --release --bin bitrev -- samples/debian-13.6.0-amd64-netinst.iso.torrent
```

Tests:

```bash
cargo test
```

Check sha256sum of the debian iso:

```bash
openssl dgst -sha256 debian-13.6.0-amd64-netinst.iso
```

Should be [ee8d8579128977d7dc39d48f43aec5ab06b7f09e1f40a9d98f2a9d149221704a](https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/SHA256SUMS) for this example.

