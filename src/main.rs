use maolan_player::app;

use maolan_widgets::iced::{
    Settings, Theme, daemon,
    executor::Executor,
    futures::{Future, io},
};
use maolan_widgets::iced_fonts::LUCIDE_FONT_BYTES;

struct PlayerExecutor(tokio::runtime::Runtime);

impl Executor for PlayerExecutor {
    fn new() -> Result<Self, io::Error> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("player-tokio")
            .enable_all()
            .build()
            .map(Self)
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        let _handle = self.0.spawn(future);
    }

    fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _guard = self.0.enter();
        f()
    }

    fn block_on<T>(&self, future: impl Future<Output = T>) -> T {
        self.0.block_on(future)
    }
}

fn main() -> maolan_widgets::iced::Result {
    // Device override: CLI arg > MAOLAN_PLAYER_DEVICE env var; otherwise the
    // persisted settings file or the per-OS default applies. The player
    // always starts hidden; the tray Show action opens the window.
    let device_override = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("MAOLAN_PLAYER_DEVICE").ok());

    let device_override = std::rc::Rc::new(device_override);
    let device_boot = device_override.clone();
    // A daemon keeps running with zero windows: closing the window destroys
    // it (true "close to tray" on Wayland, where hide/minimize don't exist)
    // and the tray Show action opens a fresh one.
    daemon(
        move || app::new(device_boot.as_ref().clone()),
        app::update,
        app::view_window,
    )
    .executor::<PlayerExecutor>()
    .title(|_: &app::PlayerApp, _| String::from("Maolan Player"))
    .settings(Settings {
        antialiasing: true,
        ..Settings::default()
    })
    .theme(|_: &app::PlayerApp, _| Theme::Dark)
    .font(LUCIDE_FONT_BYTES)
    .subscription(app::subscription)
    .run()
}
