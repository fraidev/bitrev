# Fuzz targets

libFuzzer targets for every attacker-controlled wire parser. The crate is a
standalone workspace (`fuzz/Cargo.toml`) so `cargo test` / `cargo clippy` on
stable never build it.

Issue: https://github.com/fraidev/bitrev/issues/65

## Targets

| Target | Parser |
| --- | --- |
| `message_read` | `protocol::decode_frame` (BEP-0003 length prefix + message) |
| `handshake_read` | `Handshake::from_bytes` |
| `metainfo_from_bytes` | `file::from_bytes` |
| `tracker_response` | `BencodeResponse::from_bytes` then `get_peers` |
| `udp_tracker_packet` | `protocol_udp::parse_udp_packet` |
| `extension_handshake_decode` | `ExtensionHandshake::decode` |
| `resume_decode` | `resume::decode` |

New parsers (KRPC, `ut_metadata`, magnet, `ut_pex`, LPD, `ipfilter.dat`) add one
target here with a seed corpus under `corpus/<target>/`.

## Setup

Nightly rustc and `cargo-fuzz`:

```sh
rustup toolchain install nightly
cargo install cargo-fuzz
```

## Run

From the repository root:

```sh
cargo +nightly fuzz run message_read
cargo +nightly fuzz run handshake_read
# ...
```

Smoke each target for 30 seconds (what CI does):

```sh
cargo +nightly fuzz run <target> -- -max_total_time=30
```

Seed inputs live in `fuzz/corpus/<target>/` and are committed. Keep each
target's corpus under 1 MiB.

## Reproduce a crash

Crashes land in `fuzz/artifacts/<target>/crash-*`.

```sh
cargo +nightly fuzz run <target> fuzz/artifacts/<target>/crash-<hash>
```

Minimize before committing a regression test:

```sh
cargo +nightly fuzz tmin <target> fuzz/artifacts/<target>/crash-<hash>
```

## Regression rule

Every fixed crash becomes a unit test that feeds the minimized bytes to the
same parser and asserts `Ok` or `Err`, never a panic. Put the bytes next to
the existing table-driven cases in that module. Do not leave the only
reproduction in `fuzz/artifacts/`.
