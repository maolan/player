//! Audio settings: backend/device selection and persistence.
//!
//! Mirrors the editor's startup audio setup (`editor/src/app.rs`): the
//! backend is a UI grouping that maps to the engine device id string, which
//! is what `Action::OpenAudioDevice` actually takes — `"jack"` selects the
//! JACK runtime (`engine/src/engine/hardware.rs:203`), everything else is
//! handed to the platform `HwDriver`.

use std::fmt;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::buffer::default_period_frames;

const APP_DIR: &str = "maolan";
const PLAYER_DIR: &str = "player";
const SETTINGS_FILE: &str = "config.toml";
pub const DEFAULT_BITS: i32 = 32;
pub const DEFAULT_N_PERIODS: usize = 2;

/// Audio backend, mirroring the editor's `AudioEngineOption`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Backend {
    #[cfg(target_os = "freebsd")]
    #[default]
    Oss,
    #[cfg(target_os = "linux")]
    Alsa,
    #[cfg(target_os = "windows")]
    Wasapi,
    #[cfg(target_os = "macos")]
    CoreAudio,
    #[cfg(unix)]
    Jack,
}

impl Backend {
    pub const ALL: &'static [Self] = &[
        #[cfg(target_os = "freebsd")]
        Self::Oss,
        #[cfg(target_os = "linux")]
        Self::Alsa,
        #[cfg(target_os = "windows")]
        Self::Wasapi,
        #[cfg(target_os = "macos")]
        Self::CoreAudio,
        #[cfg(unix)]
        Self::Jack,
    ];

    #[cfg(unix)]
    pub fn is_jack(self) -> bool {
        matches!(self, Self::Jack)
    }

    #[cfg(not(unix))]
    pub fn is_jack(self) -> bool {
        false
    }

    /// Device id used when the user picks this backend without choosing a
    /// specific discovered device. On FreeBSD this is the OS default pcm
    /// unit from the `hw.snd.default_unit` sysctl (e.g. "/dev/dsp5"),
    /// falling back to "/dev/dsp".
    pub fn default_device_id(self) -> String {
        match self {
            #[cfg(target_os = "freebsd")]
            Self::Oss => default_oss_device_id(),
            #[cfg(target_os = "linux")]
            Self::Alsa => String::from("default"),
            #[cfg(target_os = "windows")]
            Self::Wasapi => String::from("default"),
            #[cfg(target_os = "macos")]
            Self::CoreAudio => String::from("default"),
            #[cfg(unix)]
            Self::Jack => String::from("jack"),
        }
    }
}

/// Pure: OSS pcm unit number → device node path.
pub fn format_dsp_device(unit: u32) -> String {
    format!("/dev/dsp{unit}")
}

/// FreeBSD default OSS device: unit from `hw.snd.default_unit`, "/dev/dsp"
/// when the sysctl cannot be read or parsed.
#[cfg(target_os = "freebsd")]
pub fn default_oss_device_id() -> String {
    resolve_default_oss_device(default_sound_unit)
}

#[cfg(target_os = "freebsd")]
fn resolve_default_oss_device(read_unit: impl Fn() -> Option<u32>) -> String {
    read_unit()
        .map(format_dsp_device)
        .unwrap_or_else(|| String::from("/dev/dsp"))
}

/// Read `hw.snd.default_unit` via sysctlbyname(3).
#[cfg(target_os = "freebsd")]
fn default_sound_unit() -> Option<u32> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&value);
    let name = b"hw.snd.default_unit\0";
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr().cast::<libc::c_char>(),
            (&mut value as *mut libc::c_int).cast::<libc::c_void>(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0 && value >= 0).then_some(value as u32)
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            #[cfg(target_os = "freebsd")]
            Self::Oss => write!(f, "OSS"),
            #[cfg(target_os = "linux")]
            Self::Alsa => write!(f, "ALSA"),
            #[cfg(target_os = "windows")]
            Self::Wasapi => write!(f, "WASAPI"),
            #[cfg(target_os = "macos")]
            Self::CoreAudio => write!(f, "CoreAudio"),
            #[cfg(unix)]
            Self::Jack => write!(f, "JACK"),
        }
    }
}

/// Backend for a device id: the engine dispatches on the literal `"jack"`;
/// anything else uses the platform's default backend.
pub fn backend_for_device(device_id: &str) -> Backend {
    #[cfg(unix)]
    if device_id.eq_ignore_ascii_case("jack") {
        return Backend::Jack;
    }
    Backend::default()
}

/// A selectable output device.
#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub label: String,
    /// FreeBSD OSS kernel buffer size; 0 when unknown (ladder fallback).
    pub max_buffer_bytes: usize,
    pub max_channels: usize,
}

impl PartialEq for Device {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Device {}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label)
    }
}

/// Discover the output devices for a backend, editor-style: JACK offers a
/// single pseudo-device; FreeBSD uses the engine's OSS discovery; Linux
/// parses `/proc/asound/pcm`; other platforms offer the engine default.
pub fn discover_devices(backend: Backend) -> Vec<Device> {
    if backend.is_jack() {
        return vec![Device {
            id: String::from("jack"),
            label: String::from("JACK"),
            max_buffer_bytes: 0,
            max_channels: 0,
        }];
    }
    #[cfg(target_os = "freebsd")]
    {
        let mut devices: Vec<Device> =
            maolan_engine::audio_devices::discover_freebsd_audio_devices()
                .into_iter()
                .filter(|descriptor| descriptor.supports_output)
                .map(|descriptor| Device {
                    id: descriptor.id,
                    label: descriptor.label,
                    max_buffer_bytes: descriptor.max_buffer_bytes,
                    max_channels: descriptor.max_channels,
                })
                .collect();
        devices.sort_by_key(|device| device.label.to_lowercase());
        devices.dedup_by(|a, b| a.id == b.id);
        if !devices.is_empty() {
            return devices;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut devices = parse_alsa_playback_devices(&alsa_pcm_listing());
        devices.sort_by_key(|device| device.label.to_lowercase());
        devices.dedup_by(|a, b| a.id == b.id);
        if !devices.is_empty() {
            return devices;
        }
    }
    let id = backend.default_device_id();
    vec![Device {
        label: id.clone(),
        id,
        max_buffer_bytes: 0,
        max_channels: 0,
    }]
}

#[cfg(target_os = "linux")]
fn alsa_pcm_listing() -> String {
    std::fs::read_to_string("/proc/asound/pcm").unwrap_or_default()
}

/// Parse playback entries of `/proc/asound/pcm` (`card-dev: ... : playback`),
/// mirroring the editor's Linux discovery (`editor/src/app.rs` `platform_linux`).
#[cfg(target_os = "linux")]
fn parse_alsa_playback_devices(contents: &str) -> Vec<Device> {
    let mut devices = Vec::new();
    for line in contents.lines() {
        let Some((card_dev, rest)) = line.split_once(':') else {
            continue;
        };
        if !rest.contains("playback") {
            continue;
        }
        let mut parts = card_dev.trim().split('-');
        let (Some(card), Some(dev)) = (parts.next(), parts.next()) else {
            continue;
        };
        if card.parse::<u32>().is_err() || dev.parse::<u32>().is_err() {
            continue;
        }
        let device_name = rest.split(':').next().unwrap_or("").trim();
        let label = if device_name.is_empty() {
            format!("hw:{card},{dev}")
        } else {
            format!("{device_name} (hw:{card},{dev})")
        };
        devices.push(Device {
            id: format!("hw:{card},{dev}"),
            label,
            max_buffer_bytes: 0,
            max_channels: 0,
        });
    }
    devices
}

/// Persisted audio settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub device_id: String,
    pub period_frames: usize,
    pub n_periods: usize,
    /// Streaming clip ring capacity multiplier (periods per channel ring);
    /// 0 means the engine default (8), otherwise clamped to 2..=32.
    #[serde(default = "default_ring_buffer_multiplier")]
    pub ring_buffer_multiplier: usize,
    /// Master output volume in dB (0.0 = unity), applied to the engine's
    /// "hw:out" level.
    #[serde(default)]
    pub volume_db: f32,
    /// Format for displayed song titles. Placeholders: {artist}, {song}
    /// (or {title}), {album}, {track}, {date}, {genre}. Missing fields
    /// render as empty; if the result is empty the file name is used.
    #[serde(default = "default_title_format")]
    pub title_format: String,
    /// When true, reaching the end of the last song wraps back to the first.
    #[serde(default)]
    pub loop_playlist: bool,
}

pub fn default_ring_buffer_multiplier() -> usize {
    8
}

pub fn default_title_format() -> String {
    String::from("{artist} - {song}")
}

impl Default for Settings {
    fn default() -> Self {
        let backend = Backend::default();
        let device_id = backend.default_device_id();
        Self {
            period_frames: default_period_frames(&device_id, DEFAULT_BITS),
            device_id,
            n_periods: DEFAULT_N_PERIODS,
            ring_buffer_multiplier: default_ring_buffer_multiplier(),
            loop_playlist: false,
            volume_db: 0.0,
            title_format: default_title_format(),
        }
    }
}

impl Settings {
    fn store_path_at(home: Option<PathBuf>) -> Option<PathBuf> {
        home.map(|home| {
            home.join(".config")
                .join(APP_DIR)
                .join(PLAYER_DIR)
                .join(SETTINGS_FILE)
        })
    }

    fn load_from(home: Option<PathBuf>) -> Self {
        Self::store_path_at(home)
            .and_then(|path| fs::read_to_string(path).ok())
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn load() -> Self {
        Self::load_from(std::env::var_os("HOME").map(PathBuf::from))
    }

    fn save_to(&self, home: Option<PathBuf>) -> Result<(), String> {
        let path = Self::store_path_at(home).ok_or_else(|| String::from("No home directory."))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let text = toml::to_string_pretty(self).map_err(|err| err.to_string())?;
        fs::write(path, text).map_err(|err| err.to_string())
    }

    pub fn save(&self) -> Result<(), String> {
        self.save_to(std::env::var_os("HOME").map(PathBuf::from))
    }

    /// Settings with a CLI/env device override applied (override wins over
    /// the persisted device; buffer sizes are kept).
    pub fn with_device_override(&self, device_id: &str) -> Self {
        let mut settings = self.clone();
        settings.device_id = String::from(device_id);
        settings
    }

    /// The engine `OpenAudioDevice` request for these settings.
    pub fn open_action(&self) -> (String, usize, usize) {
        (self.device_id.clone(), self.period_frames, self.n_periods)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn config_dir(home: Option<PathBuf>) -> Option<PathBuf> {
        home.map(|home| home.join(".config").join(APP_DIR).join(PLAYER_DIR))
    }

    fn temp_dir() -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("maolan-player-settings-test-{unique}"))
    }

    #[test]
    fn save_load_round_trip() {
        let dir = temp_dir();
        let settings = Settings {
            device_id: String::from("/dev/dsp0"),
            period_frames: 4096,
            n_periods: 2,
            ring_buffer_multiplier: 8,
            volume_db: -6.0,
            title_format: default_title_format(),
            loop_playlist: false,
        };
        settings.save_to(Some(dir.clone())).expect("save settings");
        let file = config_dir(Some(dir.clone())).unwrap().join(SETTINGS_FILE);
        assert!(file.exists());
        let text = fs::read_to_string(&file).unwrap();
        assert!(text.contains("device_id = \"/dev/dsp0\""));
        assert_eq!(Settings::load_from(Some(dir.clone())), settings);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn load_returns_defaults_for_missing_or_invalid_file() {
        let dir = temp_dir();
        assert_eq!(Settings::load_from(Some(dir.clone())), Settings::default());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(SETTINGS_FILE), "not json").unwrap();
        assert_eq!(Settings::load_from(Some(dir.clone())), Settings::default());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn device_override_replaces_only_device() {
        let settings = Settings {
            device_id: String::from("/dev/dsp0"),
            period_frames: 2048,
            n_periods: 2,
            ring_buffer_multiplier: 8,
            volume_db: 0.0,
            title_format: default_title_format(),
            loop_playlist: false,
        };
        let overridden = settings.with_device_override("jack");
        assert_eq!(overridden.device_id, "jack");
        assert_eq!(overridden.period_frames, 2048);
        assert_eq!(overridden.n_periods, 2);
    }

    #[test]
    fn dsp_unit_formats_device_node() {
        assert_eq!(format_dsp_device(0), "/dev/dsp0");
        assert_eq!(format_dsp_device(5), "/dev/dsp5");
    }

    #[cfg(target_os = "freebsd")]
    #[test]
    fn default_oss_device_uses_sysctl_unit_with_fallback() {
        assert_eq!(resolve_default_oss_device(|| Some(5)), "/dev/dsp5");
        assert_eq!(resolve_default_oss_device(|| None), "/dev/dsp");
        // Live sysctl (informational): matches the sndstat default pcm.
        let resolved = default_oss_device_id();
        assert!(resolved.starts_with("/dev/dsp"), "unexpected {resolved}");
    }

    #[test]
    fn backend_mapping_round_trips() {
        assert_eq!(backend_for_device("jack"), Backend::Jack);
        assert_eq!(backend_for_device("JACK"), Backend::Jack);
        assert_eq!(
            backend_for_device("/dev/dsp0"),
            Backend::default(),
            "non-jack ids use the platform default backend"
        );
        for backend in Backend::ALL {
            let id = backend.default_device_id();
            assert!(!id.is_empty());
            if backend.is_jack() {
                assert_eq!(backend_for_device(&id), Backend::Jack);
            }
        }
    }

    #[test]
    fn jack_device_list_is_single_pseudo_device() {
        #[cfg(unix)]
        {
            let devices = discover_devices(Backend::Jack);
            assert_eq!(devices.len(), 1);
            assert_eq!(devices[0].id, "jack");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_alsa_playback_entries() {
        let contents = "00-00: ALC892 Analog : ALC892 Analog : playback 1 : capture 1\n\
                        00-03: HDMI 0 : HDMI 0 : playback 1\n\
                        bad-line\n";
        let devices = parse_alsa_playback_devices(contents);
        assert_eq!(devices.len(), 2);
        assert_eq!(devices[0].id, "hw:00,00");
        assert!(devices.iter().all(|d| d.id.starts_with("hw:")));
        assert!(devices.iter().any(|d| d.label.contains("HDMI")));
    }
}
