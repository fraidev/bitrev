# 18 - Bandwidth Limits and Download Queue

Related issue: #38. Config keys `upload_limit` / `download_limit` and `seed.ratio_limit` are already sketched in spec 13.

## Current state

No rate limiting. Every torrent starts immediately. A daemon with many *arr-added torrents will saturate the uplink and exceed connection caps.

## Requirements

### Rate limits

- Session-global `download_limit` / `upload_limit` in bytes/sec. `0` means unlimited (default).
- Per-torrent overrides, `0` meaning "use global".
- Alternative (scheduled) limits: `alt_download_limit`, `alt_upload_limit`, plus a weekly schedule or a manual toggle. When alt mode is on, the alt pair replaces the global pair. qBittorrent's "alternative speed limits" is the model; the compat layer (spec 24) maps to it.
- Enforcement: token bucket or equivalent on the write path (upload) and on issuing new block requests (download). Burst of one piece is acceptable. Limits apply to payload bytes, not TCP overhead.

`Session::set_rate_limits`, `set_torrent_rate_limits`, `set_alt_limits`, `set_alt_mode(bool)`.

### Queue

Config:

| Key | Default | Notes |
|-----|---------|-------|
| `queue.max_active_downloads` | 5 | torrents in `Downloading` |
| `queue.max_active_uploads` | 8 | torrents in `Seeding` that still upload |
| `queue.max_active` | 0 (unlimited) | downloads + uploads |
| `queue.dont_count_slow` | true | a torrent under ~1 KiB/s does not consume a slot |

- Newly added torrents that would exceed a cap enter `Queued` and stay paused-for-queue until a slot frees (completion, pause, remove). `Queued` torrents do not announce or connect. Resume records queued-not-paused.
- Force-start (`Session::set_force_start(id, true)`) bypasses the queue. Sonarr uses this.
- Order is insertion order unless the user sets a queue position (`set_queue_position`).

### Seed limits

Honor spec 13:

- `seed.ratio_limit` (0 = unlimited). When `uploaded / downloaded` reaches the limit, pause the torrent (state `Paused`, qBittorrent `pausedUP` / `stoppedUP`).
- `seed.time_limit` minutes after completion (0 = unlimited).
- Per-torrent overrides (`ratio_limit`, `seeding_time_limit`). Sonarr sends these on add.

A paused-at-limit torrent MUST stay on disk and in the library so Sonarr can import and then remove it.

## Non-goals

- Per-peer rate limits.
- QoS / DSCP marking.
- Network-interface binding (can land later as a config key).
