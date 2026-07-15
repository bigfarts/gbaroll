//! gbaroll — a generic GBA link-cable rollback netplay client.
//!
//! Every GBA on the emulated cable (2 to 4) runs locally in one
//! `mgba_siolink::Link`; the link is the rollback unit and the only true
//! inputs are the joypads, so any link-capable game works with no
//! per-game code. Netplay is a full WebRTC mesh rendezvoused through the
//! `gbaroll-signaling` server; the data protocol is `rennet` frames over
//! an unreliable datachannel per peer. Sessions record roundless
//! `gbaroll-replay` files.

mod config;
mod library;
mod net;
mod nointro;
mod platform;
mod session;
#[cfg(test)]
mod smoke_test;
mod ui;

use std::sync::OnceLock;

/// The shared tokio runtime for signaling + peer transport. The UI and
/// the emulator drive thread are plain threads; only networking is async.
pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime")
    })
}

fn main() -> iced::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    mgba::log::install_default_logger();

    // SDL3 (audio + gamepad) initializes on the main thread, before iced
    // takes it over.
    platform::sdl_init::init();
    platform::gamepad::init();

    iced::application(ui::App::new, ui::App::update, ui::App::view)
        .settings(iced::Settings {
            vsync: false,
            ..iced::Settings::default()
        })
        .title(ui::App::title)
        .theme(ui::App::theme)
        .subscription(ui::App::subscription)
        .window(iced::window::Settings {
            size: iced::Size::new(1100.0, 720.0),
            min_size: Some(iced::Size::new(860.0, 600.0)),
            ..iced::window::Settings::default()
        })
        .run()
}
