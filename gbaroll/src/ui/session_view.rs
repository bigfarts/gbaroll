//! The fullscreen session view: the framebuffer shader widget (integer
//! or fit scaling), the replay transport with async scrubbing, input capture, the unified
//! link panel, and the Esc menu / end-of-session overlays.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use iced::keyboard::key::{Code, Physical};
use iced::widget::{button, column, container, pick_list, row, slider, stack, text, text_input};
use iced::{Element, Length, Theme};

use super::{Message, PlayerChoice, SpeedChoice, PADDING, SPEED_CHOICES};
use crate::config::Config;
use crate::library::Library;
use crate::platform::input::{self, HeldState, MappedKey};
use crate::platform::input_capture::{Input, InputCapture};
use crate::platform::video::framebuffer;
use crate::session::{SessionEnd, SessionKind, SessionRuntime, Stats};

pub struct State {
    pub runtime: SessionRuntime,
    pub held: HeldState,
    pub menu_open: bool,
    pub selected_speed: u32,
    pub speed_up_held: bool,

    // The unified link / lobby / telemetry panel.
    pub panel_open: bool,
    pub link_code: String,
    pub copied_link_code: Option<String>,
    /// Why the cable last unplugged, shown quietly in the panel.
    pub link_notice: Option<String>,

    metric_history: std::collections::VecDeque<super::telemetry::MetricSample>,

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
            panel_open: false,
            link_code: String::new(),
            copied_link_code: None,
            link_notice: None,
            metric_history: std::collections::VecDeque::with_capacity(
                super::telemetry::HISTORY_LEN,
            ),
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
        self.copied_link_code = None;
        self.metric_history.clear();
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
        // Feed the telemetry ring buffer (netplay only — the other kinds
        // never produce link stats).
        if self.runtime.descriptor.kind == SessionKind::Netplay {
            if self.metric_history.len() == super::telemetry::HISTORY_LEN {
                self.metric_history.pop_front();
            }
            self.metric_history
                .push_back(super::telemetry::MetricSample::capture(&self.stats));
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
        let Some(handles) = &self.runtime.playback else {
            return;
        };
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
        let Some(handles) = &self.runtime.playback else {
            return;
        };
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
            shared.resume();
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

/// The transport play/pause button glyph for the session's logical state.
fn play_pause_icon<'a>(state: &State) -> iced::widget::Text<'a> {
    let glyph = if state.playing() {
        super::icons::Icon::Pause
    } else {
        super::icons::Icon::Play
    };
    super::icons::icon(glyph, 16.0)
}

fn transport(state: &State) -> Option<Element<'_, Message>> {
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

            Some(row![
                button(play_pause_icon(state))
                    .padding([6, 10])
                    .on_press(Message::SessionPauseToggled),
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
            .into())
        }
        SessionKind::Local | SessionKind::Netplay => None,
    }
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
        let scale = if integer_scaling {
            raw.floor().max(1.0)
        } else {
            raw.max(0.0)
        };
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
                background: Some(iced::Background::Color(
                    theme.extended_palette().background.base.color,
                )),
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
        background: Some(iced::Background::Color(iced::Color::from_rgba(
            0.0, 0.0, 0.0, 0.6,
        ))),
        ..Default::default()
    })
    .into()
}

/// The keyboard shortcuts that apply to this session kind, joined into
/// one hint line for the menu overlay.
fn shortcut_hints(kind: SessionKind, config: &Config) -> String {
    let mut hints = vec!["Esc — menu".to_string()];
    match kind {
        SessionKind::Playback => hints.push("Space — play/pause".to_string()),
        SessionKind::Local => {
            if let Some(physical) = config.mapping.slot(MappedKey::SpeedUp).first() {
                let (_, label) = input::describe(physical);
                hints.push(format!("hold {label} — fast-forward"));
            }
        }
        SessionKind::Netplay => {}
    }
    hints.join("   ·   ")
}

fn menu_overlay<'a>(
    state: &'a State,
    library: &'a Library,
    config: &'a Config,
) -> Element<'a, Message> {
    let d = &state.runtime.descriptor;
    // Playback carries no single ROM identity; the matchup names the
    // replay better than any one cartridge would.
    let title = d
        .rom_crc32
        .and_then(|crc| library.by_crc32(crc))
        .map(|rom| rom.display_name().to_string())
        .unwrap_or_else(|| {
            if d.kind == SessionKind::Playback && !d.nicks.is_empty() {
                d.nicks.join(" vs ")
            } else {
                "Session".to_string()
            }
        });
    let (caption, quit_label) = match d.kind {
        SessionKind::Local => ("Playing solo", "Quit game"),
        SessionKind::Netplay => ("Netplay", "Quit game"),
        SessionKind::Playback => ("Watching a replay", "Stop watching"),
    };

    let header = column![
        text(title).size(20),
        text(caption).size(12).style(|theme: &Theme| text::Style {
            color: Some(theme.extended_palette().background.strong.color),
        }),
    ]
    .spacing(4)
    .align_x(iced::Alignment::Center);

    let mut items = column![header]
        .spacing(16)
        .align_x(iced::Alignment::Center);
    items = items.push(
        column![
            text(format!("Volume · {:.0}%", config.volume * 100.0)).size(13),
            slider(0.0..=1.0f32, config.volume, Message::SessionVolumeChanged)
                .step(0.01)
                .width(Length::Fixed(220.0)),
        ]
        .spacing(4)
        .align_x(iced::Alignment::Center),
    );
    items = items.push(
        row![
            button(text("Back to game"))
                .padding([8, 16])
                .on_press(Message::SessionToggleMenu),
            button(text(quit_label))
                .padding([8, 16])
                .style(button::danger)
                .on_press(Message::SessionQuit),
        ]
        .spacing(10),
    );
    items = items.push(
        text(shortcut_hints(d.kind, config))
            .size(11)
            .style(|theme: &Theme| text::Style {
                color: Some(theme.extended_palette().background.strong.color),
            }),
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
        SessionEnd::PeerDisconnected { player } => {
            format!("Connection to {} lost.", nick_of(*player))
        }
        SessionEnd::Desync { tick } => format!("Desync detected at tick {tick} — session aborted."),
        SessionEnd::Error(e) => format!("Session error: {e}"),
    };
    overlay_panel(
        column![
            text(message).size(16),
            button(text("Back"))
                .padding([8, 20])
                .on_press(Message::SessionDismissEnd),
        ]
        .spacing(16)
        .align_x(iced::Alignment::Center)
        .into(),
    )
}

/// Room setup shown inside the unified panel while the cable is offline.
fn link_setup(state: &State) -> Element<'_, Message> {
    let mut panel = column![].spacing(12);
    if let Some(notice) = &state.link_notice {
        panel = panel.push(
            container(
                row![
                    super::icons::icon(super::icons::Icon::WifiOff, 15.0),
                    text(notice.clone()).size(12),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .padding(9)
            .width(Length::Fill)
            .style(|theme: &Theme| {
                let pair = theme.extended_palette().danger.weak;
                container::Style {
                    background: Some(iced::Background::Color(pair.color)),
                    text_color: Some(pair.text),
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }),
        );
    }
    // Two explicit paths — host a new room, or join a friend's — instead
    // of one input whose meaning depends on being left blank.
    panel = panel.push(
        column![
            button(
                row![
                    super::icons::icon(super::icons::Icon::Users, 15.0),
                    text("Create a room"),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .padding([9, 14])
            .width(Length::Fill)
            .style(button::primary)
            .on_press(Message::LinkCreateClicked),
            text("You'll get a code to share with the other players.").size(11).style(
                |theme: &Theme| text::Style {
                    color: Some(theme.extended_palette().background.strong.color),
                },
            ),
        ]
        .spacing(5),
    );

    let can_join = state.link_code.len() == gbaroll_signaling::ROOM_CODE_LEN;
    let mut code_input = text_input("6-character code", &state.link_code)
        .on_input(Message::LinkCodeChanged)
        .padding(9)
        .width(Length::Fill);
    if can_join {
        code_input = code_input.on_submit(Message::LinkJoinClicked);
    }
    let mut join = button(
        row![
            super::icons::icon(super::icons::Icon::Cable, 15.0),
            text("Join"),
        ]
        .spacing(7)
        .align_y(iced::Alignment::Center),
    )
    .padding([9, 14])
    .style(button::primary);
    if can_join {
        join = join.on_press(Message::LinkJoinClicked);
    }
    panel = panel.push(
        column![
            text("Or join a friend's room:").size(13),
            row![code_input, join].spacing(8).align_y(iced::Alignment::Center),
        ]
        .spacing(6),
    );
    panel.into()
}

pub fn view<'a>(
    state: &'a State,
    lobby: Option<&'a super::lobby::State>,
    library: &'a Library,
    config: &'a Config,
) -> Element<'a, Message> {
    let kind = state.runtime.descriptor.kind;
    let mut body = column![framebuffer_view(state, config)];
    if let Some(transport) = transport(state) {
        body = body.push(transport);
    }

    let panel_open = state.panel_open;
    let menu_open = state.menu_open;
    let captured = InputCapture::new(body, move |input| {
        if let Input::Keyboard(iced::keyboard::Event::KeyPressed { physical_key, .. }) = &input {
            if *physical_key == Physical::Code(Code::Escape) {
                // Escape collapses the session panel before opening the
                // menu, keeping a running lobby alive in the background.
                return Some(if menu_open {
                    Message::SessionToggleMenu
                } else if panel_open {
                    Message::SessionTogglePanel
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
    // A visible way into the session menu (Esc stays the shortcut),
    // mirroring the cable chip in the opposite corner.
    if state.end.is_none() && !state.menu_open {
        layers = layers.push(
            container(
                button(
                    row![
                        super::icons::icon(super::icons::Icon::Menu, 15.0),
                        text("Menu").size(13),
                        text("esc").size(11).style(|theme: &Theme| text::Style {
                            color: Some(theme.extended_palette().background.strong.color),
                        }),
                    ]
                    .spacing(7)
                    .align_y(iced::Alignment::Center),
                )
                .padding([6, 10])
                .style(button::secondary)
                .on_press(Message::SessionToggleMenu),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(iced::alignment::Horizontal::Left)
            .align_y(iced::alignment::Vertical::Top)
            .padding(12),
        );
    }
    // One control owns the complete cable flow: create/join, lobby, and
    // connected telemetry. Playback has no local cable control.
    if kind != SessionKind::Playback && state.end.is_none() && !state.menu_open {
        let panel = if let Some(lobby) = lobby {
            let label = lobby
                .code
                .as_ref()
                .map(|_| "Netplay lobby".to_string())
                .unwrap_or_else(|| "Connecting…".to_string());
            super::telemetry::Panel::Link {
                icon: super::icons::Icon::Users,
                title: "Netplay lobby".to_string(),
                label,
                body: super::lobby::content(lobby, library),
                room_code: lobby.code.as_deref(),
                code_copied: lobby
                    .code
                    .as_deref()
                    .is_some_and(|code| state.copied_link_code.as_deref() == Some(code)),
            }
        } else if kind == SessionKind::Netplay {
            super::telemetry::Panel::Connected {
                history: &state.metric_history,
                latest: &state.stats,
                present_delay: config.present_delay,
                room_code: state.runtime.descriptor.room_code.as_deref(),
                code_copied: state
                    .runtime
                    .descriptor
                    .room_code
                    .as_deref()
                    .is_some_and(|code| state.copied_link_code.as_deref() == Some(code)),
            }
        } else {
            super::telemetry::Panel::Link {
                icon: if state.link_notice.is_some() {
                    super::icons::Icon::WifiOff
                } else {
                    super::icons::Icon::Cable
                },
                label: "Link cable".to_string(),
                title: "Link cable".to_string(),
                body: link_setup(state),
                room_code: None,
                code_copied: false,
            }
        };
        layers = layers.push(super::telemetry::overlay(panel, state.panel_open));
    }
    if state.end.is_some() {
        layers = layers.push(end_overlay(state));
    } else if state.menu_open {
        layers = layers.push(menu_overlay(state, library, config));
    }
    layers.into()
}
