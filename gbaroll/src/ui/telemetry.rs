//! The connected cable's telemetry card (dropped from the chip row —
//! see `overlay`), plus the chip's link-quality glyph. The clock
//! metrics (TPS, skew, lead, rollback depth) are one shared reading
//! for the whole link — the engine's skew/queue are already worst-case
//! over every remote — while ping is per-peer, one card each, so the
//! panel grows with the number of clients instead of assuming a single
//! opponent.
//!
//! Sparklines draw on small 2D canvases from the runtime's sample ring,
//! redrawn per presented frame while the panel is open.

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::{icons, use_ctx};
use crate::runtime::{FRAME_REV, PANEL_OPEN, SESSION_EPOCH};
use crate::session::{MetricSample, HISTORY_LEN};

// Per-metric vertical spans (the full height of a sparkline).
const TPS_SPAN: f32 = 8.0; // ticks/sec below target
const SKEW_SPAN: f32 = 8.0; // ± ticks
const LEAD_SPAN: f32 = 24.0; // unmatched local ticks
const DEPTH_SPAN: f32 = 8.0; // rolled-back ticks
const PING_SPAN: f32 = 200.0; // ms

/// Sparkline backing store (CSS stretches the width).
const SPARK_W: u32 = 180;
const SPARK_H: u32 = 24;

/// Health tone for a reading, driving both the value colour and the
/// sparkline wash. Colours are the stylesheet's TokyoNight set.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Muted,
    Good,
    Warn,
    Bad,
}

impl Tone {
    fn css(self) -> &'static str {
        match self {
            Tone::Muted => "#565f89",
            Tone::Good => "#9ece6a",
            Tone::Warn => "#e0af68",
            Tone::Bad => "#f7768e",
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

/// One metric's view of the ring: normalised 0..1 heights (0 = bottom)
/// with a tone per point; `None` breaks the line.
type Points = Vec<Option<(f32, Tone)>>;

struct Metric {
    canvas_id: &'static str,
    icon: fn() -> Element,
    caption: String,
    fill_under: bool,
    zero: Option<f32>,
    points: Points,
    value: String,
    value_tone: Tone,
}

/// Per-seat identity colours (TokyoNight set): the lobby roster's dots
/// and the shared ping graph's traces, both indexed by player seat so
/// they agree. Deliberately disjoint from the health-tone palette
/// (green/amber/red) so identity never reads as a judgement. A wireless
/// room can seat more players than there are colours; they cycle.
pub(super) const PEER_COLORS: [&str; 5] = ["#7dcfff", "#bb9af7", "#73daca", "#ff9e64", "#7aa2f7"];

fn seat_color(player: usize) -> &'static str {
    PEER_COLORS[player % PEER_COLORS.len()]
}

/// One peer's trace on the shared ping graph: normalized heights
/// (`None` breaks the line) plus the legend's reading.
#[derive(Clone, PartialEq)]
struct PingSeries {
    nick: String,
    color: &'static str,
    points: Vec<Option<f32>>,
    latest: Option<f32>,
}

fn ping_series(
    history: &std::collections::VecDeque<MetricSample>,
    peers: &[(usize, String)],
) -> Vec<PingSeries> {
    let latest = history.back();
    peers
        .iter()
        .enumerate()
        .map(|(i, (player, nick))| PingSeries {
            nick: nick.clone(),
            color: seat_color(*player),
            points: history
                .iter()
                .map(|s| {
                    s.pings
                        .get(i)
                        .copied()
                        .flatten()
                        .map(|ms| (ms / PING_SPAN).clamp(0.0, 1.0))
                })
                .collect(),
            latest: latest.and_then(|s| s.pings.get(i).copied().flatten()),
        })
        .collect()
}

/// Redraw the shared ping graph: every peer's trace on one canvas,
/// coloured by seat rather than by health.
fn draw_ping_graph(canvas_id: &str, series: &[PingSeries]) {
    let Some(ctx) = canvas_2d(canvas_id) else {
        return;
    };
    let (canvas, ctx) = ctx;
    let (w, h) = (canvas.width() as f64, canvas.height() as f64);
    let margin = 2.0;

    ctx.set_fill_style_str("#1f2335");
    ctx.fill_rect(0.0, 0.0, w, h);

    let y_of = |n: f32| margin + (1.0 - n.clamp(0.0, 1.0)) as f64 * (h - 2.0 * margin);
    let step = w / (HISTORY_LEN - 1) as f64;
    ctx.set_line_width(1.5);
    for s in series {
        let n = s.points.len();
        let x_of = |i: usize| w - (n - 1 - i) as f64 * step;
        ctx.set_stroke_style_str(s.color);
        for i in 1..n {
            let (Some(a), Some(b)) = (s.points[i - 1], s.points[i]) else {
                continue;
            };
            ctx.begin_path();
            ctx.move_to(x_of(i - 1), y_of(a));
            ctx.line_to(x_of(i), y_of(b));
            ctx.stroke();
        }
    }
}

/// Look a sparkline canvas up and hand back its 2D context.
fn canvas_2d(
    canvas_id: &str,
) -> Option<(web_sys::HtmlCanvasElement, web_sys::CanvasRenderingContext2d)> {
    let canvas = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(canvas_id))
        .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok())?;
    let ctx = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())?;
    Some((canvas, ctx))
}

fn metrics_from(history: &std::collections::VecDeque<MetricSample>) -> Vec<Metric> {
    let latest = history.back();
    vec![
        Metric {
            canvas_id: "tele-tps",
            icon: || rsx! { icons::Gauge {} },
            caption: "Tick/s (current/max)".to_string(),
            fill_under: true,
            zero: None,
            points: history
                .iter()
                .map(|s| {
                    Some((
                        ((s.tps - (s.fps_target - TPS_SPAN)) / TPS_SPAN).clamp(0.0, 1.0),
                        tone_for_tps(s.tps, s.fps_target),
                    ))
                })
                .collect(),
            // Tango's reading: measured ticks against the throttler's
            // current target.
            value: latest
                .map(|s| format!("{:.2} / {:.2}", s.tps, s.fps_target))
                .unwrap_or("—".into()),
            value_tone: latest
                .map(|s| tone_for_tps(s.tps, s.fps_target))
                .unwrap_or(Tone::Muted),
        },
        Metric {
            canvas_id: "tele-skew",
            icon: || rsx! { icons::ArrowLeftRight {} },
            caption: "Skew".to_string(),
            fill_under: false,
            zero: Some(0.5),
            points: history
                .iter()
                .map(|s| {
                    Some((
                        (0.5 + s.skew as f32 / (2.0 * SKEW_SPAN)).clamp(0.0, 1.0),
                        tone_for_abs(s.skew, 3, 7),
                    ))
                })
                .collect(),
            value: latest.map(|s| format!("{:+}", s.skew)).unwrap_or("—".into()),
            value_tone: latest
                .map(|s| tone_for_abs(s.skew, 3, 7))
                .unwrap_or(Tone::Muted),
        },
        Metric {
            canvas_id: "tele-lead",
            icon: || rsx! { icons::Footprints {} },
            caption: "Lead".to_string(),
            fill_under: true,
            zero: None,
            points: history
                .iter()
                .map(|s| {
                    Some((
                        (s.lead as f32 / LEAD_SPAN).clamp(0.0, 1.0),
                        tone_for_abs(s.lead, 8, 16),
                    ))
                })
                .collect(),
            value: latest.map(|s| format!("{}", s.lead)).unwrap_or("—".into()),
            value_tone: latest
                .map(|s| tone_for_abs(s.lead, 8, 16))
                .unwrap_or(Tone::Muted),
        },
        Metric {
            canvas_id: "tele-depth",
            icon: || rsx! { icons::GitMerge {} },
            caption: "Misprediction depth".to_string(),
            fill_under: true,
            zero: None,
            points: history
                .iter()
                .map(|s| {
                    Some((
                        (s.depth as f32 / DEPTH_SPAN).clamp(0.0, 1.0),
                        tone_for_abs(s.depth as i32, 2, 5),
                    ))
                })
                .collect(),
            value: latest.map(|s| format!("{}", s.depth)).unwrap_or("—".into()),
            value_tone: latest
                .map(|s| tone_for_abs(s.depth as i32, 2, 5))
                .unwrap_or(Tone::Muted),
        },
    ]
}

/// Redraw one sparkline: a hairline trace over a recessed background,
/// tone-coloured per segment, newest sample pinned to the right edge.
fn draw_sparkline(canvas_id: &str, points: &Points, fill_under: bool, zero: Option<f32>) {
    let Some((canvas, ctx)) = canvas_2d(canvas_id) else {
        return;
    };
    let (w, h) = (canvas.width() as f64, canvas.height() as f64);
    let margin = 2.0;

    ctx.set_fill_style_str("#1f2335");
    ctx.fill_rect(0.0, 0.0, w, h);

    let y_of = |n: f32| margin + (1.0 - n.clamp(0.0, 1.0)) as f64 * (h - 2.0 * margin);
    // Newest sample sits at the right edge; older ones scroll left.
    let step = w / (HISTORY_LEN - 1) as f64;
    let n = points.len();
    let x_of = |i: usize| w - (n - 1 - i) as f64 * step;

    if let Some(zero) = zero {
        let y = y_of(zero);
        ctx.set_stroke_style_str("#565f89");
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(0.0, y);
        ctx.line_to(w, y);
        ctx.stroke();
    }

    // Connected segments, coloured by the newer endpoint's tone; the
    // wash (if any) fills from the baseline under each segment.
    let baseline = zero.map(y_of).unwrap_or(h - margin);
    ctx.set_line_width(1.5);
    for i in 1..n {
        let (Some((na, _)), Some((nb, tone))) = (points[i - 1], points[i]) else {
            continue;
        };
        let (xa, ya) = (x_of(i - 1), y_of(na));
        let (xb, yb) = (x_of(i), y_of(nb));
        if fill_under {
            ctx.set_global_alpha(0.16);
            ctx.set_fill_style_str(tone.css());
            ctx.begin_path();
            ctx.move_to(xa, baseline);
            ctx.line_to(xa, ya);
            ctx.line_to(xb, yb);
            ctx.line_to(xb, baseline);
            ctx.close_path();
            ctx.fill();
            ctx.set_global_alpha(1.0);
        }
        ctx.set_stroke_style_str(tone.css());
        ctx.begin_path();
        ctx.move_to(xa, ya);
        ctx.line_to(xb, yb);
        ctx.stroke();
    }
}

/// The signal-strength glyph for the cable chip, keyed off skew.
pub(super) fn signal_icon(skew: i32) -> Element {
    match skew.unsigned_abs() {
        0..=3 => rsx! { icons::SignalHigh {} },
        4..=7 => rsx! { icons::SignalMedium {} },
        _ => rsx! { icons::SignalLow {} },
    }
}

/// The chip glyph's colour for a skew reading (same tone scale the
/// skew metric card uses).
pub(super) fn skew_tone_css(skew: i32) -> &'static str {
    tone_for_abs(skew, 3, 7).css()
}

/// Which face of the connected card is up: the link statistics or the
/// room. Its own signal (not component state) so the pick survives the
/// panel collapsing and reopening.
#[derive(Clone, Copy, PartialEq, Eq)]
enum TeleTab {
    Stats,
    Room,
}

static TELE_TAB: GlobalSignal<TeleTab> = Signal::global(|| TeleTab::Stats);

/// The dropped telemetry card for a merged link, split across two tabs
/// so neither crowds the other: **Stats** (the shared clock metrics,
/// per-peer pings, the present-delay control) and **Room** (still live
/// mid-session — dynamic membership means the code keeps inviting and
/// the roster keeps changing). The way out stays pinned below both.
#[component]
pub fn TelemetryCard() -> Element {
    let ctx = use_ctx();
    let _ = SESSION_EPOCH.read();
    let _ = FRAME_REV.read();
    let tab = *TELE_TAB.read();

    // The session's peripheral names the card, as on the offline body.
    let link = ctx
        .runtime
        .borrow()
        .descriptor()
        .map(|d| d.link)
        .unwrap_or_default();
    // Whether this machine still holds the room: leaving then covers
    // both halves (the room and the link); with the room already gone
    // (server lost), only the link remains to let go of.
    let in_room = super::cable::LOBBY_UI.read().is_some();

    let peers: Vec<(usize, String)> = {
        let rt = ctx.runtime.borrow();
        rt.shared()
            .map(|s| {
                s.stats
                    .lock()
                    .unwrap()
                    .peers
                    .iter()
                    .map(|p| (p.player, p.nick.clone()))
                    .collect()
            })
            .unwrap_or_default()
    };

    // Redraw the sparklines (and the shared ping graph) after every
    // rendered frame.
    type Snapshot = (Vec<(String, Points, bool, Option<f32>)>, Vec<PingSeries>);
    let snapshot: Snapshot = {
        let rt = ctx.runtime.borrow();
        (
            metrics_from(rt.metric_history())
                .into_iter()
                .map(|m| (m.canvas_id.to_string(), m.points, m.fill_under, m.zero))
                .collect(),
            ping_series(rt.metric_history(), &peers),
        )
    };
    use_effect(use_reactive!(|snapshot| {
        let (metrics, pings) = &snapshot;
        for (id, points, fill_under, zero) in metrics {
            draw_sparkline(id, points, *fill_under, *zero);
        }
        if !pings.is_empty() {
            draw_ping_graph("tele-ping", pings);
        }
    }));

    rsx! {
        div { class: "tele-card",
            div { class: "tele-head",
                h3 { {link.label()} }
                button {
                    class: "btn ghost icon-btn",
                    onclick: move |_| *PANEL_OPEN.write() = false,
                    icons::ChevronUp {}
                }
            }
            div { class: "tabs tele-tabs",
                button {
                    class: if tab == TeleTab::Stats { "btn tab active" } else { "btn tab" },
                    onclick: move |_| *TELE_TAB.write() = TeleTab::Stats,
                    icons::ChartLine {}
                    "Stats"
                }
                button {
                    class: if tab == TeleTab::Room { "btn tab active" } else { "btn tab" },
                    onclick: move |_| *TELE_TAB.write() = TeleTab::Room,
                    icons::Users {}
                    "Room"
                }
            }
            if tab == TeleTab::Stats {
                TelemetryCards { ctx_key: FRAME_REV() }
                // A control, not a metric — visually its own group.
                div { class: "delay-section",
                    DelayControl {}
                }
            } else if in_room {
                // The room stays live while the session runs: the code
                // keeps inviting; wireless rooms re-merge on their own,
                // and a cable room's creator re-links to fold a late
                // joiner in (everyone back at a link menu first).
                super::cable::RoomSection {}
                div { class: "menu-actions",
                    super::cable::LinkUpButton {}
                }
            } else {
                p { class: "sub", "The room is gone — the session plays on." }
            }
            div { class: "menu-actions",
                button {
                    class: "btn danger",
                    onclick: {
                        let ctx = ctx.clone();
                        move |_| {
                            // Walking out of range: the room first (or a
                            // lingering unplug would read as a dead merge
                            // and drag the room into a pointless
                            // re-merge), then the link.
                            super::cable::leave();
                            ctx.runtime.borrow().unplug();
                        }
                    },
                    icons::Unplug {}
                    if in_room { "Leave the room" } else { "Disconnect" }
                }
            }
            p { class: "hint", "Your local game keeps running after you leave." }
        }
    }
}

/// The metric cards; a separate component so the caption/value text
/// re-renders per frame (`ctx_key` is FRAME_REV).
#[component]
fn TelemetryCards(ctx_key: u64) -> Element {
    let ctx = use_ctx();
    let rt = ctx.runtime.borrow();
    let peers: Vec<(usize, String)> = rt
        .shared()
        .map(|s| {
            s.stats
                .lock()
                .unwrap()
                .peers
                .iter()
                .map(|p| (p.player, p.nick.clone()))
                .collect()
        })
        .unwrap_or_default();
    let metrics = metrics_from(rt.metric_history());
    let pings = ping_series(rt.metric_history(), &peers);
    rsx! {
        for m in metrics {
            div { class: "metric-card",
                div { class: "metric-caption",
                    {(m.icon)()}
                    span { "{m.caption}" }
                }
                div { class: "spark-row",
                    canvas {
                        id: m.canvas_id,
                        width: "{SPARK_W}",
                        height: "{SPARK_H}",
                    }
                }
                // The reading sits under its chart, right-aligned —
                // the same shape as the ping legend.
                div { class: "metric-reading",
                    span { class: "metric-value mono", style: "color: {m.value_tone.css()}", "{m.value}" }
                }
            }
        }
        // Every peer's ping shares one graph, coloured by seat; the
        // legend carries the readings.
        if !pings.is_empty() {
            div { class: "metric-card",
                div { class: "metric-caption",
                    icons::Wifi {}
                    span { "Network latency" }
                }
                div { class: "spark-row",
                    canvas {
                        id: "tele-ping",
                        width: "{SPARK_W}",
                        height: "{SPARK_H}",
                    }
                }
                div { class: "ping-legend",
                    for s in pings {
                        div { class: "ping-peer",
                            span { class: "dot", style: "background: {s.color}" }
                            span { class: "nick", "{s.nick}" }
                            span {
                                class: "metric-value mono",
                                style: "color: {s.latest.map(tone_for_ping).unwrap_or(Tone::Muted).css()}",
                                {s.latest.map(|ms| format!("{ms:.0} ms")).unwrap_or("—".into())}
                            }
                        }
                    }
                }
            }
        }
    }
}

/// The last code auto-copied at room creation. The cable panel unmounts
/// and remounts as it closes and reopens, so the component can't keep
/// this one-shot itself — and only the creation should touch the
/// clipboard, never a re-render of the same room.
static AUTO_COPIED: GlobalSignal<Option<String>> = Signal::global(|| None);

/// Copy `code` to the clipboard and flash the button's "Copied" state
/// for `hold_ms`. No flash when the browser refuses the write (focus or
/// permission) — the button still offers click-to-copy.
async fn copy_code(code: String, mut copied: Signal<bool>, hold_ms: u32) {
    let clipboard = web_sys::window().unwrap().navigator().clipboard();
    if wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&code))
        .await
        .is_ok()
    {
        copied.set(true);
        gloo_timers::future::TimeoutFuture::new(hold_ms).await;
        copied.set(false);
    }
}

/// A deliberately button-shaped room code: click to copy. Shared with
/// the lobby body, which shows the same code before the cable is in.
/// With `auto_copy` (the room's creator), the code lands in the
/// clipboard by itself the moment the server assigns it.
#[component]
pub fn RoomCode(code: String, auto_copy: bool) -> Element {
    let copied = use_signal(|| false);
    // The macro moves its dependencies, so the effect gets its own copy.
    let auto_code = code.clone();
    use_effect(use_reactive!(|(auto_code, auto_copy)| {
        if auto_copy && AUTO_COPIED.peek().as_ref() != Some(&auto_code) {
            *AUTO_COPIED.write() = Some(auto_code.clone());
            spawn(copy_code(auto_code.clone(), copied, 2_500));
        }
    }));
    rsx! {
        button {
            class: if copied() { "btn primary room-code-btn" } else { "btn room-code-btn" },
            onclick: {
                let code = code.clone();
                move |_| {
                    spawn(copy_code(code.clone(), copied, 1_500));
                }
            },
            span { class: "room-code-label",
                span { class: "sub", "LINK CODE" }
                code { class: "room-code", "{code}" }
            }
            if copied() { "Copied" } else { "Click to copy" }
        }
    }
}

/// The present-delay control (tango's frame-delay slider), suggesting a
/// value from the worst peer's ping.
#[component]
fn DelayControl() -> Element {
    let ctx = use_ctx();
    let present_delay = ctx.config.read().present_delay;
    let worst_ping = {
        let rt = ctx.runtime.borrow();
        rt.shared()
            .map(|s| {
                s.stats
                    .lock()
                    .unwrap()
                    .peers
                    .iter()
                    .filter_map(|p| p.rtt_ms)
                    .fold(0.0_f32, f32::max)
            })
            .unwrap_or(0.0)
    };
    let frame_ms = 1000.0 / crate::session::EXPECTED_FPS;
    let suggest = ((worst_ping / 2.0 / frame_ms).ceil() as u32 + 1).clamp(0, 10);
    let apply = {
        let ctx = ctx.clone();
        move |v: u32| {
            let v = v.min(10);
            let mut config = ctx.config;
            config.with_mut(|c| c.present_delay = v);
            ctx.runtime.borrow().set_present_delay(v);
        }
    };
    rsx! {
        div { class: "metric-card",
            div { class: "metric-caption",
                icons::Timer {}
                span { "Frame delay" }
                span { class: "metric-value mono", "{present_delay}" }
            }
            div { class: "delay-row",
                input {
                    r#type: "range",
                    min: "0",
                    max: "10",
                    value: "{present_delay}",
                    oninput: {
                        let apply = apply.clone();
                        move |evt: FormEvent| {
                            if let Ok(v) = evt.value().parse::<u32>() {
                                apply(v);
                            }
                        }
                    },
                }
                button {
                    class: "btn ghost icon-btn",
                    title: "Suggest {suggest} from ping",
                    onclick: move |_| apply(suggest),
                    icons::Wand {}
                }
            }
        }
    }
}
