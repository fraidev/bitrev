#!/usr/bin/env bash
# Interop runner. Builds the bitrev-interop helper, starts pinned Docker
# clients on an isolated network, runs one scenario, collects logs/pcap,
# and tears down. Exit nonzero on failure.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INTEROP_DIR="$ROOT/interop"
# shellcheck source=lib.sh
source "$INTEROP_DIR/lib.sh"

COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-bitrevinterop}"
COMPOSE_FILE="$INTEROP_DIR/docker-compose.yml"
export COMPOSE_PROJECT_NAME COMPOSE_FILE ROOT INTEROP_DIR

ENABLED_SCENARIOS=(
  download-from-transmission
  seed-to-transmission
  download-from-qbittorrent
  seed-to-qbittorrent
  mixed-swarm
  private-torrent
)
SKIPPED_SCENARIOS=(
  magnet-via-dht
  mse-required
  utp-only
  sonarr-add
)

usage() {
  cat <<EOF
Usage: interop/run.sh <scenario>|--list|--all

Enabled scenarios:
$(printf '  %s\n' "${ENABLED_SCENARIOS[@]}")

Wired but skipped:
$(printf '  %s\n' "${SKIPPED_SCENARIOS[@]}")
EOF
}

list_scenarios() {
  printf '%s\n' "${ENABLED_SCENARIOS[@]}" "${SKIPPED_SCENARIOS[@]}"
}

is_skipped() {
  local s
  for s in "${SKIPPED_SCENARIOS[@]}"; do
    [[ "$s" == "$1" ]] && return 0
  done
  return 1
}

known_scenario() {
  local s
  for s in "${ENABLED_SCENARIOS[@]}" "${SKIPPED_SCENARIOS[@]}"; do
    [[ "$s" == "$1" ]] && return 0
  done
  return 1
}

build_bin() {
  mkdir -p "$BIN_DIR"
  if [[ "$(uname -s)" == "Linux" ]]; then
    cargo build --release -p interop --bin bitrev-interop --manifest-path "$ROOT/Cargo.toml"
    cp "$ROOT/target/release/bitrev-interop" "$BIN_DIR/bitrev-interop"
    if [[ -r /etc/os-release ]]; then
      # shellcheck disable=SC1091
      . /etc/os-release
      BITREV_RUNTIME_BASE="${BITREV_RUNTIME_BASE:-ubuntu:${VERSION_ID}}"
    else
      BITREV_RUNTIME_BASE="${BITREV_RUNTIME_BASE:-debian:bookworm-slim}"
    fi
  else
    BITREV_RUNTIME_BASE="${BITREV_RUNTIME_BASE:-debian:bookworm-slim}"
    docker run --rm \
      -v "$ROOT:/src" \
      -v bitrev-interop-registry:/usr/local/cargo/registry \
      -v bitrev-interop-git:/usr/local/cargo/git \
      -v bitrev-interop-target:/target \
      -e CARGO_TARGET_DIR=/target \
      -w /src \
      rust:bookworm \
      bash -c 'set -euo pipefail; apt-get update -qq && apt-get install -y -qq pkg-config libssl-dev >/dev/null && cargo build --release -p interop --bin bitrev-interop && cp /target/release/bitrev-interop /target/bitrev-interop-out'
    docker run --rm \
      -v bitrev-interop-target:/target \
      -v "$BIN_DIR:/out" \
      debian:bookworm-slim \
      cp /target/bitrev-interop-out /out/bitrev-interop
  fi
  chmod +x "$BIN_DIR/bitrev-interop"
  export BITREV_RUNTIME_BASE
}

seed_client_config() {
  rm -rf "$TRANSMISSION_CONFIG" "$QBITTORRENT_CONFIG"
  mkdir -p "$TRANSMISSION_CONFIG" "$QBITTORRENT_CONFIG/qBittorrent"
  cp "$INTEROP_DIR/clients/transmission/settings.json" "$TRANSMISSION_CONFIG/settings.json"
  cp "$INTEROP_DIR/clients/qbittorrent/qBittorrent.conf" \
    "$QBITTORRENT_CONFIG/qBittorrent/qBittorrent.conf"
  mkdir -p "$SHARED_DIR/transmission" "$SHARED_DIR/qbittorrent" "$SHARED_DIR/bitrev" \
    "$SHARED_DIR/libtorrent" "$SCENARIO_OUT/logs"
}

wait_tracker() {
  local i
  for i in $(seq 1 40); do
    if compose exec -T libtorrent curl -sf "http://tracker:6969/stats" >/dev/null 2>&1 \
      || compose exec -T libtorrent curl -sf -o /dev/null "http://tracker:6969/announce"; then
      return 0
    fi
    sleep 1
  done
  echo "opentracker did not become ready" >&2
  compose logs tracker >&2 || true
  return 1
}

wait_clients() {
  wait_tracker
  python3 "$INTEROP_DIR/scripts/transmission.py" wait --timeout 60
  python3 "$INTEROP_DIR/scripts/qbittorrent.py" wait --timeout 60
}

teardown() {
  local status=$?
  set +e
  collect_logs
  stop_pcap
  bitrev_seed_stop
  compose --profile bitrev --profile sonarr down -v --remove-orphans >/dev/null 2>&1
  docker rm -f "${COMPOSE_PROJECT_NAME}-pcap" "${COMPOSE_PROJECT_NAME}-bitrev" >/dev/null 2>&1
  return "$status"
}

run_one() {
  local scenario="$1"
  local script="$INTEROP_DIR/scenarios/${scenario}.sh"
  if [[ ! -f "$script" ]]; then
    echo "unknown scenario: $scenario" >&2
    usage >&2
    return 2
  fi

  export SCENARIO="$scenario"
  export SCENARIO_OUT="$INTEROP_DIR/out/${scenario}"
  export SHARED_DIR="$SCENARIO_OUT/shared"
  export TRANSMISSION_CONFIG="$SHARED_DIR/transmission-config"
  export QBITTORRENT_CONFIG="$SHARED_DIR/qbittorrent-config"
  export INTEROP_BIN="$BIN_DIR/bitrev-interop"
  export BITREV_RUNTIME_BASE="${BITREV_RUNTIME_BASE:-debian:bookworm-slim}"

  rm -rf "$SCENARIO_OUT"
  mkdir -p "$SCENARIO_OUT" "$SHARED_DIR" "$BIN_DIR"
  seed_client_config

  trap teardown EXIT

  echo "==> $scenario: compose up"
  compose build bitrev libtorrent
  compose up -d tracker transmission qbittorrent libtorrent
  wait_clients

  # shellcheck source=/dev/null
  source "$script"
  echo "==> $scenario: run"
  run_scenario

  collect_logs
  stop_pcap
  bitrev_seed_stop
  compose --profile bitrev --profile sonarr down -v --remove-orphans
  trap - EXIT
  echo "==> $scenario: ok"
}

main() {
  if [[ $# -lt 1 ]]; then
    usage >&2
    exit 2
  fi
  case "$1" in
    -h|--help)
      usage
      ;;
    --list)
      list_scenarios
      ;;
    --all)
      local s
      build_bin
      for s in "${ENABLED_SCENARIOS[@]}"; do
        run_one "$s"
      done
      ;;
    *)
      if ! known_scenario "$1"; then
        echo "unknown scenario: $1" >&2
        usage >&2
        exit 2
      fi
      BIN_DIR="$INTEROP_DIR/out/bin"
      mkdir -p "$BIN_DIR"
      build_bin
      run_one "$1"
      ;;
  esac
}

BIN_DIR="$INTEROP_DIR/out/bin"
export BIN_DIR INTEROP_BIN="${BIN_DIR}/bitrev-interop"

cd "$ROOT"
main "$@"
