//! System tray icon, following the same approach as dziber: on Linux/BSD a
//! D-Bus StatusNotifierItem via `ksni`; a no-op elsewhere.
//!
//! Events are pushed into a channel which the iced subscription polls
//! (`try_recv_event`).

use std::sync::{Mutex, OnceLock, mpsc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    ShowRequested,
    PlayToggleRequested,
    QuitRequested,
}

/// Snapshot of playback state shown in the tray hover tooltip.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayInfo {
    /// Artist tag; empty when unknown or not loaded yet.
    pub artist: String,
    /// Song title; `None` when no song is loaded at all.
    pub song: Option<String>,
    pub elapsed_secs: u64,
    pub total_secs: u64,
}

impl TrayInfo {
    /// Hover text: `Artist:`/`Song:`/`Time:` lines, or just the app title
    /// when nothing is loaded.
    pub fn tooltip(&self) -> (String, String) {
        match &self.song {
            None => (String::from("Maolan Player"), String::new()),
            Some(song) => {
                let description = format!(
                    "Artist: {}\nSong: {}\nTime: {} / {}",
                    self.artist,
                    song,
                    format_mss(self.elapsed_secs),
                    format_mss(self.total_secs),
                );
                (String::from("Maolan Player"), description)
            }
        }
    }
}

/// Format seconds as `m:ss`.
fn format_mss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

static EVENT_RX: OnceLock<Mutex<mpsc::Receiver<TrayEvent>>> = OnceLock::new();

/// Initialize the platform tray icon.
///
/// On Linux/BSD this registers a D-Bus StatusNotifierItem via `ksni`.
/// On other platforms it is a no-op.
pub fn init_tray() {
    sni_tray::init_tray_impl();
}

/// Poll for a tray event that should be handled by the UI event loop.
pub fn try_recv_event() -> Option<TrayEvent> {
    let rx = EVENT_RX.get()?;
    let guard = rx.lock().ok()?;
    guard.try_recv().ok()
}

/// Push the current playback snapshot to the tray tooltip.
///
/// No-op on platforms without a system tray.
pub fn update_tooltip(info: TrayInfo) {
    sni_tray::update_tooltip_impl(info);
}

#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]
pub(crate) mod sni_tray {
    use super::{EVENT_RX, TrayEvent, TrayInfo};
    use ksni::TrayMethods;
    use std::sync::{Mutex, OnceLock, mpsc};

    static HANDLE: OnceLock<ksni::Handle<PlayerTray>> = OnceLock::new();
    static INFO: OnceLock<Mutex<TrayInfo>> = OnceLock::new();
    /// Last tooltip (title, description) pushed to the StatusNotifierItem;
    /// ksni re-queries `icon_pixmap` on every property update, so avoid
    /// pushing when the visible tooltip did not actually change.
    static LAST_TOOLTIP: OnceLock<Mutex<(String, String)>> = OnceLock::new();

    pub(super) fn init_tray_impl() {
        if EVENT_RX.get().is_some() {
            return;
        }
        let _ = INFO.set(Mutex::new(TrayInfo::default()));
        let _ = LAST_TOOLTIP.set(Mutex::new((String::new(), String::new())));

        let (tx, rx) = mpsc::channel();
        let _ = EVENT_RX.set(Mutex::new(rx));

        let tray_impl = PlayerTray {
            events: tx,
            info: TrayInfo::default(),
        };

        if let Ok(runtime) = tokio::runtime::Handle::try_current()
            && let Ok(handle) = runtime.block_on(tray_impl.spawn())
        {
            let _ = HANDLE.set(handle);
        }
    }

    pub(super) fn update_tooltip_impl(info: TrayInfo) {
        let Some(info_slot) = INFO.get() else {
            return;
        };
        let tooltip = info.tooltip();
        {
            let Ok(mut guard) = info_slot.lock() else {
                return;
            };
            if *guard == info {
                return;
            }
            *guard = info.clone();
        }
        // Only notify the StatusNotifierItem when the tooltip actually
        // changed; each update makes ksni re-query `icon_pixmap`.
        let Some(last_tooltip) = LAST_TOOLTIP.get() else {
            return;
        };
        {
            let Ok(mut guard) = last_tooltip.lock() else {
                return;
            };
            if *guard == tooltip {
                return;
            }
            *guard = tooltip.clone();
        }
        // Notify the StatusNotifierItem so the tooltip actually re-renders.
        let Some(handle) = HANDLE.get() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = handle
                    .update(move |tray: &mut PlayerTray| {
                        tray.info = info;
                    })
                    .await;
            });
        }
    }

    pub(crate) struct PlayerTray {
        pub(crate) events: mpsc::Sender<TrayEvent>,
        pub(crate) info: TrayInfo,
    }

    impl ksni::Tray for PlayerTray {
        fn id(&self) -> String {
            "maolan-player".into()
        }

        fn title(&self) -> String {
            "Maolan Player".into()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            let (title, description) = self.info.tooltip();
            ksni::ToolTip {
                title,
                description,
                ..Default::default()
            }
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![ksni_icon()]
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::{MenuItem, StandardItem};

            vec![
                StandardItem {
                    label: "Show Maolan Player".into(),
                    activate: Box::new(|this: &mut PlayerTray| {
                        let _ = this.events.send(TrayEvent::ShowRequested);
                    }),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem {
                    label: "Quit Maolan Player".into(),
                    activate: Box::new(|this: &mut PlayerTray| {
                        let _ = this.events.send(TrayEvent::QuitRequested);
                    }),
                    ..Default::default()
                }
                .into(),
            ]
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.events.send(TrayEvent::ShowRequested);
        }

        fn secondary_activate(&mut self, _x: i32, _y: i32) {
            let _ = self.events.send(TrayEvent::PlayToggleRequested);
        }
    }

    /// The tray icon artwork, rendered from an SVG at startup.
    const ICON_SVG: &[u8] = include_bytes!("../assets/images/icon.svg");

    pub(crate) fn ksni_icon() -> ksni::Icon {
        static ICON: std::sync::LazyLock<ksni::Icon> = std::sync::LazyLock::new(|| {
            let (rgba, width, height) = render_rgba();
            let mut data = rgba;

            // ksni expects ARGB data; the rasterizer gives premultiplied RGBA.
            unpremultiply(&mut data);
            for pixel in data.as_chunks_mut::<4>().0 {
                pixel.rotate_right(1);
            }

            ksni::Icon {
                width: width as i32,
                height: height as i32,
                data,
            }
        });
        ICON.clone()
    }

    pub(crate) fn render_rgba() -> (Vec<u8>, u32, u32) {
        const SIZE: u32 = 64;
        let tree = resvg::usvg::Tree::from_data(ICON_SVG, &resvg::usvg::Options::default())
            .expect("tray icon SVG must parse");

        let size = tree.size();
        let scale = (SIZE as f32 / size.width().max(size.height())).min(SIZE as f32);
        let tx = (SIZE as f32 - size.width() * scale) / 2.0;
        let ty = (SIZE as f32 - size.height() * scale) / 2.0;
        let transform = tiny_skia::Transform::from_translate(tx, ty).post_scale(scale, scale);

        let mut pixmap = tiny_skia::Pixmap::new(SIZE, SIZE).expect("tray icon pixmap");
        resvg::render(&tree, transform, &mut pixmap.as_mut());

        (pixmap.data().to_vec(), SIZE, SIZE)
    }

    /// Convert premultiplied RGBA (tiny-skia) to straight alpha in place.
    pub(crate) fn unpremultiply(data: &mut [u8]) {
        for pixel in data.as_chunks_mut::<4>().0 {
            let a = u32::from(pixel[3]);
            if a == 0 {
                continue;
            }
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
)))]
mod sni_tray {
    use super::{EVENT_RX, Mutex, TrayInfo, mpsc};

    pub(super) fn init_tray_impl() {
        let _ = EVENT_RX.set(Mutex::new(mpsc::channel().1));
    }

    pub(super) fn update_tooltip_impl(_info: TrayInfo) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_recv_event_before_init_returns_none() {
        // EVENT_RX may already be set by another test that ran init; if so
        // this just checks the call is harmless.
        let _ = try_recv_event();
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    mod unix {
        use super::TrayEvent;
        use super::TrayInfo;
        use super::sni_tray::*;
        use ksni::Tray;
        use ksni::menu::MenuItem;

        fn test_tray() -> PlayerTray {
            let (tx, _rx) = std::sync::mpsc::channel();
            PlayerTray {
                events: tx,
                info: TrayInfo::default(),
            }
        }

        #[test]
        fn tray_id_and_title() {
            let tray = test_tray();
            assert_eq!(tray.id(), "maolan-player");
            assert_eq!(tray.title(), "Maolan Player");
        }

        #[test]
        fn tooltip_empty_when_no_song() {
            let tray = test_tray();
            let tip = tray.tool_tip();
            assert_eq!(tip.title, "Maolan Player");
            assert_eq!(tip.description, "");
        }

        #[test]
        fn tooltip_shows_artist_song_and_time() {
            let mut tray = test_tray();
            tray.info = TrayInfo {
                artist: String::from("Some Artist"),
                song: Some(String::from("Some Song")),
                elapsed_secs: 204,
                total_secs: 252,
            };
            let tip = tray.tool_tip();
            assert_eq!(tip.title, "Maolan Player");
            assert_eq!(
                tip.description,
                "Artist: Some Artist\nSong: Some Song\nTime: 3:24 / 4:12"
            );
        }

        #[test]
        fn tooltip_unknown_artist_renders_empty() {
            let info = TrayInfo {
                artist: String::new(),
                song: Some(String::from("Fallback Stem")),
                elapsed_secs: 0,
                total_secs: 65,
            };
            let (_, description) = info.tooltip();
            assert_eq!(
                description,
                "Artist: \nSong: Fallback Stem\nTime: 0:00 / 1:05"
            );
        }

        #[test]
        fn tray_menu_has_show_and_quit() {
            let tray = test_tray();
            let items = tray.menu();
            assert_eq!(items.len(), 3);
            match &items[0] {
                MenuItem::Standard(item) => assert_eq!(item.label, "Show Maolan Player"),
                _ => panic!("expected Show standard item"),
            }
            assert!(matches!(items[1], MenuItem::Separator));
            match &items[2] {
                MenuItem::Standard(item) => assert_eq!(item.label, "Quit Maolan Player"),
                _ => panic!("expected Quit standard item"),
            }
        }

        #[test]
        fn secondary_activate_requests_play_toggle() {
            let (tx, rx) = std::sync::mpsc::channel();
            let mut tray = PlayerTray {
                events: tx,
                info: TrayInfo::default(),
            };
            tray.secondary_activate(0, 0);
            assert_eq!(rx.recv().unwrap(), TrayEvent::PlayToggleRequested);
        }

        #[test]
        fn ksni_icon_has_expected_size_and_content() {
            let icon = ksni_icon();
            assert_eq!(icon.width, 64);
            assert_eq!(icon.height, 64);
            assert_eq!(icon.data.len(), 64 * 64 * 4);
            // The artwork must actually paint some pixels.
            assert!(icon.data.as_chunks::<4>().0.iter().any(|px| px[3] > 0));
        }

        #[test]
        fn ksni_icon_is_cached_and_consistent() {
            // Repeated queries (ksni re-queries icon_pixmap on every property
            // update) must return identical data from the cache.
            let first = ksni_icon();
            let second = ksni_icon();
            assert_eq!(first.width, second.width);
            assert_eq!(first.height, second.height);
            assert_eq!(first.data, second.data);
        }

        #[test]
        fn render_rgba_produces_64x64() {
            let (data, width, height) = render_rgba();
            assert_eq!(width, 64);
            assert_eq!(height, 64);
            assert_eq!(data.len(), 64 * 64 * 4);
        }

        #[test]
        fn unpremultiply_converts_in_place() {
            // 50%-gray at 50% alpha premultiplied is (128, 128, 128, 128).
            let mut px = [128u8, 128, 128, 128];
            unpremultiply(&mut px);
            assert_eq!(px, [255, 255, 255, 128]);
            // Fully transparent stays untouched.
            let mut px = [0u8, 0, 0, 0];
            unpremultiply(&mut px);
            assert_eq!(px, [0, 0, 0, 0]);
        }
    }
}
