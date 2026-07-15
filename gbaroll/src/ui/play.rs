//! The Play tab: ROM library on the left, the session launcher on the
//! right. Netplay isn't launched from here — you launch the game, then
//! plug the cable in from the session's link sidebar.

use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input};
use iced::{Element, Length, Theme};

use super::{App, Message, SaveChoice, PADDING};

pub struct State {
    pub search: String,
    pub selected_crc: Option<u32>,
    pub local_players: usize,
    pub local_save: SaveChoice,
}

impl Default for State {
    fn default() -> Self {
        State {
            search: String::new(),
            selected_crc: None,
            local_players: 1,
            local_save: SaveChoice::Fresh,
        }
    }
}

/// A left-aligned form field with a fixed-width label.
fn field<'a>(label: &'static str, control: Element<'a, Message>) -> Element<'a, Message> {
    row![text(label).width(Length::Fixed(64.0)).size(14), control]
        .spacing(10)
        .align_y(iced::Alignment::Center)
        .into()
}

fn rom_row<'a>(rom: &crate::library::RomInfo, selected: bool) -> Element<'a, Message> {
    let label = row![
        text(rom.display_name().to_string()).width(Length::FillPortion(5)),
        text(rom.code.clone()).width(Length::FillPortion(1)).size(12),
        text(format!("{:08x}", rom.crc32)).width(Length::FillPortion(1)).size(12),
        text(format!("{} KiB", rom.size / 1024))
            .width(Length::FillPortion(1))
            .size(12),
    ]
    .spacing(8)
    .align_y(iced::Alignment::Center);
    button(label)
        .width(Length::Fill)
        .style(if selected { button::primary } else { button::text })
        .on_press(Message::PlayRomSelected(rom.crc32))
        .into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let state = &app.play;

    let needle = state.search.to_ascii_lowercase();
    let roms: Vec<Element<'_, Message>> = app
        .library
        .roms
        .iter()
        .filter(|r| {
            needle.is_empty()
                || r.display_name().to_ascii_lowercase().contains(&needle)
                || r.title.to_ascii_lowercase().contains(&needle)
                || r.code.to_ascii_lowercase().contains(&needle)
        })
        .map(|r| rom_row(r, state.selected_crc == Some(r.crc32)))
        .collect();

    let library_pane = column![
        row![
            text_input("search…", &state.search)
                .on_input(Message::PlaySearchChanged)
                .padding(8)
                .width(Length::Fill),
            button(text("rescan")).padding([8, 14]).on_press(Message::RescanRoms),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
        if roms.is_empty() {
            Element::from(
                container(
                    text(format!(
                        "no ROMs found — drop .gba files into\n{}",
                        app.config.roms_dir.display()
                    ))
                    .size(14),
                )
                .padding(PADDING * 2.0),
            )
        } else {
            Element::from(scrollable(column(roms).spacing(3)).height(Length::Fill))
        },
    ]
    .spacing(10)
    .width(Length::FillPortion(3));

    let selected = app.play.selected_crc.and_then(|crc| app.library.by_crc32(crc));

    let mut saves: Vec<SaveChoice> = vec![SaveChoice::Fresh];
    saves.extend(crate::library::list_saves(&app.config.saves_dir).into_iter().map(SaveChoice::File));

    let launcher: Element<'_, Message> = if let Some(rom) = selected {
        let players: Vec<usize> = vec![1, 2, 3, 4];
        column![
            column![
                text(rom.display_name().to_string()).size(20),
                text(format!("{} · {} · crc32 {:08x}", rom.title, rom.code, rom.crc32)).size(13),
            ]
            .spacing(3),
            column![
                field(
                    "nickname",
                    text_input("nickname", &app.config.nick)
                        .on_input(Message::NickChanged)
                        .padding(8)
                        .width(Length::Fill)
                        .into(),
                ),
                field(
                    "players",
                    row![
                        pick_list(players, Some(state.local_players), Message::LocalPlayersChanged),
                        text("2+ links several GBAs on this machine").size(12),
                    ]
                    .spacing(10)
                    .align_y(iced::Alignment::Center)
                    .into(),
                ),
                field(
                    "save",
                    pick_list(saves.clone(), Some(state.local_save.clone()), Message::LocalSaveSelected).into(),
                ),
            ]
            .spacing(10),
            button(text("Play")).padding([10, 24]).on_press(Message::LocalClicked),
            text("Netplay plugs in mid-game: launch your game, then host or join a room from the in-session Link button.")
                .size(12),
        ]
        .spacing(16)
        .into()
    } else {
        column![text("Pick a ROM from the library to play.").size(14)].into()
    };

    row![
        library_pane,
        container(launcher)
            .padding(PADDING * 2.0)
            .width(Length::FillPortion(2))
            .height(Length::Fill)
            .style(|theme: &Theme| container::Style {
                background: Some(iced::Background::Color(theme.extended_palette().background.weak.color)),
                border: iced::Border {
                    radius: 8.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
    ]
    .spacing(PADDING * 1.5)
    .height(Length::Fill)
    .into()
}
