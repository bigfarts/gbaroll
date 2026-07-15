//! The iced application: tab shell (Play / Replays / Settings) and the
//! fullscreen session view. Netplay is not a separate flow — a running
//! solo session hosts/joins a room from its link sidebar, the cable
//! plugs in when the room starts (runtime swap to netplay), and any
//! netplay teardown unplugs back to solo.

pub mod icons;
mod lobby;
mod play;
mod replays;
mod scrubber;
mod session_view;
mod settings;
mod telemetry;

use std::sync::atomic::Ordering;
use std::sync::Arc;

use iced::widget::{button, column, container, row, text};
use iced::{Element, Length, Subscription, Task, Theme};

use crate::config::Config;
use crate::library::Library;
use crate::net::lobby::{LobbyCommand, LobbyEvent};
use crate::platform::input::{MappedKey, PhysicalInput};
use crate::session::SessionKind;

pub const PADDING: f32 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    #[default]
    Play,
    Replays,
    Settings,
}

/// A save-file choice for pick lists.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveChoice {
    Fresh,
    File(std::path::PathBuf),
}

impl std::fmt::Display for SaveChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveChoice::Fresh => write!(f, "(fresh save)"),
            SaveChoice::File(p) => write!(
                f,
                "{}",
                p.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
            ),
        }
    }
}

impl SaveChoice {
    /// Read the chosen save's bytes, if any.
    pub fn read(&self) -> anyhow::Result<Option<Vec<u8>>> {
        Ok(match self {
            SaveChoice::Fresh => None,
            SaveChoice::File(p) => {
                let bytes = std::fs::read(p)?;
                // GBA flash tops out at 128 KiB; leave headroom for
                // emulator save footers.
                if bytes.len() > 512 * 1024 {
                    anyhow::bail!("save file too large");
                }
                Some(bytes)
            }
        })
    }
}

/// A player choice for view/control pick lists.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerChoice {
    pub idx: usize,
    pub label: String,
}

impl std::fmt::Display for PlayerChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.label)
    }
}

/// Playback speed choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeedChoice(pub u32);

impl std::fmt::Display for SpeedChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}×", self.0 as f32 / 100.0)
    }
}

pub const SPEED_CHOICES: [SpeedChoice; 5] = [
    SpeedChoice(25),
    SpeedChoice(50),
    SpeedChoice(100),
    SpeedChoice(200),
    SpeedChoice(400),
];

#[derive(Debug, Clone)]
pub enum Message {
    TabSelected(Tab),
    DismissNotice,

    // Play tab.
    PlaySearchChanged(String),
    PlayRomSelected(u32),
    LocalPlayersChanged(usize),
    LocalSaveSelected(SaveChoice),
    LocalClicked,
    RescanRoms,

    // The link sidebar (host/join a room from a running solo session).
    SessionLinkToggle,
    LinkCodeChanged(String),
    LinkHostClicked,
    LinkJoinClicked,
    SessionUnplug,

    // Lobby (inside the link modal).
    LobbyPoll,
    LobbyReadyToggled(bool),
    LobbyStartClicked,
    LobbyLeaveClicked,

    // Session.
    SessionFrame,
    SessionInput(crate::platform::input::Event),
    SessionToggleMenu,
    SessionToggleTelemetry,
    SessionPauseToggled,
    SessionSpeedSelected(SpeedChoice),
    SessionViewPlayerSelected(PlayerChoice),
    SessionPresentDelayChanged(u32),
    SessionVolumeChanged(f32),
    SessionSeekChanged(u32),
    SessionSeekCommitted,
    SessionQuit,
    SessionDismissEnd,

    // Replays tab.
    ReplayWatch(usize),
    ReplayDelete(usize),
    RescanReplays,

    // Settings tab.
    NickChanged(String),
    ServerChanged(String),
    PresentDelayChanged(u32),
    VolumeChanged(f32),
    IntegerScalingToggled(bool),
    ShowHudToggled(bool),
    PickRomsDir,
    PickSavesDir,
    PickReplaysDir,
    PickDatsDir,
    DownloadGbaDat,
    DatDownloaded(Result<usize, String>),
    BindingCaptureStart(MappedKey),
    BindingCaptured(PhysicalInput),
    BindingCaptureCancel,
    BindingRemoved(MappedKey, usize),
    MappingReset,
}

pub struct App {
    pub config: Config,
    pub library: Library,
    pub dats: crate::nointro::DatIndex,
    pub dat_downloading: bool,
    pub dat_download_error: Option<String>,
    pub tab: Tab,
    pub notice: Option<String>,

    pub play: play::State,
    pub replays: replays::State,
    pub settings: settings::State,
    pub lobby: Option<lobby::State>,
    pub session: Option<session_view::State>,

    pub audio_binder: crate::platform::audio::LateBinder,
    _audio_backend: Option<crate::platform::audio::sdl::Backend>,
    /// App-lifetime vblank notify; every session's drive thread signals
    /// it, and the frame subscription's identity hangs off it.
    pub frame_notify: Arc<tokio::sync::Notify>,
}

impl App {
    pub fn new() -> (App, Task<Message>) {
        let mut config = Config::load();
        config.ensure_dirs();

        let mut audio_binder = crate::platform::audio::LateBinder::new();
        let audio_backend = match crate::platform::audio::sdl::Backend::new(audio_binder.clone()) {
            Ok(backend) => {
                audio_binder.set_sample_rate(backend.sample_rate());
                Some(backend)
            }
            Err(e) => {
                log::error!("audio unavailable: {e:#}");
                None
            }
        };
        audio_binder.set_volume(config.volume);

        let dats = crate::nointro::DatIndex::load_dir(&config.dats_dir);
        let library = Library::scan(&config.roms_dir, &dats);
        let replays = replays::State::scan(&config.replays_dir);

        // First run (no DATs yet): fetch the GBA No-Intro DAT in the
        // background so names appear without any setup.
        let mut startup = Task::none();
        let mut dat_downloading = false;
        if dats.files() == 0 {
            dat_downloading = true;
            startup = download_gba_dat_task(config.dats_dir.clone());
        }

        (
            App {
                play: play::State::default(),
                replays,
                settings: settings::State::default(),
                lobby: None,
                session: None,
                library,
                dats,
                dat_downloading,
                dat_download_error: None,
                notice: None,
                audio_binder,
                _audio_backend: audio_backend,
                frame_notify: Arc::new(tokio::sync::Notify::new()),
                config,
                tab: Tab::Play,
            },
            startup,
        )
    }

    pub fn title(&self) -> String {
        "gbaroll".to_string()
    }

    pub fn theme(&self) -> Theme {
        Theme::TokyoNight
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![Subscription::run_with(
            FrameTag {
                notify: self.frame_notify.clone(),
            },
            build_frame_stream,
        )];
        if self.lobby.is_some() {
            subs.push(iced::time::every(std::time::Duration::from_millis(100)).map(|_| Message::LobbyPoll));
        }
        Subscription::batch(subs)
    }

    fn notice(&mut self, message: impl Into<String>) {
        let message = message.into();
        log::warn!("{message}");
        self.notice = Some(message);
    }

    fn close_session(&mut self) {
        // A lobby can't outlive the session it would plug into.
        self.lobby = None;
        // Dropping the runtime asks the drive thread to quit and joins
        // it (a deliberate quit also announces itself to the peers).
        self.session = None;
        // A netplay session just recorded a replay; pick it up.
        self.replays = replays::State::scan(&self.config.replays_dir);
    }

    fn apply_joyflags(&mut self) {
        let Some(session) = &mut self.session else { return };
        let joyflags = self.config.mapping.to_mgba_keys(&session.held);
        session.runtime.shared.joyflags.store(joyflags, Ordering::Relaxed);
        // Hold-to-fast-forward for local/playback sessions.
        if session.runtime.descriptor.kind != SessionKind::Netplay {
            let speed_up = self.config.mapping.speed_up_held(&session.held);
            if speed_up != session.speed_up_held {
                session.speed_up_held = speed_up;
                let speed = if speed_up { 300 } else { session.selected_speed };
                session.runtime.shared.speed.store(speed, Ordering::Relaxed);
            }
        }
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::TabSelected(tab) => {
                self.tab = tab;
                if tab == Tab::Replays {
                    self.replays = replays::State::scan(&self.config.replays_dir);
                }
            }
            Message::DismissNotice => self.notice = None,

            // ---- play tab ----
            Message::PlaySearchChanged(s) => self.play.search = s,
            Message::PlayRomSelected(crc) => self.play.selected_crc = Some(crc),
            Message::LocalPlayersChanged(n) => self.play.local_players = n.clamp(1, 4),
            Message::LocalSaveSelected(choice) => self.play.local_save = choice,
            Message::RescanRoms => {
                self.dats = crate::nointro::DatIndex::load_dir(&self.config.dats_dir);
                self.library = Library::scan(&self.config.roms_dir, &self.dats);
                self.play.selected_crc = self
                    .play
                    .selected_crc
                    .filter(|crc| self.library.by_crc32(*crc).is_some());
            }
            Message::LocalClicked => {
                if let Err(e) = self.start_local() {
                    self.notice(format!("couldn't start session: {e:#}"));
                }
            }

            // ---- link sidebar ----
            Message::SessionLinkToggle => {
                if let Some(session) = &mut self.session {
                    session.link_open = !session.link_open;
                }
            }
            Message::LinkCodeChanged(code) => {
                if let Some(session) = &mut self.session {
                    session.link_code = code.to_ascii_uppercase();
                }
            }
            Message::LinkHostClicked => self.start_lobby(crate::net::lobby::LobbyMode::Create),
            Message::LinkJoinClicked => {
                let code = self
                    .session
                    .as_ref()
                    .map(|s| gbaroll_signaling::normalize_room_code(&s.link_code))
                    .unwrap_or_default();
                if code.is_empty() {
                    if let Some(session) = &mut self.session {
                        session.link_notice = Some("enter a room code to join".to_string());
                    }
                } else {
                    self.start_lobby(crate::net::lobby::LobbyMode::Join { code });
                }
            }
            Message::SessionUnplug => {
                if let Some(session) = &self.session {
                    session.runtime.shared.unplug.store(true, Ordering::Relaxed);
                }
            }

            // ---- lobby ----
            Message::LobbyPoll => self.poll_lobby(),
            Message::LobbyReadyToggled(ready) => {
                if let Some(lobby) = &mut self.lobby {
                    lobby.my_ready = ready;
                    lobby.handle.send(LobbyCommand::SetReady { ready });
                }
            }
            Message::LobbyStartClicked => {
                if let Some(lobby) = &self.lobby {
                    lobby.handle.send(LobbyCommand::Start);
                }
            }
            Message::LobbyLeaveClicked => {
                self.lobby = None; // Drop sends Leave.
                // If the start already froze the machine for its capture,
                // let it run again.
                if let Some(session) = &self.session {
                    session.runtime.shared.paused.store(false, Ordering::Relaxed);
                }
            }

            // ---- session ----
            Message::SessionFrame => {
                if let Some(session) = &mut self.session {
                    session.refresh();
                    // A netplay end that leaves a live machine behind is a
                    // cable unplug: continue solo instead of a dead end.
                    if session.runtime.descriptor.kind == SessionKind::Netplay {
                        if let Some(end) = session.end.clone() {
                            if end.unplugs() {
                                self.unplug_continue(&end);
                            }
                        }
                    }
                }
            }
            Message::SessionInput(event) => {
                if let Some(session) = &mut self.session {
                    session.held.apply(&event);
                }
                self.apply_joyflags();
            }
            Message::SessionToggleMenu => {
                if let Some(session) = &mut self.session {
                    if session.end.is_none() {
                        session.menu_open = !session.menu_open;
                    }
                }
            }
            Message::SessionToggleTelemetry => {
                if let Some(session) = &mut self.session {
                    session.telemetry_open = !session.telemetry_open;
                }
            }
            Message::SessionPauseToggled => {
                if let Some(session) = &mut self.session {
                    if session.runtime.descriptor.kind != SessionKind::Netplay {
                        session.toggle_pause();
                    }
                }
            }
            Message::SessionSpeedSelected(SpeedChoice(pct)) => {
                if let Some(session) = &mut self.session {
                    session.selected_speed = pct;
                    if !session.speed_up_held {
                        session.runtime.shared.speed.store(pct, Ordering::Relaxed);
                    }
                }
            }
            Message::SessionViewPlayerSelected(choice) => {
                if let Some(session) = &self.session {
                    session.runtime.shared.view_player.store(choice.idx, Ordering::Relaxed);
                }
            }
            Message::SessionPresentDelayChanged(delay) => {
                self.config.present_delay = delay.min(10);
                self.config.save();
                if let Some(session) = &self.session {
                    session
                        .runtime
                        .shared
                        .present_delay
                        .store(self.config.present_delay, Ordering::Relaxed);
                }
            }
            Message::SessionVolumeChanged(v) => {
                self.config.volume = v.clamp(0.0, 1.0);
                self.config.save();
                self.audio_binder.set_volume(self.config.volume);
            }
            Message::SessionSeekChanged(tick) => {
                if let Some(session) = &mut self.session {
                    session.scrub_drag(tick);
                }
            }
            Message::SessionSeekCommitted => {
                if let Some(session) = &mut self.session {
                    session.scrub_commit();
                }
            }
            Message::SessionQuit => self.close_session(),
            Message::SessionDismissEnd => self.close_session(),

            // ---- replays tab ----
            Message::RescanReplays => {
                self.replays = replays::State::scan(&self.config.replays_dir);
            }
            Message::ReplayWatch(index) => {
                if let Err(e) = self.watch_replay(index) {
                    self.notice(format!("couldn't play replay: {e:#}"));
                }
            }
            Message::ReplayDelete(index) => {
                if let Some(entry) = self.replays.entries.get(index) {
                    if let Err(e) = std::fs::remove_file(&entry.path) {
                        self.notice(format!("couldn't delete replay: {e}"));
                    }
                }
                self.replays = replays::State::scan(&self.config.replays_dir);
            }

            // ---- settings tab ----
            Message::NickChanged(nick) => {
                self.config.nick = nick;
                self.config.save();
            }
            Message::ServerChanged(url) => {
                self.config.signaling_server = url;
                self.config.save();
            }
            Message::PresentDelayChanged(delay) => {
                self.config.present_delay = delay.min(10);
                self.config.save();
            }
            Message::VolumeChanged(v) => {
                self.config.volume = v.clamp(0.0, 1.0);
                self.config.save();
                self.audio_binder.set_volume(self.config.volume);
            }
            Message::IntegerScalingToggled(v) => {
                self.config.integer_scaling = v;
                self.config.save();
            }
            Message::ShowHudToggled(v) => {
                self.config.show_hud = v;
                self.config.save();
            }
            Message::PickRomsDir => {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&self.config.roms_dir).pick_folder() {
                    self.config.roms_dir = dir;
                    self.config.save();
                    self.library = Library::scan(&self.config.roms_dir, &self.dats);
                }
            }
            Message::PickDatsDir => {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&self.config.dats_dir).pick_folder() {
                    self.config.dats_dir = dir;
                    self.config.save();
                    self.dats = crate::nointro::DatIndex::load_dir(&self.config.dats_dir);
                    self.library = Library::scan(&self.config.roms_dir, &self.dats);
                }
            }
            Message::DownloadGbaDat => {
                if !self.dat_downloading {
                    self.dat_downloading = true;
                    self.dat_download_error = None;
                    return download_gba_dat_task(self.config.dats_dir.clone());
                }
            }
            Message::DatDownloaded(result) => {
                self.dat_downloading = false;
                match result {
                    Ok(_) => {
                        self.dat_download_error = None;
                        self.dats = crate::nointro::DatIndex::load_dir(&self.config.dats_dir);
                        self.library = Library::scan(&self.config.roms_dir, &self.dats);
                    }
                    Err(e) => {
                        log::warn!("DAT download failed: {e}");
                        self.dat_download_error = Some(e);
                    }
                }
            }
            Message::PickSavesDir => {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&self.config.saves_dir).pick_folder() {
                    self.config.saves_dir = dir;
                    self.config.save();
                }
            }
            Message::PickReplaysDir => {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_directory(&self.config.replays_dir)
                    .pick_folder()
                {
                    self.config.replays_dir = dir;
                    self.config.save();
                    self.replays = replays::State::scan(&self.config.replays_dir);
                }
            }
            Message::BindingCaptureStart(key) => self.settings.capture_target = Some(key),
            Message::BindingCaptureCancel => self.settings.capture_target = None,
            Message::BindingCaptured(physical) => {
                if let Some(key) = self.settings.capture_target.take() {
                    let slot = self.config.mapping.slot_mut(key);
                    if !slot.contains(&physical) {
                        slot.push(physical);
                    }
                    self.config.save();
                }
            }
            Message::BindingRemoved(key, index) => {
                let slot = self.config.mapping.slot_mut(key);
                if index < slot.len() {
                    slot.remove(index);
                }
                self.config.save();
            }
            Message::MappingReset => {
                self.config.mapping = Default::default();
                self.config.save();
            }
        }
        Task::none()
    }

    pub fn view(&self) -> Element<'_, Message> {
        if let Some(session) = &self.session {
            return session_view::view(session, self.lobby.as_ref(), &self.library, &self.config);
        }

        let tab_button = |label: &'static str, tab: Tab| {
            button(text(label))
                .padding([6, 14])
                .style(if self.tab == tab {
                    button::primary
                } else {
                    button::text
                })
                .on_press(Message::TabSelected(tab))
        };

        let top_bar = container(
            row![
                text("gbaroll").size(22),
                iced::widget::Space::new().width(Length::Fixed(16.0)),
                tab_button("Play", Tab::Play),
                tab_button("Replays", Tab::Replays),
                tab_button("Settings", Tab::Settings),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
        )
        .padding(PADDING)
        .width(Length::Fill);

        let mut layers = column![top_bar];

        if let Some(notice) = &self.notice {
            layers = layers.push(
                container(
                    row![
                        text(notice.clone()),
                        iced::widget::Space::new().width(Length::Fill),
                        button(text("dismiss")).style(button::text).on_press(Message::DismissNotice),
                    ]
                    .align_y(iced::Alignment::Center),
                )
                .padding(PADDING)
                .width(Length::Fill)
                .style(|theme: &Theme| container::Style {
                    background: Some(iced::Background::Color(theme.extended_palette().danger.weak.color)),
                    text_color: Some(theme.extended_palette().danger.weak.text),
                    ..Default::default()
                }),
            );
        }

        let body: Element<'_, Message> = match self.tab {
            Tab::Play => play::view(self),
            Tab::Replays => replays::view(self),
            Tab::Settings => settings::view(self),
        };

        layers
            .push(container(body).padding(PADDING * 1.5).width(Length::Fill).height(Length::Fill))
            .into()
    }

    fn selected_rom(&self) -> Option<&crate::library::RomInfo> {
        self.play.selected_crc.and_then(|crc| self.library.by_crc32(crc))
    }

    /// Open a room from the running solo session's sidebar. The game
    /// keeps running; the cable plugs in when the room starts.
    fn start_lobby(&mut self, mode: crate::net::lobby::LobbyMode) {
        if self.lobby.is_some() {
            return;
        }
        let Some(session) = &mut self.session else { return };
        let rom = session
            .runtime
            .descriptor
            .rom_crc32
            .and_then(|crc| self.library.by_crc32(crc));
        let Some(rom) = rom else {
            session.link_notice = Some("this game's ROM is missing from the library".to_string());
            return;
        };
        let handle = crate::net::lobby::spawn(crate::net::lobby::LobbyArgs {
            server_url: self.config.signaling_server.clone(),
            nick: self.config.nick.clone(),
            rom_crc32: rom.crc32,
            rom_title: rom.display_name().to_string(),
            mode,
        });
        self.lobby = Some(lobby::State::new(handle));
        session.link_notice = None;
        session.link_open = true;
    }

    fn start_local(&mut self) -> anyhow::Result<()> {
        let rom = self.selected_rom().ok_or_else(|| anyhow::anyhow!("pick a ROM first"))?;
        let rom_crc32 = rom.crc32;
        let bytes = crate::library::read_rom(rom)?;
        let save = self.play.local_save.read()?;
        let runtime = crate::session::local::start(
            crate::session::local::LocalArgs {
                roms: vec![bytes; self.play.local_players],
                rom_crc32,
                save,
            },
            &self.audio_binder,
            self.frame_notify.clone(),
        )?;
        self.session = Some(session_view::State::new(runtime));
        Ok(())
    }

    /// Capture the running solo machine for the plug-in exchange,
    /// freezing it on exactly the captured state (this is what every
    /// peer — us included — boots the link from).
    fn capture_boot(session: Option<&session_view::State>) -> anyhow::Result<Vec<u8>> {
        let session = session.ok_or_else(|| anyhow::anyhow!("the game ended"))?;
        let link = session
            .runtime
            .link
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("this session's machine can't be captured"))?;
        session.runtime.shared.paused.store(true, Ordering::Relaxed);
        let blob = link
            .with_link(|link| {
                let state = link.capture_boot_state(0)?;
                Ok::<_, mgba::Error>(crate::net::protocol::BootBlob {
                    state,
                    save: link.export_save(0),
                })
            })
            .ok_or_else(|| anyhow::anyhow!("machine unavailable"))??;
        blob.encode()
    }

    fn poll_lobby(&mut self) {
        let Some(lobby) = &mut self.lobby else { return };
        let mut fatal: Option<String> = None;
        let mut bundle = None;
        while let Ok(event) = lobby.handle.events.try_recv() {
            match event {
                LobbyEvent::Joined { code } => lobby.code = Some(code),
                LobbyEvent::Roster { players, your_idx } => {
                    lobby.players = players;
                    lobby.my_idx = your_idx;
                    // Occupancy changes reset ready state server-side;
                    // mirror what the server reports for us.
                    lobby.my_ready = lobby.players.get(your_idx).map(|p| p.ready).unwrap_or(false);
                }
                LobbyEvent::Error(message) => {
                    lobby.status = Some(message);
                }
                LobbyEvent::Starting => {
                    // The cable is being plugged in: freeze the machine
                    // and ship its capture (self.session is a disjoint
                    // field, so borrowing it here is fine).
                    match Self::capture_boot(self.session.as_ref()) {
                        Ok(blob) => lobby.handle.send(LobbyCommand::Boot(blob)),
                        Err(e) => {
                            fatal = Some(format!("couldn't capture the machine: {e:#}"));
                            break;
                        }
                    }
                }
                LobbyEvent::Connecting(message) => {
                    lobby.status = Some(message);
                }
                LobbyEvent::Fatal(message) => {
                    fatal = Some(message);
                    break;
                }
                LobbyEvent::SessionReady(b) => {
                    bundle = Some(b);
                    break;
                }
            }
        }
        if let Some(message) = fatal {
            self.lobby = None;
            self.link_failed(message);
            return;
        }
        if let Some(bundle) = bundle {
            self.lobby = None;
            if let Err(e) = self.plug_in(*bundle) {
                self.link_failed(format!("couldn't plug in: {e:#}"));
            }
        }
    }

    /// A lobby or plug-in failure: let the (possibly frozen) game run
    /// again and surface the reason in the sidebar.
    fn link_failed(&mut self, message: String) {
        log::warn!("{message}");
        if let Some(session) = &mut self.session {
            session.runtime.shared.paused.store(false, Ordering::Relaxed);
            session.link_notice = Some(message);
            session.link_open = true;
        } else {
            self.notice(message);
        }
    }

    /// The cable plugs in: swap the frozen solo runtime for a netplay
    /// runtime booted from the exchanged captures. The view (and the
    /// player's held keys) carry across the swap.
    fn plug_in(&mut self, bundle: crate::net::lobby::SessionBundle) -> anyhow::Result<()> {
        let mut roms = Vec::new();
        let mut rom_meta = Vec::new();
        for player in &bundle.players {
            let info = self.library.by_crc32(player.rom_crc32).ok_or_else(|| {
                anyhow::anyhow!(
                    "missing a copy of {}'s ROM (crc32 {:08x})",
                    player.nick,
                    player.rom_crc32
                )
            })?;
            roms.push(crate::library::read_rom(info)?);
            rom_meta.push((info.crc32, info.title.clone(), info.code.clone()));
        }
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("the game ended before the cable plugged in"))?;
        // The audio slot must free up before the netplay runtime binds.
        session.runtime.release_audio();
        let runtime = crate::session::netplay::start(
            crate::session::netplay::NetplayArgs {
                bundle,
                roms,
                rom_meta,
                replays_dir: self.config.replays_dir.clone(),
                present_delay: self.config.present_delay,
            },
            &self.audio_binder,
            self.frame_notify.clone(),
        )?;
        session.swap_runtime(runtime);
        session.link_notice = None;
        session.link_open = true;
        self.apply_joyflags();
        Ok(())
    }

    /// The cable unplugs: swap the finished netplay runtime for a solo
    /// continuation of the local machine, and note why in the sidebar.
    fn unplug_continue(&mut self, end: &crate::session::SessionEnd) {
        use crate::session::SessionEnd;
        let Some(session) = &mut self.session else { return };
        let Some(handoff) = session.runtime.shared.handoff.lock().unwrap().take() else {
            // No continuation material (the capture failed): leave the
            // end overlay to say what happened.
            return;
        };
        let nick_of = |player: usize| {
            session
                .runtime
                .descriptor
                .nicks
                .get(player)
                .cloned()
                .unwrap_or_else(|| format!("player {}", player + 1))
        };
        let reason = match end {
            SessionEnd::Unplugged => "Unplugged.".to_string(),
            SessionEnd::PeerQuit { player } => format!("{} unplugged.", nick_of(*player)),
            SessionEnd::PeerDisconnected { player } => format!("Connection to {} lost.", nick_of(*player)),
            SessionEnd::Desync { tick } => format!("Desync at tick {tick} — cable unplugged."),
            _ => "Unplugged.".to_string(),
        };
        let rom_crc32 = session.runtime.descriptor.rom_crc32.unwrap_or_default();
        session.runtime.release_audio();
        match crate::session::local::resume(handoff, rom_crc32, &self.audio_binder, self.frame_notify.clone()) {
            Ok(runtime) => {
                session.swap_runtime(runtime);
                session.link_notice = Some(reason);
                session.link_open = true;
                self.apply_joyflags();
                // The netplay session recorded a replay on the way out.
                self.replays = replays::State::scan(&self.config.replays_dir);
            }
            Err(e) => {
                // Fall back to the end overlay.
                log::error!("couldn't continue solo after the unplug: {e:#}");
            }
        }
    }

    fn watch_replay(&mut self, index: usize) -> anyhow::Result<()> {
        let entry = self
            .replays
            .entries
            .get(index)
            .ok_or_else(|| anyhow::anyhow!("replay disappeared"))?;
        let bytes = std::fs::read(&entry.path)?;
        let replay = gbaroll_replay::Replay::parse(&bytes)?;
        let mut roms = Vec::new();
        for player in &replay.metadata.players {
            let info = self.library.by_crc32(player.rom_crc32).ok_or_else(|| {
                anyhow::anyhow!(
                    "missing ROM {} (crc32 {:08x})",
                    player.rom_title,
                    player.rom_crc32
                )
            })?;
            roms.push(crate::library::read_rom(info)?);
        }
        let runtime = crate::session::playback::start(
            crate::session::playback::PlaybackArgs {
                replay,
                roms,
                path: entry.path.clone(),
            },
            &self.audio_binder,
            self.frame_notify.clone(),
        )?;
        self.session = Some(session_view::State::new(runtime));
        Ok(())
    }
}

fn download_gba_dat_task(dats_dir: std::path::PathBuf) -> Task<Message> {
    Task::perform(crate::nointro::fetch_gba_dat(dats_dir), |result| {
        Message::DatDownloaded(result.map_err(|e| format!("{e:#}")))
    })
}

/// Stable identity for the frame subscription (the notify is
/// app-lifetime, so the stream survives across sessions).
struct FrameTag {
    notify: Arc<tokio::sync::Notify>,
}

impl std::hash::Hash for FrameTag {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        "session-frame".hash(h);
    }
}

fn build_frame_stream(tag: &FrameTag) -> impl futures::Stream<Item = Message> {
    let notify = tag.notify.clone();
    futures::stream::unfold(notify, |notify| async move {
        notify.notified().await;
        Some((Message::SessionFrame, notify))
    })
}

/// Format a tick count as mm:ss at the GBA frame rate.
pub fn format_ticks(ticks: u32) -> String {
    let seconds = (ticks as f32 / crate::session::EXPECTED_FPS) as u32;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
