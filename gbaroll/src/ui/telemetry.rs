//! The unified cable panel — room setup and lobby while offline, then
//! telemetry and disconnect controls while linked. It collapses to one
//! top-right status chip. The clock metrics
//! (TPS, skew, lead, rollback depth) are one shared reading for the whole
//! link — the engine's skew/queue are already worst-case over every
//! remote — while ping is per-peer, one row each, so the card grows with
//! the number of clients instead of assuming a single opponent.

use std::collections::VecDeque;

use iced::widget::canvas::{self, Canvas, Frame, Path, Stroke};
use iced::widget::{button, column, container, row, slider, text};
use iced::{mouse, Color, Element, Length, Point, Rectangle, Renderer, Size, Theme};

use super::icons::{self, Icon};
use super::Message;
use crate::session::Stats;

/// Samples retained per metric (~3 s at the GBA tick rate), matching
/// tango's window.
pub const HISTORY_LEN: usize = 180;

const PANEL_W: f32 = 244.0;
const LINK_PANEL_W: f32 = 360.0;
const VALUE_W: f32 = 58.0;
const SPARK_H: f32 = 24.0;

// Per-metric vertical spans (the full height of a sparkline).
const TPS_SPAN: f32 = 8.0; // ticks/sec below target
const SKEW_SPAN: f32 = 8.0; // ± ticks
const LEAD_SPAN: f32 = 24.0; // unmatched local ticks
const DEPTH_SPAN: f32 = 8.0; // rolled-back ticks
const PING_SPAN: f32 = 200.0; // ms

/// One per-frame snapshot, kept in a ring buffer so each metric can draw
/// a sparkline. `pings` is indexed by peer slot (same order as
/// [`Stats::peers`]).
#[derive(Clone)]
pub struct MetricSample {
    pub tps: f32,
    pub fps_target: f32,
    pub skew: i32,
    pub lead: i32,
    pub depth: u32,
    pub pings: Vec<Option<f32>>,
}

impl MetricSample {
    pub fn capture(stats: &Stats) -> Self {
        Self {
            tps: stats.tps,
            fps_target: stats.fps_target,
            skew: stats.skew,
            lead: stats.queue_len as i32,
            depth: stats.rolled_back,
            pings: stats.peers.iter().map(|p| p.rtt_ms).collect(),
        }
    }
}

/// Health tone for a reading, driving both the value colour and the
/// sparkline wash.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Muted,
    Good,
    Warn,
    Bad,
}

impl Tone {
    fn color(self, theme: &Theme) -> Color {
        let palette = theme.extended_palette();
        match self {
            Tone::Muted => palette.background.strong.color,
            Tone::Good => palette.success.base.color,
            Tone::Warn => Color::from_rgb(0.92, 0.67, 0.18),
            Tone::Bad => palette.danger.base.color,
        }
    }
}

fn tone_for_tps(tps: f32, target: f32) -> Tone {
    if tps >= target - 1.0 {
        Tone::Good
    } else if tps >= target - 5.0 {
        Tone::Warn
    } else {
        Tone::Bad
    }
}

fn tone_for_abs(v: i32, good: i32, warn: i32) -> Tone {
    let v = v.unsigned_abs() as i32;
    if v <= good {
        Tone::Good
    } else if v <= warn {
        Tone::Warn
    } else {
        Tone::Bad
    }
}

fn tone_for_ping(ms: f32) -> Tone {
    if ms < 80.0 {
        Tone::Good
    } else if ms < 140.0 {
        Tone::Warn
    } else {
        Tone::Bad
    }
}

/// A single hairline trace over a recessed background, tone-coloured per
/// segment. Points are normalised 0..1 (0 = bottom), oldest first, newest
/// pinned to the right edge; `None` breaks the line. `zero` draws a
/// reference line (for signed metrics); `fill_under` washes the area
/// below the trace.
struct Sparkline {
    points: Vec<Option<(f32, Tone)>>,
    fill_under: bool,
    zero: Option<f32>,
}

impl<M> canvas::Program<M> for Sparkline {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = Frame::new(renderer, bounds.size());
        let palette = theme.extended_palette();
        let (w, h) = (bounds.width, bounds.height);
        let margin = 2.0;

        let bg = Path::rounded_rectangle(Point::ORIGIN, Size::new(w, h), 4.0.into());
        frame.fill(&bg, palette.background.weak.color);

        let y_of = |n: f32| margin + (1.0 - n.clamp(0.0, 1.0)) * (h - 2.0 * margin);
        // Newest sample sits at the right edge; older ones scroll left.
        let step = w / (HISTORY_LEN - 1) as f32;
        let n = self.points.len();
        let x_of = |i: usize| w - (n - 1 - i) as f32 * step;

        if let Some(zero) = self.zero {
            let y = y_of(zero);
            let line = Path::line(Point::new(0.0, y), Point::new(w, y));
            frame.stroke(
                &line,
                Stroke::default().with_color(palette.background.strong.color).with_width(1.0),
            );
        }

        // Connected segments, coloured by the newer endpoint's tone; the
        // wash (if any) fills from the baseline under each segment.
        let baseline = self.zero.map(y_of).unwrap_or(h - margin);
        for i in 1..n {
            let (Some((na, _)), Some((nb, tone))) = (self.points[i - 1], self.points[i]) else {
                continue;
            };
            let (pa, pb) = (Point::new(x_of(i - 1), y_of(na)), Point::new(x_of(i), y_of(nb)));
            if self.fill_under {
                let mut wash = tone.color(theme);
                wash.a = 0.16;
                let poly = Path::new(|b| {
                    b.move_to(Point::new(pa.x, baseline));
                    b.line_to(pa);
                    b.line_to(pb);
                    b.line_to(Point::new(pb.x, baseline));
                    b.close();
                });
                frame.fill(&poly, wash);
            }
            frame.stroke(
                &Path::line(pa, pb),
                Stroke::default().with_color(tone.color(theme)).with_width(1.5),
            );
        }

        vec![frame.into_geometry()]
    }
}

/// Build one metric card: caption row over a sparkline + right-aligned
/// value. `norm` maps each sample to (0..1 height, tone); `value` reads
/// the newest sample for the numeric readout.
#[allow(clippy::too_many_arguments)]
fn metric_card<'a>(
    icon: Icon,
    caption: &'a str,
    history: &VecDeque<MetricSample>,
    fill_under: bool,
    zero: Option<f32>,
    norm: impl Fn(&MetricSample) -> Option<(f32, Tone)>,
    value: impl Fn(&MetricSample) -> (String, Tone),
) -> Element<'a, Message> {
    let points: Vec<Option<(f32, Tone)>> = history.iter().map(&norm).collect();
    let spark = Canvas::new(Sparkline {
        points,
        fill_under,
        zero,
    })
    .width(Length::Fill)
    .height(Length::Fixed(SPARK_H));

    let (value_str, tone) = history
        .back()
        .map(&value)
        .unwrap_or_else(|| ("—".to_string(), Tone::Muted));

    column![
        row![
            icons::icon(icon, 13.0).style(|theme: &Theme| iced::widget::text::Style {
                color: Some(theme.extended_palette().background.strong.color),
            }),
            text(caption).size(12).style(|theme: &Theme| iced::widget::text::Style {
                color: Some(theme.extended_palette().background.strong.color),
            }),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
        row![
            spark,
            text(value_str)
                .size(13)
                .width(Length::Fixed(VALUE_W))
                .align_x(iced::alignment::Horizontal::Right)
                .style(move |theme: &Theme| iced::widget::text::Style { color: Some(tone.color(theme)) }),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center),
    ]
    .spacing(3)
    .into()
}

/// The signal-strength glyph for the collapsed chip, keyed off skew.
fn signal_icon(skew: i32) -> Icon {
    match skew.unsigned_abs() {
        0..=3 => Icon::SignalHigh,
        4..=7 => Icon::SignalMedium,
        _ => Icon::SignalLow,
    }
}

/// Content modes for the one cable-status control.
pub enum Panel<'a> {
    Link {
        icon: Icon,
        label: String,
        title: String,
        body: Element<'a, Message>,
        room_code: Option<&'a str>,
        code_copied: bool,
    },
    Connected {
        history: &'a VecDeque<MetricSample>,
        latest: &'a Stats,
        present_delay: u32,
        room_code: Option<&'a str>,
        code_copied: bool,
    },
}

/// The top-right cable overlay: collapsed status chip or expanded panel.
pub fn overlay(panel: Panel<'_>, expanded: bool) -> Element<'_, Message> {
    let content = match panel {
        Panel::Link {
            icon,
            label,
            title,
            body,
            room_code,
            code_copied,
        } => {
            if expanded {
                link_card(title, body, room_code, code_copied)
            } else {
                action_chip(icon, label)
            }
        }
        Panel::Connected {
            history,
            latest,
            present_delay,
            room_code,
            code_copied,
        } => {
            if expanded {
                expanded_card(history, latest, present_delay, room_code, code_copied)
            } else {
                collapsed_chip(latest)
            }
        }
    };
    // Keep status and controls in the same predictable corner in every
    // cable state.
    container(content)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_x(iced::alignment::Horizontal::Right)
        .align_y(iced::alignment::Vertical::Top)
        .padding(12)
        .into()
}

fn action_chip(icon: Icon, label: String) -> Element<'static, Message> {
    button(
        row![icons::icon(icon, 16.0), text(label).size(13)]
            .spacing(7)
            .align_y(iced::Alignment::Center),
    )
    .padding([6, 10])
    .style(button::secondary)
    .on_press(Message::SessionTogglePanel)
    .into()
}

fn link_card<'a>(
    title: String,
    body: Element<'a, Message>,
    room_code: Option<&str>,
    code_copied: bool,
) -> Element<'a, Message> {
    let head = row![
        text(title).size(14).width(Length::Fill),
        button(icons::icon(Icon::ChevronUp, 16.0))
            .padding([2, 6])
            .style(button::text)
            .on_press(Message::SessionTogglePanel),
    ]
    .align_y(iced::Alignment::Center);

    let mut content = column![head].spacing(12).width(Length::Fixed(LINK_PANEL_W));
    if let Some(code) = room_code {
        content = content.push(link_code_button(code, code_copied));
    }
    panel_container(content.push(body).into())
}

/// A deliberately button-shaped room code: its label explains what the
/// value is, and the trailing affordance explains what clicking does.
pub fn link_code_button(code: &str, copied: bool) -> Element<'static, Message> {
    let (icon, action) = if copied {
        (Icon::Check, "Copied")
    } else {
        (Icon::Copy, "Click to copy")
    };
    button(
        row![
            column![
                text("LINK CODE").size(10),
                text(code.to_string())
                    .size(18)
                    .font(iced::Font::MONOSPACE),
            ]
            .spacing(1)
            .width(Length::Fill),
            icons::icon(icon, 14.0),
            text(action).size(12),
        ]
        .spacing(7)
        .align_y(iced::Alignment::Center),
    )
    .padding([8, 10])
    .width(Length::Fill)
    .style(if copied {
        button::primary
    } else {
        button::secondary
    })
    .on_press(Message::LinkCodeCopyClicked(code.to_string()))
    .into()
}

fn collapsed_chip(latest: &Stats) -> Element<'_, Message> {
    let tone = tone_for_abs(latest.skew, 3, 7);
    button(
        row![
            icons::icon(signal_icon(latest.skew), 16.0),
            text(format!("{:+}", latest.skew)).size(13).font(iced::Font::MONOSPACE),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
    )
    .padding([6, 10])
    .style(move |theme: &Theme, status| {
        let mut base = button::secondary(theme, status);
        base.text_color = tone.color(theme);
        base
    })
    .on_press(Message::SessionTogglePanel)
    .into()
}

fn expanded_card<'a>(
    history: &'a VecDeque<MetricSample>,
    latest: &'a Stats,
    present_delay: u32,
    room_code: Option<&str>,
    code_copied: bool,
) -> Element<'a, Message> {
    let target = latest.fps_target;

    let head = row![
        text("Link telemetry").size(14).width(Length::Fill),
        button(icons::icon(Icon::ChevronUp, 16.0))
            .padding([2, 6])
            .style(button::text)
            .on_press(Message::SessionTogglePanel),
    ]
    .align_y(iced::Alignment::Center);

    let tps = metric_card(
        Icon::Gauge,
        "Ticks/sec",
        history,
        true,
        None,
        move |s| Some((((s.tps - (s.fps_target - TPS_SPAN)) / TPS_SPAN).clamp(0.0, 1.0), tone_for_tps(s.tps, s.fps_target))),
        move |s| (format!("{:.0}", s.tps), tone_for_tps(s.tps, target)),
    );
    let skew = metric_card(
        Icon::ArrowLeftRight,
        "Skew",
        history,
        false,
        Some(0.5),
        |s| Some((0.5 + s.skew as f32 / (2.0 * SKEW_SPAN), tone_for_abs(s.skew, 3, 7))),
        |s| (format!("{:+}", s.skew), tone_for_abs(s.skew, 3, 7)),
    );
    let lead = metric_card(
        Icon::Footprints,
        "Lead",
        history,
        true,
        None,
        |s| Some(((s.lead as f32 / LEAD_SPAN).clamp(0.0, 1.0), tone_for_abs(s.lead, 8, 16))),
        |s| (format!("{}", s.lead), tone_for_abs(s.lead, 8, 16)),
    );
    let depth = metric_card(
        Icon::GitMerge,
        "Rollback",
        history,
        true,
        None,
        |s| Some(((s.depth as f32 / DEPTH_SPAN).clamp(0.0, 1.0), tone_for_abs(s.depth as i32, 2, 5))),
        |s| (format!("{}", s.depth), tone_for_abs(s.depth as i32, 2, 5)),
    );

    let mut cards = column![head].spacing(8).width(Length::Fixed(PANEL_W));
    if let Some(code) = room_code {
        cards = cards.push(link_code_button(code, code_copied));
    }
    cards = cards.push(tps).push(skew).push(lead).push(depth);

    // One ping card per peer, captioned with the peer's nick — this is
    // what makes the panel scale with the number of clients.
    for (i, peer) in latest.peers.iter().enumerate() {
        cards = cards.push(metric_card(
            Icon::Wifi,
            peer.nick.as_str(),
            history,
            true,
            None,
            move |s| {
                s.pings
                    .get(i)
                    .copied()
                    .flatten()
                    .map(|ms| ((ms / PING_SPAN).clamp(0.0, 1.0), tone_for_ping(ms)))
            },
            move |s| match s.pings.get(i).copied().flatten() {
                Some(ms) => (format!("{ms:.0} ms"), tone_for_ping(ms)),
                None => ("—".to_string(), Tone::Muted),
            },
        ));
    }

    // Present-delay control (tango's frame-delay slider), suggesting a
    // value from the worst peer's ping.
    let suggested = latest
        .peers
        .iter()
        .filter_map(|p| p.rtt_ms)
        .fold(0.0_f32, f32::max);
    let suggest = ((suggested / 2.0 / (1000.0 / crate::session::EXPECTED_FPS)).ceil() as u32 + 1).clamp(0, 10);
    let delay = column![
        row![
            icons::icon(Icon::Timer, 13.0).style(|theme: &Theme| iced::widget::text::Style {
                color: Some(theme.extended_palette().background.strong.color),
            }),
            text("Input delay").size(12).width(Length::Fill),
            text(format!("{present_delay}")).size(13).font(iced::Font::MONOSPACE),
        ]
        .spacing(6)
        .align_y(iced::Alignment::Center),
        slider(0..=10u32, present_delay, Message::SessionPresentDelayChanged),
        button(text(format!("Suggest {suggest} from ping")).size(11))
            .padding([3, 8])
            .style(button::secondary)
            .on_press(Message::SessionPresentDelayChanged(suggest)),
    ]
    .spacing(6);

    cards = cards.push(delay);

    cards = cards.push(
        column![
            button(
                row![icons::icon(Icon::Unplug, 14.0), text("Disconnect cable")]
                    .spacing(7)
                    .align_y(iced::Alignment::Center),
            )
            .padding([7, 12])
            .width(Length::Fill)
            .style(button::danger)
            .on_press(Message::SessionUnplug),
            text("Your local game keeps running after disconnecting.").size(11),
        ]
        .spacing(6),
    );

    panel_container(cards.into())
}

fn panel_container(content: Element<'_, Message>) -> Element<'_, Message> {
    container(content)
        .padding(12)
        .style(|theme: &Theme| container::Style {
            background: Some(iced::Background::Color(theme.extended_palette().background.base.color)),
            border: iced::Border {
                radius: 10.0.into(),
                width: 1.0,
                color: theme.extended_palette().background.strong.color,
            },
            ..Default::default()
        })
        .into()
}
