# Interop harness

Repeatable checks that bitrev speaks BitTorrent with Transmission, qBittorrent, and libtorrent. In-process tests cannot catch a bug that is symmetric on both bitrev sides.

Tracked as GitHub issue [#73](https://github.com/fraidev/bitrev/issues/73).

## Prerequisites

- Docker with Compose v2
- Python 3 (stdlib only, used by the RPC helpers)
- A Rust toolchain on Linux. macOS builds the helper inside `rust:bookworm` so the binary can run in Linux containers.
- `tcpdump` is optional on the host. The runner always starts a capture sidecar.

Linux is the supported path (same as CI). macOS is best effort: Docker Desktop networking is not the same as a Linux bridge.

## Running one scenario

From the repo root:

```bash
interop/run.sh download-from-transmission
interop/run.sh seed-to-transmission
interop/run.sh --list
interop/run.sh --all
```

`run.sh` builds `bitrev-interop` in release, starts the pinned compose stack, generates a fixture torrent, runs the scenario, writes logs and a pcap under `interop/out/<scenario>/`, then tears the stack down. Nonzero exit means failure.

Enabled scenarios (byte-identical payload SHA-1 within a time bound):

| Scenario | What it checks |
| --- | --- |
| `download-from-transmission` | Transmission seeds, bitrev downloads through opentracker |
| `seed-to-transmission` | bitrev seeds, Transmission downloads (upload, choke, `have`) |
| `download-from-qbittorrent` | qBittorrent seeds, bitrev downloads |
| `seed-to-qbittorrent` | bitrev seeds, qBittorrent downloads |
| `mixed-swarm` | Two-file torrent split across Transmission and qBittorrent so bitrev must talk to both |
| `private-torrent` | Private flag. The inspect peer records bitrev's handshake: no DHT bit, no `ut_pex` |

Wired but skipped until their issue lands: `magnet-via-dht` (#5, #6), `mse-required` (#7), `utp-only` (#20), `sonarr-add` (#46).

`cargo test` does not start Docker. The helper crate lives in `crates/interop` and is a workspace member, but it is not a default member.

## Adding a scenario

1. Create `interop/scenarios/<name>.sh` that defines `run_scenario`. Reuse helpers from `interop/lib.sh`.
2. Add the name to `ENABLED_SCENARIOS` in `interop/run.sh`, or to `SKIPPED_SCENARIOS` if it should stay skipped.
3. Add a row to the table above.

That is the whole hook. The runner owns build, compose, logs, pcap, and teardown.

## Reading the pcap

Each run writes `interop/out/<scenario>/wire.pcap` from a `tcpdump` sidecar attached to the reference client's network namespace (Transmission or qBittorrent, or the tracker for `private-torrent`).

```bash
tcpdump -nn -r interop/out/download-from-transmission/wire.pcap
wireshark interop/out/download-from-transmission/wire.pcap
```

Filter BitTorrent: `tcp.port == 51413 or tcp.port == 6881 or tcp.port == 6969`. Handshake is the 68-byte `BitTorrent protocol` banner. Use this when two clients complete locally but disagree on the wire.

Compose logs are under `interop/out/<scenario>/logs/`.

## Layout

```
interop/
  docker-compose.yml     pinned clients on bridge `btnet`
  run.sh                 entrypoint
  lib.sh                 shared helpers
  scenarios/             one file per scenario
  scripts/               Transmission RPC and qBittorrent Web API
  libtorrent/            scripted peer and handshake inspector
  clients/               Transmission and qBittorrent config
  images/                runtime and libtorrent Dockerfiles
  out/                   gitignored results
```

Image tags are pinned in `docker-compose.yml`. Do not use `latest`.
