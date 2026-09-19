# Private torrent. libtorrent container records bitrev's handshake: no DHT
# bit, no ut_pex.
run_scenario() {
  PRIVATE=1
  BITREV_DHT=1
  gen_fixture_host
  mkdir -p "$SHARED_DIR/bitrev" "$SCENARIO_OUT"
  cp "$SHARED_DIR/fixture/data/payload.bin" "$SHARED_DIR/bitrev/payload.bin"
  bitrev_seed_start
  wait_bitrev
  start_pcap tracker
  compose exec -T libtorrent python3 /interop/libtorrent/inspect_handshake.py \
    --host bitrev \
    --port 6881 \
    --info-hash "$(info_hash)" \
    --out /out/handshake.json \
    --timeout "$TIMEOUT"
  bitrev_seed_stop
}
