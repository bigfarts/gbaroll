//! The Settings tab: identity, directories, netplay, video/audio, and
//! the input binding editor (with live capture).

use iced::widget::{
    button, checkbox, column, container, row, scrollable, slider, text, text_input,
};
use iced::{Element, Length, Theme};

use super::{App, Message, PADDING};
use crate::platform::input::{self, MappedKey, PhysicalInput};
use crate::platform::input_capture::{Input, InputCapture};

pub const MAPPED_KEYS: [(MappedKey, &str); 11] = [
    (MappedKey::Up, "Up"),
    (MappedKey::Down, "Down"),
    (MappedKey::Left, "Left"),
    (MappedKey::Right, "Right"),
    (MappedKey::A, "A"),
    (MappedKey::B, "B"),
    (MappedKey::L, "L"),
    (MappedKey::R, "R"),
    (MappedKey::Start, "Start"),
    (MappedKey::Select, "Select"),
    (MappedKey::SpeedUp, "Fast-forward"),
];

#[derive(Default)]
pub struct State {
    pub capture_target: Option<MappedKey>,
}

fn section<'a>(title: &'a str, content: Element<'a, Message>) -> Element<'a, Message> {
    container(column![text(title).size(16), content].spacing(10))
        .padding(PADDING * 1.5)
        .width(Length::Fill)
        .style(|theme: &Theme| container::Style {
            background: Some(iced::Background::Color(
                theme.extended_palette().background.weak.color,
            )),
            border: iced::Border {
                radius: 6.0.into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .into()
}

/// The label column width, shared by every settings row so controls line
/// up in one column.
const LABEL_WIDTH: f32 = 150.0;

fn labeled<'a>(label: &'a str, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    row![
        text(label).width(Length::Fixed(LABEL_WIDTH)).size(14),
        content.into()
    ]
    .spacing(12)
    .align_y(iced::Alignment::Center)
    .into()
}

fn dir_row<'a>(label: &'a str, path: &std::path::Path, pick: Message) -> Element<'a, Message> {
    labeled(
        label,
        row![
            container(text(path.display().to_string()).size(12))
                .width(Length::Fill)
                .padding([6, 10])
                .style(|theme: &Theme| container::Style {
                    background: Some(iced::Background::Color(theme.extended_palette().background.base.color)),
                    border: iced::Border {
                        radius: 6.0.into(),
                        ..Default::default()
                    },
                    ..Default::default()
                }),
            button(text("Change…")).padding([6, 12]).on_press(pick),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    )
}

fn bindings_editor(app: &App) -> Element<'_, Message> {
    let mut rows = column![].spacing(8);
    for (key, label) in MAPPED_KEYS {
        let mut chips = row![].spacing(4);
        for (index, physical) in app.config.mapping.slot(key).iter().enumerate() {
            let (_, chip_label) = input::describe(physical);
            chips = chips.push(
                button(
                    row![text(chip_label).size(12), super::icons::icon(super::icons::Icon::X, 12.0)]
                        .spacing(5)
                        .align_y(iced::Alignment::Center),
                )
                .padding([3, 7])
                .style(button::secondary)
                .on_press(Message::BindingRemoved(key, index)),
            );
        }
        let capturing = app.settings.capture_target == Some(key);
        chips = chips.push(if capturing {
            button(text("press a key/button… (Esc cancels)").size(12))
                .padding([2, 6])
                .style(button::primary)
                .on_press(Message::BindingCaptureCancel)
        } else {
            button(text("+ add").size(12))
                .padding([2, 6])
                .on_press(Message::BindingCaptureStart(key))
        });
        rows = rows.push(labeled(label, chips));
    }
    rows = rows.push(
        button(text("reset to defaults"))
            .padding([4, 10])
            .style(button::secondary)
            .on_press(Message::MappingReset),
    );
    rows.into()
}

pub fn view(app: &App) -> Element<'_, Message> {
    let config = &app.config;

    let identity = section(
        "Identity",
        labeled(
            "Nickname",
            text_input("nickname", &config.nick)
                .on_input(Message::NickChanged)
                .padding(8)
                .width(Length::Fixed(260.0)),
        ),
    );

    let dat_status = if app.dat_downloading {
        "downloading the GBA No-Intro DAT…".to_string()
    } else if let Some(e) = &app.dat_download_error {
        format!("download failed: {e}")
    } else if app.dats.is_empty() {
        "no names loaded".to_string()
    } else {
        format!("{} name(s) from {} DAT file(s)", app.dats.len(), app.dats.files())
    };
    let mut download = button(text("Download").size(13)).padding([6, 12]);
    if !app.dat_downloading {
        download = download.on_press(Message::DownloadGbaDat);
    }
    let dirs = section(
        "Directories",
        column![
            dir_row("ROMs", &config.roms_dir, Message::PickRomsDir),
            dir_row("Saves", &config.saves_dir, Message::PickSavesDir),
            dir_row("Replays", &config.replays_dir, Message::PickReplaysDir),
            dir_row("No-Intro DATs", &config.dats_dir, Message::PickDatsDir),
            labeled(
                "Display names",
                row![text(dat_status).size(13).width(Length::Fill), download]
                    .spacing(8)
                    .align_y(iced::Alignment::Center),
            ),
        ]
        .spacing(10)
        .into(),
    );

    let netplay = section(
        "Netplay",
        column![
            labeled(
                "Signaling server",
                text_input("ws://host:1984", &config.signaling_server)
                    .on_input(Message::ServerChanged)
                    .padding(8)
                    .width(Length::Fill),
            ),
            labeled(
                "Input delay",
                row![
                    slider(0..=10u32, config.present_delay, Message::PresentDelayChanged)
                        .width(Length::Fixed(220.0)),
                    text(format!("{} ticks", config.present_delay)).size(13),
                ]
                .spacing(12)
                .align_y(iced::Alignment::Center),
            ),
        ]
        .spacing(10)
        .into(),
    );

    let av = section(
        "Video / audio",
        column![
            labeled(
                "Volume",
                row![
                    slider(0.0..=1.0f32, config.volume, Message::VolumeChanged)
                        .step(0.01)
                        .width(Length::Fixed(220.0)),
                    text(format!("{:.0}%", config.volume * 100.0)).size(13),
                ]
                .spacing(12)
                .align_y(iced::Alignment::Center),
            ),
            labeled(
                "Integer scaling",
                checkbox(config.integer_scaling).on_toggle(Message::IntegerScalingToggled),
            ),
            labeled(
                "Netplay stats HUD",
                checkbox(config.show_hud).on_toggle(Message::ShowHudToggled),
            ),
        ]
        .spacing(10)
        .into(),
    );

    let input_section = section("Input bindings", bindings_editor(app));

    let body = scrollable(
        column![identity, dirs, netplay, av, input_section]
            .spacing(PADDING * 1.5)
            .width(Length::Fill),
    )
    .height(Length::Fill);

    if app.settings.capture_target.is_some() {
        // Wrap the pane in an input capture so the next key or pad
        // button becomes the binding (Escape cancels).
        InputCapture::new(body, |input| {
            if let Input::Keyboard(iced::keyboard::Event::KeyPressed { physical_key, .. }) = &input
            {
                if *physical_key
                    == iced::keyboard::key::Physical::Code(iced::keyboard::key::Code::Escape)
                {
                    return Some(Message::BindingCaptureCancel);
                }
                return Some(Message::BindingCaptured(PhysicalInput::Key(
                    input::KeyPhysical(*physical_key),
                )));
            }
            match input.to_event() {
                Some(input::Event::Button {
                    button,
                    pressed: true,
                }) => Some(Message::BindingCaptured(PhysicalInput::Button(button))),
                Some(input::Event::Axis { axis, value }) if value.abs() > input::AXIS_THRESHOLD => {
                    Some(Message::BindingCaptured(PhysicalInput::Axis {
                        axis,
                        dir: if value > 0.0 {
                            input::AxisDir::Positive
                        } else {
                            input::AxisDir::Negative
                        },
                    }))
                }
                _ => None,
            }
        })
        .into()
    } else {
        body.into()
    }
}
