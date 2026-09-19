# Two-file torrent. Transmission has file 0, qBittorrent has file 1, bitrev
# needs both so cross-client exchange is required.
run_scenario() {
  FIXTURE_FILES="a.bin:65536 b.bin:65536"
  FIXTURE_NAME="mixed"
  gen_fixture_host
  mkdir -p "$SHARED_DIR/transmission/mixed" "$SHARED_DIR/qbittorrent/mixed" "$SHARED_DIR/bitrev"
  cp "$SHARED_DIR/fixture/data/a.bin" "$SHARED_DIR/transmission/mixed/a.bin"
  cp "$SHARED_DIR/fixture/data/b.bin" "$SHARED_DIR/qbittorrent/mixed/b.bin"

  python3 "$INTEROP_DIR/scripts/transmission.py" wait --timeout 60
  python3 "$INTEROP_DIR/scripts/transmission.py" add \
    --filename /shared/fixture/mixed.torrent \
    --download-dir /shared/transmission \
    --unwanted 1

  python3 "$INTEROP_DIR/scripts/qbittorrent.py" wait --timeout 60
  python3 "$INTEROP_DIR/scripts/qbittorrent.py" add \
    --torrent "$SHARED_DIR/fixture/mixed.torrent" \
    --savepath /shared/qbittorrent \
    --unwanted 0

  start_pcap transmission
  bitrev_download /shared/fixture/mixed.torrent /shared/bitrev/mixed
}
