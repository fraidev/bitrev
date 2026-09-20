# 16. File Priorities and Selective Download

Related issue: #36.

## Current state

Every file in a multi-file torrent is downloaded. The piece picker has no notion of skip or sequential order.

## Requirements

### File priority

Each file has one of: `Skip`, `Low`, `Normal` (default), `High`.

- `Skip`: those bytes are not wanted. Pieces that overlap only skipped files MUST never be requested. Pieces that overlap a skipped file and a wanted file stay wanted (the skipped file still receives the overlapping bytes on disk; that is acceptable).
- `High` pieces are preferred over `Normal`, which are preferred over `Low`, all else equal (rarest-first still applies inside a priority band).
- Changing a file from `Skip` to anything else MUST queue the missing pieces. Changing to `Skip` MUST cancel outstanding requests for now-unwanted pieces.
- Priorities persist in resume data (spec 12).

`Session` API:

- `set_file_priority(id, file_index, priority)`
- `set_file_priorities(id, Vec<FilePriority>)`
- `file_priorities(id) -> Vec<FilePriority>`
- `files(id) -> Vec<FileInfo>` where `FileInfo` has index, path, length, progress, priority.

### Sequential download

A per-torrent `sequential: bool` flag (default false).

- When set, the picker prefers lowest wanted piece index instead of rarest-first. First and last pieces of each wanted file SHOULD be fetched early (qBittorrent "first/last piece priority"), so media players can read headers and duration.
- Sequential MUST still verify SHA-1 per piece. It is an ordering hint, not a correctness change.
- `Session::set_sequential(id, bool)`.

### Progress accounting

`TorrentSnapshot.progress` and `left` MUST be computed over wanted pieces only. A torrent with every file skipped is complete (progress 1.0, state Seeding or Paused).

### CLI

`--sequential` on add. File-priority flags can wait for the API/UI; the engine API is the requirement.

## Non-goals

- Partial-piece streaming HTTP server.
- BitTorrent `so=` magnet select-only (can be layered later on this priority map).
- Padding-file awareness. This is v1 only.
