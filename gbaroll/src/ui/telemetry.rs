//! The unified cable overlay — a top-right status chip that expands to
//! room setup / the lobby while offline, then to link telemetry and
//! disconnect controls while connected. The clock metrics (TPS, skew,
//! lead, rollback depth) are one shared reading for the whole link —
//! the engine's skew/queue are already worst-case over every remote —
//! while ping is per-peer, one card each, so the panel grows with the
//! number of clients instead of assuming a single opponent.
//!
//! Sparklines draw on small 2D canvases from the runtime's sample ring,
//! redrawn per presented frame while the panel is open.

use dioxus::prelude::*;
use wasm_bindgen::JsCast;

use super::{cable, icons, use_ctx};
use crate::runtime::{FRAME_REV, PANEL_OPEN, SESSION_EPOCH};
use crate::session::{MetricSample, SessionKind, HISTORY_LEN};

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

fn metrics_from(history: &std::collections::VecDeque<MetricSample>, peer_nicks: &[String]) -> Vec<Metric> {
    let latest = history.back();
    let mut metrics = vec![
        Metric {
            canvas_id: "tele-tps",
            icon: || rsx! { icons::Gauge {} },
            caption: "Ticks/sec".to_string(),
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
            value: latest.map(|s| format!("{:.0}", s.tps)).unwrap_or("—".into()),
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
            caption: "Rollback".to_string(),
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
    ];

    // One ping card per peer, captioned with the peer's nick — this is
    // what makes the panel scale with the number of clients.
    const PING_IDS: [&str; 3] = ["tele-ping-0", "tele-ping-1", "tele-ping-2"];
    for (i, nick) in peer_nicks.iter().enumerate().take(PING_IDS.len()) {
        let latest_ping = latest.and_then(|s| s.pings.get(i).copied().flatten());
        metrics.push(Metric {
            canvas_id: PING_IDS[i],
            icon: || rsx! { icons::Wifi {} },
            caption: nick.clone(),
            fill_under: true,
            zero: None,
            points: history
                .iter()
                .map(|s| {
                    s.pings
                        .get(i)
                        .copied()
                        .flatten()
                        .map(|ms| ((ms / PING_SPAN).clamp(0.0, 1.0), tone_for_ping(ms)))
                })
                .collect(),
            value: latest_ping
                .map(|ms| format!("{ms:.0} ms"))
                .unwrap_or("—".into()),
            value_tone: latest_ping.map(tone_for_ping).unwrap_or(Tone::Muted),
        });
    }
    metrics
}

/// Redraw one sparkline: a hairline trace over a recessed background,
/// tone-coloured per segment, newest sample pinned to the right edge.
fn draw_sparkline(canvas_id: &str, points: &Points, fill_under: bool, zero: Option<f32>) {
    let Some(canvas) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.get_element_by_id(canvas_id))
        .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok())
    else {
        return;
    };
    let Some(ctx) = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|c| c.dyn_into::<web_sys::CanvasRenderingContext2d>().ok())
    else {
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

/// The signal-strength glyph for the collapsed chip, keyed off skew.
fn signal_icon(skew: i32) -> Element {
    match skew.unsigned_abs() {
        0..=3 => rsx! { icons::SignalHigh {} },
        4..=7 => rsx! { icons::SignalMedium {} },
        _ => rsx! { icons::SignalLow {} },
    }
}

/// The top-right cable overlay: collapsed status chip or expanded panel.
#[component]
pub fn CableOverlay() -> Element {
    let ctx = use_ctx();
    let _ = SESSION_EPOCH.read();
    let _ = FRAME_REV.read();
    let expanded = *PANEL_OPEN.read();

    let (is_netplay, skew, peer_nicks, room_code) = {
        let rt = ctx.runtime.borrow();
        match rt.descriptor() {
            Some(d) if d.kind == SessionKind::Netplay => {
                let stats = rt.shared().map(|s| s.stats.lock().unwrap().clone());
                (
                    true,
                    stats.as_ref().map(|s| s.skew).unwrap_or(0),
                    stats
                        .map(|s| s.peers.iter().map(|p| p.nick.clone()).collect::<Vec<_>>())
                        .unwrap_or_default(),
                    d.room_code.clone(),
                )
            }
            _ => (false, 0, Vec::new(), None),
        }
    };

    // Redraw the sparklines after every rendered frame while expanded.
    let metrics_snapshot: Vec<(String, Points, bool, Option<f32>)> = if is_netplay && expanded {
        let rt = ctx.runtime.borrow();
        metrics_from(rt.metric_history(), &peer_nicks)
            .into_iter()
            .map(|m| (m.canvas_id.to_string(), m.points, m.fill_under, m.zero))
            .collect()
    } else {
        Vec::new()
    };
    use_effect(use_reactive!(|metrics_snapshot| {
        for (id, points, fill_under, zero) in &metrics_snapshot {
            draw_sparkline(id, points, *fill_under, *zero);
        }
    }));

    let lobby_running = cable::LOBBY_UI.read().is_some();

    rsx! {
        div { class: "cable-overlay",
            if !expanded {
                // Collapsed: one status chip.
                button {
                    class: "btn status-chip",
                    onclick: move |_| *PANEL_OPEN.write() = true,
                    if is_netplay {
                        {signal_icon(skew)}
                        span { class: "mono", style: "color: {tone_for_abs(skew, 3, 7).css()}", "{skew:+}" }
                    } else if lobby_running {
                        icons::Gamepad2 {}
                        "Netplay lobby"
                    } else {
                        icons::Cable {}
                        "Link cable"
                    }
                }
            } else if is_netplay {
                // Expanded, connected: the telemetry card.
                div { class: "tele-card",
                    div { class: "tele-head",
                        h3 { "Link telemetry" }
                        button {
                            class: "btn ghost icon-btn",
                            onclick: move |_| *PANEL_OPEN.write() = false,
                            icons::ChevronUp {}
                        }
                    }
                    if let Some(code) = &room_code {
                        RoomCode { code: code.clone() }
                    }
                    TelemetryCards { ctx_key: FRAME_REV() }
                    DelayControl {}
                    div { class: "menu-actions",
                        button {
                            class: "btn danger",
                            onclick: {
                                let ctx = ctx.clone();
                                move |_| ctx.runtime.borrow().unplug()
                            },
                            icons::Unplug {}
                            "Disconnect cable"
                        }
                    }
                    p { class: "hint", "Your local game keeps running after disconnecting." }
                }
            } else {
                // Expanded, offline/lobby: room setup and the roster.
                div { class: "tele-card",
                    div { class: "tele-head",
                        h3 { if lobby_running { "Netplay lobby" } else { "Link cable" } }
                        button {
                            class: "btn ghost icon-btn",
                            onclick: move |_| *PANEL_OPEN.write() = false,
                            icons::ChevronUp {}
                        }
                    }
                    cable::CableBody {}
                }
            }
        }
    }
}

/// The metric cards; a separate component so the caption/value text
/// re-renders per frame (`ctx_key` is FRAME_REV).
#[component]
fn TelemetryCards(ctx_key: u64) -> Element {
    let ctx = use_ctx();
    let rt = ctx.runtime.borrow();
    let peer_nicks: Vec<String> = rt
        .shared()
        .map(|s| s.stats.lock().unwrap().peers.iter().map(|p| p.nick.clone()).collect())
        .unwrap_or_default();
    let metrics = metrics_from(rt.metric_history(), &peer_nicks);
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
                    span { class: "metric-value mono", style: "color: {m.value_tone.css()}", "{m.value}" }
                }
            }
        }
    }
}

/// A deliberately button-shaped room code: click to copy. Shared with
/// the lobby body, which shows the same code before the cable is in.
#[component]
pub fn RoomCode(code: String) -> Element {
    let mut copied = use_signal(|| false);
    rsx! {
        button {
            class: if copied() { "btn primary room-code-btn" } else { "btn room-code-btn" },
            onclick: {
                let code = code.clone();
                move |_| {
                    let code = code.clone();
                    spawn(async move {
                        let clipboard = web_sys::window().unwrap().navigator().clipboard();
                        let _ =
                            wasm_bindgen_futures::JsFuture::from(clipboard.write_text(&code)).await;
                        copied.set(true);
                        gloo_timers::future::TimeoutFuture::new(1_500).await;
                        copied.set(false);
                    });
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
                span { "Input delay" }
                span { class: "metric-value mono", "{present_delay}" }
            }
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
                class: "btn ghost",
                onclick: move |_| apply(suggest),
                "Suggest {suggest} from ping"
            }
        }
    }
}
