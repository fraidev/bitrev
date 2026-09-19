#!/usr/bin/env python3
"""qBittorrent Web API helper. Stdlib only."""

from __future__ import annotations

import argparse
import json
import os
import time
import urllib.error
import urllib.parse
import urllib.request

URL = os.environ.get("QBITTORRENT_URL", "http://127.0.0.1:18080")
USER = os.environ.get("QBITTORRENT_USER", "admin")
PASS = os.environ.get("QBITTORRENT_PASS", "adminadmin")


class Qbittorrent:
    def __init__(self, url: str) -> None:
        self.url = url.rstrip("/")
        self.opener = urllib.request.build_opener(
            urllib.request.HTTPCookieProcessor()
        )

    def _open(self, path: str, data: bytes | None = None, headers: dict | None = None):
        req = urllib.request.Request(self.url + path, data=data)
        for key, value in (headers or {}).items():
            req.add_header(key, value)
        return self.opener.open(req, timeout=10)

    def login(self) -> None:
        body = urllib.parse.urlencode(
            {"username": USER, "password": PASS}
        ).encode()
        try:
            self._open(
                "/api/v2/auth/login",
                data=body,
                headers={"Content-Type": "application/x-www-form-urlencoded"},
            ).read()
        except urllib.error.HTTPError:
            pass

    def version(self) -> str:
        return self._open("/api/v2/app/version").read().decode().strip()

    def wait(self, timeout: float) -> None:
        deadline = time.time() + timeout
        last = None
        while time.time() < deadline:
            try:
                self.login()
                self.version()
                return
            except Exception as err:  # noqa: BLE001
                last = err
                time.sleep(0.5)
        raise SystemExit(f"qbittorrent webui not ready: {last}")

    def add(
        self,
        torrent_path: str,
        savepath: str,
        skip_checking: bool,
        paused: bool,
        unwanted: list[int],
    ) -> None:
        with open(torrent_path, "rb") as fh:
            torrent_bytes = fh.read()
        boundary = "----BitrevInteropBoundary"
        chunks: list[bytes] = []

        def field(name: str, value: str) -> None:
            chunks.append(f"--{boundary}\r\n".encode())
            chunks.append(
                f'Content-Disposition: form-data; name="{name}"\r\n\r\n'.encode()
            )
            chunks.append(value.encode())
            chunks.append(b"\r\n")

        chunks.append(f"--{boundary}\r\n".encode())
        chunks.append(
            b'Content-Disposition: form-data; name="torrents"; filename="payload.torrent"\r\n'
        )
        chunks.append(b"Content-Type: application/x-bittorrent\r\n\r\n")
        chunks.append(torrent_bytes)
        chunks.append(b"\r\n")
        field("savepath", savepath)
        field("skip_checking", "true" if skip_checking else "false")
        field("paused", "true" if paused else "false")
        field("autoTMM", "false")
        chunks.append(f"--{boundary}--\r\n".encode())
        body = b"".join(chunks)
        self._open(
            "/api/v2/torrents/add",
            data=body,
            headers={"Content-Type": f"multipart/form-data; boundary={boundary}"},
        ).read()
        if unwanted:
            info: list[dict] = []
            deadline = time.time() + 15
            while time.time() < deadline:
                info = self.info()
                if info:
                    break
                time.sleep(0.25)
            if not info:
                raise SystemExit("qbittorrent add produced no torrent")
            torrent_hash = info[0]["hash"]
            for idx in unwanted:
                data = urllib.parse.urlencode(
                    {"hash": torrent_hash, "id": str(idx), "priority": "0"}
                ).encode()
                self._open(
                    "/api/v2/torrents/filePrio",
                    data=data,
                    headers={"Content-Type": "application/x-www-form-urlencoded"},
                ).read()

    def info(self) -> list[dict]:
        raw = self._open("/api/v2/torrents/info").read().decode()
        return json.loads(raw)

    def wait_done(self, timeout: float) -> dict:
        deadline = time.time() + timeout
        last = None
        last_reannounce = 0.0
        while time.time() < deadline:
            torrents = self.info()
            if torrents:
                t = torrents[0]
                last = t
                progress = float(t.get("progress") or 0)
                state = t.get("state", "")
                if progress >= 0.999 and state not in {
                    "error",
                    "missingFiles",
                    "unknown",
                }:
                    print(json.dumps(t))
                    return t
                now = time.time()
                if now - last_reannounce >= 2:
                    data = urllib.parse.urlencode({"hashes": t["hash"]}).encode()
                    try:
                        self._open(
                            "/api/v2/torrents/reannounce",
                            data=data,
                            headers={
                                "Content-Type": "application/x-www-form-urlencoded"
                            },
                        ).read()
                    except urllib.error.HTTPError:
                        pass
                    last_reannounce = now
            time.sleep(0.5)
        raise SystemExit(f"qbittorrent did not finish: {last}")


def main() -> None:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="cmd", required=True)
    wait = sub.add_parser("wait")
    wait.add_argument("--timeout", type=float, default=60)
    add = sub.add_parser("add")
    add.add_argument("--torrent", required=True)
    add.add_argument("--savepath", required=True)
    add.add_argument("--skip-checking", action="store_true")
    add.add_argument("--paused", action="store_true")
    add.add_argument("--unwanted", nargs="*", default=[], type=int)
    done = sub.add_parser("wait-done")
    done.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()
    client = Qbittorrent(URL)
    if args.cmd == "wait":
        client.wait(args.timeout)
    elif args.cmd == "add":
        client.wait(30)
        client.add(
            args.torrent,
            args.savepath,
            args.skip_checking,
            args.paused,
            args.unwanted,
        )
    elif args.cmd == "wait-done":
        client.wait(30)
        client.wait_done(args.timeout)


if __name__ == "__main__":
    main()
