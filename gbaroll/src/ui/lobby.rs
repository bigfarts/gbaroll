//! Lobby state + the body it renders inside the link modal: player slots
//! (with per-player ROM identity and "do I have that ROM?" checks), ready
//! flags, and the host's start button. The game keeps running behind the
//! modal until the cable plugs in.

use iced::widget::{button, checkbox, column, container, row, text};
use iced::{Element, Length, Theme};

use super::Message;
use crate::library::Library;
use crate::net::lobby::LobbyHandle;

pub struct State {
    pub handle: LobbyHandle,
    pub code: Option<String>,
    pub players: Vec<gbaroll_signaling::PlayerInfo>,
    pub my_idx: usize,
    pub my_ready: bool,
    /// Progress line for the connecting/exchange phase, shown in the modal.
    pub status: Option<String>,
}

impl State {
    pub fn new(handle: LobbyHandle) -> State {
        State {
            handle,
            code: None,
            players: Vec::new(),
            my_idx: 0,
            my_ready: false,
            status: None,
        }
    }
}

/// The lobby body for the link modal (the modal supplies the title bar +
/// close button around this).
pub fn content<'a>(state: &'a State, library: &'a Library) -> Element<'a, Message> {
    // Player slots: everyone's nick + ROM, and whether *we* have a copy
    // of their ROM (we can't ready up until we have them all).
    let mut have_all_roms = true;
    let mut slots = column![].spacing(6);
    for (i, player) in state.players.iter().enumerate() {
        let have_rom = library.by_crc32(player.rom_crc32).is_some();
        have_all_roms &= have_rom;
        let ready = i == 0 || player.ready;
        let marker = if i == state.my_idx { " (you)" } else { "" };
        let rom_status: Element<'_, Message> = if have_rom {
            text("").into()
        } else {
            text("missing ROM")
                .size(12)
                .style(|theme: &Theme| text::Style {
                    color: Some(theme.extended_palette().danger.base.color),
                })
                .into()
        };
        slots = slots.push(
            container(
                column![
                    row![
                        text(format!("P{} · {}{marker}", i + 1, player.nick)).width(Length::Fill),
                        text(if ready { "ready" } else { "not ready" }).size(13),
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                    row![text(player.rom_title.clone()).size(12).width(Length::Fill), rom_status]
                        .spacing(8)
                        .align_y(iced::Alignment::Center),
                ]
                .spacing(3),
            )
            .padding([8, 10])
            .width(Length::Fill)
            // Set the text colour to the palette's paired text for each
            // background, so ready rows aren't unreadable light-on-green.
            .style(move |theme: &Theme| {
                let pair = if ready {
                    theme.extended_palette().success.weak
                } else {
                    theme.extended_palette().background.weak
                };
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
    for i in state.players.len()..gbaroll_signaling::MAX_PLAYERS {
        slots = slots.push(
            container(text(format!("P{} — open", i + 1)).size(13))
                .padding([8, 10])
                .width(Length::Fill),
        );
    }

    // The host never readies up; everyone else flips the flag. The state
    // that used to ride the ready-up (the save image) now travels
    // peer-to-peer at start: your machine as it runs IS the commitment.
    let controls: Element<'_, Message> = if state.my_idx == 0 {
        let all_ready = state.players.len() >= 2 && state.players.iter().skip(1).all(|p| p.ready);
        let mut plug = button(text("Plug in")).padding([8, 16]);
        if all_ready && have_all_roms {
            plug = plug.on_press(Message::LobbyStartClicked);
        }
        let hint = if all_ready {
            "everyone's ready — plug the cable in".to_string()
        } else {
            "waiting for everyone to ready up…".to_string()
        };
        column![plug, text(hint).size(12)].spacing(6).into()
    } else {
        let mut ready_box = checkbox(state.my_ready);
        if have_all_roms || state.my_ready {
            ready_box = ready_box.on_toggle(Message::LobbyReadyToggled);
        }
        column![
            row![ready_box, text("Ready").size(15)]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            text("the host plugs in once everyone is ready").size(12),
        ]
        .spacing(6)
        .into()
    };

    let mut panel = column![].spacing(14);
    panel = panel.push(slots);
    if !have_all_roms {
        panel = panel.push(
            text("You're missing a copy of someone's ROM — add it to your library first.")
                .size(12)
                .style(|theme: &Theme| text::Style {
                    color: Some(theme.extended_palette().danger.base.color),
                }),
        );
    }
    panel = panel.push(controls);
    if let Some(status) = &state.status {
        panel = panel.push(text(status.clone()).size(12));
    }
    panel = panel.push(
        button(text("Leave room"))
            .padding([6, 14])
            .style(button::danger)
            .on_press(Message::LobbyLeaveClicked),
    );
    panel.into()
}
