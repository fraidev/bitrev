# qBittorrent seeds. bitrev downloads through opentracker.
run_scenario() {
  gen_fixture_host
  copy_single_payload "$SHARED_DIR/qbittorrent"
  python3 "$INTEROP_DIR/scripts/qbittorrent.py" wait --timeout 60
  python3 "$INTEROP_DIR/scripts/qbittorrent.py" add \
    --torrent "$SHARED_DIR/fixture/payload.bin.torrent" \
    --savepath /shared/qbittorrent \
    --skip-checking
  python3 "$INTEROP_DIR/scripts/qbittorrent.py" wait-done --timeout "$TIMEOUT"
  start_pcap qbittorrent
  bitrev_download
}
