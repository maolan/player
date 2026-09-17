//! Buffer-size defaults for the audio device request.
//!
//! Backend behavior in maolan-engine (see `engine/src/hw/`):
//! - OSS (FreeBSD): `period_frames` is the in-process read/write chunk
//!   (`engine/src/hw/oss/mod.rs:258`); the kernel buffer is negotiated
//!   separately. Device discovery exposes `max_buffer_bytes`
//!   (`engine/src/hw/freebsd.rs:45`), and the editor converts it to period
//!   options capped at 64 KiB fragments (`editor/src/app.rs:4181`).
//! - ALSA: `set_period_size_near` / `set_buffer_size_near` use
//!   `ValueOr::Nearest` (`engine/src/hw/alsa.rs:901-906`), so an oversized
//!   request is clamped to the device bounds and the effective value is
//!   echoed in the engine response. Requesting the editor's largest
//!   conventional option is safe.
//! - WASAPI (exclusive mode) and CoreAudio have no pre-open query API in the
//!   engine and hard-error on oversized requests: WASAPI passes
//!   `period_frames` straight to `IAudioClient::Initialize`
//!   (`engine/src/hw/wasapi.rs:837-848`), and CoreAudio errors in
//!   `set_maximum_frames_per_slice` (`engine/src/hw/coreaudio.rs:1155`).
//!   Keep the editor default there.
//! - JACK: the server decides the cycle size; the request is only echoed.

const FALLBACK_PERIOD_FRAMES: usize = 1024;
const MAX_OSS_FRAGMENT_BYTES: usize = 1 << 16;

/// Largest OSS period (in frames) implied by a device's maximum kernel
/// buffer, following the editor's `oss_period_frame_options` conversion
/// (`editor/src/app.rs:4181`): fragment sizes grow in powers of two from one
/// frame up to `max_buffer_bytes`, capped at 64 KiB.
/// Editor-style default period ladder (`editor/src/app.rs:4174`), used for
/// every backend except OSS devices that advertise a maximum buffer size.
pub const DEFAULT_PERIOD_LADDER: [usize; 13] = [
    16, 32, 64, 128, 256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536,
];

/// Editor-style OSS period ladder (`editor/src/app.rs:4181`): fragment sizes
/// grow in powers of two from one frame up to `max_buffer_bytes`, capped at
/// 64 KiB; each entry is the period in frames for that fragment size.
pub fn oss_period_options(
    max_buffer_bytes: usize,
    channels: usize,
    bits: usize,
) -> Option<Vec<usize>> {
    if max_buffer_bytes == 0 || channels == 0 {
        return None;
    }
    let bytes_per_sample = match bits {
        8 => 1,
        16 => 2,
        24 => 3,
        32 => 4,
        _ => return None,
    };
    let frame_bytes = channels.checked_mul(bytes_per_sample)?.max(1);
    let min_bytes = frame_bytes.next_power_of_two();
    let max_bytes = max_buffer_bytes.min(MAX_OSS_FRAGMENT_BYTES).max(min_bytes);
    let mut options = Vec::new();
    let mut bytes = min_bytes;
    while bytes <= max_bytes {
        options.push(bytes / frame_bytes);
        bytes = bytes.checked_mul(2)?;
    }
    (!options.is_empty()).then_some(options)
}

/// Period options for a device: the OSS ladder when the device advertises a
/// maximum buffer size (FreeBSD), otherwise the default power-of-two ladder.
pub fn period_options(max_buffer_bytes: usize, channels: usize, bits: usize) -> Vec<usize> {
    if max_buffer_bytes > 0
        && channels > 0
        && let Some(options) = oss_period_options(max_buffer_bytes, channels, bits)
    {
        return options;
    }
    DEFAULT_PERIOD_LADDER.to_vec()
}

pub fn max_oss_period_frames(
    max_buffer_bytes: usize,
    channels: usize,
    bits: usize,
) -> Option<usize> {
    oss_period_options(max_buffer_bytes, channels, bits)?
        .into_iter()
        .last()
}

/// Default output period for the selected device and sample format.
#[cfg(target_os = "freebsd")]
pub fn default_period_frames(device: &str, bits: i32) -> usize {
    maolan_engine::audio_devices::discover_freebsd_audio_devices()
        .into_iter()
        .find(|descriptor| descriptor.id == device)
        .and_then(|descriptor| {
            max_oss_period_frames(
                descriptor.max_buffer_bytes,
                descriptor.max_channels.max(1),
                bits as usize,
            )
        })
        .unwrap_or(FALLBACK_PERIOD_FRAMES)
}

/// ALSA clamps oversized periods to the device bounds
/// (`set_period_size_near` with `Nearest`), so request the editor's largest
/// conventional option and let the driver pick the effective value.
#[cfg(target_os = "linux")]
pub fn default_period_frames(_device: &str, _bits: i32) -> usize {
    65_536
}

/// WASAPI exclusive mode and CoreAudio fail the open when the requested
/// period exceeds device limits and neither exposes a pre-open query, so
/// keep the conservative editor default.
#[cfg(any(target_os = "windows", target_os = "macos"))]
pub fn default_period_frames(_device: &str, _bits: i32) -> usize {
    FALLBACK_PERIOD_FRAMES
}

/// JACK (and other unix fallbacks): the server decides the cycle size.
#[cfg(all(
    unix,
    not(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))
))]
pub fn default_period_frames(_device: &str, _bits: i32) -> usize {
    FALLBACK_PERIOD_FRAMES
}

#[cfg(not(unix))]
pub fn default_period_frames(_device: &str, _bits: i32) -> usize {
    FALLBACK_PERIOD_FRAMES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oss_options_grow_by_powers_of_two_up_to_device_max() {
        assert_eq!(oss_period_options(32, 2, 32), Some(vec![1, 2, 4]));
        assert_eq!(max_oss_period_frames(64, 2, 32), Some(8));
    }

    #[test]
    fn oss_options_are_capped_at_64k_fragment() {
        // 2 ch * 2 bytes = 4 bytes/frame; cap 65536 bytes -> 16384 frames.
        assert_eq!(max_oss_period_frames(1 << 20, 2, 16), Some(16_384));
    }

    #[test]
    fn period_options_fall_back_to_default_ladder() {
        assert_eq!(period_options(0, 2, 32), DEFAULT_PERIOD_LADDER.to_vec());
        assert_eq!(period_options(4096, 0, 32), DEFAULT_PERIOD_LADDER.to_vec());
        assert_eq!(
            period_options(4096, 2, 32),
            oss_period_options(4096, 2, 32).unwrap()
        );
    }

    #[test]
    fn non_freebsd_platforms_use_fixed_defaults() {
        assert!(default_period_frames("anything", 32) >= FALLBACK_PERIOD_FRAMES);
    }
}
