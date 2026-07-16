//! Inline lucide glyphs (<https://lucide.dev>, ISC license). Rendered
//! as 1em SVGs stroked with `currentColor`, so they follow the
//! surrounding text's size and colour with no per-icon styling.

use dioxus::prelude::*;

/// The shared frame: every lucide icon is a 24×24 outline with the
/// same stroke settings.
#[component]
fn Lucide(children: Element) -> Element {
    rsx! {
        svg {
            class: "icon",
            view_box: "0 0 24 24",
            width: "1em",
            height: "1em",
            fill: "none",
            stroke: "currentColor",
            stroke_width: "2",
            stroke_linecap: "round",
            stroke_linejoin: "round",
            {children}
        }
    }
}

#[component]
pub fn Play() -> Element {
    rsx! {
        Lucide { polygon { points: "6 3 20 12 6 21 6 3" } }
    }
}

#[component]
pub fn Pause() -> Element {
    rsx! {
        Lucide {
            rect { x: "14", y: "4", width: "4", height: "16", rx: "1" }
            rect { x: "6", y: "4", width: "4", height: "16", rx: "1" }
        }
    }
}

/// lucide `settings-2`.
#[component]
pub fn Sliders() -> Element {
    rsx! {
        Lucide {
            path { d: "M20 7h-9" }
            path { d: "M14 17H5" }
            circle { cx: "17", cy: "17", r: "3" }
            circle { cx: "7", cy: "7", r: "3" }
        }
    }
}

#[component]
pub fn Gamepad2() -> Element {
    rsx! {
        Lucide {
            line { x1: "6", x2: "10", y1: "11", y2: "11" }
            line { x1: "8", x2: "8", y1: "9", y2: "13" }
            line { x1: "15", x2: "15.01", y1: "12", y2: "12" }
            line { x1: "18", x2: "18.01", y1: "10", y2: "10" }
            path { d: "M17.32 5H6.68a4 4 0 0 0-3.978 3.59c-.006.052-.01.101-.017.152C2.604 9.416 2 14.456 2 16a3 3 0 0 0 3 3c1 0 1.5-.5 2-1l1.414-1.414A2 2 0 0 1 9.828 16h4.344a2 2 0 0 1 1.414.586L17 18c.5.5 1 1 2 1a3 3 0 0 0 3-3c0-1.545-.604-6.584-.685-7.258-.007-.05-.011-.1-.017-.151A4 4 0 0 0 17.32 5z" }
        }
    }
}

#[component]
pub fn Trash2() -> Element {
    rsx! {
        Lucide {
            path { d: "M3 6h18" }
            path { d: "M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6" }
            path { d: "M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2" }
            line { x1: "10", x2: "10", y1: "11", y2: "17" }
            line { x1: "14", x2: "14", y1: "11", y2: "17" }
        }
    }
}

#[component]
pub fn Download() -> Element {
    rsx! {
        Lucide {
            path { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }
            polyline { points: "7 10 12 15 17 10" }
            line { x1: "12", x2: "12", y1: "15", y2: "3" }
        }
    }
}

#[component]
pub fn Upload() -> Element {
    rsx! {
        Lucide {
            path { d: "M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" }
            polyline { points: "17 8 12 3 7 8" }
            line { x1: "12", x2: "12", y1: "3", y2: "15" }
        }
    }
}

#[component]
pub fn RefreshCw() -> Element {
    rsx! {
        Lucide {
            path { d: "M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8" }
            path { d: "M21 3v5h-5" }
            path { d: "M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16" }
            path { d: "M8 16H3v5" }
        }
    }
}

#[component]
pub fn X() -> Element {
    rsx! {
        Lucide {
            path { d: "M18 6 6 18" }
            path { d: "m6 6 12 12" }
        }
    }
}

#[component]
pub fn Keyboard() -> Element {
    rsx! {
        Lucide {
            path { d: "M10 8h.01" }
            path { d: "M12 12h.01" }
            path { d: "M14 8h.01" }
            path { d: "M16 12h.01" }
            path { d: "M18 8h.01" }
            path { d: "M6 8h.01" }
            path { d: "M7 16h10" }
            path { d: "M8 12h.01" }
            rect { width: "20", height: "16", x: "2", y: "4", rx: "2" }
        }
    }
}

#[component]
pub fn Cable() -> Element {
    rsx! {
        Lucide {
            path { d: "M17 21v-2a1 1 0 0 1-1-1v-1a2 2 0 0 1 2-2h2a2 2 0 0 1 2 2v1a1 1 0 0 1-1 1v2" }
            path { d: "M19 15V6.5a1 1 0 0 0-7 0v11a1 1 0 0 1-7 0V9" }
            path { d: "M21 21v-2h-4" }
            path { d: "M3 5h4V3" }
            path { d: "M7 5a1 1 0 0 1 1 1v1a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V6a1 1 0 0 1 1-1V3" }
        }
    }
}

#[component]
pub fn Unplug() -> Element {
    rsx! {
        Lucide {
            path { d: "m19 5 3-3" }
            path { d: "m2 22 3-3" }
            path { d: "M6.3 20.3a2.4 2.4 0 0 0 3.4 0L12 18l-6-6-2.3 2.3a2.4 2.4 0 0 0 0 3.4Z" }
            path { d: "M7.5 13.5 10 11" }
            path { d: "M10.5 16.5 13 14" }
            path { d: "m12 6 6 6 2.3-2.3a2.4 2.4 0 0 0 0-3.4l-2.6-2.6a2.4 2.4 0 0 0-3.4 0Z" }
        }
    }
}

#[component]
pub fn Gauge() -> Element {
    rsx! {
        Lucide {
            path { d: "m12 14 4-4" }
            path { d: "M3.34 19a10 10 0 1 1 17.32 0" }
        }
    }
}

#[component]
pub fn ArrowLeftRight() -> Element {
    rsx! {
        Lucide {
            path { d: "M8 3 4 7l4 4" }
            path { d: "M4 7h16" }
            path { d: "m16 21 4-4-4-4" }
            path { d: "M20 17H4" }
        }
    }
}

#[component]
pub fn Footprints() -> Element {
    rsx! {
        Lucide {
            path { d: "M4 16v-2.38C4 11.5 2.97 10.5 3 8c.03-2.72 1.49-6 4.5-6C9.37 2 10 3.8 10 5.5c0 3.11-2 5.66-2 8.68V16a2 2 0 1 1-4 0Z" }
            path { d: "M20 20v-2.38c0-2.12 1.03-3.12 1-5.62-.03-2.72-1.49-6-4.5-6C14.63 6 14 7.8 14 9.5c0 3.11 2 5.66 2 8.68V20a2 2 0 1 0 4 0Z" }
            path { d: "M16 17h4" }
            path { d: "M4 13h4" }
        }
    }
}

#[component]
pub fn GitMerge() -> Element {
    rsx! {
        Lucide {
            circle { cx: "18", cy: "18", r: "3" }
            circle { cx: "6", cy: "6", r: "3" }
            path { d: "M6 21V9a9 9 0 0 0 9 9" }
        }
    }
}

#[component]
pub fn Wifi() -> Element {
    rsx! {
        Lucide {
            path { d: "M12 20h.01" }
            path { d: "M2 8.82a15 15 0 0 1 20 0" }
            path { d: "M5 12.859a10 10 0 0 1 14 0" }
            path { d: "M8.5 16.429a5 5 0 0 1 7 0" }
        }
    }
}

#[component]
pub fn SignalHigh() -> Element {
    rsx! {
        Lucide {
            path { d: "M2 20h.01" }
            path { d: "M7 20v-4" }
            path { d: "M12 20v-8" }
            path { d: "M17 20V8" }
        }
    }
}

#[component]
pub fn SignalMedium() -> Element {
    rsx! {
        Lucide {
            path { d: "M2 20h.01" }
            path { d: "M7 20v-4" }
            path { d: "M12 20v-8" }
        }
    }
}

#[component]
pub fn SignalLow() -> Element {
    rsx! {
        Lucide {
            path { d: "M2 20h.01" }
            path { d: "M7 20v-4" }
        }
    }
}

#[component]
pub fn ChevronUp() -> Element {
    rsx! {
        Lucide { path { d: "m18 15-6-6-6 6" } }
    }
}

#[component]
pub fn Timer() -> Element {
    rsx! {
        Lucide {
            line { x1: "10", x2: "14", y1: "2", y2: "2" }
            line { x1: "12", x2: "15", y1: "14", y2: "11" }
            circle { cx: "12", cy: "14", r: "8" }
        }
    }
}
