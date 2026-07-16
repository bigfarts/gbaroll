//! Lobby state + the body it renders inside the cable panel: player slots
//! (with per-player ROM identity and "do I have that ROM?" checks), ready
//! flags, and the host's start button. The game keeps running behind the
//! panel until the cable plugs in.

use iced::widget::{button, checkbox, column, container, row, text};
use iced::{Element, Length, Theme};

use super::Message;
use crate::library::Library;
use crate::net::lobby::LobbyHandle;

/// Fixed height for every roster slot (filled or open), so someone
/// joining never shifts the layout.
const SLOT_HEIGHT: f32 = 52.0;

pub struct State {
    pub handle: LobbyHandle,
    pub code: Option<String>,
    pub players: Vec<gbaroll_signaling::PlayerInfo>,
    pub my_idx: usize,
    pub my_ready: bool,
    /// Progress line for the connecting/exchange phase, shown in the panel.
    pub status: Option<String>,
    /// The room has started and is building the peer mesh. Lobby controls
    /// stay hidden during this one-way transition.
    pub starting: bool,
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
            starting: false,
        }
    }
}

fn display_rom_title<'a>(player: &'a gbaroll_signaling::PlayerInfo, library: &'a Library) -> &'a str {
    library
        .by_crc32(player.rom_crc32)
        .map(|rom| rom.display_name())
        .unwrap_or(player.rom_title.as_str())
}

/// The lobby body for the unified cable panel.
pub fn content<'a>(state: &'a State, library: &'a Library) -> Element<'a, Message> {
    if state.code.is_none() {
        return column![
            container(
                row![
                    super::icons::icon(super::icons::Icon::LoaderCircle, 16.0),
                    text(
                        state
                            .status
                            .as_deref()
                            .unwrap_or("Connecting to the lobby…")
                    )
                    .size(13),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .padding(10)
            .width(Length::Fill)
            .style(|theme: &Theme| {
                let pair = theme.extended_palette().primary.weak;
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
            text("Your game will keep running while the room connects.").size(12),
            button(
                row![
                    super::icons::icon(super::icons::Icon::LogOut, 14.0),
                    text("Cancel"),
                ]
                .spacing(7)
                .align_y(iced::Alignment::Center),
            )
            .padding([6, 12])
            .style(button::secondary)
            .on_press(Message::LobbyLeaveClicked),
        ]
        .spacing(12)
        .into();
    }

    // Player slots: everyone's nick + ROM, and whether *we* have a copy
    // of their ROM (we can't ready up until we have them all).
    let mut have_all_roms = true;
    let mut slots = column![].spacing(6);
    for (i, player) in state.players.iter().enumerate() {
        let local_rom = library.by_crc32(player.rom_crc32);
        let have_rom = local_rom.is_some();
        let rom_title = display_rom_title(player, library);
        have_all_roms &= have_rom;
        let ready = i == 0 || player.ready;
        let marker = if i == state.my_idx { " · You" } else { "" };
        let status = if i == 0 {
            "Host"
        } else if ready {
            "Ready"
        } else {
            "Waiting"
        };
        let status_badge =
            container(text(status).size(11))
                .padding([3, 8])
                .style(move |theme: &Theme| {
                    let pair = if ready {
                        theme.extended_palette().success.weak
                    } else {
                        theme.extended_palette().background.strong
                    };
                    container::Style {
                        background: Some(iced::Background::Color(pair.color)),
                        text_color: Some(pair.text),
                        border: iced::Border {
                            radius: 99.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }
                });
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
                        status_badge,
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                    row![
                        text(rom_title).size(12).width(Length::Fill),
                        rom_status
                    ]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
                ]
                .spacing(3),
            )
            .padding([0, 10])
            .width(Length::Fill)
            .height(Length::Fixed(SLOT_HEIGHT))
            .align_y(iced::alignment::Vertical::Center)
            // Set the text colour to the palette's paired text for each
            // background, so ready rows aren't unreadable light-on-green.
            .style(move |theme: &Theme| {
                let pair = theme.extended_palette().background.weak;
                container::Style {
                    background: Some(iced::Background::Color(pair.color)),
                    text_color: Some(pair.text),
                    border: iced::Border {
                        radius: 6.0.into(),
                        width: 1.0,
                        color: theme.extended_palette().background.strong.color,
                    },
                    ..Default::default()
                }
            }),
        );
    }
    // Open seats are the same fixed height as filled ones, so a join never
    // reflows the roster.
    for i in state.players.len()..gbaroll_signaling::MAX_PLAYERS {
        slots = slots.push(
            container(
                row![
                    text(format!("P{}", i + 1)).size(13),
                    iced::widget::Space::new().width(Length::Fill),
                    text("Open seat").size(12),
                ]
                .align_y(iced::Alignment::Center),
            )
            .padding([0, 10])
            .width(Length::Fill)
            .height(Length::Fixed(SLOT_HEIGHT))
            .align_y(iced::alignment::Vertical::Center)
            .style(|theme: &Theme| container::Style {
                border: iced::Border {
                    radius: 6.0.into(),
                    width: 1.0,
                    color: theme.extended_palette().background.weak.color,
                },
                ..Default::default()
            }),
        );
    }

    // The host never readies up; everyone else flips the flag. The state
    // that used to ride the ready-up (the save image) now travels
    // peer-to-peer at start: your machine as it runs IS the commitment.
    let controls: Element<'_, Message> = if state.starting {
        container(
            row![
                super::icons::icon(super::icons::Icon::LoaderCircle, 16.0),
                text(state.status.as_deref().unwrap_or("Connecting players…")).size(13),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .padding(10)
        .width(Length::Fill)
        .style(|theme: &Theme| {
            let pair = theme.extended_palette().primary.weak;
            container::Style {
                background: Some(iced::Background::Color(pair.color)),
                text_color: Some(pair.text),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        })
        .into()
    } else if state.my_idx == 0 {
        let all_ready = state.players.len() >= 2 && state.players.iter().skip(1).all(|p| p.ready);
        let waiting = state.players.iter().skip(1).filter(|p| !p.ready).count();
        let mut plug = button(
            row![
                super::icons::icon(super::icons::Icon::Cable, 16.0),
                text("Start netplay"),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
        )
        .padding([9, 16])
        .width(Length::Fill)
        .style(button::primary);
        if all_ready && have_all_roms {
            plug = plug.on_press(Message::LobbyStartClicked);
        }
        let hint = if state.players.len() < 2 {
            "Waiting for someone to join.".to_string()
        } else if !have_all_roms {
            "Add the missing ROM before starting.".to_string()
        } else if waiting > 0 {
            format!(
                "Waiting for {waiting} {} to get ready.",
                if waiting == 1 { "player" } else { "players" }
            )
        } else {
            "Everyone is ready. This will plug in the virtual cable.".to_string()
        };
        column![plug, text(hint).size(12)].spacing(6).into()
    } else {
        let mut ready_box = checkbox(state.my_ready);
        if have_all_roms || state.my_ready {
            ready_box = ready_box.on_toggle(Message::LobbyReadyToggled);
        }
        column![
            row![ready_box, text("Ready to play").size(15)]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            text(if have_all_roms {
                "The host can start once every player is ready."
            } else {
                "Add the missing ROM before marking yourself ready."
            })
            .size(12),
        ]
        .spacing(6)
        .into()
    };

    let mut panel = column![].spacing(14);
    panel = panel.push(slots);
    if !have_all_roms {
        panel = panel.push(
            container(
                row![
                    super::icons::icon(super::icons::Icon::CircleAlert, 15.0),
                    text("A required ROM is missing from your library.").size(12),
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
    panel = panel.push(controls);
    if !state.starting {
        if let Some(status) = &state.status {
            panel = panel.push(text(status.clone()).size(12));
        }
    }
    panel = panel.push(row![
        iced::widget::Space::new().width(Length::Fill),
        button(
            row![
                super::icons::icon(super::icons::Icon::LogOut, 14.0),
                text("Leave room"),
            ]
            .spacing(7)
            .align_y(iced::Alignment::Center),
        )
        .padding([6, 12])
        .style(button::secondary)
        .on_press(Message::LobbyLeaveClicked),
    ]);
    panel.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::RomInfo;

    #[test]
    fn resolves_rom_name_by_crc_with_raw_title_fallback() {
        let player = gbaroll_signaling::PlayerInfo {
            nick: "peer".to_string(),
            ready: false,
            rom_crc32: 0x1234_5678,
            rom_title: "RAW HEADER".to_string(),
        };
        let library = Library {
            roms: vec![RomInfo {
                path: std::path::PathBuf::from("game.gba"),
                title: "RAW HEADER".to_string(),
                code: "ABCE".to_string(),
                crc32: player.rom_crc32,
                size: 1,
                dat_name: Some("Canonical Game (USA)".to_string()),
            }],
        };

        assert_eq!(display_rom_title(&player, &library), "Canonical Game (USA)");
        assert_eq!(display_rom_title(&player, &Library::default()), "RAW HEADER");
    }
}
