#!/usr/bin/env python3
"""Connect to bitrev and record the BEP-0003 / BEP-0010 handshakes."""

from __future__ import annotations

import argparse
import json
import socket
import struct
import time

PSTR = b"BitTorrent protocol"
EXT_FLAG_BYTE = 5
EXT_FLAG_BIT = 0x10
DHT_FLAG_BYTE = 7
DHT_FLAG_BIT = 0x01
MSG_EXTENDED = 20


def hex_to_bytes(value: str) -> bytes:
    value = value.strip().lower()
    if len(value) != 40:
        raise SystemExit(f"info-hash must be 40 hex chars, got {len(value)}")
    return bytes.fromhex(value)


def connect_with_retry(host: str, port: int, timeout: float) -> socket.socket:
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        sock.settimeout(min(5.0, max(0.5, deadline - time.time())))
        try:
            sock.connect((host, port))
            return sock
        except OSError as err:
            last = err
            sock.close()
            time.sleep(0.25)
    raise SystemExit(f"connect {host}:{port} failed: {last}")


def read_exact(sock: socket.socket, n: int) -> bytes:
    buf = b""
    while len(buf) < n:
        chunk = sock.recv(n - len(buf))
        if not chunk:
            raise SystemExit(f"peer closed after {len(buf)}/{n} bytes")
        buf += chunk
    return buf


def outgoing_handshake(info_hash: bytes) -> bytes:
    reserved = bytearray(8)
    reserved[EXT_FLAG_BYTE] |= EXT_FLAG_BIT
    peer_id = b"-LT1100-interopinspect"
    return bytes([len(PSTR)]) + PSTR + bytes(reserved) + info_hash + peer_id


def parse_handshake(raw: bytes) -> dict:
    if len(raw) != 68 or raw[0] != 19:
        raise SystemExit(f"bad handshake length {len(raw)}")
    reserved = raw[20:28]
    return {
        "pstr": raw[1:20].decode("ascii", errors="replace"),
        "reserved_hex": reserved.hex(),
        "dht": bool(reserved[DHT_FLAG_BYTE] & DHT_FLAG_BIT),
        "extension_protocol": bool(reserved[EXT_FLAG_BYTE] & EXT_FLAG_BIT),
        "info_hash": raw[28:48].hex(),
        "peer_id": raw[48:68].decode("latin1", errors="replace"),
        "peer_id_hex": raw[48:68].hex(),
    }


def read_extended_handshake(sock: socket.socket, timeout: float) -> dict:
    sock.settimeout(timeout)
    deadline = time.time() + timeout
    while time.time() < deadline:
        header = read_exact(sock, 4)
        (length,) = struct.unpack("!I", header)
        if length == 0:
            continue
        if length > 1024 * 1024:
            raise SystemExit(f"oversize frame {length}")
        payload = read_exact(sock, length)
        msg_id = payload[0]
        if msg_id != MSG_EXTENDED:
            continue
        if len(payload) < 2:
            continue
        ext_id = payload[1]
        body = payload[2:]
        if ext_id != 0:
            continue
        return {
            "payload_hex": body.hex(),
            "has_ut_pex": b"4:ut_pex" in body,
            "has_ut_metadata": b"11:ut_metadata" in body,
        }
    raise SystemExit("timed out waiting for extension handshake")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", type=int, default=6881)
    parser.add_argument("--info-hash", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--timeout", type=float, default=30)
    args = parser.parse_args()

    info_hash = hex_to_bytes(args.info_hash)
    sock = connect_with_retry(args.host, args.port, args.timeout)
    try:
        sock.sendall(outgoing_handshake(info_hash))
        raw = read_exact(sock, 68)
        handshake = parse_handshake(raw)
        extension = None
        if handshake["extension_protocol"]:
            extension = read_extended_handshake(sock, args.timeout)
    finally:
        sock.close()

    report = {
        "handshake": handshake,
        "extension": extension,
    }
    with open(args.out, "w", encoding="utf-8") as fh:
        json.dump(report, fh, indent=2)
        fh.write("\n")
    print(json.dumps(report))

    errors = []
    if handshake["dht"]:
        errors.append("DHT reserved bit is set")
    if extension and extension["has_ut_pex"]:
        errors.append("ut_pex advertised in extension handshake")
    if errors:
        raise SystemExit("; ".join(errors))


if __name__ == "__main__":
    main()
