#!/usr/bin/env python3
"""libtorrent-backed scripted peer for skipped interop scenarios."""

from __future__ import annotations

import argparse
import os
import time

try:
    import libtorrent as lt
except ImportError as err:
    raise SystemExit(f"python3-libtorrent is required: {err}") from err


def settings_pack(
    port: int,
    encryption: str,
    utp_only: bool,
    pex: bool,
    dht: bool,
) -> "lt.settings_pack":
    pack = lt.settings_pack()
    pack.set_str(lt.settings_pack.listen_interfaces, f"0.0.0.0:{port}")
    pack.set_bool(lt.settings_pack.enable_dht, dht)
    pack.set_bool(lt.settings_pack.enable_lsd, False)
    pack.set_bool(lt.settings_pack.enable_upnp, False)
    pack.set_bool(lt.settings_pack.enable_natpmp, False)
    pack.set_bool(lt.settings_pack.enable_outgoing_utp, True)
    pack.set_bool(lt.settings_pack.enable_incoming_utp, True)
    pack.set_bool(lt.settings_pack.enable_outgoing_tcp, not utp_only)
    pack.set_bool(lt.settings_pack.enable_incoming_tcp, not utp_only)
    # 0 = forced, 1 = enabled, 2 = disabled
    if encryption == "required":
        pack.set_int(lt.settings_pack.out_enc_policy, 0)
        pack.set_int(lt.settings_pack.in_enc_policy, 0)
    elif encryption == "disabled":
        pack.set_int(lt.settings_pack.out_enc_policy, 2)
        pack.set_int(lt.settings_pack.in_enc_policy, 2)
    else:
        pack.set_int(lt.settings_pack.out_enc_policy, 1)
        pack.set_int(lt.settings_pack.in_enc_policy, 1)
    return pack


def add_torrent(ses: "lt.session", torrent: str, save_path: str, seed: bool):
    info = lt.torrent_info(torrent)
    flags = info.flags() if hasattr(info, "flags") else 0
    atp = lt.add_torrent_params()
    atp.ti = info
    atp.save_path = save_path
    if seed:
        atp.flags |= getattr(lt.torrent_flags, "seed_mode", 0)
    handle = ses.add_torrent(atp)
    return handle, info


def wait_done(handle, timeout: float) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        status = handle.status()
        if status.is_seeding or status.progress >= 0.999:
            return
        time.sleep(0.5)
    status = handle.status()
    raise SystemExit(
        f"libtorrent did not finish progress={status.progress:.3f} error={status.errc}"
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("role", choices=["seed", "leech"])
    parser.add_argument("--torrent", required=True)
    parser.add_argument("--save-path", required=True)
    parser.add_argument("--port", type=int, default=6882)
    parser.add_argument(
        "--encryption",
        choices=["disabled", "enabled", "required"],
        default="enabled",
    )
    parser.add_argument("--utp-only", action="store_true")
    parser.add_argument("--pex", action=argparse.BooleanOptionalAction, default=True)
    parser.add_argument("--dht", action=argparse.BooleanOptionalAction, default=False)
    parser.add_argument("--timeout", type=float, default=90)
    args = parser.parse_args()

    os.makedirs(args.save_path, exist_ok=True)
    ses = lt.session(
        settings_pack(args.port, args.encryption, args.utp_only, args.pex, args.dht)
    )
    handle, _info = add_torrent(ses, args.torrent, args.save_path, args.role == "seed")
    if args.role == "leech":
        wait_done(handle, args.timeout)
        print("complete")
        return
    print(f"seeding on {args.port}", flush=True)
    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
