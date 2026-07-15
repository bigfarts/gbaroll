//! The framebuffer effect registry. gbaroll ships only the nearest
//! pass-through; the [`Effect`] plumbing is the same as tango's, so GPU
//! upscalers can be dropped in later as extra WGSL fragments.

use crate::platform::video::framebuffer::Effect;

/// Shared infrastructure WGSL (vertex shader, bindings, `load`); prepended to
/// every effect.
pub(crate) const COMMON: &str = include_str!("common.wgsl");

/// Nearest pass-through.
pub const PASSTHROUGH: Effect = Effect {
    id: "",
    name: "—",
    scale: 1,
    parts: &[COMMON, include_str!("passthrough.wgsl")],
};

#[allow(dead_code)] // the picker registry, populated as effects are added
pub static EFFECTS: &[&Effect] = &[&PASSTHROUGH];

/// Resolve an effect key; unknown / empty keys fall back to pass-through.
#[allow(dead_code)]
pub fn effect_for(id: &str) -> &'static Effect {
    EFFECTS
        .iter()
        .find(|effect| effect.id == id)
        .cloned()
        .unwrap_or(&PASSTHROUGH)
}
