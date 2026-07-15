//! The fullscreen session view: the framebuffer shader widget (integer
//! or fit scaling), a kind-specific header/footer (netplay stats,
//! playback transport with async scrubbing), input capture, the link
//! sidebar (host/join a room next to the running game, roster while
//! linked), and the Esc menu / end-of-session overlays.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use iced::keyboard::key::{Code, Physical};
use iced::widget::{button, column, container, pick_list, row, slider, stack, text, text_input};
use iced::{Element, Length, Theme};

use super::{Message, PlayerChoice, SpeedChoice, PADDING, SPEED_CHOICES};
use crate::config::Config;
use crate::library::Library;
use crate::platform::input::HeldState;
use crate::platform::input_capture::{Input, InputCapture};
use crate::platform::video::framebuffer;
use crate::session::{SessionEnd, SessionKind, SessionRuntime, Stats};

pub struct State {
    pub runtime: SessionRuntime,
    pub held: HeldState,
    pub menu_open: bool,
    pub selected_speed: u32,
    pub speed_up_held: bool,

    // The link sidebar.
    pub link_open: bool,
    pub link_code: String,
    /// Why the cable last unplugged, shown quietly in the sidebar.
    pub link_notice: Option<String>,

    // Scrub-drag bookkeeping (playback only).
    pub scrub_preview: Option<u32>,
    scrub_resume: bool,
    scrub_blitted: bool,

    // Mirrors refreshed once per published frame.
    pub stats: Stats,
    pub end: Option<SessionEnd>,
    frame: Option<framebuffer::Frame>,
    seen_revision: u64,
    frame_revision: u64,
}

impl State {
    pub fn new(runtime: SessionRuntime) -> State {
        State {
            runtime,
            held: HeldState::default(),
            menu_open: false,
            selected_speed: 100,
            speed_up_held: false,
            link_open: false,
            link_code: String::new(),
            link_notice: None,
            scrub_preview: None,
            scrub_resume: false,
            scrub_blitted: false,
            stats: Stats::default(),
            end: None,
            frame: None,
            seen_revision: 0,
            frame_revision: 0,
        }
    }

    /// Swap the underlying runtime in place — the plug-in / unplug
    /// transitions. The old runtime drops here (joining its drive
    /// thread); the view keeps its held input and the last frame, so the
    /// machine appears continuous across the swap. The caller must have
    /// [released](SessionRuntime::release_audio) the old runtime's audio
    /// before booting the new one.
    pub fn swap_runtime(&mut self, runtime: SessionRuntime) {
        self.runtime = runtime;
        self.menu_open = false;
        self.speed_up_held = false;
        self.scrub_preview = None;
        self.scrub_resume = false;
        self.scrub_blitted = false;
        self.stats = Stats::default();
        self.end = None;
        self.seen_revision = 0;
    }

    /// Pull the newest published frame + stats + end state. Called on
    /// every frame notify.
    pub fn refresh(&mut self) {
        let shared = &self.runtime.shared;
        let revision = shared.vbuf_rev.load(Ordering::Acquire);
        if revision != self.seen_revision {
            self.seen_revision = revision;
            let pixels = shared.vbuf.lock().unwrap().clone();
            self.frame_revision = self.frame_revision.wrapping_add(1);
            self.frame = Some(framebuffer::Frame {
                pixels: Arc::new(pixels),
                width: crate::platform::video::SCREEN_WIDTH as u32,
                height: crate::platform::video::SCREEN_HEIGHT as u32,
                revision: self.frame_revision,
                effect: &crate::platform::video::effects::PASSTHROUGH,
            });
        }
        self.stats = shared.stats.lock().unwrap().clone();
        self.end = shared.end.lock().unwrap().clone();
        if self.end.is_some() {
            self.menu_open = false;
        }
    }

    /// Logical playing state for the transport: paused-for-a-seek still
    /// reads as playing when the chase will resume.
    fn playing(&self) -> bool {
        let paused = self.runtime.shared.paused.load(Ordering::Relaxed);
        let resume_pending = self
            .runtime
            .playback
            .as_ref()
            .is_some_and(|h| h.seek.resume_pending());
        !paused || resume_pending
    }

    /// Begin or continue a scrub drag at `target`: freeze playback under
    /// the cursor and blit the nearest captured snapshot for instant,
    /// emulation-free feedback. The real (async) seek fires on commit.
    pub fn scrub_drag(&mut self, target: u32) {
        let Some(handles) = &self.runtime.playback else { return };
        let shared = &self.runtime.shared;
        let press = self.scrub_preview.is_none();
        if press {
            self.scrub_resume = self.playing();
            handles.seek.clear_resume();
            shared.paused.store(true, Ordering::Relaxed);
        }
        self.scrub_preview = Some(target);

        if let Some(snap) = handles.nearest_snapshot(target) {
            // Until the drag has blitted once, the live frame is still on
            // screen and beats a farther keyframe.
            let current = shared.position.load(Ordering::Relaxed);
            if self.scrub_blitted || snap.tick.abs_diff(target) <= current.abs_diff(target) {
                crate::session::playback::publish_snapshot(shared, &snap);
                self.scrub_blitted = true;
            }
        }
    }

    /// Release the drag: fire the async seek (resuming afterwards if
    /// playback was running when the drag started).
    pub fn scrub_commit(&mut self) {
        let Some(handles) = &self.runtime.playback else { return };
        if let Some(target) = self.scrub_preview.take() {
            let total = self.runtime.shared.total_ticks.load(Ordering::Relaxed);
            handles.seek.request(target.min(total), self.scrub_resume);
        }
        self.scrub_resume = false;
        self.scrub_blitted = false;
    }

    /// Toggle pause, respecting an in-flight seek's pending resume.
    pub fn toggle_pause(&mut self) {
        let shared = &self.runtime.shared;
        if self.playing() {
            if let Some(handles) = &self.runtime.playback {
                handles.seek.clear_resume();
            }
            shared.paused.store(true, Ordering::Relaxed);
        } else {
            shared.paused.store(false, Ordering::Relaxed);
        }
    }
}

fn player_choices(state: &State) -> Vec<PlayerChoice> {
    state
        .runtime
        .descriptor
        .nicks
        .iter()
        .enumerate()
        .map(|(idx, nick)| PlayerChoice {
            idx,
            label: format!("P{}: {}", idx + 1, nick),
        })
        .collect()
}

fn header(state: &State) -> Element<'_, Message> {
    let d = &state.runtime.descriptor;
    let content: Element<'_, Message> = match d.kind {
        SessionKind::Netplay => {
            let mut items = row![].spacing(16).align_y(iced::Alignment::Center);
            if let Some(code) = &d.room_code {
                items = items.push(text(format!("room {code}")).size(13));
            }
            for peer in &state.stats.peers {
                items = items.push(
                    text(format!(
                        "{}: {}",
                        peer.nick,
                        peer.rtt_ms.map(|ms| format!("{ms:.0}ms")).unwrap_or_else(|| "…".into())
                    ))
                    .size(13),
                );
            }
            items = items.push(
                text(format!(
                    "queue {} · rollback {} · {:.1}fps",
                    state.stats.queue_len, state.stats.rolled_back, state.stats.fps_target
                ))
                .size(13),
            );
            items.into()
        }
        SessionKind::Local => text("local session — Esc for menu").size(13).into(),
        SessionKind::Playback => {
            let name = d
                .replay_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            text(format!("{} — {}", name, d.nicks.join(" vs "))).size(13).into()
        }
    };
    container(content).padding([4, 8]).width(Length::Fill).into()
}

/// The transport play/pause button glyph for the session's logical state.
fn play_pause_icon<'a>(state: &State) -> iced::widget::Text<'a> {
    let glyph = if state.playing() {
        super::icons::Icon::Pause
    } else {
        super::icons::Icon::Play
    };
    super::icons::icon(glyph, 16.0)
}

fn transport(state: &State) -> Element<'_, Message> {
    let shared = &state.runtime.shared;
    let d = &state.runtime.descriptor;
    match d.kind {
        SessionKind::Playback => {
            let handles = state.runtime.playback.as_ref();
            let position = state
                .scrub_preview
                .or_else(|| handles.and_then(|h| h.seek.pending_target()))
                .unwrap_or_else(|| shared.position.load(Ordering::Relaxed));
            let total = shared.total_ticks.load(Ordering::Relaxed);
            let prefetched = handles
                .map(|h| h.prefetch_progress.load(Ordering::Relaxed))
                .unwrap_or(0);

            let bar = super::scrubber::Scrubber::new(
                position,
                total,
                prefetched,
                Message::SessionSeekChanged,
                |_| Message::SessionSeekCommitted,
            )
            .view();

            row![
                button(play_pause_icon(state)).padding([6, 10]).on_press(Message::SessionPauseToggled),
                pick_list(
                    SPEED_CHOICES.to_vec(),
                    Some(SpeedChoice(state.selected_speed)),
                    Message::SessionSpeedSelected
                ),
                bar,
                text(format!(
                    "{} / {}",
                    super::format_ticks(position),
                    super::format_ticks(total)
                ))
                .size(13),
                pick_list(
                    player_choices(state),
                    Some(current_player_choice(state)),
                    Message::SessionViewPlayerSelected
                ),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center)
            .padding([4, 8])
            .into()
        }
        SessionKind::Local => {
            let mut items = row![button(play_pause_icon(state))
                .padding([6, 10])
                .on_press(Message::SessionPauseToggled)]
            .spacing(8)
            .align_y(iced::Alignment::Center);
            if d.num_players > 1 {
                items = items
                    .push(pick_list(
                        player_choices(state),
                        Some(current_player_choice(state)),
                        Message::SessionViewPlayerSelected,
                    ))
                    .push(text("controls follow the selected player").size(12));
            }
            items = items.push(iced::widget::Space::new().width(Length::Fill));
            if link_capable(state) {
                items = items.push(link_toggle(state));
            }
            items.padding([4, 8]).into()
        }
        SessionKind::Netplay => row![
            text("Esc for menu").size(12),
            iced::widget::Space::new().width(Length::Fill),
            link_toggle(state),
        ]
        .align_y(iced::Alignment::Center)
        .padding([4, 8])
        .into(),
    }
}

/// Whether this session has a cable port the sidebar manages: solo local
/// sessions (a free port to plug into) and netplay sessions (a live
/// cable to unplug).
fn link_capable(state: &State) -> bool {
    match state.runtime.descriptor.kind {
        SessionKind::Local => state.runtime.descriptor.num_players == 1,
        SessionKind::Netplay => true,
        SessionKind::Playback => false,
    }
}

fn link_toggle(state: &State) -> Element<'_, Message> {
    button(text("Link").size(13))
        .padding([4, 10])
        .style(if state.link_open { button::primary } else { button::secondary })
        .on_press(Message::SessionLinkToggle)
        .into()
}

fn current_player_choice(state: &State) -> PlayerChoice {
    let idx = state
        .runtime
        .shared
        .view_player
        .load(Ordering::Relaxed)
        .min(state.runtime.descriptor.num_players - 1);
    PlayerChoice {
        idx,
        label: format!("P{}: {}", idx + 1, state.runtime.descriptor.nicks[idx]),
    }
}

fn framebuffer_view<'a>(state: &'a State, config: &'a Config) -> Element<'a, Message> {
    let integer_scaling = config.integer_scaling;
    let frame = state.frame.clone();
    iced::widget::responsive(move |size| {
        let img_w = crate::platform::video::SCREEN_WIDTH as f32;
        let img_h = crate::platform::video::SCREEN_HEIGHT as f32;
        let raw = (size.width / img_w).min(size.height / img_h);
        let scale = if integer_scaling { raw.floor().max(1.0) } else { raw.max(0.0) };
        let (w, h) = (img_w * scale, img_h * scale);

        let frame = frame.clone().unwrap_or_else(framebuffer::Frame::black);
        let fb = iced::widget::shader::Shader::new(framebuffer::Program::new(frame))
            .width(Length::Fixed(w))
            .height(Length::Fixed(h));

        container(fb)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::alignment::Horizontal::Center)
            .align_y(iced::alignment::Vertical::Center)
            .style(|_theme: &Theme| container::Style {
                background: Some(iced::Background::Color(iced::Color::BLACK)),
                ..Default::default()
            })
            .into()
    })
    .into()
}

fn overlay_panel<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    container(
        container(content)
            .padding(PADDING * 2.0)
            .style(|theme: &Theme| container::Style {
                background: Some(iced::Background::Color(theme.extended_palette().background.base.color)),
                border: iced::Border {
                    radius: 8.0.into(),
                    width: 1.0,
                    color: theme.extended_palette().background.strong.color,
                },
                ..Default::default()
            }),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(iced::alignment::Horizontal::Center)
    .align_y(iced::alignment::Vertical::Center)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6))),
        ..Default::default()
    })
    .into()
}

fn menu_overlay<'a>(state: &'a State, config: &'a Config) -> Element<'a, Message> {
    let mut items = column![text("Paused").size(20)].spacing(12).align_x(iced::Alignment::Center);
    if state.runtime.descriptor.kind == SessionKind::Netplay {
        items = items.push(
            column![
                text(format!("input delay: {} ticks", config.present_delay)).size(13),
                slider(0..=10u32, config.present_delay, Message::SessionPresentDelayChanged).width(Length::Fixed(220.0)),
            ]
            .spacing(4)
            .align_x(iced::Alignment::Center),
        );
    }
    items = items.push(
        column![
            text(format!("volume: {:.0}%", config.volume * 100.0)).size(13),
            slider(0.0..=1.0f32, config.volume, Message::SessionVolumeChanged)
                .step(0.01)
                .width(Length::Fixed(220.0)),
        ]
        .spacing(4)
        .align_x(iced::Alignment::Center),
    );
    items = items.push(
        row![
            button(text("Resume")).padding([8, 16]).on_press(Message::SessionToggleMenu),
            button(text("Quit session"))
                .padding([8, 16])
                .style(button::danger)
                .on_press(Message::SessionQuit),
        ]
        .spacing(10),
    );
    overlay_panel(items.into())
}

fn end_overlay(state: &State) -> Element<'_, Message> {
    let end = state.end.as_ref().expect("end overlay without end");
    let nick_of = |player: usize| {
        state
            .runtime
            .descriptor
            .nicks
            .get(player)
            .cloned()
            .unwrap_or_else(|| format!("player {}", player + 1))
    };
    let message = match end {
        SessionEnd::LocalQuit => "Session ended.".to_string(),
        SessionEnd::Unplugged => "Unplugged.".to_string(),
        SessionEnd::PeerQuit { player } => format!("{} left the session.", nick_of(*player)),
        SessionEnd::PeerDisconnected { player } => format!("Connection to {} lost.", nick_of(*player)),
        SessionEnd::Desync { tick } => format!("Desync detected at tick {tick} — session aborted."),
        SessionEnd::Error(e) => format!("Session error: {e}"),
    };
    overlay_panel(
        column![
            text(message).size(16),
            button(text("Back")).padding([8, 20]).on_press(Message::SessionDismissEnd),
        ]
        .spacing(16)
        .align_x(iced::Alignment::Center)
        .into(),
    )
}

/// A centered modal card with a title bar + close button over a dimmed
/// backdrop. Close routes through the Link toggle, which just hides the
/// modal — any running lobby keeps going behind it.
fn modal_card<'a>(title: String, body: Element<'a, Message>) -> Element<'a, Message> {
    let head = row![
        text(title).size(18).width(Length::Fill),
        button(super::icons::icon(super::icons::Icon::X, 18.0))
            .padding([4, 8])
            .style(button::text)
            .on_press(Message::SessionLinkToggle),
    ]
    .align_y(iced::Alignment::Center);

    container(
        container(column![head, body].spacing(16))
            .padding(PADDING * 2.0)
            .width(Length::Fixed(440.0))
            .style(|theme: &Theme| container::Style {
                background: Some(iced::Background::Color(theme.extended_palette().background.base.color)),
                border: iced::Border {
                    radius: 10.0.into(),
                    width: 1.0,
                    color: theme.extended_palette().background.strong.color,
                },
                ..Default::default()
            }),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .align_x(iced::alignment::Horizontal::Center)
    .align_y(iced::alignment::Vertical::Center)
    .style(|_theme: &Theme| container::Style {
        background: Some(iced::Background::Color(iced::Color::from_rgba(0.0, 0.0, 0.0, 0.6))),
        ..Default::default()
    })
    .into()
}

/// The link modal: the lobby while a room is up, the live roster + unplug
/// while the cable is in, and host/join otherwise.
fn link_modal<'a>(
    state: &'a State,
    lobby: Option<&'a super::lobby::State>,
    library: &'a Library,
) -> Element<'a, Message> {
    let d = &state.runtime.descriptor;
    let (title, body): (String, Element<'a, Message>) = if let Some(lobby) = lobby {
        let code = lobby.code.clone().unwrap_or_else(|| "…".to_string());
        (format!("Room {code}"), super::lobby::content(lobby, library))
    } else if d.kind == SessionKind::Netplay {
        let mut peers = column![].spacing(4);
        for peer in &state.stats.peers {
            peers = peers.push(
                text(format!(
                    "{} · {}",
                    peer.nick,
                    peer.rtt_ms.map(|ms| format!("{ms:.0} ms")).unwrap_or_else(|| "…".into())
                ))
                .size(14),
            );
        }
        let title = format!("Linked · room {}", d.room_code.clone().unwrap_or_default());
        let body = column![
            peers,
            button(text("Unplug")).padding([8, 16]).style(button::danger).on_press(Message::SessionUnplug),
            text("your game continues on its own after unplugging").size(12),
        ]
        .spacing(12)
        .into();
        (title, body)
    } else {
        let mut panel = column![].spacing(12);
        if let Some(notice) = &state.link_notice {
            panel = panel.push(text(notice.clone()).size(13).style(|theme: &Theme| text::Style {
                color: Some(theme.extended_palette().danger.base.color),
            }));
        }
        panel = panel.push(
            text("Host a room or join one — the cable plugs into the running game when the room starts.").size(13),
        );
        panel = panel.push(button(text("Host a room")).padding([8, 16]).on_press(Message::LinkHostClicked));
        panel = panel.push(
            row![
                text_input("room code", &state.link_code)
                    .on_input(Message::LinkCodeChanged)
                    .on_submit(Message::LinkJoinClicked)
                    .padding(8)
                    .width(Length::Fill),
                button(text("Join")).padding([8, 16]).on_press(Message::LinkJoinClicked),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        );
        ("Link cable".to_string(), panel.into())
    };

    modal_card(title, body)
}

pub fn view<'a>(
    state: &'a State,
    lobby: Option<&'a super::lobby::State>,
    library: &'a Library,
    config: &'a Config,
) -> Element<'a, Message> {
    let show_header = config.show_hud || state.runtime.descriptor.kind != SessionKind::Netplay;
    let mut body = column![];
    if show_header {
        body = body.push(header(state));
    }
    body = body.push(framebuffer_view(state, config));
    body = body.push(transport(state));

    let kind = state.runtime.descriptor.kind;
    let link_open = state.link_open;
    let captured = InputCapture::new(body, move |input| {
        if let Input::Keyboard(iced::keyboard::Event::KeyPressed { physical_key, .. }) = &input {
            if *physical_key == Physical::Code(Code::Escape) {
                // Escape backs out of the link modal first, else the menu.
                return Some(if link_open {
                    Message::SessionLinkToggle
                } else {
                    Message::SessionToggleMenu
                });
            }
            if kind == SessionKind::Playback && *physical_key == Physical::Code(Code::Space) {
                return Some(Message::SessionPauseToggled);
            }
        }
        input.to_event().map(Message::SessionInput)
    });

    let mut layers = stack![Element::from(captured)];
    if state.link_open && link_capable(state) && state.end.is_none() {
        layers = layers.push(link_modal(state, lobby, library));
    }
    if state.end.is_some() {
        layers = layers.push(end_overlay(state));
    } else if state.menu_open {
        layers = layers.push(menu_overlay(state, config));
    }
    layers.into()
}
