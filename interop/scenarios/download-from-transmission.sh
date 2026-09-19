# Transmission seeds. bitrev downloads through opentracker.
run_scenario() {
  gen_fixture_host
  copy_single_payload "$SHARED_DIR/transmission"
  python3 "$INTEROP_DIR/scripts/transmission.py" wait --timeout 60
  python3 "$INTEROP_DIR/scripts/transmission.py" add \
    --filename /shared/fixture/payload.bin.torrent \
    --download-dir /shared/transmission
  python3 "$INTEROP_DIR/scripts/transmission.py" wait-done --timeout "$TIMEOUT"
  start_pcap transmission
  bitrev_download
}
