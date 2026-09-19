# Benchmarks

Three layers. Micro and the localhost swarm are automated. Comparing against
other clients is a documented manual procedure and never runs in CI.

Nothing in CI talks to the public network. `bench_swarm` binds 127.0.0.1
only. `benches/compare.sh` is the exception: it joins a real swarm.

## Micro

Criterion benches in `crates/bit_rev/benches/micro.rs`.

```bash
cargo bench -p bit_rev
```

Reports land in `target/criterion/`. Coverage:

- metainfo parse (`file::from_bytes` on every `samples/*.torrent`)
- message serialize and parse (`Piece` with a 16 KiB payload)
- SHA-1 piece verification at 256 KiB and 1 MiB
- `map_piece_to_files` over a 128-file layout
- `build_tracker_url`
- `ResumeData` encode and decode

CI compiles and runs each bench once with no measurement:

```bash
cargo bench -- --test
```

## Macro (localhost swarm)

`crates/bit_rev/examples/bench_swarm.rs`. Always `--release`. Spins N
in-process seeders on loopback, downloads a generated M-MiB torrent, and
prints a table.

Default seeders are real `Session` seeders (issue 8). Pass `--latency-ms`
(or `--seeder-peer`) to use the mock `SeederPeer` instead, which can inject
per-read and per-write delay.

```bash
cargo run --release -p bit_rev --example bench_swarm
cargo run --release -p bit_rev --example bench_swarm -- --size-mib 64 --json
cargo run --release -p bit_rev --example bench_swarm -- \
    --size-mib 64 --seeders 4 --piece-length 262144 --latency-ms 5
```

The table reports wall-clock, MiB/s, peak RSS (`getrusage` `ru_maxrss`,
bytes, platform-gated), user and sys CPU, bytes received versus payload
(overhead proxy: verified payload plus `duplicate_bytes`), and a duplicate
block estimate derived from `duplicate_bytes` (the engine exposes bytes,
not a block counter).

`--json` writes one JSON object to stdout and nothing else. That is the
hook for later tracking. This repo does not ship continuous benchmark
infrastructure.

CI smoke (generous timeout, still localhost):

```bash
cargo run --release -p bit_rev --example bench_swarm -- --size-mib 64 --json
```

## Reference comparison (manual, real swarm)

`benches/compare.sh` downloads a well-seeded public torrent with bitrev and
a reference client. **This touches real trackers and real peers. Never
enable it in CI.**

Suggested torrent: Debian netinst, already in the tree as
`samples/debian-13.6.0-amd64-netinst.iso.torrent`
([current amd64 bt-cd](https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/)).
Any other well-seeded Linux ISO is fine. Record which file you used.

```bash
# macOS: /usr/bin/time -l    Linux: /usr/bin/time -v
benches/compare.sh samples/debian-13.6.0-amd64-netinst.iso.torrent
```

The script builds `target/release/bitrev` if needed, prefers
`transmission-cli`, falls back to `aria2c`, and writes stdout/stderr plus a
notes template under `target/bench-compare/notes/`. Fill in wall-clock,
peak RSS, and CPU from the time report.

Disk I/O is not automated. While a run is in progress:

- macOS: `sudo fs_usage -f filesys -w <pid>`
- Linux: `sudo iotop -p <pid>` or `pidstat -d 1 -p <pid>`

Note whether writes looked sequential, whether the client preallocated, and
how chatty the swarm was. Those notes belong next to the numbers. A
seed-starved public swarm is not comparable to `bench_swarm`.

## Profiles

`cargo flamegraph` is the supported way to look at a hot path. It is not
wired into CI.

```bash
cargo install flamegraph
cargo flamegraph -p bit_rev --bench micro -- sha1_piece
cargo flamegraph -p bit_rev --example bench_swarm -- --size-mib 64
```

On macOS, Instruments works too: `cargo instruments -t time -p bit_rev --example bench_swarm`.

## Shared harness

`crates/testkit` (`publish = false`) holds `TorrentFixture`, `SeederPeer`,
the mock trackers, and `resource_usage` / `peak_rss_bytes`. Integration
tests re-export it from `crates/bit_rev/tests/common/mod.rs`. The interop
harness (task-05) should depend on `testkit` the same way.

Peak RSS used to shell out to `ps`. It now uses `libc::getrusage`
`ru_maxrss` (bytes on macOS, kilobytes on Linux, converted to bytes).

## Baseline

Recorded so task-03 (picker) and task-08 (storage) have a before. Re-run
the same commands after those land. Single-sample swarm numbers, not a
criterion mean. This machine is quiet enough for a before/after, not for
a paper.

- Machine: Apple M5 Pro, 24 GiB, Darwin 25.6.0 arm64
- rustc: 1.98.1 (48a229cea 2026-09-01)
- Commit: 2ca95f2 plus this benchmark work
- Commands: `cargo bench -p bit_rev` and
  `cargo run --release -p bit_rev --example bench_swarm -- --size-mib 64 --seeders 4`

### Localhost swarm (64 MiB, 256 KiB pieces, 4 seeders)

| seeder | wall-clock | MiB/s | peak RSS | user CPU | sys CPU | received / payload | dup bytes |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| session | 0.838 s | 76.38 | 144588800 B | 0.182 s | 0.279 s | 67125248 / 67108864 | 16384 |
| seeder_peer | 0.854 s | 74.95 | 143343616 B | 0.134 s | 0.267 s | 67125248 / 67108864 | 16384 |

`--json` on the session run produced valid JSON with the same shape
(`seeder_kind`, `wall_secs`, `mib_per_sec`, `peak_rss_bytes`, `user_secs`,
`sys_secs`, `bytes_received`, `payload_bytes`, `overhead_ratio`,
`duplicate_bytes`, `duplicate_blocks`).

### Micro (criterion median)

| bench | time |
| --- | ---: |
| metainfo_parse / debian-13.6.0-amd64-netinst.iso.torrent | 57.183 µs |
| metainfo_parse / gimp-3.0.4-arm64.dmg.torrent | 23.386 µs |
| metainfo_parse / lots-of-numbers.torrent | 3.246 µs |
| metainfo_parse / test_folder-d984f67af9917b214cd8b6048ab5624c7df6a07a.torrent | 13.837 µs |
| message_serialize / piece_16kib | 530.81 ns |
| message_parse / piece_16kib | 407.05 ns |
| sha1_piece / 256kib | 211.22 µs |
| sha1_piece / 1mib | 839.25 µs |

## After storage (#75)

Same machine. `sha1` crate (hardware SHA-1) replaced `sha1_smol` for
piece hashing. Positional I/O, sparse preallocation, hashing on
`spawn_blocking`.

Command:
`cargo run --release -p bit_rev --example bench_swarm -- --size-mib 1024 --seeders 4 --json`

### Localhost swarm (1 GiB, 256 KiB pieces, 4 session seeders)

| | wall-clock | MiB/s | peak RSS | user CPU | sys CPU |
| --- | ---: | ---: | ---: | ---: | ---: |
| before | 15.844 s | 64.63 | 1095024640 B | 3.297 s | 6.005 s |
| after | 1.987 s | 515.27 | 29474816 B | 2.499 s | 3.477 s |

### sha1_piece (criterion median)

| bench | before (`sha1_smol`) | after (`sha1`) |
| --- | ---: | ---: |
| 256kib | 211.22 µs | 185.90 µs |
| 1mib | 839.25 µs | 738.42 µs |
| map_piece_to_files / 128_files | 1.227 µs |
| build_tracker_url | 393.21 ns |
| resume / encode | 1.244 µs |
| resume / decode | 1.513 µs |
