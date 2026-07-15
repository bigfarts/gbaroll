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
                .padding(6),
            button(text("rescan")).on_press(Message::RescanRoms),
        ]
        .spacing(6),
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
            Element::from(scrollable(column(roms).spacing(2)).height(Length::Fill))
        },
    ]
    .spacing(8)
    .width(Length::FillPortion(3));

    let selected = app.play.selected_crc.and_then(|crc| app.library.by_crc32(crc));

    let mut saves: Vec<SaveChoice> = vec![SaveChoice::Fresh];
    saves.extend(crate::library::list_saves(&app.config.saves_dir).into_iter().map(SaveChoice::File));

    let launcher: Element<'_, Message> = if let Some(rom) = selected {
        let players: Vec<usize> = vec![1, 2, 3, 4];
        column![
            text(rom.display_name().to_string()).size(20),
            text(format!("{} · {} · crc32 {:08x}", rom.title, rom.code, rom.crc32)).size(13),
            text(rom.path.display().to_string()).size(11),
            iced::widget::Space::new().height(Length::Fixed(8.0)),
            row![text("nickname:"), text_input("nickname", &app.config.nick).on_input(Message::NickChanged).padding(6).width(Length::Fixed(160.0))]
                .spacing(6)
                .align_y(iced::Alignment::Center),
            iced::widget::Space::new().height(Length::Fixed(8.0)),
            row![
                text("players:"),
                pick_list(players, Some(state.local_players), Message::LocalPlayersChanged),
                text("(2+ = a whole link on this machine)").size(11),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
            row![
                text("save:"),
                pick_list(saves.clone(), Some(state.local_save.clone()), Message::LocalSaveSelected),
            ]
            .spacing(6)
            .align_y(iced::Alignment::Center),
            button(text("Play")).padding(8).on_press(Message::LocalClicked),
            iced::widget::Space::new().height(Length::Fixed(8.0)),
            text("Netplay plugs in mid-game: launch your game, then host or join a room from the Link sidebar.")
                .size(12),
        ]
        .spacing(6)
        .into()
    } else {
        column![text("Pick a ROM from the library to play.").size(14)].into()
    };

    row![
        library_pane,
        container(launcher)
            .padding(PADDING)
            .width(Length::FillPortion(2))
            .height(Length::Fill)
            .style(|theme: &Theme| container::Style {
                background: Some(iced::Background::Color(theme.extended_palette().background.weak.color)),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
    ]
    .spacing(PADDING)
    .height(Length::Fill)
    .into()
}
