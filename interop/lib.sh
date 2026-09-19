# shellcheck shell=bash
# Shared helpers for interop/run.sh and interop/scenarios/*.sh

TIMEOUT="${TIMEOUT:-90}"
ANNOUNCE_URL="${ANNOUNCE_URL:-http://tracker:6969/announce}"

compose() {
  docker compose --project-name "$COMPOSE_PROJECT_NAME" -f "$COMPOSE_FILE" "$@"
}

bitrev_compose() {
  compose --profile bitrev "$@"
}

bitrev_run() {
  bitrev_compose run --rm --no-deps --use-aliases \
    -e RUST_LOG="${RUST_LOG:-info}" \
    bitrev "$@"
}

bitrev_run_named() {
  local name="$1"
  shift
  docker rm -f "$name" >/dev/null 2>&1 || true
  bitrev_compose run -d --no-deps --use-aliases --name "$name" \
    -e RUST_LOG="${RUST_LOG:-info}" \
    bitrev "$@"
}

# Fixture generation: native binary on Linux, container binary elsewhere.
interop_bin() {
  if [[ "$(uname -s)" == "Linux" ]]; then
    "$INTEROP_BIN" "$@"
  else
    bitrev_run "$@"
  fi
}

run_gen_fixture() {
  local bin="$1"
  local dir="$2"
  shift 2
  # Remaining args are optional flags. Always pass the required ones so
  # bash 3.2 with `set -u` never expands an empty array.
  if [[ -n "${FIXTURE_FILES:-}" ]]; then
    local spec
    local files=()
    for spec in $FIXTURE_FILES; do
      files+=(--file "$spec")
    done
    if [[ "${PRIVATE:-0}" == "1" ]]; then
      "$bin" gen-fixture --dir "$dir" --announce "$ANNOUNCE_URL" --private \
        --name "${FIXTURE_NAME:-mixed}" "${files[@]}"
    else
      "$bin" gen-fixture --dir "$dir" --announce "$ANNOUNCE_URL" \
        --name "${FIXTURE_NAME:-mixed}" "${files[@]}"
    fi
  elif [[ "${PRIVATE:-0}" == "1" ]]; then
    "$bin" gen-fixture --dir "$dir" --announce "$ANNOUNCE_URL" --private
  else
    "$bin" gen-fixture --dir "$dir" --announce "$ANNOUNCE_URL"
  fi
}

gen_fixture() {
  run_gen_fixture interop_bin /shared/fixture
}

# Host-path variant used when the binary can run on the host.
gen_fixture_host() {
  if [[ "$(uname -s)" == "Linux" ]]; then
    run_gen_fixture "$INTEROP_BIN" "$SHARED_DIR/fixture"
  else
    gen_fixture
  fi
}

payload_sha1() {
  tr -d '[:space:]' <"$SHARED_DIR/fixture/payload.sha1"
}

assert_payload() {
  local path="$1"
  local want got
  want="$(payload_sha1)"
  got="$(python3 -c "import hashlib,sys; print(hashlib.sha1(open(sys.argv[1],'rb').read()).hexdigest())" "$path")"
  if [[ "$got" != "$want" ]]; then
    echo "sha1 mismatch for $path: got $got want $want" >&2
    return 1
  fi
  echo "sha1 $got"
}

info_hash() {
  tr -d '[:space:]' <"$SHARED_DIR/fixture/info-hash.txt"
}

start_pcap() {
  local service="${1:-transmission}"
  local cid
  cid="$(compose ps -q "$service")"
  if [[ -z "$cid" ]]; then
    echo "pcap: no container for $service" >&2
    return 1
  fi
  docker rm -f "${COMPOSE_PROJECT_NAME}-pcap" >/dev/null 2>&1 || true
  docker run -d --name "${COMPOSE_PROJECT_NAME}-pcap" \
    --network "container:${cid}" \
    --cap-add NET_RAW --cap-add NET_ADMIN \
    -v "${SCENARIO_OUT}:/out" \
    nicolaka/netshoot:v0.16 \
    tcpdump -i any -U -nn -w /out/wire.pcap >/dev/null
}

stop_pcap() {
  if docker inspect "${COMPOSE_PROJECT_NAME}-pcap" >/dev/null 2>&1; then
    docker stop "${COMPOSE_PROJECT_NAME}-pcap" >/dev/null 2>&1 || true
    sleep 0.5
    docker rm -f "${COMPOSE_PROJECT_NAME}-pcap" >/dev/null 2>&1 || true
  fi
}

collect_logs() {
  mkdir -p "$SCENARIO_OUT/logs"
  compose logs --no-color >"$SCENARIO_OUT/logs/compose.log" 2>&1 || true
  local svc
  for svc in tracker transmission qbittorrent libtorrent; do
    compose logs --no-color "$svc" >"$SCENARIO_OUT/logs/${svc}.log" 2>&1 || true
  done
  if docker inspect "${COMPOSE_PROJECT_NAME}-bitrev" >/dev/null 2>&1; then
    docker logs "${COMPOSE_PROJECT_NAME}-bitrev" >"$SCENARIO_OUT/logs/bitrev.log" 2>&1 || true
  fi
}

bitrev_download() {
  local torrent="${1:-/shared/fixture/payload.bin.torrent}"
  local output="${2:-/shared/bitrev/payload.bin}"
  bitrev_run download \
    --torrent "$torrent" \
    --output "$output" \
    --state-dir /shared/bitrev/state \
    --port 6881 \
    --timeout "$TIMEOUT" \
    --expect-sha1 "$(payload_sha1)" \
    --encryption prefer-plaintext
}

bitrev_seed_start() {
  local torrent="${1:-/shared/fixture/payload.bin.torrent}"
  local output="${2:-/shared/bitrev/payload.bin}"
  local flags=(--encryption prefer-plaintext)
  if [[ "${BITREV_DHT:-0}" == "1" ]]; then
    flags+=(--dht)
  fi
  if [[ -n "${BITREV_PEERS:-}" ]]; then
    local peer
    for peer in $BITREV_PEERS; do
      flags+=(--peer "$peer")
    done
  fi
  bitrev_run_named "${COMPOSE_PROJECT_NAME}-bitrev" seed \
    --torrent "$torrent" \
    --output "$output" \
    --state-dir /shared/bitrev/state \
    --port 6881 \
    "${flags[@]}"
}

wait_bitrev() {
  local name="${COMPOSE_PROJECT_NAME}-bitrev"
  local i
  for i in $(seq 1 40); do
    if docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null | grep -q true; then
      if docker logs "$name" 2>&1 | grep -q "listening"; then
        sleep 1
        return 0
      fi
    fi
    sleep 0.25
  done
  echo "bitrev seeder did not start" >&2
  docker logs "$name" >&2 || true
  return 1
}

bitrev_seed_stop() {
  if docker inspect "${COMPOSE_PROJECT_NAME}-bitrev" >/dev/null 2>&1; then
    docker logs "${COMPOSE_PROJECT_NAME}-bitrev" >"$SCENARIO_OUT/logs/bitrev.log" 2>&1 || true
    docker stop "${COMPOSE_PROJECT_NAME}-bitrev" >/dev/null 2>&1 || true
    docker rm -f "${COMPOSE_PROJECT_NAME}-bitrev" >/dev/null 2>&1 || true
  fi
}

copy_single_payload() {
  local dest_dir="$1"
  mkdir -p "$dest_dir"
  cp "$SHARED_DIR/fixture/data/payload.bin" "$dest_dir/payload.bin"
}

skip_until_issue() {
  local issue="$1"
  echo "SKIP: enable after issue #${issue} lands"
  exit 0
}
