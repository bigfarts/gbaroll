//! Lobby state + the sidebar panel it renders as. The lobby lives inside
//! a running solo session's link sidebar: room code, player slots (with
//! per-player ROM identity and "do I have that ROM?" checks), ready
//! flags, chat, and the host's start button — the game keeps running
//! next to it until the cable plugs in.

use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input};
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
    pub chat: Vec<(String, String)>,
    pub chat_input: String,
    pub status: Option<String>,
    pub connecting: bool,
}

impl State {
    pub fn new(handle: LobbyHandle) -> State {
        State {
            handle,
            code: None,
            players: Vec::new(),
            my_idx: 0,
            my_ready: false,
            chat: Vec::new(),
            chat_input: String::new(),
            status: None,
            connecting: false,
        }
    }
}

/// The lobby's sidebar content (the surrounding panel chrome belongs to
/// the session view).
pub fn sidebar<'a>(state: &'a State, library: &'a Library) -> Element<'a, Message> {
    let header = row![
        column![
            text("Room").size(12),
            text(state.code.clone().unwrap_or_else(|| "…".to_string())).size(22),
        ],
        iced::widget::Space::new().width(Length::Fill),
        button(text("Leave").size(13))
            .style(button::danger)
            .on_press(Message::LobbyLeaveClicked),
    ]
    .align_y(iced::Alignment::Center);

    // Player slots: everyone's nick + ROM, and whether *we* have a copy
    // of their ROM (we can't ready up until we have them all).
    let mut have_all_roms = true;
    let mut slots = column![].spacing(4);
    for (i, player) in state.players.iter().enumerate() {
        let have_rom = library.by_crc32(player.rom_crc32).is_some();
        have_all_roms &= have_rom;
        let marker = if i == state.my_idx { " (you)" } else { "" };
        let rom_status = if have_rom {
            text("✓").size(11)
        } else {
            text("✗ missing ROM").size(11).style(|theme: &Theme| text::Style {
                color: Some(theme.extended_palette().danger.base.color),
            })
        };
        slots = slots.push(
            container(
                column![
                    row![
                        text(format!("P{} {}{marker}", i + 1, player.nick)).size(13).width(Length::Fill),
                        text(if player.ready { "ready" } else { "not ready" }).size(11),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                    row![text(player.rom_title.clone()).size(11).width(Length::Fill), rom_status].spacing(6),
                ]
                .spacing(2),
            )
            .padding(6)
            .width(Length::Fill)
            .style(move |theme: &Theme| container::Style {
                background: Some(iced::Background::Color(if player.ready {
                    theme.extended_palette().success.weak.color
                } else {
                    theme.extended_palette().background.weak.color
                })),
                border: iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
        );
    }
    for i in state.players.len()..gbaroll_signaling::MAX_PLAYERS {
        slots = slots.push(
            container(text(format!("P{} — open", i + 1)).size(11))
                .padding(6)
                .width(Length::Fill),
        );
    }

    // The host never readies up; everyone else flips the flag. The state
    // that used to ride the ready-up (the save image) now travels
    // peer-to-peer at start: your machine as it runs IS the commitment.
    let controls: Element<'_, Message> = if state.my_idx == 0 {
        let all_ready = state.players.len() >= 2 && state.players.iter().skip(1).all(|p| p.ready);
        let mut btn = button(text("Plug in").size(13)).padding(8);
        if all_ready && have_all_roms {
            btn = btn.on_press(Message::LobbyStartClicked);
        }
        column![
            btn,
            text(if all_ready {
                "".to_string()
            } else {
                "waiting for everyone to ready up".to_string()
            })
            .size(11),
        ]
        .spacing(4)
        .into()
    } else {
        let mut ready_box = checkbox(state.my_ready);
        if have_all_roms || state.my_ready {
            ready_box = ready_box.on_toggle(Message::LobbyReadyToggled);
        }
        column![
            row![ready_box, text("ready").size(13)].spacing(6).align_y(iced::Alignment::Center),
            text("the host plugs in once everyone is ready").size(11),
        ]
        .spacing(4)
        .into()
    };

    let chat_lines: Vec<Element<'_, Message>> = state
        .chat
        .iter()
        .map(|(nick, line)| text(format!("{nick}: {line}")).size(12).into())
        .collect();
    let chat = column![
        scrollable(column(chat_lines).spacing(2))
            .height(Length::Fill)
            .anchor_bottom(),
        text_input("say something…", &state.chat_input)
            .on_input(Message::LobbyChatChanged)
            .on_submit(Message::LobbyChatSubmitted)
            .padding(6)
            .size(13),
    ]
    .spacing(6)
    .height(Length::Fill);

    let mut panel = column![header, slots].spacing(10);
    if !have_all_roms {
        panel = panel.push(text("You're missing a copy of someone's ROM — add it to your library first.").size(11));
    }
    panel = panel.push(controls);
    if let Some(status) = &state.status {
        panel = panel.push(text(status.clone()).size(11));
    }
    panel.push(chat).height(Length::Fill).into()
}
