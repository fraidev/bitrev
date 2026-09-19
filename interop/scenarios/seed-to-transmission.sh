# bitrev seeds. Transmission downloads (upload path, choking, have).
run_scenario() {
  gen_fixture_host
  mkdir -p "$SHARED_DIR/bitrev" "$SHARED_DIR/transmission"
  cp "$SHARED_DIR/fixture/data/payload.bin" "$SHARED_DIR/bitrev/payload.bin"
  BITREV_PEERS="transmission:51413"
  bitrev_seed_start
  wait_bitrev
  python3 "$INTEROP_DIR/scripts/transmission.py" wait --timeout 60
  python3 "$INTEROP_DIR/scripts/transmission.py" add \
    --filename /shared/fixture/payload.bin.torrent \
    --download-dir /shared/transmission
  start_pcap transmission
  python3 "$INTEROP_DIR/scripts/transmission.py" wait-done --timeout "$TIMEOUT"
  assert_payload "$SHARED_DIR/transmission/payload.bin"
  bitrev_seed_stop
}
