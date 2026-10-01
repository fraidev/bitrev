# BitRev

BitRev is a BitTorrent client written entirely in Rust.
This Project it's a rewrite of [tornado](https://github.com/fraidev/tornado) in Rust.

## Setup

Exemple of how to download a debian iso:

```bash
cargo run --release --bin bitrev -- samples/debian-13.6.0-amd64-netinst.iso.torrent
```

Daemon (Sonarr, Radarr, and the Web UI talk to this process):

```bash
RUST_LOG=info cargo run --bin bitrev -- serve
```

`RUST_LOG` selects the tracing filter. Unset, `bitrev serve` logs at `info` (raise it with `-v`, lower it with `-q`). A target filter such as `RUST_LOG=tower_http=debug,server=debug` logs each HTTP request with method, path, status, and latency. The default bind is `127.0.0.1:8080`. Open that URL in a browser for the Web UI. `GET /healthz` returns `{"ok":true}` without auth.

Tests:

```bash
cargo test
```

Check sha256sum of the debian iso:

```bash
openssl dgst -sha256 debian-13.6.0-amd64-netinst.iso
```

Should be [ee8d8579128977d7dc39d48f43aec5ab06b7f09e1f40a9d98f2a9d149221704a](https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/SHA256SUMS) for this example.

