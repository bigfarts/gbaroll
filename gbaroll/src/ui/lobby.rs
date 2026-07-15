//! The lobby: room code, player slots (with per-player ROM identity and
//! "do I have that ROM?" checks), save commitment behind the ready flag,
//! chat, and the host's start button.

use iced::widget::{button, checkbox, column, container, pick_list, row, scrollable, text, text_input};
use iced::{Element, Length, Theme};

use super::{Message, SaveChoice, PADDING};
use crate::library::Library;
use crate::net::lobby::LobbyHandle;

pub struct State {
    pub handle: LobbyHandle,
    pub code: Option<String>,
    pub players: Vec<gbaroll_signaling::PlayerInfo>,
    pub my_idx: usize,
    pub my_ready: bool,
    pub save_choice: SaveChoice,
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
            save_choice: SaveChoice::Fresh,
            chat: Vec::new(),
            chat_input: String::new(),
            status: None,
            connecting: false,
        }
    }
}

pub fn view<'a>(state: &'a State, library: &'a Library, saves_dir: &std::path::Path) -> Element<'a, Message> {
    if state.connecting {
        return container(
            column![
                text("Connecting to peers…").size(22),
                text(state.status.clone().unwrap_or_default()).size(14),
            ]
            .spacing(8)
            .align_x(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .align_y(iced::alignment::Vertical::Center)
        .into();
    }

    let header = row![
        column![
            text("Room").size(13),
            text(state.code.clone().unwrap_or_else(|| "…".to_string())).size(30),
        ],
        iced::widget::Space::new().width(Length::Fill),
        button(text("Leave")).style(button::danger).on_press(Message::LobbyLeaveClicked),
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
            text("✓ have this ROM").size(12)
        } else {
            text("✗ missing this ROM").size(12).style(|theme: &Theme| text::Style {
                color: Some(theme.extended_palette().danger.base.color),
            })
        };
        slots = slots.push(
            container(
                row![
                    text(format!("P{}", i + 1)).size(16).width(Length::Fixed(36.0)),
                    column![
                        text(format!("{}{marker}", player.nick)),
                        row![
                            text(player.rom_title.clone()).size(12),
                            text(format!("({:08x})", player.rom_crc32)).size(11),
                            rom_status,
                        ]
                        .spacing(6),
                    ]
                    .width(Length::Fill),
                    text(if player.ready { "ready" } else { "not ready" }).size(13),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
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
            container(text(format!("P{} — open", i + 1)).size(13))
                .padding(6)
                .width(Length::Fill),
        );
    }

    let mut saves: Vec<SaveChoice> = vec![SaveChoice::Fresh];
    saves.extend(crate::library::list_saves(saves_dir).into_iter().map(SaveChoice::File));

    let mut ready_box = checkbox(state.my_ready);
    if have_all_roms || state.my_ready {
        ready_box = ready_box.on_toggle(Message::LobbyReadyToggled);
    }
    let ready_row = row![
        text("my save:"),
        pick_list(saves, Some(state.save_choice.clone()), Message::LobbySaveSelected),
        ready_box,
        text("ready"),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);

    let start_row: Element<'_, Message> = if state.my_idx == 0 {
        let all_ready = state.players.len() >= 2 && state.players.iter().all(|p| p.ready);
        let mut btn = button(text("Start session")).padding(8);
        if all_ready {
            btn = btn.on_press(Message::LobbyStartClicked);
        }
        row![
            btn,
            text(if all_ready {
                "".to_string()
            } else {
                "waiting for 2+ players, all ready".to_string()
            })
            .size(12),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .into()
    } else {
        text("waiting for the host to start…").size(13).into()
    };

    let chat_lines: Vec<Element<'_, Message>> = state
        .chat
        .iter()
        .map(|(nick, line)| text(format!("{nick}: {line}")).size(13).into())
        .collect();
    let chat = column![
        scrollable(column(chat_lines).spacing(2))
            .height(Length::Fill)
            .anchor_bottom(),
        text_input("say something…", &state.chat_input)
            .on_input(Message::LobbyChatChanged)
            .on_submit(Message::LobbyChatSubmitted)
            .padding(6),
    ]
    .spacing(6)
    .width(Length::FillPortion(2))
    .height(Length::Fill);

    let left = column![
        header,
        slots,
        if !have_all_roms {
            Element::from(
                text("You're missing a copy of someone's ROM — add it to your library and rescan to ready up.")
                    .size(12),
            )
        } else {
            Element::from(iced::widget::Space::new())
        },
        ready_row,
        start_row,
        text(state.status.clone().unwrap_or_default()).size(12),
    ]
    .spacing(10)
    .width(Length::FillPortion(3));

    row![left, chat].spacing(PADDING * 2.0).height(Length::Fill).into()
}
