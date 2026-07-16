//! The Play tab: ROM library on the left, the session launcher on the
//! right. Netplay isn't launched from here — you launch the game, then
//! plug the cable in from the session's cable panel.

use iced::widget::{button, column, container, pick_list, row, scrollable, text, text_input};
use iced::{Element, Length, Theme};

use super::{App, Message, SaveChoice, PADDING};

pub struct State {
    pub search: String,
    pub selected_crc: Option<u32>,
    pub local_save: SaveChoice,
}

impl Default for State {
    fn default() -> Self {
        State {
            search: String::new(),
            selected_crc: None,
            local_save: SaveChoice::Fresh,
        }
    }
}

/// A left-aligned form field with a fixed-width label.
fn field<'a>(label: &'static str, control: Element<'a, Message>) -> Element<'a, Message> {
    row![text(label).width(Length::Fixed(92.0)).size(13), control]
        .spacing(10)
        .align_y(iced::Alignment::Center)
        .into()
}

fn rom_row<'a>(rom: &crate::library::RomInfo, selected: bool) -> Element<'a, Message> {
    let label = column![
        text(rom.display_name().to_string()).size(14),
        text(format!(
            "{}  ·  {} KiB  ·  {}",
            rom.code,
            rom.size / 1024,
            rom.title
        ))
        .size(11),
    ]
    .spacing(2);
    button(label)
        .padding([8, 10])
        .width(Length::Fill)
        .style(if selected {
            button::primary
        } else {
            button::text
        })
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

    let library_list: Element<'_, Message> = if roms.is_empty() {
        container(
            column![
                super::icons::icon(super::icons::Icon::FolderOpen, 28.0),
                text(if state.search.is_empty() {
                    "No games found"
                } else {
                    "No games match your search"
                })
                .size(15),
                if state.search.is_empty() {
                    text(format!(
                        "Add .gba files to {}",
                        app.config.roms_dir.display()
                    ))
                    .size(12)
                } else {
                    text("Try a title or game code.").size(12)
                },
            ]
            .spacing(8)
            .align_x(iced::Alignment::Center),
        )
        .padding(PADDING * 2.0)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .align_y(iced::alignment::Vertical::Center)
        .into()
    } else {
        scrollable(column(roms).spacing(3))
            .height(Length::Fill)
            .into()
    };

    let library_pane = container(
        column![
            row![
                column![
                    text("Game library").size(18),
                    text(format!(
                        "{} {}",
                        app.library.roms.len(),
                        if app.library.roms.len() == 1 {
                            "game"
                        } else {
                            "games"
                        }
                    ))
                    .size(11),
                ]
                .spacing(1),
                iced::widget::Space::new().width(Length::Fill),
                button(
                    row![
                        super::icons::icon(super::icons::Icon::RefreshCw, 14.0),
                        text("Rescan").size(12),
                    ]
                    .spacing(6)
                    .align_y(iced::Alignment::Center),
                )
                .padding([6, 10])
                .style(button::secondary)
                .on_press(Message::RescanRoms),
            ]
            .align_y(iced::Alignment::Center),
            row![
                super::icons::icon(super::icons::Icon::Search, 14.0).style(
                    |theme: &Theme| iced::widget::text::Style {
                        color: Some(theme.extended_palette().background.strong.color),
                    }
                ),
                text_input("Search by title or code…", &state.search)
                    .on_input(Message::PlaySearchChanged)
                    .padding(8)
                    .width(Length::Fill),
            ]
            .spacing(8)
            .align_y(iced::Alignment::Center),
            library_list,
        ]
        .spacing(12),
    )
    .padding(PADDING * 1.5)
    .width(Length::FillPortion(3))
    .height(Length::Fill)
    .style(|theme: &Theme| container::Style {
        background: Some(iced::Background::Color(
            theme.extended_palette().background.weak.color,
        )),
        border: iced::Border {
            radius: 8.0.into(),
            ..Default::default()
        },
        ..Default::default()
    });

    let selected = app
        .play
        .selected_crc
        .and_then(|crc| app.library.by_crc32(crc));

    let mut saves: Vec<SaveChoice> = vec![SaveChoice::Fresh];
    saves.extend(
        crate::library::list_saves(&app.config.saves_dir)
            .into_iter()
            .map(SaveChoice::File),
    );

    let launcher: Element<'_, Message> = if let Some(rom) = selected {
        column![
            column![
                text("Selected game").size(12),
                text(rom.display_name().to_string()).size(21),
                text(format!("{}  ·  {} KiB", rom.code, rom.size / 1024)).size(12),
            ]
            .spacing(4),
            text("Session setup").size(15),
            column![
                field(
                    "Online name",
                    text_input("nickname", &app.config.nick)
                        .on_input(Message::NickChanged)
                        .padding(8)
                        .width(Length::Fill)
                        .into(),
                ),
                field(
                    "Save data",
                    pick_list(saves.clone(), Some(state.local_save.clone()), Message::LocalSaveSelected).into(),
                ),
            ]
            .spacing(10),
            button(
                row![
                    super::icons::icon(super::icons::Icon::Play, 16.0),
                    text("Start game"),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .padding([10, 18])
            .width(Length::Fill)
            .style(button::primary)
            .on_press(Message::LocalClicked),
            container(
                row![
                    super::icons::icon(super::icons::Icon::Cable, 17.0),
                    text("To play with friends, start the game, then open \"Link cable\" in the top-right corner.")
                        .size(12),
                ]
                .spacing(9)
                .align_y(iced::Alignment::Center),
            )
            .padding(10)
            .style(|theme: &Theme| container::Style {
                background: Some(iced::Background::Color(
                    theme.extended_palette().primary.weak.color,
                )),
                text_color: Some(theme.extended_palette().primary.weak.text),
                border: iced::Border {
                    radius: 6.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
        ]
        .spacing(18)
        .into()
    } else {
        container(
            column![
                super::icons::icon(super::icons::Icon::Gamepad2, 32.0),
                text("Choose a game").size(17),
                text("Select a title from your library to configure a session.").size(12),
            ]
            .spacing(8)
            .align_x(iced::Alignment::Center),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Center)
        .align_y(iced::alignment::Vertical::Center)
        .into()
    };

    row![
        library_pane,
        container(launcher)
            .padding(PADDING * 2.0)
            .width(Length::FillPortion(2))
            .height(Length::Fill)
            .style(|theme: &Theme| container::Style {
                background: Some(iced::Background::Color(
                    theme.extended_palette().background.weak.color
                )),
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
