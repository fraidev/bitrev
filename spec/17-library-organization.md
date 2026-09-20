# 17 - Library Organization

Related issue: #37. Required for useful Sonarr/Radarr use (they send `category` and `savepath` on add).

## Current state

A torrent has an output directory and nothing else. No categories, tags, completed-folder, or watch directory.

## Requirements

### Categories

- A category is a name plus a default save path (path may be empty, meaning "use the session download_dir").
- A torrent has zero or one category.
- `create_category(name, save_path)`, `edit_category`, `remove_category` (torrents in a removed category keep their files, category field becomes empty), `categories() -> Vec<Category>`.
- Adding a torrent with a category that does not exist MUST create it (qBittorrent does this; Sonarr relies on it).
- Persist categories in `<state_dir>/categories.toml` (or next to resume data). Persist the torrent's category in resume data.

### Tags

- A torrent has a set of string tags. Tags are free-form, not predeclared.
- `set_tags(id, tags)`, `add_tags`, `remove_tags`.
- Persist in resume data.

### Save path

- Per-torrent `save_path` overrides category path and `download_dir`.
- `set_save_path(id, path, move_files: bool)`: if `move_files`, relocate on-disk data then update resume data. Snapshot state is `Moving` until done. Failure sets `Error` and leaves files where they are.

### Completed directory

Config: `completed_dir` (empty = disabled).

- When a torrent reaches 100% wanted pieces, if `completed_dir` is set, move data there (or to `completed_dir/<category>` when a category is set). Then update `save_path`.
- Sonarr/Radarr usually import from the save path and then ask the client to delete. Move-on-complete is for standalone use; MUST be skippable per torrent (`auto_tmm` / "automatic torrent management" style flag, default off when a save path was supplied on add).

### Watch directory

Config: `watch_dir` (empty = disabled).

- Poll or inotify/FSEvents the directory.
- A new `*.torrent` is added (then the file SHOULD be moved to `<state_dir>/watch-processed/` or deleted, configurable).
- A new `*.magnet` file (or a file whose contents start with `magnet:`) is added the same way.
- Duplicate info hash: ignore, do not error the watcher.
- Optional per-subdir category: `watch_dir/tv/*.torrent` gets category `tv` if that convention is enabled.

### Add options

`AddTorrentOptions` fields from spec 15 become live: `save_path`, `category`, `tags`, `paused`.

## Non-goals

- RSS (Sonarr/Radarr own that).
- Automatic renaming of media files.
- Cross-seed / hardlink helpers.
