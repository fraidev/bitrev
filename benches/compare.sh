#!/usr/bin/env bash
# Manual reference comparison. Never run in CI.
#
# This script downloads a real torrent from the public internet using bitrev
# and a reference client (transmission-cli or aria2c). It talks to real
# trackers and real swarms. Do not run it on a firewalled CI runner, and do
# not treat its numbers as a localhost benchmark.
#
# Usage:
#   benches/compare.sh [TORRENT]
#
# TORRENT defaults to samples/debian-13.6.0-amd64-netinst.iso.torrent
# (Debian netinst, a well-seeded Linux ISO). Override REF to pick the
# reference client, BITREV for a prebuilt binary, and OUT_ROOT for the
# download parent directory.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TORRENT="${1:-$ROOT/samples/debian-13.6.0-amd64-netinst.iso.torrent}"
OUT_ROOT="${OUT_ROOT:-$ROOT/target/bench-compare}"
BITREV="${BITREV:-$ROOT/target/release/bitrev}"
REF="${REF:-}"
STAMP="$(date +%Y%m%d-%H%M%S)"
NOTES_DIR="$OUT_ROOT/notes"

if [[ ! -f "$TORRENT" ]]; then
  echo "compare.sh: torrent not found: $TORRENT" >&2
  exit 1
fi

if [[ "$(uname -s)" == "Darwin" ]]; then
  TIME_CMD=(/usr/bin/time -l)
else
  TIME_CMD=(/usr/bin/time -v)
fi

if [[ ! -x "$BITREV" ]]; then
  echo "compare.sh: building release bitrev..." >&2
  (cd "$ROOT" && cargo build --release -p cli --bin bitrev)
  BITREV="$ROOT/target/release/bitrev"
fi

if [[ -z "$REF" ]]; then
  if command -v transmission-cli >/dev/null 2>&1; then
    REF="transmission-cli"
  elif command -v aria2c >/dev/null 2>&1; then
    REF="aria2c"
  else
    echo "compare.sh: install transmission-cli or aria2c, or set REF" >&2
    exit 1
  fi
fi

if ! command -v "$REF" >/dev/null 2>&1 && [[ ! -x "$REF" ]]; then
  echo "compare.sh: reference client not found: $REF" >&2
  exit 1
fi

cat <<EOF >&2
============================================================
WARNING: this talks to a real BitTorrent swarm.
Torrent:  $TORRENT
bitrev:   $BITREV
reference: $REF
time:     ${TIME_CMD[*]}
============================================================
EOF

mkdir -p "$NOTES_DIR"
BITREV_DIR="$OUT_ROOT/bitrev-$STAMP"
REF_DIR="$OUT_ROOT/ref-$STAMP"
mkdir -p "$BITREV_DIR" "$REF_DIR"

run_timed() {
  local label="$1"
  local log="$2"
  shift 2
  echo >&2
  echo "----- $label -----" >&2
  echo "command: $*" >&2
  # /usr/bin/time writes its report to stderr. Keep a copy and show live output.
  set +e
  "${TIME_CMD[@]}" "$@" > >(tee "$log.stdout") 2> >(tee "$log.stderr" >&2)
  local rc=$?
  set -e
  echo "exit $rc" | tee -a "$log.stderr" >&2
  return "$rc"
}

run_timed "bitrev" "$NOTES_DIR/bitrev-$STAMP" \
  "$BITREV" --output "$BITREV_DIR" --no-seed "$TORRENT"

ref_name="$(basename "$REF")"
case "$ref_name" in
  transmission-cli)
    # -w is portable. Some builds keep seeding after completion; stop them
    # once the download finishes if you only want the leech numbers.
    run_timed "transmission-cli" "$NOTES_DIR/ref-$STAMP" \
      "$REF" -w "$REF_DIR" "$TORRENT"
    ;;
  aria2c)
    run_timed "aria2c" "$NOTES_DIR/ref-$STAMP" \
      "$REF" --dir="$REF_DIR" --seed-time=0 --allow-overwrite=true "$TORRENT"
    ;;
  *)
    echo "compare.sh: unknown reference client '$REF'." >&2
    echo "Run it yourself under: ${TIME_CMD[*]} $REF ..." >&2
    exit 1
    ;;
esac

NOTES="$NOTES_DIR/run-$STAMP.md"
cat >"$NOTES" <<EOF
# compare.sh run $STAMP

- torrent: \`$TORRENT\`
- machine: $(uname -a)
- bitrev: \`$BITREV\`
- reference: \`$REF\`
- time: \`${TIME_CMD[*]}\`
- bitrev output: \`$BITREV_DIR\`
- reference output: \`$REF_DIR\`

## Recorded numbers

Fill these from the \`.stderr\` files next to this note (macOS \`time -l\`
or Linux \`time -v\`).

| client | wall-clock | peak RSS | user CPU | sys CPU |
| --- | --- | --- | --- | --- |
| bitrev |  |  |  |  |
| $ref_name |  |  |  |  |

On macOS, \`time -l\` prints "real", "user", "sys", and "maximum resident set size".
On Linux, \`time -v\` prints "Elapsed (wall clock) time", "User time",
"System time", and "Maximum resident set size (kbytes)".

## Disk I/O notes

Optional, one client at a time, while the download is running:

- macOS: \`sudo fs_usage -f filesys -w <pid>\`
- Linux: \`sudo iotop -p <pid>\` or \`pidstat -d 1 -p <pid>\`

Write down whether the client looked sequential, chatty, or bursty, and
whether it preallocated.

## Swarm notes

Peer count, seed/leech mix, and whether the run felt seed-limited go here.
These numbers are not comparable to \`bench_swarm\`, which is localhost only.

EOF

echo >&2
echo "Wrote $NOTES" >&2
echo "bitrev time report:  $NOTES_DIR/bitrev-$STAMP.stderr" >&2
echo "reference time report: $NOTES_DIR/ref-$STAMP.stderr" >&2
echo "Fill in the table in $NOTES before comparing runs." >&2
