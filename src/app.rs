use std::path::PathBuf;
use std::time::{Duration, Instant};

use maolan_engine::audio_codec::decode_audio_to_f32_interleaved_sync;
use maolan_engine::client::Client as EngineClient;
use maolan_engine::kind::Kind;
use maolan_engine::message::{Action as EngineAction, Message as EngineMessage, generate_clip_id};
use maolan_widgets::audio_setup::{AudioSetupAction, AudioSetupState, audio_setup};
use maolan_widgets::iced::futures::SinkExt;
use maolan_widgets::iced::widget::{
    Button, Column, Row, Stack, Text, button, column, container, row, scrollable, text, text_input,
};
use maolan_widgets::iced::{
    self, Color, Element, Length, Subscription, Task, Theme, stream, time, window,
};
use maolan_widgets::iced_fonts::lucide::{
    file_plus, folder_open, list_ordered, pause, play, repeat, settings as settings_icon, shuffle,
    skip_back, skip_forward, square, trash_two,
};

use crate::playlist::{
    Playlist, Playlists, collect_audio_files, filter_audio_files, format_song_title,
    next_playlist_name, resolve_tab_name, song_title,
};
use crate::settings::{
    Backend, DEFAULT_BITS, DEFAULT_N_PERIODS, Device, Settings, backend_for_device,
    default_ring_buffer_multiplier, discover_devices,
};

const TRACK: &str = "player";
const VOLUME_MIN_DB: f32 = -60.0;
const VOLUME_MAX_DB: f32 = 5.0;
const VOLUME_RANGE: std::ops::RangeInclusive<f32> = VOLUME_MIN_DB..=VOLUME_MAX_DB;

/// Format a dB value with sign and one decimal, zero-padded to two integer
/// digits (`-05.3 dB`, `+03.0 dB`) so the label width stays constant while
/// the value moves between -10 and +10.
fn format_db_padded(db: f32) -> String {
    let sign = if db < 0.0 { '-' } else { '+' };
    let abs = db.abs();
    if abs < 10.0 {
        format!("{sign}0{abs:.1} dB")
    } else {
        format!("{sign}{abs:.1} dB")
    }
}

fn format_volume(db: f32) -> String {
    if db <= VOLUME_MIN_DB {
        String::from("mute")
    } else {
        format_db_padded(db)
    }
}

/// Peak level across channels for the VU readout.
fn peak_readout(levels_db: &[f32]) -> String {
    let peak = levels_db.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if peak <= -90.0 {
        String::from("-inf dB")
    } else {
        format_db_padded(peak)
    }
}
const SAMPLE_RATE_HZ: i32 = 48_000;

#[derive(Debug, Clone)]
pub struct LoadedSong {
    pub length_frames: usize,
}

#[derive(Debug, Clone)]
pub enum Message {
    EngineReady(Result<EngineClient, String>),
    AddFilesPressed,
    AddFolderPressed,
    FilesAdded(Vec<PathBuf>),
    Select(usize),
    PlaySelected,
    /// Play the restored "current" song when nothing is selected yet.
    PlayCurrent,
    PlayIndex(usize),
    SongLoaded(Result<LoadedSong, String>),
    PlayToggled,
    /// Always start/resume playback (keyboard X), unlike the toggle.
    PlayPressed,
    /// Always pause playback (keyboard C), unlike the toggle.
    PausePressed,
    StopPressed,
    NextPressed,
    PreviousPressed,
    RemovePressed,
    Tick,
    SettingsPressed,
    SettingsClosed,
    SettingsAction(AudioSetupAction<Backend, Device>),
    VolumeChanged(f32),
    VolumeReleased,
    PositionChanged(f32),
    PositionReleased,
    RowPressed(usize),
    /// Switch the active playlist tab.
    TabPressed(usize),
    /// Rename a playlist tab (inline edit; started by double-clicking a tab).
    TabRenameChanged(String),
    /// Commit the tab rename draft (Enter, or clicking another tab).
    TabRenameSubmitted,
    /// Discard the tab rename draft (Escape).
    TabRenameCancelled,
    /// Close a playlist tab.
    TabClosePressed(usize),
    /// Open a new playlist tab.
    TabAddPressed,
    MeterTick,
    LoopToggled,
    ShufflePressed,
    SortPressed,
    TitleFormatChanged(String),
    TitleFormatSubmitted,
    WindowOpened(iced::window::Id),
    WindowCloseRequested(iced::window::Id),
    TrayShowRequested,
    TrayQuitRequested,
    QuitRequested,
    /// Keyboard events that don't map to an action.
    Ignored,
}

/// Two presses on the same playlist row within this window count as a
/// double-click and start playback.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);
/// Meter poll rate. Lower than the maolan mixer's meter strip (40 ms) to
/// keep idle CPU down; 10 fps is smooth enough for a playback VU meter.
const METER_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub struct PlayerApp {
    client: Option<EngineClient>,
    status: String,
    playlists: Playlists,
    titles: Vec<String>,
    selected: Option<usize>,
    playing: bool,
    /// Paused-with-remembered-position: the engine was stopped but the resume
    /// frame is kept so Play continues from where pause happened.
    paused: bool,
    /// Transport frame to resume from after a pause-as-stop; cleared by hard
    /// stop and by switching to another song.
    paused_position: Option<usize>,
    song_length_frames: usize,
    position_frames: usize,
    seek_seconds: Option<f32>,
    last_row_press: Option<(usize, Instant)>,
    /// Last tab press for double-click detection (double-click renames).
    last_tab_press: Option<(usize, Instant)>,
    /// Tab currently being renamed inline, plus the drafted name.
    renaming_tab: Option<usize>,
    rename_draft: String,
    /// Stable widget id so the rename input can be focused when editing
    /// starts.
    rename_input_id: iced::widget::Id,
    volume_db: f32,
    /// Last master output levels (dB) from the engine, for the VU meter.
    levels_db: Vec<f32>,
    loop_playlist: bool,
    /// Editable copy of the metadata display format while typing.
    title_format_edit: String,
    main_window_id: Option<iced::window::Id>,
    settings: Settings,
    device_override: Option<String>,
    show_settings: bool,
    setup: Option<AudioSetupState<Backend, Device>>,
    /// Bumped by every mutation that changes how the playlist rows look or
    /// behave; the single `lazy` wrapping the rows column rebuilds only when
    /// this (plus the cheap scalar row inputs) changes, so 25 fps meter ticks
    /// don't rebuild hundreds of rows.
    playlist_revision: u64,
    /// (artist, song) metadata of the current song for the tray tooltip,
    /// cached at load time so the periodic tick doesn't re-read the file.
    tray_song: Option<(String, String)>,
    /// Whole seconds value last pushed to the tray tooltip, to avoid
    /// re-sending on every 250 ms tick.
    tray_pushed_secs: Option<u64>,
}

impl PlayerApp {
    /// Display title for a playlist row or status line: the formatted
    /// metadata title, falling back to the extension-less file stem.
    fn display_title(&self, index: usize) -> String {
        self.titles
            .get(index)
            .cloned()
            .unwrap_or_else(|| self.playlists.active().title(index))
    }

    /// Rebuild the display titles for the active playlist's entries from the
    /// current metadata format string.
    fn refresh_titles(&mut self) {
        self.playlist_revision += 1;
        let format = self.settings.title_format.clone();
        self.titles = self
            .playlists
            .active()
            .entries
            .iter()
            .map(|path| format_song_title(path, &format))
            .collect();
    }

    /// Settings with the CLI/env device override applied, if any.
    fn effective_settings(&self) -> Settings {
        match &self.device_override {
            Some(device_id) => self.settings.with_device_override(device_id),
            None => self.settings.clone(),
        }
    }

    /// Playback snapshot for the tray hover tooltip.
    fn tray_info(&self) -> crate::tray::TrayInfo {
        let (artist, song) = match &self.tray_song {
            Some((artist, song)) => (artist.clone(), Some(song.clone())),
            None => (String::new(), None),
        };
        crate::tray::TrayInfo {
            artist,
            song,
            elapsed_secs: (self.position_frames / SAMPLE_RATE_HZ as usize) as u64,
            total_secs: (self.song_length_frames / SAMPLE_RATE_HZ as usize) as u64,
        }
    }

    /// Cache the current song's (artist, song) metadata for the tray tooltip,
    /// falling back to the file stem when tags are missing.
    fn cache_tray_song(&mut self, path: &std::path::Path) {
        let meta = maolan_engine::audio_codec::read_audio_metadata(path).unwrap_or_default();
        let song = meta
            .title
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("Unknown")
                    .to_string()
            });
        self.tray_song = Some((meta.artist.unwrap_or_default(), song));
    }

    fn build_setup_state(&self) -> AudioSetupState<Backend, Device> {
        let effective = self.effective_settings();
        let backend = backend_for_device(&effective.device_id);
        let devices = discover_devices(backend);
        let selected_device = devices
            .iter()
            .find(|device| device.id == effective.device_id)
            .cloned()
            .or_else(|| devices.first().cloned());
        let period_frames = period_options_for(selected_device.as_ref());
        let selected_period_frames = if period_frames.contains(&effective.period_frames) {
            Some(effective.period_frames)
        } else {
            period_frames.last().copied()
        };
        AudioSetupState {
            backends: Backend::ALL.to_vec(),
            selected_backend: backend,
            show_input_device: false,
            input_devices: Vec::new(),
            selected_input_device: None,
            show_output_device: true,
            output_devices: devices,
            selected_output_device: selected_device,
            show_sample_rate: false,
            sample_rates: Vec::new(),
            selected_sample_rate: None,
            show_bit_depth: false,
            bit_depths: Vec::new(),
            selected_bit_depth: None,
            show_period_frames: true,
            period_frames,
            selected_period_frames,
            show_n_periods: false,
            n_periods: Vec::new(),
            selected_n_periods: None,
            show_exclusive: false,
            exclusive: true,
            show_sync_mode: false,
            sync_mode: false,
            plugins_loaded: true,
            can_start: true,
            status_message: String::new(),
        }
    }
}

/// Period options for the selected device (OSS ladder when the device
/// advertises a kernel buffer size, default ladder otherwise).
fn period_options_for(device: Option<&Device>) -> Vec<usize> {
    let (max_buffer_bytes, max_channels) = device
        .map(|device| (device.max_buffer_bytes, device.max_channels.max(1)))
        .unwrap_or((0, 0));
    crate::buffer::period_options(max_buffer_bytes, max_channels, DEFAULT_BITS as usize)
}

pub fn new(device_override: Option<String>) -> (PlayerApp, Task<Message>) {
    let playlists = Playlists::load();
    let settings = Settings::load();
    let effective = match &device_override {
        Some(device_id) => settings.with_device_override(device_id),
        None => settings.clone(),
    };
    let (device, period_frames, n_periods) = effective.open_action();
    let ring_buffer_multiplier = effective.ring_buffer_multiplier;
    let volume_db = effective.volume_db;
    // Always start hidden: the daemon runs with no window until the tray
    // Show action opens one.
    let task = Task::perform(
        open_engine(device, period_frames, n_periods, ring_buffer_multiplier),
        Message::EngineReady,
    );
    let mut app = PlayerApp {
        client: None,
        status: String::from("Starting audio engine..."),
        playlists,
        titles: Vec::new(),
        selected: None,
        playing: false,
        paused: false,
        paused_position: None,
        song_length_frames: 0,
        position_frames: 0,
        seek_seconds: None,
        last_row_press: None,
        last_tab_press: None,
        renaming_tab: None,
        rename_draft: String::new(),
        rename_input_id: iced::widget::Id::unique(),
        volume_db,
        levels_db: vec![-90.0, -90.0],
        loop_playlist: effective.loop_playlist,
        title_format_edit: effective.title_format.clone(),
        main_window_id: None,
        settings,
        device_override,
        show_settings: false,
        setup: None,
        playlist_revision: 0,
        tray_song: None,
        tray_pushed_secs: None,
    };
    // The persisted "current" song (from player-state.toml) is already
    // restored and clamped by `Playlists::load`, so the transport Play
    // button can resume it without loading audio yet; the highlight appears
    // once the window opens.
    app.refresh_titles();
    crate::tray::init_tray();
    (app, task)
}

pub fn update(app: &mut PlayerApp, message: Message) -> Task<Message> {
    match message {
        Message::EngineReady(Ok(client)) => {
            app.client = Some(client);
            app.status = String::from("Ready.");
            // Rows gain their on_press handlers once the engine is up.
            app.playlist_revision += 1;
            // Re-apply the persisted master volume on the fresh engine.
            let volume_db = app.volume_db;
            if let Some(client) = app.client.clone() {
                return Task::perform(
                    send_action(
                        client,
                        EngineAction::TrackLevel("hw:out".to_string(), volume_db),
                    ),
                    |_| Message::Tick,
                );
            }
            Task::none()
        }
        Message::EngineReady(Err(err)) => {
            app.status = format!("Engine error: {err}");
            Task::none()
        }
        Message::AddFilesPressed => {
            let files = rfd::FileDialog::new()
                .add_filter(
                    "Audio",
                    &[
                        "wav", "flac", "mp3", "ogg", "oga", "opus", "m4a", "aac", "mp4",
                    ],
                )
                .pick_files()
                .unwrap_or_default();
            update(app, Message::FilesAdded(files))
        }
        Message::AddFolderPressed => {
            let files = rfd::FileDialog::new()
                .pick_folder()
                .map(|dir| collect_audio_files(&dir))
                .unwrap_or_default();
            update(app, Message::FilesAdded(files))
        }
        Message::FilesAdded(files) => {
            let files = filter_audio_files(files);
            if files.is_empty() {
                app.status = String::from("No supported audio files found.");
            } else {
                let first = app.playlists.active_mut().add(files);
                let _ = app.playlists.save_playlists();
                app.selected = Some(first);
                app.refresh_titles();
                app.status = String::from("Files added.");
            }
            Task::none()
        }
        Message::Select(index) => {
            app.selected = Some(index);
            app.playlist_revision += 1;
            Task::none()
        }
        Message::RowPressed(index) => {
            let now = Instant::now();
            let double_click = app.last_row_press.is_some_and(|(last, at)| {
                last == index && now.duration_since(at) < DOUBLE_CLICK_WINDOW
            });
            app.last_row_press = Some((index, now));
            if double_click {
                app.last_row_press = None;
                update(app, Message::PlayIndex(index))
            } else {
                update(app, Message::Select(index))
            }
        }
        Message::TabPressed(index) => {
            if index >= app.playlists.playlists.len() {
                return Task::none();
            }
            let now = Instant::now();
            let double_click = app.last_tab_press.is_some_and(|(last, at)| {
                last == index && now.duration_since(at) < DOUBLE_CLICK_WINDOW
            });
            app.last_tab_press = Some((index, now));
            // While renaming, a click on the tab being edited does not
            // switch tabs (it goes to the text input); a click on another
            // tab commits the draft first, then switches.
            if app.renaming_tab == Some(index) {
                return Task::none();
            }
            if app.renaming_tab.is_some() {
                commit_tab_rename(app);
            }
            if double_click {
                app.last_tab_press = None;
                app.renaming_tab = Some(index);
                app.rename_draft = app.playlists.playlists[index].name.clone();
                return iced::widget::operation::focus(app.rename_input_id.clone());
            }
            if index != app.playlists.active_tab {
                app.playlists.active_tab = index;
                app.selected = None;
                app.refresh_titles();
                let _ = app.playlists.save_state();
            }
            Task::none()
        }
        Message::TabRenameChanged(draft) => {
            app.rename_draft = draft;
            Task::none()
        }
        Message::TabRenameSubmitted => {
            commit_tab_rename(app);
            Task::none()
        }
        Message::TabRenameCancelled => {
            app.renaming_tab = None;
            app.rename_draft = String::new();
            Task::none()
        }
        Message::TabClosePressed(index) => {
            if index < app.playlists.playlists.len() {
                if app.renaming_tab == Some(index) {
                    app.renaming_tab = None;
                    app.rename_draft = String::new();
                }
                let was_current_tab = index == app.playlists.current_tab;
                app.playlists.playlists.remove(index);
                if app.playlists.playlists.is_empty() {
                    app.playlists.playlists.push(Playlist::default());
                }
                let len = app.playlists.playlists.len();
                if let Some(renaming) = app.renaming_tab
                    && index < renaming
                {
                    app.renaming_tab = Some(renaming - 1);
                }
                if index < app.playlists.active_tab {
                    app.playlists.active_tab -= 1;
                } else if index == app.playlists.active_tab {
                    app.playlists.active_tab = app.playlists.active_tab.min(len - 1);
                }
                // Keep current_tab pointing at the same playlist after the
                // indices shift; closing the tab of the playing song just
                // drops the highlight, playback continues.
                if index < app.playlists.current_tab {
                    app.playlists.current_tab -= 1;
                } else if was_current_tab {
                    app.playlists.current = None;
                }
                let _ = app.playlists.save_playlists();
                let _ = app.playlists.save_state();
                app.refresh_titles();
            }
            Task::none()
        }
        Message::TabAddPressed => {
            let name = next_playlist_name(&app.playlists.playlists);
            app.playlists.playlists.push(Playlist {
                name,
                ..Default::default()
            });
            app.playlists.active_tab = app.playlists.playlists.len() - 1;
            let _ = app.playlists.save_playlists();
            let _ = app.playlists.save_state();
            app.selected = None;
            app.refresh_titles();
            Task::none()
        }
        Message::PlaySelected => match app.selected {
            Some(index) => update(app, Message::PlayIndex(index)),
            None => Task::none(),
        },
        Message::PlayCurrent => {
            // Nothing selected yet but a persisted current song exists:
            // switch to its tab and start it there.
            match app.playlists.current {
                Some(index) => {
                    if app.playlists.active_tab != app.playlists.current_tab {
                        app.playlists.active_tab = app.playlists.current_tab;
                        app.refresh_titles();
                    }
                    update(app, Message::PlayIndex(index))
                }
                None => Task::none(),
            }
        }
        Message::PlayIndex(index) => {
            let Some(client) = app.client.clone() else {
                return Task::none();
            };
            if index >= app.playlists.active().entries.len() {
                return Task::none();
            }
            app.status = format!("Loading {}...", app.display_title(index));
            let path = app.playlists.active().entries[index].clone();
            app.cache_tray_song(&path);
            app.playlists.current_tab = app.playlists.active_tab;
            app.playlists.current = Some(index);
            // The current-song highlight lives in the rows.
            app.playlist_revision += 1;
            // A fresh song always starts from the beginning.
            app.paused_position = None;
            let _ = app.playlists.save_state();
            Task::perform(play_song(client, path), Message::SongLoaded)
        }
        Message::SongLoaded(Ok(song)) => {
            app.playing = true;
            app.paused = false;
            app.paused_position = None;
            app.song_length_frames = song.length_frames;
            app.position_frames = 0;
            app.seek_seconds = None;
            app.status = app
                .playlists
                .current
                .map(|index| app.display_title(index))
                .unwrap_or_else(|| String::from("Playing"));
            crate::tray::update_tooltip(app.tray_info());
            app.tray_pushed_secs = Some(0);
            Task::none()
        }
        Message::SongLoaded(Err(err)) => {
            app.playing = false;
            app.paused = false;
            app.paused_position = None;
            app.tray_song = None;
            crate::tray::update_tooltip(app.tray_info());
            app.status = err.clone();
            Task::none()
        }
        Message::PlayToggled => {
            if app.playing && !app.paused {
                pause_as_stop(app)
            } else if app.paused {
                resume_playback(app)
            } else if app.selected.is_some() {
                update(app, Message::PlaySelected)
            } else {
                update(app, Message::PlayCurrent)
            }
        }
        Message::PlayPressed => {
            if app.paused {
                resume_playback(app)
            } else if !app.playing {
                if app.selected.is_some() {
                    update(app, Message::PlaySelected)
                } else {
                    update(app, Message::PlayCurrent)
                }
            } else {
                Task::none()
            }
        }
        Message::PausePressed => {
            if app.playing && !app.paused {
                return pause_as_stop(app);
            }
            Task::none()
        }
        Message::StopPressed => {
            app.playing = false;
            app.paused = false;
            app.paused_position = None;
            app.position_frames = 0;
            app.seek_seconds = None;
            app.tray_song = None;
            app.tray_pushed_secs = None;
            crate::tray::update_tooltip(app.tray_info());
            if let Some(client) = app.client.clone() {
                return Task::perform(send_action(client, EngineAction::Stop), |_| Message::Tick);
            }
            Task::none()
        }
        Message::NextPressed => {
            let playlist = &app.playlists.playlists[app.playlists.current_tab];
            let next = app
                .playlists
                .current
                .and_then(|index| playlist.next(index))
                .or((playlist.entries.len() > 1).then_some(0));
            match next {
                Some(index) => update(app, Message::PlayIndex(index)),
                None => update(app, Message::StopPressed),
            }
        }
        Message::PreviousPressed => {
            let playlist = &app.playlists.playlists[app.playlists.current_tab];
            match app
                .playlists
                .current
                .and_then(|index| playlist.previous(index))
            {
                Some(index) => update(app, Message::PlayIndex(index)),
                None => Task::none(),
            }
        }
        Message::RemovePressed => {
            if let Some(index) = app.selected {
                let was_current = app.playlists.current_tab == app.playlists.active_tab
                    && app.playlists.current == Some(index);
                app.playlists.remove_entry(app.playlists.active_tab, index);
                let _ = app.playlists.save_playlists();
                let _ = app.playlists.save_state();
                app.refresh_titles();
                // `remove_entry` already shifted/cleared the current song
                // when it lives in this tab; removal in another tab leaves
                // it untouched.
                app.selected = None;
                if was_current {
                    return update(app, Message::StopPressed);
                }
            }
            Task::none()
        }
        Message::LoopToggled => {
            app.loop_playlist = !app.loop_playlist;
            app.settings.loop_playlist = app.loop_playlist;
            if let Err(err) = app.settings.save() {
                app.status = err;
            }
            Task::none()
        }
        Message::ShufflePressed => {
            if app.playlists.active().entries.len() > 1 {
                let before = app.playlists.active().entries.clone();
                let shuffled = app.playlists.active().shuffled();
                app.playlists.active_mut().reorder(shuffled);
                if app.playlists.current_tab == app.playlists.active_tab {
                    remap_indices(app, &before);
                }
                let _ = app.playlists.save_playlists();
                let _ = app.playlists.save_state();
                app.refresh_titles();
            }
            Task::none()
        }
        Message::SortPressed => {
            if app.playlists.active().entries.len() > 1 {
                let before = app.playlists.active().entries.clone();
                let sorted = app.playlists.active().sorted_by_titles(&app.titles);
                app.playlists.active_mut().reorder(sorted);
                if app.playlists.current_tab == app.playlists.active_tab {
                    remap_indices(app, &before);
                }
                let _ = app.playlists.save_playlists();
                let _ = app.playlists.save_state();
                app.refresh_titles();
            }
            Task::none()
        }
        Message::TitleFormatChanged(value) => {
            app.title_format_edit = value;
            Task::none()
        }
        Message::TitleFormatSubmitted => {
            app.settings.title_format = app.title_format_edit.trim().to_string();
            if app.settings.title_format.is_empty() {
                app.settings.title_format = crate::settings::default_title_format();
                app.title_format_edit = app.settings.title_format.clone();
            }
            if let Err(err) = app.settings.save() {
                app.status = err;
            }
            app.refresh_titles();
            Task::none()
        }
        Message::WindowOpened(id) => {
            app.main_window_id = Some(id);
            Task::none()
        }
        Message::WindowCloseRequested(id) => {
            // "Close to tray" on Wayland: destroying the window is the only
            // way to make it disappear; the daemon keeps running and the
            // tray Show action reopens it. Quit via the tray menu.
            app.main_window_id = None;
            window::close(id)
        }
        Message::TrayShowRequested => {
            if let Some(id) = app.main_window_id {
                window::gain_focus(id)
            } else {
                open_main_window()
            }
        }
        Message::TrayQuitRequested | Message::QuitRequested => std::process::exit(0),
        Message::Ignored => Task::none(),
        Message::MeterTick => {
            if let Some(meters) = app.client.as_ref().and_then(|c| c.meter_snapshot()) {
                app.levels_db = meters.hw_out_db;
            }
            Task::none()
        }
        Message::Tick => {
            if !app.playing || app.paused {
                return Task::none();
            }
            let snapshot = app
                .client
                .as_ref()
                .and_then(|client| client.transport_snapshot());
            let Some(snapshot) = snapshot else {
                return Task::none();
            };
            if !snapshot.playing {
                app.playing = false;
                app.paused = false;
                app.status = String::from("Stopped.");
                return Task::none();
            }
            app.position_frames = snapshot.sample;
            let secs = (app.position_frames / SAMPLE_RATE_HZ as usize) as u64;
            if app.tray_pushed_secs != Some(secs) {
                app.tray_pushed_secs = Some(secs);
                crate::tray::update_tooltip(app.tray_info());
            }
            if app.song_length_frames > 0 && snapshot.sample >= app.song_length_frames {
                let playlist = &app.playlists.playlists[app.playlists.current_tab];
                let next = app
                    .playlists
                    .current
                    .and_then(|index| playlist.next(index))
                    .or_else(|| app.loop_playlist.then_some(0))
                    .filter(|_| !playlist.entries.is_empty());
                match next {
                    Some(index) => update(app, Message::PlayIndex(index)),
                    None => update(app, Message::StopPressed),
                }
            } else {
                Task::none()
            }
        }
        Message::PositionChanged(seconds) => {
            app.seek_seconds = Some(seconds);
            Task::none()
        }
        Message::PositionReleased => {
            let Some(seconds) = app.seek_seconds.take() else {
                return Task::none();
            };
            let frames = (seconds.max(0.0) * SAMPLE_RATE_HZ as f32).round() as usize;
            app.position_frames = frames;
            if app.paused {
                // While paused (engine stopped), a slider seek becomes the
                // resume point.
                app.paused_position = Some(frames);
            }
            if let Some(client) = app.client.clone() {
                return Task::perform(
                    send_action(client, EngineAction::TransportPosition(frames)),
                    |_| Message::Tick,
                );
            }
            Task::none()
        }
        Message::VolumeChanged(volume_db) => {
            app.volume_db = volume_db;
            if let Some(client) = app.client.clone() {
                return Task::perform(
                    send_action(
                        client,
                        EngineAction::TrackLevel("hw:out".to_string(), volume_db),
                    ),
                    |_| Message::Tick,
                );
            }
            Task::none()
        }
        Message::VolumeReleased => {
            app.settings.volume_db = app.volume_db;
            if let Err(err) = app.settings.save() {
                app.status = err;
            }
            Task::none()
        }
        Message::SettingsPressed => {
            app.show_settings = true;
            app.setup = Some(app.build_setup_state());
            Task::none()
        }
        Message::SettingsClosed => {
            app.show_settings = false;
            app.setup = None;
            Task::none()
        }
        Message::SettingsAction(action) => {
            let Some(setup) = app.setup.as_mut() else {
                return Task::none();
            };
            match action {
                AudioSetupAction::BackendSelected(backend) => {
                    let devices = discover_devices(backend);
                    let selected_device = devices.first().cloned();
                    let period_frames = period_options_for(selected_device.as_ref());
                    setup.selected_backend = backend;
                    setup.output_devices = devices;
                    setup.selected_output_device = selected_device;
                    setup.period_frames = period_frames.clone();
                    // Default to the largest buffer for the new backend.
                    setup.selected_period_frames = period_frames.last().copied();
                }
                AudioSetupAction::OutputDeviceSelected(device) => {
                    let period_frames = period_options_for(Some(&device));
                    setup.selected_output_device = Some(device);
                    setup.period_frames = period_frames.clone();
                    setup.selected_period_frames = setup
                        .selected_period_frames
                        .filter(|selected| period_frames.contains(selected))
                        .or_else(|| period_frames.last().copied());
                }
                AudioSetupAction::PeriodFramesSelected(period) => {
                    setup.selected_period_frames = Some(period);
                }
                AudioSetupAction::Start => {
                    let Some(setup) = app.setup.take() else {
                        return Task::none();
                    };
                    let device_id = setup
                        .selected_output_device
                        .map(|device| device.id)
                        .unwrap_or_else(|| setup.selected_backend.default_device_id());
                    let period_frames = setup.selected_period_frames.unwrap_or(1024);
                    app.settings = Settings {
                        device_id,
                        period_frames,
                        n_periods: DEFAULT_N_PERIODS,
                        ring_buffer_multiplier: default_ring_buffer_multiplier(),
                        volume_db: app.volume_db,
                        title_format: app.settings.title_format.clone(),
                        loop_playlist: app.loop_playlist,
                    };
                    if let Err(err) = app.settings.save() {
                        app.status = err;
                    }
                    app.show_settings = false;
                    app.playing = false;
                    app.paused = false;
                    app.paused_position = None;
                    app.playlists.current = None;
                    app.song_length_frames = 0;
                    app.tray_song = None;
                    app.tray_pushed_secs = None;
                    crate::tray::update_tooltip(app.tray_info());
                    // Dropping the client closes the command channel, which
                    // makes the engine work loop exit and release the audio
                    // device (`engine/src/engine/runtime.rs`: `rx.recv()`
                    // returning None breaks the loop). Wait briefly so the
                    // old engine releases the device before reopening it.
                    app.client = None;
                    app.playlist_revision += 1;
                    app.status = String::from("Reopening audio device...");
                    let settings = app.effective_settings();
                    let (device, period_frames, n_periods) = settings.open_action();
                    let ring_buffer_multiplier = settings.ring_buffer_multiplier;
                    return Task::perform(
                        reopen_engine(device, period_frames, n_periods, ring_buffer_multiplier),
                        Message::EngineReady,
                    );
                }
                _ => {}
            }
            Task::none()
        }
    }
}

/// Cache key for the single `lazy` wrapping the whole playlist rows column.
/// The `revision` covers entry/title/order mutations (bumped by
/// `refresh_titles` and the other row-affecting updates); the cheap scalar
/// row inputs are hashed too so a missed bump still can't serve stale
/// selection/highlight/interactivity.
#[derive(Hash)]
struct RowsKey {
    revision: u64,
    engine_ok: bool,
    is_current_tab: bool,
    selected: Option<usize>,
    current: Option<usize>,
}

/// Build the playlist rows column: one button per entry (current-song and
/// selection highlights, click to select, double-click to play) plus the
/// empty-state hint. Owned data only — this is the content of the `lazy`
/// wrapping the playlist, so it must produce a `'static` tree.
fn playlist_rows_column(
    entries: &[PathBuf],
    titles: &[String],
    selected: Option<usize>,
    current: Option<usize>,
    is_current_tab: bool,
    engine_ok: bool,
) -> Column<'static, Message> {
    let mut list = Column::new().spacing(2);
    if entries.is_empty() {
        list = list.push(
            container(text("Playlist is empty. Add audio files with the folder button.").size(14))
                .padding(10),
        );
    }
    for (index, path) in entries.iter().enumerate() {
        let title = titles
            .get(index)
            .cloned()
            .unwrap_or_else(|| song_title(path));
        // The highlight tracks the song actually playing, which may live in
        // another tab than the one shown.
        let is_current = is_current_tab && current == Some(index);
        let is_selected = selected == Some(index);
        let text_widget = Text::new(title).size(14);
        let text_widget = if is_current {
            text_widget.color(iced::Color::from_rgb(0.45, 0.85, 0.45))
        } else {
            text_widget
        };
        let mut row =
            Button::new(text_widget)
                .width(Length::Fill)
                .style(move |theme: &Theme, status| {
                    let mut style = button::text(theme, status);
                    if is_current {
                        // Explicitly darken the row: the palette's weak/strong
                        // steps are brighter than base in the dark theme.
                        let base = theme.extended_palette().background.base.color;
                        style.background = Some(iced::Background::Color(iced::Color::from_rgba(
                            base.r * 0.55,
                            base.g * 0.55,
                            base.b * 0.55,
                            base.a,
                        )));
                    } else if is_selected {
                        style.background = Some(iced::Background::Color(
                            theme.extended_palette().background.strong.color,
                        ));
                    }
                    style
                });
        if engine_ok {
            row = row.on_press(Message::RowPressed(index));
        }
        list = list.push(row);
    }
    list
}

/// Daemon view entry: one view per open window (the player has at most one).
pub fn view_window(app: &PlayerApp, _window: window::Id) -> Element<'_, Message> {
    view(app)
}

pub fn view(app: &PlayerApp) -> Element<'_, Message> {
    let engine_ok = app.client.is_some();
    let playlist = app.playlists.active();
    let is_current_tab = app.playlists.active_tab == app.playlists.current_tab;
    let total_seconds = app.song_length_frames as f32 / SAMPLE_RATE_HZ as f32;
    let shown_seconds = app
        .seek_seconds
        .unwrap_or(app.position_frames as f32 / SAMPLE_RATE_HZ as f32);
    let position = row![
        maolan_widgets::slider::slider(
            0.0..=total_seconds.max(0.001),
            shown_seconds.clamp(0.0, total_seconds.max(0.001)),
            Message::PositionChanged,
        )
        .horizontal()
        .height(Length::Fixed(20.0))
        .width(Length::Fill)
        .step(0.1)
        .on_release(Message::PositionReleased),
        text(format!(
            "{} / {}",
            format_time(
                shown_seconds as usize * SAMPLE_RATE_HZ as usize,
                SAMPLE_RATE_HZ as u32
            ),
            format_time(app.song_length_frames, SAMPLE_RATE_HZ as u32)
        ))
        .size(14),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    let transport = row![
        button(file_plus().size(20)).on_press(Message::AddFilesPressed),
        button(folder_open().size(20)).on_press(Message::AddFolderPressed),
        button(skip_back().size(20)).on_press_maybe(
            (playlist.entries.len() > 1 && engine_ok).then_some(Message::PreviousPressed)
        ),
        // Combined play/pause toggle: shows the pause symbol while audio is
        // playing and the play symbol otherwise. Plays the selection, or —
        // when nothing is selected — the restored current song from the last
        // session.
        if app.playing && !app.paused {
            button(pause().size(20)).on_press_maybe(engine_ok.then_some(Message::PlayToggled))
        } else {
            button(play().size(20)).on_press_maybe(if app.selected.is_some() {
                engine_ok.then_some(Message::PlayToggled)
            } else {
                (app.playlists.current.is_some() && engine_ok).then_some(Message::PlayToggled)
            })
        },
        button(square().size(20))
            .on_press_maybe((app.playing || app.paused).then_some(Message::StopPressed)),
        button(skip_forward().size(20)).on_press_maybe(
            (playlist.entries.len() > 1 && engine_ok).then_some(Message::NextPressed)
        ),
        button(trash_two().size(20)).on_press_maybe(app.selected.map(|_| Message::RemovePressed)),
        {
            let loop_btn = button(repeat().size(20)).on_press(Message::LoopToggled);
            // While loop mode is active, highlight with the same background
            // the UI uses for selection; plain like the other buttons when
            // off.
            if app.loop_playlist {
                // On: same default style as the other transport buttons.
                loop_btn
            } else {
                // Off: default style rendered disabled, like the transport
                // buttons when nothing is selected or playing.
                loop_btn.style(|theme: &Theme, _status| {
                    button::primary(theme, button::Status::Disabled)
                })
            }
        },
        button(shuffle().size(20)).on_press(Message::ShufflePressed),
        button(list_ordered().size(20)).on_press(Message::SortPressed),
        position.width(Length::Fill),
        button(settings_icon().size(20)).on_press(Message::SettingsPressed),
    ]
    .spacing(6)
    .align_y(iced::Alignment::Center);

    // Vertical master volume alongside the playlist (vertical is the
    // widget's default orientation).
    let volume = column![
        maolan_widgets::slider::slider(VOLUME_RANGE, app.volume_db, Message::VolumeChanged)
            .on_release(Message::VolumeReleased)
            .width(Length::Fixed(24.0))
            .height(Length::Fill),
        // Fixed width so the dB label stays on one row, centered under the
        // fader regardless of the fader column's narrow width.
        text(format_volume(app.volume_db))
            .size(12)
            .width(Length::Shrink)
            .align_x(iced::alignment::Horizontal::Center),
    ]
    .spacing(6)
    .width(Length::Shrink)
    .align_x(iced::Alignment::Center);

    // The whole rows column sits behind ONE `lazy`, keyed on the playlist
    // revision plus the scalar row inputs, so the 25 fps VU meter redraws
    // don't rebuild and re-layout hundreds of rows. (Per-row `lazy` inside
    // this scrollable rendered nothing at runtime — hundreds of reconciled
    // lazy siblings are not trustworthy here; one lazy is the documented
    // pattern.)
    let rows_key = RowsKey {
        revision: app.playlist_revision,
        engine_ok,
        is_current_tab,
        selected: app.selected,
        current: app.playlists.current,
    };
    let entries = playlist.entries.clone();
    let titles = app.titles.clone();
    let app_selected = app.selected;
    let app_current = app.playlists.current;
    let list = iced::widget::lazy(rows_key, move |_key| {
        playlist_rows_column(
            &entries,
            &titles,
            app_selected,
            app_current,
            is_current_tab,
            engine_ok,
        )
    });

    // Tab bar: one tab per playlist, "+" opens a new one. Each tab is a
    // clickable label plus a small close button; double-clicking a tab
    // replaces its label with an inline rename input.
    let mut tabs = Row::new().spacing(4).align_y(iced::Alignment::Center);
    for (index, tab_playlist) in app.playlists.playlists.iter().enumerate() {
        let close = button(text("×").size(12))
            .on_press(Message::TabClosePressed(index))
            .style(button::text);
        if app.renaming_tab == Some(index) {
            let input = text_input("Tab name", &app.rename_draft)
                .id(app.rename_input_id.clone())
                .on_input(Message::TabRenameChanged)
                .on_submit(Message::TabRenameSubmitted)
                .size(13)
                .width(Length::Fixed(140.0));
            tabs = tabs.push(row![input, close].align_y(iced::Alignment::Center));
            continue;
        }
        let is_active = index == app.playlists.active_tab;
        let label = text(tab_playlist.name.clone()).size(13);
        let tab_button = if is_active {
            Button::new(label)
        } else {
            Button::new(label)
                .style(|theme: &Theme, _status| button::text(theme, button::Status::Disabled))
        }
        .on_press(Message::TabPressed(index));
        tabs = tabs.push(row![tab_button, close].align_y(iced::Alignment::Center));
    }
    tabs = tabs.push(
        button(text("+").size(13))
            .on_press(Message::TabAddPressed)
            .style(button::text),
    );
    let tab_bar = scrollable(tabs)
        .direction(scrollable::Direction::Horizontal(
            scrollable::Scrollbar::default(),
        ))
        .width(Length::Fill);

    let body = column![
        transport,
        tab_bar,
        container(scrollable(list).height(Length::Fill))
            .style(container::rounded_box)
            .height(Length::Fill),
        text(&app.status).size(12),
    ]
    .spacing(10);

    // Vertical VU strip showing the engine's master output levels, with a
    // numeric peak readout below.
    let vu = column![
        maolan_widgets::meters::meters(app.levels_db.len().max(1), &app.levels_db, 0.0),
        // Same fixed width as the fader label so the readouts line up.
        text(peak_readout(&app.levels_db))
            .size(12)
            .width(Length::Shrink)
            .align_x(iced::alignment::Horizontal::Center),
    ]
    .spacing(6)
    .width(Length::Shrink)
    .align_x(iced::Alignment::Center);

    // dB scale between the VU and the fader, matching the mixer's strip
    // layout (fader, ticks, meter) as far as the player's rail allows. The
    // ticks share the fader column's structure so the scale endpoints line
    // up with the slider's travel.
    let db_ticks = column![
        maolan_widgets::ticks::ticks(VOLUME_RANGE, 0.0),
        text("").size(12).width(Length::Shrink),
    ]
    .spacing(6)
    .width(Length::Shrink)
    .height(Length::Fill)
    .align_x(iced::Alignment::Center);

    // Fixed-width rail for the meter strip: VU + dB ticks + volume fader.
    // Rail for the meter strip: VU + dB ticks + volume fader. Shrinks to
    // its fixed-width strip columns.
    let rail = container(row![vu, db_ticks, volume].padding(10)).width(Length::Shrink);

    let main = container(row![body.width(Length::Fill), rail].spacing(10))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12);

    if app.show_settings
        && let Some(setup) = app.setup.clone()
    {
        let panel = container(
            column![
                text("Audio settings").size(18),
                audio_setup(setup, Message::SettingsAction),
                column![
                    text("Title format").size(14),
                    row![
                        text_input("{artist} - {song}", &app.title_format_edit)
                            .on_input(Message::TitleFormatChanged)
                            .on_submit(Message::TitleFormatSubmitted)
                            .width(Length::Fixed(240.0)),
                        text("{artist} {song} {album} {track} {date} {genre}").size(11),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                ]
                .spacing(6),
                button("Close").on_press(Message::SettingsClosed),
            ]
            .spacing(10),
        )
        .padding(16)
        .style(container::rounded_box)
        .width(Length::Fixed(420.0));

        let overlay = container(panel)
            .width(Length::Fill)
            .height(Length::Fill)
            .center_x(Length::Fill)
            .center_y(Length::Fill)
            .style(|_: &Theme| container::Style {
                background: Some(Color::from_rgba(0.0, 0.0, 0.0, 0.6).into()),
                ..container::Style::default()
            });

        return Stack::new()
            .push(main)
            .push(overlay)
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
    }

    main.into()
}

/// After a reorder of the active playlist, keep `current`/`selected`
/// pointing at the same songs by matching paths against the pre-reorder
/// order.
fn remap_indices(app: &mut PlayerApp, before: &[PathBuf]) {
    let entries = &app.playlists.active().entries;
    let remap = |old: Option<usize>| -> Option<usize> {
        let path = old.and_then(|i| before.get(i))?;
        entries.iter().position(|p| p == path)
    };
    let current = remap(app.playlists.current);
    let selected = remap(app.selected);
    app.playlists.current = current;
    app.selected = selected;
}

/// Commit the inline tab rename, if one is in progress: the trimmed draft
/// becomes the tab name (an empty draft keeps the old name; a name used by
/// another tab gets a numeric suffix — see [`resolve_tab_name`]), and the
/// `.pls` store is rewritten since section names changed.
fn commit_tab_rename(app: &mut PlayerApp) {
    let Some(index) = app.renaming_tab.take() else {
        return;
    };
    let draft = std::mem::take(&mut app.rename_draft);
    let Some(playlist) = app.playlists.playlists.get(index) else {
        return;
    };
    let name = resolve_tab_name(&app.playlists.playlists, index, &draft);
    if name != playlist.name {
        app.playlists.playlists[index].name = name;
        let _ = app.playlists.save_playlists();
    }
}

/// Open the main player window (used at startup and when restoring from
/// the tray).
fn open_main_window() -> Task<Message> {
    let (_id, task) = window::open(window::Settings {
        exit_on_close_request: false,
        ..Default::default()
    });
    task.map(Message::WindowOpened)
}

pub fn subscription(_app: &PlayerApp) -> Subscription<Message> {
    let tray_sub = Subscription::run(|| {
        stream::channel(16, async |mut output| {
            loop {
                while let Some(event) = crate::tray::try_recv_event() {
                    let msg = match event {
                        crate::tray::TrayEvent::ShowRequested => Message::TrayShowRequested,
                        crate::tray::TrayEvent::PlayToggleRequested => Message::PlayToggled,
                        crate::tray::TrayEvent::QuitRequested => Message::TrayQuitRequested,
                    };
                    let _ = output.send(msg).await;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    });
    // Ctrl+Q quits; Z/X/C/V/B drive transport; Escape cancels an in-progress
    // tab rename. `listen()` only yields events not consumed by focused
    // widgets, so these never shadow text input.
    let keyboard_sub = iced::keyboard::listen().map(|event| {
        use iced::keyboard::{Event, Key, key::Named};
        if let Event::KeyPressed {
            key: Key::Named(Named::Escape),
            ..
        } = event
        {
            return Message::TabRenameCancelled;
        }
        if let Event::KeyPressed {
            key: Key::Character(c),
            modifiers,
            ..
        } = event
        {
            if modifiers.control() && c.eq_ignore_ascii_case("q") {
                return Message::QuitRequested;
            }
            if !modifiers.control() {
                return match c.to_lowercase().as_str() {
                    "z" => Message::PreviousPressed,
                    "x" => Message::PlayPressed,
                    "c" => Message::PausePressed,
                    "v" => Message::StopPressed,
                    "b" => Message::NextPressed,
                    _ => Message::Ignored,
                };
            }
        }
        Message::Ignored
    });
    Subscription::batch([
        keyboard_sub,
        time::every(Duration::from_millis(250)).map(|_| Message::Tick),
        time::every(METER_POLL_INTERVAL).map(|_| Message::MeterTick),
        iced::window::open_events().map(Message::WindowOpened),
        iced::window::close_requests().map(Message::WindowCloseRequested),
        tray_sub,
    ])
}

async fn reopen_engine(
    device: String,
    period_frames: usize,
    n_periods: usize,
    ring_buffer_multiplier: usize,
) -> Result<EngineClient, String> {
    // Give the previous engine a moment to exit and release the audio device
    // after its command channel closed (see `Message::SettingsAction::Start`).
    tokio::time::sleep(Duration::from_millis(300)).await;
    open_engine(device, period_frames, n_periods, ring_buffer_multiplier).await
}

pub async fn open_engine(
    device: String,
    period_frames: usize,
    n_periods: usize,
    ring_buffer_multiplier: usize,
) -> Result<EngineClient, String> {
    let client = EngineClient::default();
    let mut rx = client.subscribe().await;
    send_engine(&client, EngineAction::Stop).await?;
    send_engine(
        &client,
        EngineAction::OpenAudioDevice {
            device: device.clone(),
            input_device: None,
            sample_rate_hz: SAMPLE_RATE_HZ,
            bits: 32,
            exclusive: true,
            period_frames,
            nperiods: n_periods,
            sync_mode: false,
            actual_period_frames: 0,
            input_channels: 0,
            output_channels: 0,
            bytes_per_frame: 0,
            ring_buffer_multiplier,
            auto_open_midi_devices: false,
        },
    )
    .await?;
    wait_for_engine_response(&mut rx, |action| {
        matches!(action, EngineAction::OpenAudioDevice { .. })
    })
    .await?;
    send_engine(
        &client,
        EngineAction::AddTrack {
            name: TRACK.to_string(),
            // Clip (disk) audio is mixed into the track's input lanes, so a
            // playback track needs one input port per output channel even
            // though no live input is ever connected.
            audio_ins: 2,
            midi_ins: 0,
            audio_outs: 2,
            midi_outs: 0,
            folder: false,
            mixosc_addr: None,
        },
    )
    .await?;
    wait_for_engine_response(
        &mut rx,
        |action| matches!(action, EngineAction::AddTrack { name, .. } if name == TRACK),
    )
    .await?;
    send_engine(&client, EngineAction::SetClipPlaybackEnabled(true)).await?;
    wait_for_engine_response(&mut rx, |action| {
        matches!(action, EngineAction::SetClipPlaybackEnabled(true))
    })
    .await?;
    Ok(client)
}

pub async fn play_song(client: EngineClient, path: PathBuf) -> Result<LoadedSong, String> {
    let mut rx = client.subscribe().await;
    // Fast path: metadata probe for length/channels; the engine's streaming
    // clip decodes the actual audio incrementally. Fall back to a full decode
    // only if probing fails.
    let (length_frames, channels) = match maolan_engine::audio_codec::probe_audio_file(&path) {
        Ok(info) => {
            let source_frames = info.frames.unwrap_or(0);
            let length = if source_frames > 0 {
                (source_frames * SAMPLE_RATE_HZ as u64 / info.sample_rate.max(1) as u64) as usize
            } else {
                0
            };
            (length, info.channels.max(1))
        }
        Err(_) => {
            let (samples, channels, _) = decode_audio_to_f32_interleaved_sync(&path)
                .map_err(|err| format!("Failed to decode '{}': {err}", path.display()))?;
            let channels = channels.max(1);
            let length_frames = samples.len() / channels;
            (length_frames, channels)
        }
    };

    send_engine(&client, EngineAction::Stop).await?;
    let _ = send_engine(
        &client,
        EngineAction::RemoveClip {
            track_name: TRACK.to_string(),
            kind: Kind::Audio,
            clip_indices: vec![0],
        },
    )
    .await;

    send_engine(
        &client,
        EngineAction::AddClip {
            clip_id: generate_clip_id(),
            name: path.to_string_lossy().to_string(),
            track_name: TRACK.to_string(),
            start: 0,
            length: length_frames,
            offset: 0,
            input_channel: 0,
            muted: false,
            reversed: false,
            gain_db: 0.0,
            peaks_file: None,
            kind: Kind::Audio,
            fade_enabled: true,
            fade_in_samples: 240,
            fade_out_samples: 240,
            source_name: None,
            source_offset: None,
            source_length: None,
            preview_name: None,
            pitch_correction_points: Vec::new(),
            pitch_correction_frame_likeness: None,
            pitch_correction_inertia_ms: None,
            pitch_correction_formant_compensation: None,
            plugin_graph_json: None,
        },
    )
    .await?;
    wait_for_engine_response(
        &mut rx,
        |action| matches!(action, EngineAction::AddClip { track_name, .. } if track_name == TRACK),
    )
    .await?;

    for channel in 0..channels.min(2) {
        send_engine(
            &client,
            EngineAction::Connect {
                from_track: TRACK.to_string(),
                from_port: channel,
                to_track: "hw:out".to_string(),
                to_port: channel,
                kind: Kind::Audio,
            },
        )
        .await?;
        wait_for_engine_response(&mut rx, |action| {
            matches!(action, EngineAction::Connect {
                from_track,
                from_port,
                kind,
                ..
            } if from_track == TRACK && *from_port == channel && *kind == Kind::Audio)
        })
        .await?;
    }

    send_engine(&client, EngineAction::TransportPosition(0)).await?;
    send_engine(&client, EngineAction::Play).await?;
    wait_for_engine_response(&mut rx, |action| matches!(action, EngineAction::Play)).await?;

    Ok(LoadedSong { length_frames })
}

/// Pause the current song by stopping the engine, remembering the transport
/// position so playback can resume from it.
///
/// The engine's `Pause` only mutes clips without restoring them on `Play`,
/// so the player pauses via `Stop` (which restores clip playback) and keeps
/// the resume frame itself.
fn pause_as_stop(app: &mut PlayerApp) -> Task<Message> {
    let frames = app
        .client
        .as_ref()
        .and_then(|client| client.transport_snapshot())
        .map(|snapshot| snapshot.sample)
        .unwrap_or(app.position_frames);
    app.paused = true;
    app.paused_position = Some(frames);
    app.position_frames = frames;
    if let Some(client) = app.client.clone() {
        return Task::perform(send_action(client, EngineAction::Stop), |_| Message::Tick);
    }
    Task::none()
}

/// Resume the paused song from the remembered position: seek the engine
/// there, re-enable clip playback (mirroring `open_engine`), and play.
fn resume_playback(app: &mut PlayerApp) -> Task<Message> {
    let Some(client) = app.client.clone() else {
        return Task::none();
    };
    let Some(frames) = app.paused_position.take() else {
        return Task::none();
    };
    app.paused = false;
    app.playing = true;
    app.position_frames = frames;
    Task::perform(resume_song(client, frames), |result| match result {
        Ok(()) => Message::Tick,
        Err(err) => Message::SongLoaded(Err(err)),
    })
}

async fn send_action(client: EngineClient, action: EngineAction) -> Result<(), String> {
    send_engine(&client, action).await
}

/// Seek to `frames`, re-enable clip playback, and start the transport.
async fn resume_song(client: EngineClient, frames: usize) -> Result<(), String> {
    send_engine(&client, EngineAction::TransportPosition(frames)).await?;
    send_engine(&client, EngineAction::SetClipPlaybackEnabled(true)).await?;
    send_engine(&client, EngineAction::Play).await
}

async fn send_engine(client: &EngineClient, action: EngineAction) -> Result<(), String> {
    client.send(EngineMessage::Request(action)).await
}

async fn wait_for_engine_response(
    rx: &mut tokio::sync::mpsc::Receiver<EngineMessage>,
    mut accepts: impl FnMut(&EngineAction) -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(String::from("Timed out waiting for audio engine."));
        }
        let Some(message) = tokio::time::timeout(remaining, rx.recv())
            .await
            .map_err(|_| String::from("Timed out waiting for audio engine."))?
        else {
            return Err(String::from("Audio engine response channel closed."));
        };
        if let EngineMessage::Response(result) = message {
            match result {
                Ok(action) if accepts(&action) => return Ok(()),
                Ok(_) => {}
                Err(err) => return Err(err),
            }
        }
    }
}

fn format_time(frames: usize, sample_rate: u32) -> String {
    let seconds = frames as f64 / sample_rate.max(1) as f64;
    let minutes = (seconds / 60.0) as u64;
    let seconds = (seconds as u64) % 60;
    format!("{minutes}:{seconds:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_padded_between_minus_10_and_10() {
        assert_eq!(format_db_padded(-5.3), "-05.3 dB");
        assert_eq!(format_db_padded(3.04), "+03.0 dB");
        assert_eq!(format_db_padded(0.0), "+00.0 dB");
    }

    #[test]
    fn db_not_padded_outside_range() {
        assert_eq!(format_db_padded(-12.4), "-12.4 dB");
        assert_eq!(format_db_padded(11.0), "+11.0 dB");
    }

    #[test]
    fn peak_readout_floor_and_value() {
        assert_eq!(peak_readout(&[-90.0, -90.0]), "-inf dB");
        assert_eq!(peak_readout(&[-90.0, -4.9]), "-04.9 dB");
    }

    /// Mirror of the playlist list in `view()`: the rows column behind one
    /// `lazy`, keyed on the same `RowsKey` fields.
    fn test_list(titles: &[&str], revision: u64) -> Element<'static, Message> {
        let key = RowsKey {
            revision,
            engine_ok: true,
            is_current_tab: true,
            selected: None,
            current: None,
        };
        let entries: Vec<PathBuf> = titles
            .iter()
            .enumerate()
            .map(|(i, _)| PathBuf::from(format!("/music/song{i}.flac")))
            .collect();
        let titles: Vec<String> = titles.iter().map(|title| (*title).to_string()).collect();
        iced::widget::lazy(key, move |_key| {
            playlist_rows_column(&entries, &titles, None, None, true, true)
        })
        .into()
    }

    #[test]
    fn appending_rows_reconciles_tree() {
        use maolan_widgets::iced::advanced::widget::Tree;
        let initial = test_list(&["a", "b"], 0);
        let mut tree = Tree::new(initial.as_widget());
        // Simulate FilesAdded: revision bumps, the column gains a row.
        let updated = test_list(&["a", "b", "c"], 1);
        tree.diff(updated.as_widget());
        // lazy > column: the column must expose the new rows.
        let column = &tree.children[0];
        assert_eq!(column.children.len(), 3);
    }

    #[test]
    fn unchanged_revision_keeps_cached_rows() {
        use maolan_widgets::iced::advanced::widget::Tree;
        let initial = test_list(&["a", "b"], 7);
        let mut tree = Tree::new(initial.as_widget());
        // A meter tick: the view is rebuilt with the SAME revision; the lazy
        // must not drop or duplicate its cached content tree.
        for _ in 0..10 {
            let tick = test_list(&["a", "b"], 7);
            tree.diff(tick.as_widget());
            let column = &tree.children[0];
            assert_eq!(column.children.len(), 2);
        }
    }

    #[test]
    fn removing_rows_reconciles_tree() {
        use maolan_widgets::iced::advanced::widget::Tree;
        let initial = test_list(&["a", "b", "c"], 0);
        let mut tree = Tree::new(initial.as_widget());
        let updated = test_list(&["a"], 1);
        tree.diff(updated.as_widget());
        let column = &tree.children[0].children[0];
        assert_eq!(column.children.len(), 1);
    }

    /// A `PlayerApp` with one empty playlist tab, built without the engine,
    /// tray, or config-file side effects of `app::new`.
    fn test_app() -> PlayerApp {
        PlayerApp {
            client: None,
            status: String::new(),
            playlists: Playlists {
                playlists: vec![Playlist::default()],
                active_tab: 0,
                current_tab: 0,
                current: None,
            },
            titles: Vec::new(),
            selected: None,
            playing: false,
            paused: false,
            paused_position: None,
            song_length_frames: 0,
            position_frames: 0,
            seek_seconds: None,
            last_row_press: None,
            last_tab_press: None,
            renaming_tab: None,
            rename_draft: String::new(),
            rename_input_id: iced::widget::Id::unique(),
            volume_db: 0.0,
            levels_db: vec![-90.0, -90.0],
            loop_playlist: false,
            title_format_edit: String::new(),
            playlist_revision: 0,
            main_window_id: None,
            settings: Settings::default(),
            device_override: None,
            show_settings: false,
            setup: None,
            tray_song: None,
            tray_pushed_secs: None,
        }
    }

    /// Descend into the playlist rows column of the real `view()` tree. The
    /// container widget is transparent in iced's widget tree (its `children()`
    /// delegates to its content), so the path is: main container > body
    /// column > [transport, tab bar, scrollable, status] > lazy > rows Column.
    fn playlist_rows_tree(
        tree: &maolan_widgets::iced::advanced::widget::Tree,
    ) -> &maolan_widgets::iced::advanced::widget::Tree {
        &tree.children[0].children[2].children[0].children[0]
    }

    #[test]
    fn files_added_rows_reconcile_in_real_view() {
        use maolan_widgets::iced::advanced::widget::Tree;
        let mut app = test_app();
        let mut tree = {
            let initial = view(&app);
            let tree = Tree::new(initial.as_widget());
            assert_eq!(playlist_rows_tree(&tree).children.len(), 1);
            tree
        };
        // Same steps as `Message::FilesAdded`, minus playlist-file IO.
        let files = vec![
            PathBuf::from("/nonexistent/a.flac"),
            PathBuf::from("/nonexistent/b.flac"),
        ];
        let first = app.playlists.active_mut().add(files);
        app.selected = Some(first);
        app.refresh_titles();
        let updated = view(&app);
        tree.diff(updated.as_widget());
        // The empty-state hint is replaced by one row per entry.
        assert_eq!(playlist_rows_tree(&tree).children.len(), 2);
    }

    /// The meter tick redraws the view at 25 fps; simulate a batch of
    /// unchanged redraws before the mutation, then the FilesAdded redraw.
    #[test]
    fn files_added_after_unchanged_redraws_reconciles() {
        use maolan_widgets::iced::advanced::widget::Tree;
        let mut app = test_app();
        app.playlists
            .active_mut()
            .add(vec![PathBuf::from("/nonexistent/a.flac")]);
        app.refresh_titles();
        let mut tree = {
            let initial = view(&app);
            Tree::new(initial.as_widget())
        };
        for _ in 0..10 {
            let tick = view(&app);
            tree.diff(tick.as_widget());
            assert_eq!(playlist_rows_tree(&tree).children.len(), 1);
        }
        app.playlists
            .active_mut()
            .add(vec![PathBuf::from("/nonexistent/b.flac")]);
        app.refresh_titles();
        let updated = view(&app);
        tree.diff(updated.as_widget());
        assert_eq!(playlist_rows_tree(&tree).children.len(), 2);
    }

    /// Selection and current-song changes bump the revision, so the lazy
    /// rows column rebuilds even though the entries are unchanged.
    #[test]
    fn selection_change_rebuilds_rows() {
        use maolan_widgets::iced::advanced::widget::Tree;
        let mut app = test_app();
        app.playlists
            .active_mut()
            .add(vec![PathBuf::from("/nonexistent/a.flac")]);
        app.refresh_titles();
        let mut tree = {
            let initial = view(&app);
            Tree::new(initial.as_widget())
        };
        let revision_before = app.playlist_revision;
        let _ = update(&mut app, Message::Select(0));
        assert!(app.playlist_revision > revision_before);
        let updated = view(&app);
        tree.diff(updated.as_widget());
        // One entry: the hint is gone, one row remains.
        assert_eq!(playlist_rows_tree(&tree).children.len(), 1);
    }

    #[test]
    fn added_entries_get_titles() {
        let mut app = test_app();
        app.playlists
            .active_mut()
            .add(vec![PathBuf::from("/nonexistent/a.flac")]);
        app.refresh_titles();
        assert_eq!(app.titles.len(), app.playlists.active().entries.len());
        assert_eq!(app.titles[0], "a");
    }
}
