//! The Replays tab: a table of recorded sessions, watchable when the
//! library holds every side's ROM.

use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length, Theme};

use super::{App, Message, PADDING};

pub struct Entry {
    pub path: std::path::PathBuf,
    pub metadata: gbaroll_replay::Metadata,
    pub ticks: u32,
    pub complete: bool,
}

#[derive(Default)]
pub struct State {
    pub entries: Vec<Entry>,
}

impl State {
    pub fn scan(replays_dir: &std::path::Path) -> State {
        let mut entries = Vec::new();
        let Ok(dir) = std::fs::read_dir(replays_dir) else {
            return State { entries };
        };
        for entry in dir.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case(gbaroll_replay::FILE_EXTENSION))
                != Some(true)
            {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else { continue };
            match gbaroll_replay::Replay::parse(&bytes) {
                Ok(replay) => entries.push(Entry {
                    path,
                    ticks: replay.inputs.len() as u32,
                    complete: replay.is_complete,
                    metadata: replay.metadata,
                }),
                Err(e) => log::warn!("skipping {}: {e}", path.display()),
            }
        }
        entries.sort_by(|a, b| {
            b.metadata
                .started_at_unix_micros
                .cmp(&a.metadata.started_at_unix_micros)
                .then_with(|| b.path.cmp(&a.path))
        });
        State { entries }
    }
}

fn format_date(micros: Option<u64>) -> String {
    let Some(micros) = micros else { return "?".to_string() };
    chrono::DateTime::from_timestamp_micros(micros as i64)
        .map(|utc| {
            utc.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_string())
}

pub fn view(app: &App) -> Element<'_, Message> {
    let state = &app.replays;

    if state.entries.is_empty() {
        return container(
            column![
                text("No replays yet.").size(16),
                text(format!(
                    "Netplay sessions record themselves into\n{}",
                    app.config.replays_dir.display()
                ))
                .size(13),
                button(text("rescan")).on_press(Message::RescanReplays),
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

    let mut rows = column![].spacing(6);
    for (index, entry) in state.entries.iter().enumerate() {
        let meta = &entry.metadata;
        let players = meta
            .players
            .iter()
            .map(|p| p.nick.as_str())
            .collect::<Vec<_>>()
            .join(" vs ");
        let roms: Vec<&str> = {
            // Prefer the No-Intro name from our own library; the stored
            // header title is the fallback for ROMs we no longer have.
            let mut titles: Vec<&str> = meta
                .players
                .iter()
                .map(|p| {
                    app.library
                        .by_crc32(p.rom_crc32)
                        .map(|r| r.display_name())
                        .unwrap_or(p.rom_title.as_str())
                })
                .collect();
            titles.dedup();
            titles
        };
        let have_all = meta.players.iter().all(|p| app.library.by_crc32(p.rom_crc32).is_some());

        let mut watch = button(text("Watch")).padding([4, 10]);
        if have_all {
            watch = watch.on_press(Message::ReplayWatch(index));
        }

        rows = rows.push(
            container(
                row![
                    column![
                        row![
                            text(format_date(meta.started_at_unix_micros)).size(14),
                            text(players).size(14),
                            if entry.complete {
                                text("")
                            } else {
                                text("(incomplete)").size(12)
                            },
                        ]
                        .spacing(10),
                        row![
                            text(roms.join(" + ")).size(12),
                            text(format!(
                                "{} players · {}",
                                meta.players.len(),
                                super::format_ticks(entry.ticks)
                            ))
                            .size(12),
                            if have_all {
                                text("")
                            } else {
                                text("missing ROM(s)").size(12).style(|theme: &Theme| text::Style {
                                    color: Some(theme.extended_palette().danger.base.color),
                                })
                            },
                        ]
                        .spacing(10),
                    ]
                    .spacing(2)
                    .width(Length::Fill),
                    watch,
                    button(text("Delete"))
                        .padding([4, 10])
                        .style(button::danger)
                        .on_press(Message::ReplayDelete(index)),
                ]
                .spacing(8)
                .align_y(iced::Alignment::Center),
            )
            .padding(PADDING)
            .width(Length::Fill)
            .style(|theme: &Theme| container::Style {
                background: Some(iced::Background::Color(theme.extended_palette().background.weak.color)),
                border: iced::Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }),
        );
    }

    column![
        row![
            text(format!("{} replay(s)", state.entries.len())),
            iced::widget::Space::new().width(Length::Fill),
            button(text("rescan")).padding([8, 14]).on_press(Message::RescanReplays),
        ]
        .align_y(iced::Alignment::Center),
        scrollable(rows).height(Length::Fill),
    ]
    .spacing(12)
    .into()
}
