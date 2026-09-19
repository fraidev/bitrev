# bitrev seeds. qBittorrent downloads.
run_scenario() {
  gen_fixture_host
  mkdir -p "$SHARED_DIR/bitrev" "$SHARED_DIR/qbittorrent"
  cp "$SHARED_DIR/fixture/data/payload.bin" "$SHARED_DIR/bitrev/payload.bin"
  BITREV_PEERS="qbittorrent:6881"
  bitrev_seed_start
  wait_bitrev
  python3 "$INTEROP_DIR/scripts/qbittorrent.py" wait --timeout 60
  python3 "$INTEROP_DIR/scripts/qbittorrent.py" add \
    --torrent "$SHARED_DIR/fixture/payload.bin.torrent" \
    --savepath /shared/qbittorrent
  start_pcap qbittorrent
  python3 "$INTEROP_DIR/scripts/qbittorrent.py" wait-done --timeout "$TIMEOUT"
  assert_payload "$SHARED_DIR/qbittorrent/payload.bin"
  bitrev_seed_stop
}
