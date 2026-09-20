# 15. Session control plane

The `Session` is a long-lived owner of many torrents. Frontends (CLI, daemon,
Web UI, Sonarr/Radarr via later HTTP layers) talk to this API. They do not
drive peer connections themselves.

This spec is the library contract. HTTP, JSON, and auth are spec 24.

## TorrentId

`TorrentId` is the info hash (`[u8; 20]`). `Display` is 40 lowercase hex
characters, the same encoding as resume filenames.

Adding the same hash twice is idempotent: the existing id is returned and no
second swarm is started.

## TorrentState

Per-torrent state replaces the global `DownloadState` that previously paused
every torrent at once. The wire/tracker layer still uses `DownloadState` as a
derived transfer-enable flag (`Downloading` vs `Paused`).

| State | Meaning | Transfer |
| --- | --- | --- |
| `Checking` | Hashing existing pieces (`recheck`, or a verify on add) | no |
| `Metadata` | Magnet / info-hash only; waiting for the info dict | yes (ut_metadata) |
| `Downloading` | Missing pieces, transferring | yes |
| `Seeding` | Complete, uploading | yes |
| `Paused` | User paused. Peers stay connected but do not request or unchoke | no |
| `Queued` | Waiting for a queue slot (issue 38). No announces or peer connects | no |
| `Moving` | Move-on-complete in progress (issue 37). Nothing enters this state yet | no |
| `Error(String)` | Failed. Message is in the snapshot | no |

Transitions emit `SessionEvent::StateChanged`. `Queued` and `Moving` exist so
later issues do not change the enum.

Session-wide `pause_all()` / `resume_all()` remain for the one-shot CLI. They
pause or resume every torrent. `Session::pause()` / `resume()` without an id
are not part of this API.

## Session API

```text
Session::new() / Session::with_options(opts)
    One-shot. Does not reload resume files. CLI uses this.

Session::open(opts) -> Session
    Scans resume_dir(state_dir) for *.resume. load_optional each.
    Re-adds from the cached .torrent at torrent_path (or the standard
    torrents/<hex>.torrent path). Restores paused, counters, category,
    tags, sequential, file_priorities. Missing or corrupt entries are
    skipped with a warning.

add_torrent(AddTorrentOptions) -> AddTorrentResult
    Result.id is the TorrentId. Result still carries torrent, torrent_meta,
    pr_rx, resume_status, already_have for the CLI. Duplicate hash returns
    the existing id.

remove_torrent(id, delete_files)
    Shut the torrent down, announce stopped, delete the resume file and
    the cached .torrent. When delete_files is true, delete data through
    storage::file_path.

pause(id) / resume(id)
recheck(id)
    Go to Checking, clear the in-memory bitfield, run verify_existing_pieces,
    then return to Paused, Seeding, or Downloading.

list() -> Vec<TorrentSnapshot>
snapshot(id) -> Option<TorrentSnapshot>
subscribe() -> broadcast::Receiver<SessionEvent>
transfer_stats() -> TransferStats
```

`AddTorrentOptions` keeps `From<TorrentMeta>` and `TryFrom<&str>` (path or
magnet). New optional fields, stored on `TorrentSession` and in resume data:

- `save_path` (alias of `output_dir`)
- `paused`
- `category` (empty until issue 37)
- `tags` (empty until issue 37)
- `sequential` (false until issue 36)
- `skip_checking`
- `file_priorities` (stored, ignored until issue 36)

`skip_checking` skips `verify_existing_pieces` on add. `verify(true)` still
forces a slow-path rehash. File-priority and category behavior are not
implemented here.

## TorrentSnapshot

| Field | Source |
| --- | --- |
| `id` | info hash |
| `name` | torrent name |
| `info_hash` | `[u8; 20]` |
| `state` | `TorrentState` |
| `error` | `Some` when state is `Error` |
| `progress` | `downloaded / size` (0 when size is 0) |
| `downloaded` | `TorrentDownloadedState::downloaded_bytes` |
| `uploaded` | `TorrentSession.uploaded` |
| `left` | `TorrentDownloadedState::left_bytes` |
| `size` | torrent length |
| `download_rate` / `upload_rate` | 5 s EMA, bytes/s |
| `peers` | connected entries in `PeerStates.states` |
| `seeds` | connected peers whose bitfield has every piece |
| `save_path` | output dir / file path |
| `torrent_path` | cached `.torrent` |
| `category` / `tags` / `sequential` | stored values, unused |
| `added_at` / `completed_at` | unix seconds |
| `piece_count` / `pieces_have` | piece vector / downloaded flags |
| `ratio` | `uploaded / downloaded` (0 when downloaded is 0) |

Rates are a roughly 5 s EMA computed by a 1 Hz session task from the
downloaded and uploaded counters. The first tick only records a baseline.

## SessionEvent

```text
Added { id }
Removed { id }
StateChanged { id, state }
Progress { id, snapshot }     // coalesced at 1 Hz; snapshot is boxed
Completed { id }
Error { id, message }
```

`tokio::sync::broadcast`. Capacity is small. A lagged subscriber sees
`RecvError::Lagged` and may continue or drop. The engine never awaits a
receiver (`let _ = tx.send(...)`).

`Progress` is emitted for torrents in `Downloading`, `Seeding`, `Metadata`,
or `Checking`. `Completed` fires once when `completed_at` is first set.

## Resume

`RESUME_VERSION` is 2. Version 1 files still load. Missing v2 keys default
to empty / false / empty priorities.

v2 adds `category`, `tags`, `sequential`, and `file_priorities`. `paused`
stays per-torrent (it was already a resume field). `Session::open` is what
reloads a previous session. `Session::new` does not.

## Out of scope

- HTTP, JSON, auth (spec 24, issues 44-47)
- File priority behavior and sequential download (issue 36)
- Category/tag behavior and move-on-complete (issue 37)
- Queue slots. `Queued` exists, nothing enters it (issue 38)
- CLI progress UI rewrite
