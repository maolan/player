# Maolan Player

A simple desktop audio player built on [maolan-engine](https://github.com/maolan) and [maolan-widgets](iced 0.14 widgets). It plays through the same engine and audio backends as the Maolan DAW, with a playlist and basic transport controls.

## Features

- Multiple playlists in tabs: add/close tabs with the tab bar, click a tab to switch, double-click a tab label to rename it inline (Enter or clicking another tab commits, Escape cancels; an empty name reverts and a name already used by another tab gets a " (2)"-style suffix). Each playlist has add-files / add-folder (recursive) import, removal, click-to-select, and double-click-to-play; persisted to `~/.config/maolan/player/playlist.pls`.
- Transport: play/pause toggle, stop, previous song, next song, loop, shuffle, sort. "Previous"/"next" switch songs rather than seeking. Playback auto-advances at the end of a song.
- Audio settings screen (gear button): audio backend, output device, and buffer size — the same `audio_setup` widget as the Maolan editor's start screen. Settings persist to `~/.config/maolan/player/config.toml` (TOML, like the Maolan DAW's `config.toml`).
- Decode via maolan-engine: WAV/PCM, FLAC, MP3, Ogg FLAC (OxideAV) with a Symphonia fallback (Ogg Vorbis, AAC, ALAC, MP4/M4A).

## Building and running

Requires a Rust toolchain (edition 2024). Debug builds work but the engine is heavy — release is recommended for actual listening:

```bash
cargo build --release
cargo run --release
```

The audio device is chosen with this precedence:

1. First command-line argument, e.g. `maolan-player /dev/dsp5`
2. The `MAOLAN_PLAYER_DEVICE` environment variable
3. Saved settings from the settings screen
4. Per-OS default:
   - FreeBSD: OSS device for the `hw.snd.default_unit` sysctl (e.g. `/dev/dsp5`); falls back to `/dev/dsp`
   - Linux: ALSA `default`
   - Windows: WASAPI `default`
   - macOS: CoreAudio default output device
   - JACK is available on Unix via any of the overrides above (`jack`)

## Playlist file format

The playlist store is a Winamp `.pls` (INI-like) file extended to multiple sections: one `[<tab name>]` section per playlist with the standard `NumberOfEntries` / `FileN` / `TitleN` / `LengthN` keys (the multi-section layout is a documented deviation from plain .pls, used to keep all tabs in one file). Player state — the active tab and the single current song, a `(tab, index)` pair — is kept separately in `~/.config/maolan/player/player-state.toml` as `active_tab = <n>`, `current_tab = <n>`, and `current = <index>`; only one song can be current across all tabs. TOML has no null, so `current = -1` means "no current song" (an absent `current`/`current_tab` also defaults to none/tab 0). This keeps song changes rewriting only this small file, not the whole playlist. It is re-persisted whenever the current song or active tab changes, clamped to the entry count on load, and restored on startup so the transport Play button resumes it. `TitleN` falls back to the file name without extension and `LengthN` is always `-1` (unknown), since the store keeps no cached durations. On startup the player loads the `.pls` store and then the state file; any load failure (missing or malformed file) falls back to defaults. Legacy stores (`playlist.json`, `player-state.json`, a `[player]` section in the `.pls`, the old `~/.config/maolan-player/` location) are no longer read.

## Controls

| Button | Action |
|---|---|
| File-plus | Add audio files (multi-select dialog) |
| Folder | Add a folder recursively |
| Skip-back | Previous song in the playlist |
| Play / Pause | Toggle play/pause (shows pause symbol while playing) |
| Square | Stop |
| Skip-forward | Next song in the playlist |
| Trash | Remove selected song from the playlist |
| Repeat | Toggle looping the playlist at the end of a song |
| Shuffle | Randomize the playlist order |
| List-ordered | Sort the playlist by title |
| Gear | Audio settings (backend, device, buffer size) |

## Keyboard shortcuts

Global shortcuts (no modifiers), active anywhere in the window but not while a text input (e.g., the title format field) is focused:

| Key | Action |
|---|---|
| Z | Previous song |
| X | Play (start, or resume from where pause stopped) |
| C | Pause (stops the engine but remembers the position; play resumes from there) |
| V | Stop (clears the remembered position; play starts from the beginning) |
| B | Next song |
| Ctrl+Q | Quit |

Pause is implemented as an engine stop (the engine's pause only mutes
clips), so the remembered position is what makes Play resume mid-song.
Switching songs discards it.

## Known issues

- On FreeBSD OSS, large buffer sizes (periods above ~1024 frames) can play back slower than realtime with some device setups (observed with virtual_oss): the playhead crawls and audio drops out. The default picks a small period; if you raise the buffer in settings and hit this, lower it again. The engine-side root cause is not yet fixed.
- Applying new audio settings restarts the engine client in place (the engine only tears down its hardware worker cleanly for JACK).

## Repository layout

- `src/main.rs` — iced application entry, per-OS default device, tokio executor
- `src/app.rs` — engine client wiring, transport state, UI update/view
- `src/playlist.rs` — playlist model (per-tab) and multi-playlist store with persistence, recursive folder import
- `src/settings.rs` — backend/device model, discovery, settings persistence
- `src/buffer.rs` — buffer-size ladders per backend
- `examples/playback_repro.rs` — headless playback check (engine open + play + transport sampling)
