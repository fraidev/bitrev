# bitrev HTTP server

`bitrev serve` binds the daemon from `server.host` / `server.port` (default `127.0.0.1:8080`). Cookie name is `SID`.

## Sonarr and Radarr

Point a qBittorrent download client at this process. The facade is mounted at `/api/v2` when `server.qbittorrent_compat` is true (the default).

| Sonarr / Radarr field | Value |
| --- | --- |
| Client | qBittorrent |
| Host | `127.0.0.1` |
| Port | `8080` |
| Username | `server.username` (default `admin`) |
| Password | `server.password`, or the generated password printed on first start |
| Category | `tv-sonarr` or `radarr` |

The API advertises qBittorrent `v4.6.7` and Web API `2.9.3`, so those apps use `pause` / `resume`. `stop` / `start` are accepted as aliases.

Check a running daemon without Sonarr:

```sh
BITREV_PASSWORD=secret crates/server/scripts/sonarr-check.sh
```

The script logs in, adds a magnet in category `tv-sonarr`, and requires that hash to show up in `torrents/info?category=tv-sonarr`. Override the target with `BITREV_URL` (default `http://127.0.0.1:8080`).
