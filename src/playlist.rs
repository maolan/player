use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const APP_DIR: &str = "maolan";
const PLAYER_DIR: &str = "player";
const PLAYLIST_FILE: &str = "playlist.pls";
/// Small transient state (active tab + the single current song), persisted
/// separately so song changes don't rewrite the whole playlist file.
const STATE_FILE: &str = "player-state.toml";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    #[serde(default)]
    pub name: String,
    pub entries: Vec<PathBuf>,
}

/// Transient player state persisted separately from the playlist content:
/// the active tab plus the single current song as a (tab, index) pair
/// (`current = None` means no current song). Stored as `player-state.toml`
/// so that song changes don't rewrite the whole `playlist.pls` file. TOML
/// has no null, so `current = -1` encodes "none".
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    #[serde(default)]
    pub active_tab: usize,
    #[serde(default)]
    pub current_tab: usize,
    /// `-1` encodes "no current song" (TOML has no null).
    #[serde(default, with = "current_codec")]
    pub current: Option<usize>,
}

mod current_codec {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S>(current: &Option<usize>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        current
            .map_or(-1i64, |index| index as i64)
            .serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<usize>, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(Option::<i64>::deserialize(deserializer)?.and_then(|index| usize::try_from(index).ok()))
    }
}

impl PlayerState {
    fn from_store(store: &Playlists) -> Self {
        PlayerState {
            active_tab: store.active_tab,
            current_tab: store.current_tab,
            current: store.current,
        }
    }

    /// Apply the state to a loaded store, clamping indices to what exists.
    fn apply_to(self, store: &mut Playlists) {
        store.active_tab = self.active_tab.min(store.playlists.len() - 1);
        store.current_tab = self.current_tab.min(store.playlists.len() - 1);
        store.current = self.current;
        store.clamp_current();
    }
}

/// All open playlists plus the active tab and the single current song; this
/// is what gets persisted.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Playlists {
    pub playlists: Vec<Playlist>,
    #[serde(default)]
    pub active_tab: usize,
    /// The tab the current song belongs to; meaningful only when `current`
    /// is `Some`.
    #[serde(default)]
    pub current_tab: usize,
    /// Index of the last-started ("current") song within
    /// `playlists[current_tab]`; cleared when the entry it points at
    /// disappears. There is at most one current song across all tabs.
    #[serde(default)]
    pub current: Option<usize>,
}

impl Playlists {
    fn read_at(home: &Option<PathBuf>) -> Option<String> {
        let dir = Self::config_dir_at(home.as_ref().cloned())?;
        fs::read_to_string(dir.join(PLAYLIST_FILE)).ok()
    }

    pub fn load_from(home: Option<PathBuf>) -> Self {
        let store = Self::read_at(&home).and_then(|text| Self::parse(&text));
        let mut store = store.unwrap_or_default();
        if store.playlists.is_empty() {
            store.playlists.push(Playlist::default());
        }
        // The state file wins; without it defaults apply (no current,
        // tab 0), clamped to what exists.
        match Self::read_state_at(&home).and_then(|text| toml::from_str::<PlayerState>(&text).ok())
        {
            Some(state) => state.apply_to(&mut store),
            None => {
                store.active_tab = store.active_tab.min(store.playlists.len() - 1);
                store.clamp_current();
            }
        }
        store
    }

    pub fn load() -> Self {
        Self::load_from(std::env::var_os("HOME").map(PathBuf::from))
    }

    fn config_dir_at(home: Option<PathBuf>) -> Option<PathBuf> {
        home.map(|home| home.join(".config").join(APP_DIR).join(PLAYER_DIR))
    }

    fn save_playlists_to(&self, home: Option<PathBuf>) -> Result<(), String> {
        let path = Self::config_dir_at(home)
            .ok_or_else(|| String::from("No home directory."))?
            .join(PLAYLIST_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let text = pls_encode(self);
        fs::write(path, text).map_err(|err| err.to_string())
    }

    /// Persist playlist content (add/remove/reorder/tab changes) to
    /// `playlist.pls`.
    pub fn save_playlists(&self) -> Result<(), String> {
        self.save_playlists_to(std::env::var_os("HOME").map(PathBuf::from))
    }

    fn save_state_to(&self, home: Option<PathBuf>) -> Result<(), String> {
        let path = Self::config_dir_at(home)
            .ok_or_else(|| String::from("No home directory."))?
            .join(STATE_FILE);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let text = toml::to_string_pretty(&PlayerState::from_store(self))
            .map_err(|err| err.to_string())?;
        fs::write(path, text).map_err(|err| err.to_string())
    }

    /// Persist the transient state (active tab + the single current song) to
    /// the small `player-state.toml`, leaving `playlist.pls` untouched.
    pub fn save_state(&self) -> Result<(), String> {
        self.save_state_to(std::env::var_os("HOME").map(PathBuf::from))
    }

    fn read_state_at(home: &Option<PathBuf>) -> Option<String> {
        let dir = Self::config_dir_at(home.as_ref().cloned())?;
        fs::read_to_string(dir.join(STATE_FILE)).ok()
    }

    pub fn active(&self) -> &Playlist {
        &self.playlists[self.active_tab]
    }

    pub fn active_mut(&mut self) -> &mut Playlist {
        &mut self.playlists[self.active_tab]
    }

    /// Drop or clamp `current` when it no longer fits the entry list of
    /// `current_tab`.
    pub fn clamp_current(&mut self) {
        if let Some(index) = self.current {
            self.current = match self.playlists.get(self.current_tab) {
                Some(playlist) if !playlist.entries.is_empty() => {
                    Some(index.min(playlist.entries.len() - 1))
                }
                _ => None,
            };
        }
    }

    /// Remove the entry at `index` from tab `tab`, keeping the single
    /// current song pointing at the same entry (or clearing it when it was
    /// the one removed). Returns false when the index is out of range.
    pub fn remove_entry(&mut self, tab: usize, index: usize) -> bool {
        if tab >= self.playlists.len() || !self.playlists[tab].remove(index) {
            return false;
        }
        if self.current_tab == tab {
            self.current = match self.current {
                Some(current) if current == index => None,
                Some(current) if current > index => Some(current - 1),
                current => current,
            };
        }
        true
    }
}

impl Default for Playlists {
    fn default() -> Self {
        Playlists {
            playlists: vec![Playlist::default()],
            active_tab: 0,
            current_tab: 0,
            current: None,
        }
    }
}

impl Playlists {
    /// Parse the multi-section `.pls` store.
    fn parse(text: &str) -> Option<Self> {
        pls_decode(text)
    }
}

/// Encode the store as a Winamp `.pls` (INI-like) file, extended to
/// multiple sections: one `[<playlist name>]` section per tab with the
/// standard NumberOfEntries/FileN/TitleN/LengthN keys (the multi-section
/// layout is a documented deviation from plain .pls, used to keep all tabs
/// in one file). Player state (active tab, current song) is NOT written
/// here; it lives in `player-state.toml` — a plain .pls has no fields for
/// it, and persisting it here would rewrite the whole file on every song
/// change. Titles fall back to the file stem and lengths are -1 (unknown)
/// because the store holds no cached durations or titles.
fn pls_encode(store: &Playlists) -> String {
    let mut out = String::new();
    for playlist in &store.playlists {
        out.push_str(&format!("[{}]\n", playlist.name));
        out.push_str(&format!("NumberOfEntries={}\n", playlist.entries.len()));
        for (index, path) in playlist.entries.iter().enumerate() {
            let n = index + 1;
            out.push_str(&format!("File{n}={}\n", path.display()));
            out.push_str(&format!("Title{n}={}\n", song_title(path)));
            out.push_str(&format!("Length{n}=-1\n"));
        }
        out.push('\n');
    }
    out
}

/// Minimal INI parser for the multi-section `.pls` layout written by
/// [`pls_encode`]. Returns None when the text is not a .pls file.
fn pls_decode(text: &str) -> Option<Playlists> {
    let mut sections: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            sections.push((line[1..line.len() - 1].trim().to_string(), Vec::new()));
        } else if let Some((key, value)) = line.split_once('=') {
            sections
                .last_mut()?
                .1
                .push((key.trim().to_string(), value.to_string()));
        } else {
            return None;
        }
    }

    let mut store = Playlists::default();
    store.playlists.clear();
    for (name, keys) in &sections {
        // A leftover non-standard "player" section (from older stores) is
        // ignored; state lives in `player-state.toml` now.
        if name.eq_ignore_ascii_case("player") {
            continue;
        }
        let count = keys
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case("NumberOfEntries"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())?;
        let mut playlist = Playlist {
            name: name.clone(),
            entries: Vec::new(),
        };
        for n in 1..=count {
            let file_key = format!("File{n}");
            let (_, value) = keys.iter().find(|(key, _)| key == &file_key)?;
            playlist.entries.push(PathBuf::from(value.trim()));
        }
        store.playlists.push(playlist);
    }
    if store.playlists.is_empty() {
        return None;
    }
    store.active_tab = store.active_tab.min(store.playlists.len() - 1);
    store.clamp_current();
    Some(store)
}

impl Playlist {
    pub fn add(&mut self, paths: Vec<PathBuf>) -> usize {
        let first_new = self.entries.len();
        for path in paths {
            if !self.entries.contains(&path) {
                self.entries.push(path);
            }
        }
        first_new
    }

    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        self.entries.remove(index);
        true
    }

    /// Replace the entry order wholesale (shuffle / sort); the caller
    /// persists the store.
    pub fn reorder(&mut self, entries: Vec<PathBuf>) {
        self.entries = entries;
    }

    /// Deterministic-ish Fisher–Yates shuffle (no external RNG dependency).
    pub fn shuffled(&self) -> Vec<PathBuf> {
        let mut entries = self.entries.clone();
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9e37_79b9_7f4a_7c15);
        let mut state = seed.max(1);
        let mut next = move || {
            // xorshift64*
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_f491_4f6c_dd1d)
        };
        for i in (1..entries.len()).rev() {
            let j = (next() % (i as u64 + 1)) as usize;
            entries.swap(i, j);
        }
        entries
    }

    /// Entries sorted by display title (case-insensitive, path as
    /// tie-break), matching the playlist's cached titles by index.
    pub fn sorted_by_titles(&self, titles: &[String]) -> Vec<PathBuf> {
        let mut pairs: Vec<(String, PathBuf)> = self
            .entries
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let title = titles.get(i).cloned().unwrap_or_else(|| song_title(path));
                (title, path.clone())
            })
            .collect();
        pairs.sort_by(|a, b| {
            a.0.to_lowercase()
                .cmp(&b.0.to_lowercase())
                .then_with(|| a.1.cmp(&b.1))
        });
        pairs.into_iter().map(|(_, path)| path).collect()
    }

    pub fn next(&self, current: usize) -> Option<usize> {
        (current + 1 < self.entries.len()).then_some(current + 1)
    }

    pub fn previous(&self, current: usize) -> Option<usize> {
        current
            .checked_sub(1)
            .filter(|index| *index < self.entries.len())
    }

    /// File-stem title (no extension, no metadata); used as a fallback and
    /// for status text. Display rows use the formatted metadata titles.
    pub fn title(&self, index: usize) -> String {
        self.entries
            .get(index)
            .and_then(|path| path.file_stem())
            .and_then(|name| name.to_str())
            .unwrap_or("Unknown")
            .to_string()
    }
}

/// Smallest unused "Playlist N" name for a new tab.
pub fn next_playlist_name(playlists: &[Playlist]) -> String {
    (1..)
        .map(|n| format!("Playlist {n}"))
        .find(|name| !playlists.iter().any(|playlist| playlist.name == *name))
        .expect("an unused playlist name exists")
}

/// Resolve a tab rename draft: trimmed; empty keeps the current name
/// (revert); a name already used by another tab gets a " (2)", " (3)", ...
/// suffix so the `.pls` store keeps unique section names.
pub fn resolve_tab_name(playlists: &[Playlist], index: usize, draft: &str) -> String {
    let taken = |name: &str| {
        playlists
            .iter()
            .enumerate()
            .any(|(i, playlist)| i != index && playlist.name == name)
    };
    let base = draft.trim();
    let current = playlists
        .get(index)
        .map(|playlist| playlist.name.as_str())
        .unwrap_or_default();
    if base.is_empty() {
        return current.to_string();
    }
    if !taken(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base} ({n})");
        if !taken(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

pub fn is_supported_audio_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let ext = ext.to_ascii_lowercase();
            matches!(
                ext.as_str(),
                "wav" | "flac" | "mp3" | "ogg" | "oga" | "opus" | "m4a" | "aac" | "mp4"
            )
        })
        .unwrap_or(false)
}

pub fn filter_audio_files(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .filter(|path| is_supported_audio_file(path))
        .collect()
}

pub fn collect_audio_files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    collect_audio_files_into(dir, &mut found);
    found.sort();
    found
}

fn collect_audio_files_into(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_audio_files_into(&path, found);
        } else if file_type.is_file() && is_supported_audio_file(&path) {
            found.push(path);
        }
    }
}

/// File-stem fallback title (extension stripped); display code should
/// prefer [`format_song_title`].
pub fn song_title(path: &Path) -> String {
    path.file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or("Unknown")
        .to_string()
}

/// Display title for a song: metadata formatted with `format` (see
/// [`crate::settings::Settings::title_format`]), falling back to the file
/// name when tags are missing or the result is empty.
pub fn format_song_title(path: &Path, format: &str) -> String {
    let meta = maolan_engine::audio_codec::read_audio_metadata(path).unwrap_or_default();
    let fallback = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Unknown")
        .to_string();
    render_title(&meta, format, &fallback)
}

/// Pure half of [`format_song_title`]: substitute `{artist}`, `{song}`/
/// `{title}`, `{album}`, `{track}`, `{date}`, `{genre}`; collapse the
/// separators around missing fields; fall back when nothing remains.
pub fn render_title(
    meta: &maolan_engine::audio_codec::AudioMetadata,
    format: &str,
    fallback: &str,
) -> String {
    let lookup = |key: &str| -> Option<&str> {
        match key {
            "artist" => meta.artist.as_deref(),
            "song" | "title" => meta.title.as_deref(),
            "album" => meta.album.as_deref(),
            "track" => meta.track_number.as_deref(),
            "date" => meta.date.as_deref(),
            "genre" => meta.genre.as_deref(),
            _ => None,
        }
    };
    let mut out = String::with_capacity(format.len() + 16);
    let mut rest = format;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let key = &rest[start + 1..start + end];
        out.push_str(lookup(key).unwrap_or(""));
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    // Trim separator runs left behind by missing fields.
    let trimmed = out
        .trim()
        .trim_matches(|c: char| c == '-' || c == '|' || c == '~' || c.is_whitespace())
        .trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn meta(
        artist: Option<&str>,
        title: Option<&str>,
    ) -> maolan_engine::audio_codec::AudioMetadata {
        maolan_engine::audio_codec::AudioMetadata {
            artist: artist.map(String::from),
            title: title.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn render_title_formats_artist_and_song() {
        let result = render_title(
            &meta(Some("Audioslave"), Some("Cochise")),
            "{artist} - {song}",
            "fallback",
        );
        assert_eq!(result, "Audioslave - Cochise");
    }

    #[test]
    fn render_title_uses_fallback_when_tags_missing() {
        assert_eq!(
            render_title(&meta(None, None), "{artist} - {song}", "01. Cochise"),
            "01. Cochise"
        );
        // A missing artist leaves no dangling separator.
        assert_eq!(
            render_title(
                &meta(None, Some("Cochise")),
                "{artist} - {song}",
                "fallback"
            ),
            "Cochise"
        );
    }

    #[test]
    fn render_title_supports_other_placeholders() {
        let mut m = meta(Some("A"), Some("T"));
        m.album = Some("Album".into());
        m.track_number = Some("3".into());
        m.date = Some("2002".into());
        m.genre = Some("Rock".into());
        assert_eq!(
            render_title(&m, "{track}. {song} ({album}, {date}) [{genre}]", "x"),
            "3. T (Album, 2002) [Rock]"
        );
    }

    #[test]
    fn shuffled_preserves_entries() {
        let mut playlist = Playlist {
            name: String::from("Playlist 1"),
            entries: paths(10),
        };
        let original = playlist.entries.clone();
        let shuffled = playlist.shuffled();
        assert_eq!(shuffled.len(), original.len());
        let mut a = shuffled.clone();
        let mut b = original.clone();
        a.sort();
        b.sort();
        assert_eq!(a, b);
        // Does not mutate the playlist itself.
        assert_eq!(playlist.entries, original);
        let _ = &mut playlist;
    }

    #[test]
    fn sorted_by_titles_orders_case_insensitively() {
        let playlist = Playlist {
            name: String::from("Playlist 1"),
            entries: vec![
                PathBuf::from("/m/zeta.flac"),
                PathBuf::from("/m/Alpha.flac"),
                PathBuf::from("/m/beta.flac"),
            ],
        };
        let titles = vec![
            String::from("Zeta"),
            String::from("alpha"),
            String::from("Beta"),
        ];
        let sorted = playlist.sorted_by_titles(&titles);
        assert_eq!(
            sorted,
            vec![
                PathBuf::from("/m/Alpha.flac"),
                PathBuf::from("/m/beta.flac"),
                PathBuf::from("/m/zeta.flac"),
            ]
        );
    }

    fn paths(n: usize) -> Vec<PathBuf> {
        (0..n)
            .map(|i| PathBuf::from(format!("/music/song{i}.flac")))
            .collect()
    }

    fn temp_playlist_path() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("maolan-player-test-{unique}"))
    }

    #[test]
    fn next_index_advances_until_end() {
        let playlist = Playlist {
            name: String::from("Playlist 1"),
            entries: paths(3),
        };
        assert_eq!(playlist.next(0), Some(1));
        assert_eq!(playlist.next(1), Some(2));
        assert_eq!(playlist.next(2), None);
    }

    #[test]
    fn previous_index_goes_to_start() {
        let playlist = Playlist {
            name: String::from("Playlist 1"),
            entries: paths(3),
        };
        assert_eq!(playlist.previous(2), Some(1));
        assert_eq!(playlist.previous(1), Some(0));
        assert_eq!(playlist.previous(0), None);
    }

    #[test]
    fn add_deduplicates_and_remove_shifts() {
        let mut playlist = Playlist {
            name: String::from("Playlist 1"),
            entries: paths(2),
        };
        playlist.add(vec![PathBuf::from("/music/song0.flac")]);
        assert_eq!(playlist.entries.len(), 2);
        playlist.add(vec![PathBuf::from("/music/new.mp3")]);
        assert_eq!(playlist.entries.len(), 3);
        assert!(playlist.remove(0));
        assert_eq!(playlist.entries.len(), 2);
        assert!(!playlist.remove(5));
    }

    #[test]
    fn save_load_round_trip() {
        let dir = temp_playlist_path();
        let file = dir
            .join(".config")
            .join(APP_DIR)
            .join(PLAYER_DIR)
            .join(PLAYLIST_FILE);
        let state_file = dir
            .join(".config")
            .join(APP_DIR)
            .join(PLAYER_DIR)
            .join(STATE_FILE);
        let store = Playlists {
            playlists: vec![
                Playlist {
                    name: String::from("Playlist 1"),
                    entries: paths(3),
                },
                Playlist {
                    name: String::from("Playlist 2"),
                    entries: paths(2),
                },
            ],
            active_tab: 1,
            current_tab: 0,
            current: Some(1),
        };

        store
            .save_playlists_to(Some(dir.clone()))
            .expect("save playlists");
        store.save_state_to(Some(dir.clone())).expect("save state");
        assert!(file.exists());
        assert!(state_file.exists());
        // The store is a multi-section Winamp .pls file with playlist
        // sections only — no [player] section, no CurrentN keys.
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("[Playlist 1]"));
        assert!(text.contains("[Playlist 2]"));
        assert!(!text.contains("[player]"));
        assert!(!text.contains("Current"));
        assert!(!text.contains("ActiveTab"));
        assert!(text.contains("NumberOfEntries=3"));
        assert!(text.contains("Length1=-1"));
        let loaded = Playlists::load_from(Some(dir.clone()));
        assert_eq!(loaded, store);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn state_round_trips_through_toml_file() {
        let dir = temp_playlist_path();
        let state_file = dir
            .join(".config")
            .join(APP_DIR)
            .join(PLAYER_DIR)
            .join(STATE_FILE);
        let store = Playlists {
            playlists: vec![
                Playlist {
                    name: String::from("Rock"),
                    entries: paths(3),
                },
                Playlist {
                    name: String::from("Chill"),
                    entries: paths(2),
                },
            ],
            active_tab: 1,
            current_tab: 0,
            current: Some(2),
        };
        store.save_state_to(Some(dir.clone())).expect("save state");
        store
            .save_playlists_to(Some(dir.clone()))
            .expect("save playlists");
        let text = fs::read_to_string(&state_file).unwrap();
        assert!(text.contains("active_tab = 1"));
        assert!(text.contains("current_tab = 0"));
        assert!(text.contains("current = 2"));
        let loaded = Playlists::load_from(Some(dir.clone()));
        assert_eq!(loaded, store);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn state_clamps_across_tabs() {
        let dir = temp_playlist_path();
        let file = dir
            .join(".config")
            .join(APP_DIR)
            .join(PLAYER_DIR)
            .join(PLAYLIST_FILE);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            "[Rock]\nNumberOfEntries=2\nFile1=/music/song0.flac\nFile2=/music/song1.flac\n\n\
             [Chill]\nNumberOfEntries=3\nFile1=/music/song0.flac\nFile2=/music/song1.flac\nFile3=/music/song2.flac\n",
        )
        .unwrap();
        fs::write(
            dir.join(".config")
                .join(APP_DIR)
                .join(PLAYER_DIR)
                .join(STATE_FILE),
            "active_tab = 5\ncurrent_tab = 7\ncurrent = 3\n",
        )
        .unwrap();
        let loaded = Playlists::load_from(Some(dir.clone()));
        assert_eq!(loaded.active_tab, 1);
        assert_eq!(loaded.current_tab, 1);
        assert_eq!(loaded.current, Some(2));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_state_file_defaults_to_no_current_tab_zero() {
        let dir = temp_playlist_path();
        let file = dir
            .join(".config")
            .join(APP_DIR)
            .join(PLAYER_DIR)
            .join(PLAYLIST_FILE);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            "[Rock]\nNumberOfEntries=2\nFile1=/music/song0.flac\nFile2=/music/song1.flac\n",
        )
        .unwrap();
        let loaded = Playlists::load_from(Some(dir.clone()));
        assert_eq!(loaded.playlists.len(), 1);
        assert_eq!(loaded.active_tab, 0);
        assert_eq!(loaded.current, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_and_reorder_persist_updated_state() {
        let dir = temp_playlist_path();
        let mut store = Playlists {
            playlists: vec![Playlist {
                name: String::from("Rock"),
                entries: paths(3),
            }],
            active_tab: 0,
            current_tab: 0,
            current: Some(2),
        };
        // Removing an earlier entry shifts the current index down; the
        // in-memory change is what save_state persists.
        assert!(store.remove_entry(0, 0));
        assert_eq!(store.current, Some(1));
        store.save_state_to(Some(dir.clone())).expect("save state");
        store
            .save_playlists_to(Some(dir.clone()))
            .expect("save playlists");
        let loaded = Playlists::load_from(Some(dir.clone()));
        assert_eq!(loaded.current, Some(1));

        // A reorder remaps the current song to its new index (the app
        // matches paths against the pre-reorder order).
        let before = store.playlists[0].entries.clone();
        let reversed: Vec<PathBuf> = store.playlists[0].entries.iter().rev().cloned().collect();
        let current_path = store.current.and_then(|index| before.get(index)).cloned();
        store.playlists[0].reorder(reversed);
        store.current = current_path.and_then(|path| {
            store.playlists[0]
                .entries
                .iter()
                .position(|entry| *entry == path)
        });
        store.save_state_to(Some(dir.clone())).expect("save state");
        store
            .save_playlists_to(Some(dir.clone()))
            .expect("save playlists");
        let loaded = Playlists::load_from(Some(dir.clone()));
        assert_eq!(loaded.current, store.current);
        assert!(loaded.current.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pls_decode_reads_standard_single_section_file() {
        // A classic single-[playlist] .pls (as other apps write it) loads
        // as one unnamed tab.
        let text = "[playlist]\n\
                    NumberOfEntries=2\n\
                    File1=/music/a.flac\n\
                    Title1=Alpha\n\
                    Length1=-1\n\
                    File2=/music/b.mp3\n\
                    Title2=Beta\n\
                    Length2=180\n";
        let store = pls_decode(text).expect("decode .pls");
        assert_eq!(store.playlists.len(), 1);
        assert_eq!(
            store.playlists[0].entries,
            vec![
                PathBuf::from("/music/a.flac"),
                PathBuf::from("/music/b.mp3")
            ]
        );
        assert_eq!(store.active_tab, 0);
    }

    #[test]
    fn pls_round_trip_preserves_tabs() {
        let store = Playlists {
            playlists: vec![
                Playlist {
                    name: String::from("Rock"),
                    entries: vec![PathBuf::from("/music/a b.flac")],
                },
                Playlist {
                    name: String::from("Chill"),
                    entries: paths(2),
                },
            ],
            active_tab: 1,
            ..Default::default()
        };
        let decoded = pls_decode(&pls_encode(&store)).expect("decode");
        // .pls carries playlist content only; state lives elsewhere.
        assert_eq!(decoded.playlists, store.playlists);
        assert_eq!(decoded.active_tab, 0);
    }

    #[test]
    fn pls_decode_rejects_non_pls_text() {
        assert!(pls_decode("").is_none());
        assert!(pls_decode("{\"playlists\": []}").is_none());
        assert!(pls_decode("no equals sign here").is_none());
        assert!(pls_decode("[player]\nActiveTab=0\n").is_none());
        assert!(pls_decode("[Rock]\nNumberOfEntries=1\n").is_none());
    }

    #[test]
    fn load_always_keeps_at_least_one_playlist() {
        let dir = temp_playlist_path();
        let loaded = Playlists::load_from(Some(dir.clone()));
        assert_eq!(loaded.playlists.len(), 1);
        assert_eq!(loaded.active_tab, 0);
    }

    #[test]
    fn remove_entry_shifts_and_clears_current() {
        let mut store = Playlists {
            playlists: vec![Playlist {
                name: String::from("Rock"),
                entries: paths(3),
            }],
            active_tab: 0,
            current_tab: 0,
            current: Some(2),
        };
        // Removing an earlier row shifts the current index down.
        assert!(store.remove_entry(0, 0));
        assert_eq!(store.current, Some(1));
        // Removing the current row clears it.
        assert!(store.remove_entry(0, 1));
        assert_eq!(store.current, None);
        // Removing after the current row leaves it alone.
        let mut store = Playlists {
            playlists: vec![Playlist {
                name: String::from("Rock"),
                entries: paths(3),
            }],
            active_tab: 0,
            current_tab: 0,
            current: Some(0),
        };
        assert!(store.remove_entry(0, 2));
        assert_eq!(store.current, Some(0));
        // A removal in another tab does not touch the current song.
        let mut store = Playlists {
            playlists: vec![
                Playlist {
                    name: String::from("Rock"),
                    entries: paths(3),
                },
                Playlist {
                    name: String::from("Chill"),
                    entries: paths(3),
                },
            ],
            active_tab: 1,
            current_tab: 0,
            current: Some(1),
        };
        assert!(store.remove_entry(1, 0));
        assert_eq!(store.current, Some(1));
    }

    fn named(names: &[&str]) -> Vec<Playlist> {
        names
            .iter()
            .map(|name| Playlist {
                name: (*name).to_string(),
                entries: Vec::new(),
            })
            .collect()
    }

    #[test]
    fn next_playlist_name_skips_taken_names() {
        let playlists = named(&["Playlist 1", "Playlist 2"]);
        assert_eq!(next_playlist_name(&playlists), "Playlist 3");
        assert_eq!(next_playlist_name(&[]), "Playlist 1");
    }

    #[test]
    fn resolve_tab_name_trims_and_keeps_unique_names() {
        let playlists = named(&["Rock", "Chill"]);
        assert_eq!(resolve_tab_name(&playlists, 0, "  Jazz  "), "Jazz");
        // Unchanged name is fine for the tab itself.
        assert_eq!(resolve_tab_name(&playlists, 0, "Rock"), "Rock");
        // Whitespace-only or empty draft reverts to the current name.
        assert_eq!(resolve_tab_name(&playlists, 0, "   "), "Rock");
        assert_eq!(resolve_tab_name(&playlists, 1, ""), "Chill");
    }

    #[test]
    fn resolve_tab_name_suffixes_collisions() {
        let playlists = named(&["Rock", "Chill"]);
        assert_eq!(resolve_tab_name(&playlists, 1, "Rock"), "Rock (2)");
        let playlists = named(&["Rock", "Rock (2)", "Chill"]);
        assert_eq!(resolve_tab_name(&playlists, 2, "Rock"), "Rock (3)");
        // Renaming the tab that already owns the suffixed name keeps it.
        assert_eq!(resolve_tab_name(&playlists, 1, "Rock (2)"), "Rock (2)");
    }

    #[test]
    fn filter_keeps_audio_extensions_case_insensitively() {
        let files = vec![
            PathBuf::from("a.wav"),
            PathBuf::from("b.FLAC"),
            PathBuf::from("c.mp3"),
            PathBuf::from("d.txt"),
            PathBuf::from("e"),
        ];
        let filtered = filter_audio_files(files);
        assert_eq!(
            filtered,
            vec![
                PathBuf::from("a.wav"),
                PathBuf::from("b.FLAC"),
                PathBuf::from("c.mp3")
            ]
        );
    }

    #[test]
    fn supported_extensions_are_case_insensitive() {
        assert!(is_supported_audio_file(Path::new("song.FLAC")));
        assert!(is_supported_audio_file(Path::new("song.wav")));
        assert!(is_supported_audio_file(Path::new("song.Opus")));
        assert!(is_supported_audio_file(Path::new("song.mp4")));
        assert!(!is_supported_audio_file(Path::new("notes.txt")));
        assert!(!is_supported_audio_file(Path::new("no_extension")));
        assert!(!is_supported_audio_file(Path::new(".hidden")));
    }

    #[test]
    fn collect_audio_files_walks_recursively_and_sorts() {
        let dir = temp_playlist_path();
        let nested = dir.join("nested");
        let deep = nested.join("deep");
        fs::create_dir_all(&deep).unwrap();
        fs::write(dir.join("b.mp3"), "").unwrap();
        fs::write(dir.join("a.flac"), "").unwrap();
        fs::write(dir.join("notes.txt"), "").unwrap();
        fs::write(nested.join("c.WAV"), "").unwrap();
        fs::write(deep.join("d.ogg"), "").unwrap();
        fs::write(deep.join("cover.png"), "").unwrap();

        let found = collect_audio_files(&dir);
        assert_eq!(
            found,
            vec![
                dir.join("a.flac"),
                dir.join("b.mp3"),
                nested.join("c.WAV"),
                deep.join("d.ogg"),
            ]
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn collect_audio_files_skips_unreadable_and_empty_dirs() {
        let dir = temp_playlist_path();
        fs::create_dir_all(&dir).unwrap();
        assert!(collect_audio_files(&dir).is_empty());
        assert!(collect_audio_files(&dir.join("does-not-exist")).is_empty());
        let _ = fs::remove_dir_all(dir);
    }
}
