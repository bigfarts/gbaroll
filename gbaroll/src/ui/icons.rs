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
