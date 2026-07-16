//! gbaroll — a generic GBA link-cable rollback netplay client, in the
//! browser.
//!
//! Every GBA on the emulated cable (2 to 4) runs locally in one
//! `mgba_siolink::Link`; the link is the rollback unit and the only true
//! inputs are the joypads, so any link-capable game works with no
//! per-game code. Netplay is a full WebRTC mesh rendezvoused through the
//! `gbaroll-signaling` server; the data protocol is `rennet` frames over
//! an unreliable datachannel per peer.
//!
//! This crate builds for wasm32 only (`dx serve` / `dx build`); the
//! retired native desktop client lives at the `native-final` git tag.
//! Undeclared portable modules (session/, platform/, net/) are ported
//! in place, milestone by milestone.

#[cfg(not(target_arch = "wasm32"))]
compile_error!("gbaroll is browser-only: build with `dx serve` (wasm32-unknown-unknown)");

mod config;
mod library;
mod net;
mod nointro;
mod platform;
mod runtime;
mod session;
mod storage;
mod ui;
mod web;

fn main() {
    web::main();
}
