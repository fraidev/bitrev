#!/usr/bin/env python3
"""Transmission RPC helper. Stdlib only."""

from __future__ import annotations

import argparse
import base64
import json
import os
import time
import urllib.error
import urllib.request

URL = os.environ.get("TRANSMISSION_URL", "http://127.0.0.1:19091/transmission/rpc")
USER = os.environ.get("TRANSMISSION_USER", "interop")
PASS = os.environ.get("TRANSMISSION_PASS", "interop")


class Transmission:
    def __init__(self, url: str, user: str, password: str) -> None:
        self.url = url
        self.auth = "Basic " + base64.b64encode(f"{user}:{password}".encode()).decode()
        self.session_id = ""

    def call(self, method: str, arguments: dict | None = None) -> dict:
        payload = {"method": method, "arguments": arguments or {}}
        body = json.dumps(payload).encode()
        for _ in range(3):
            req = urllib.request.Request(self.url, data=body, method="POST")
            req.add_header("Content-Type", "application/json")
            req.add_header("Authorization", self.auth)
            if self.session_id:
                req.add_header("X-Transmission-Session-Id", self.session_id)
            try:
                with urllib.request.urlopen(req, timeout=10) as resp:
                    return json.loads(resp.read().decode())
            except urllib.error.HTTPError as err:
                if err.code == 409:
                    self.session_id = err.headers.get("X-Transmission-Session-Id", "")
                    continue
                raise
        raise RuntimeError("transmission session id dance failed")

    def wait_rpc(self, timeout: float) -> None:
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            try:
                self.call("session-get")
                return
            except Exception as err:  # noqa: BLE001
                last = err
                time.sleep(0.5)
        raise SystemExit(f"transmission rpc not ready: {last}")


def cmd_wait(args: argparse.Namespace) -> None:
    Transmission(URL, USER, PASS).wait_rpc(args.timeout)


def cmd_add(args: argparse.Namespace) -> None:
    client = Transmission(URL, USER, PASS)
    client.wait_rpc(30)
    result = client.call(
        "torrent-add",
        {
            "filename": args.filename,
            "download-dir": args.download_dir,
            "paused": False,
        },
    )
    if result.get("result") != "success":
        raise SystemExit(f"torrent-add failed: {result}")
    added = result.get("arguments", {}).get("torrent-added") or result.get(
        "arguments", {}
    ).get("torrent-duplicate")
    if not added:
        raise SystemExit(f"torrent-add missing torrent: {result}")
    torrent_id = added["id"]
    if args.unwanted:
        client.call(
            "torrent-set",
            {"ids": [torrent_id], "files-unwanted": [int(i) for i in args.unwanted]},
        )
    print(torrent_id)


def cmd_wait_done(args: argparse.Namespace) -> None:
    client = Transmission(URL, USER, PASS)
    deadline = time.time() + args.timeout
    last = None
    last_reannounce = 0.0
    while time.time() < deadline:
        result = client.call(
            "torrent-get",
            {"fields": ["id", "name", "percentDone", "status", "errorString"]},
        )
        torrents = result.get("arguments", {}).get("torrents", [])
        if torrents:
            t = torrents[0]
            last = t
            status = int(t.get("status") or 0)
            # 1/2/3 are check or download-wait. 0 stopped, 4 download, 5/6 seed.
            if float(t.get("percentDone") or 0) >= 0.999 and status not in (1, 2, 3):
                print(json.dumps(t))
                return
            now = time.time()
            if now - last_reannounce >= 2:
                client.call("torrent-reannounce", {"ids": [t["id"]]})
                last_reannounce = now
        time.sleep(0.5)
    raise SystemExit(f"transmission did not finish: {last}")


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)
    wait = sub.add_parser("wait")
    wait.add_argument("--timeout", type=float, default=60)
    add = sub.add_parser("add")
    add.add_argument("--filename", required=True)
    add.add_argument("--download-dir", required=True)
    add.add_argument("--unwanted", nargs="*", default=[])
    done = sub.add_parser("wait-done")
    done.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()
    if args.cmd == "wait":
        cmd_wait(args)
    elif args.cmd == "add":
        cmd_add(args)
    elif args.cmd == "wait-done":
        cmd_wait_done(args)


if __name__ == "__main__":
    main()
