//! The cable panel's offline/lobby body (the connected state lives in
//! `telemetry.rs`): room create/join, then the roster while the lobby
//! runs. One spawned drain task per lobby owns the event stream and
//! drives the whole plug-in flow — capture on `Starting`, ROM
//! resolution + `Runtime::plug_in` on `SessionReady`.

use std::cell::RefCell;

use dioxus::prelude::*;
use gbaroll_signaling::PlayerInfo;

use super::{icons, use_ctx, Ctx};
use crate::net::lobby::{self, LobbyCommand, LobbyEvent, LobbyMode};
use crate::runtime::{LINK_NOTICE, PANEL_OPEN, SESSION_EPOCH};

/// The roster mirror the panel renders; `None` = no lobby running.
pub static LOBBY_UI: GlobalSignal<Option<LobbyUi>> = Signal::global(|| None);

#[derive(Clone, Default)]
pub struct LobbyUi {
    pub code: Option<String>,
    pub players: Vec<PlayerInfo>,
    pub my_idx: usize,
    pub my_ready: bool,
    pub starting: bool,
    pub status: Option<String>,
}

thread_local! {
    /// The live lobby's command sender (the drain task owns the rest).
    static LOBBY_CMDS: RefCell<Option<futures::channel::mpsc::UnboundedSender<LobbyCommand>>> =
        const { RefCell::new(None) };
}

fn send_cmd(cmd: LobbyCommand) {
    LOBBY_CMDS.with(|c| {
        if let Some(tx) = c.borrow().as_ref() {
            let _ = tx.unbounded_send(cmd);
        }
    });
}

/// Leave any running lobby (also called by Quit game).
pub fn leave() {
    send_cmd(LobbyCommand::Leave);
    LOBBY_CMDS.with(|c| *c.borrow_mut() = None);
    *LOBBY_UI.write() = None;
}

/// A lobby or plug-in failure: let the (possibly frozen) game run again
/// and surface the reason in the panel.
fn link_failed(ctx: &Ctx, message: String) {
    log::warn!("{message}");
    if let Some(shared) = ctx.runtime.borrow().shared() {
        shared.resume();
    }
    *LINK_NOTICE.write() = Some(message);
    LOBBY_CMDS.with(|c| *c.borrow_mut() = None);
    *LOBBY_UI.write() = None;
}

/// Open a room from the running solo session's cable panel. The game
/// keeps running; the cable plugs in when the room starts.
fn start_lobby(ctx: &Ctx, mode: LobbyMode) {
    if LOBBY_UI.peek().is_some() {
        return;
    }
    let (server_url, nick) = {
        let config = ctx.config.read();
        (config.signaling_server.clone(), config.nick.clone())
    };
    let (rom_crc32, rom_title) = {
        let rt = ctx.runtime.borrow();
        let Some(crc) = rt.descriptor().and_then(|d| d.rom_crc32) else {
            *LINK_NOTICE.write() = Some("this session can't host a cable".to_string());
            return;
        };
        let lib = ctx.library.read();
        let Some(info) = lib
            .as_ref()
            .and_then(|v| v.as_ref())
            .and_then(|(lib, _)| lib.by_crc32(crc))
        else {
            *LINK_NOTICE.write() =
                Some("this game's ROM is missing from the library".to_string());
            return;
        };
        // The wire carries stable cartridge metadata. Every client
        // resolves the CRC32 through its own DAT-backed library.
        (info.crc32, info.title.clone())
    };

    let handle = lobby::spawn(lobby::LobbyArgs {
        server_url,
        nick,
        rom_crc32,
        rom_title,
        mode,
    });
    *LINK_NOTICE.write() = None;
    *LOBBY_UI.write() = Some(LobbyUi::default());
    *PANEL_OPEN.write() = true;
    drain(ctx.clone(), handle);
}

/// The per-lobby event loop: mirror roster state into [`LOBBY_UI`],
/// answer `Starting` with the machine capture, and finish with
/// `Runtime::plug_in`.
fn drain(ctx: Ctx, handle: lobby::LobbyHandle) {
    // Keep the sender for the panel's buttons; the handle (and its
    // Drop-sends-Leave) lives in the task.
    let mut handle = handle;
    LOBBY_CMDS.with(|c| *c.borrow_mut() = Some(handle.sender()));
    spawn(async move {
        use futures::StreamExt;
        while let Some(event) = handle.events.next().await {
            match event {
                LobbyEvent::Joined { code } => {
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        ui.code = Some(code);
                        ui.status = None;
                    }
                }
                LobbyEvent::Roster { players, your_idx } => {
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        // Occupancy changes reset ready state server-side;
                        // mirror what the server reports for us.
                        ui.my_ready = players.get(your_idx).map(|p| p.ready).unwrap_or(false);
                        ui.players = players;
                        ui.my_idx = your_idx;
                        if !ui.starting {
                            ui.status = None;
                        }
                    }
                }
                LobbyEvent::Error(message) => {
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        ui.starting = false;
                        ui.status = Some(message);
                    }
                }
                LobbyEvent::Starting => {
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        ui.starting = true;
                    }
                    // The cable is being plugged in: freeze the machine
                    // and ship its capture.
                    match ctx.runtime.borrow_mut().capture_boot_blob() {
                        Ok(blob) => handle.send(LobbyCommand::Boot(blob)),
                        Err(e) => {
                            link_failed(&ctx, format!("couldn't capture the machine: {e:#}"));
                            return;
                        }
                    }
                }
                LobbyEvent::Connecting(message) => {
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        ui.starting = true;
                        ui.status = Some(message);
                    }
                }
                LobbyEvent::Fatal(message) => {
                    link_failed(&ctx, message);
                    return;
                }
                LobbyEvent::SessionReady(bundle) => {
                    // Resolve every player's ROM out of OPFS by CRC32.
                    let mut roms = Vec::new();
                    let (storage, lib) = {
                        let storage = ctx.storage.read().clone().flatten();
                        let lib = ctx
                            .library
                            .read()
                            .clone()
                            .flatten()
                            .map(|(lib, _)| lib)
                            .unwrap_or_default();
                        (storage, lib)
                    };
                    let Some(storage) = storage else {
                        link_failed(&ctx, "storage unavailable".to_string());
                        return;
                    };
                    for player in &bundle.players {
                        let Some(info) = lib.by_crc32(player.rom_crc32) else {
                            link_failed(
                                &ctx,
                                format!(
                                    "missing a copy of {}'s ROM (crc32 {:08x})",
                                    player.nick, player.rom_crc32
                                ),
                            );
                            return;
                        };
                        match crate::library::read_rom(&storage, info).await {
                            Ok(bytes) => roms.push(bytes),
                            Err(e) => {
                                link_failed(&ctx, format!("{e:#}"));
                                return;
                            }
                        }
                    }
                    let present_delay = ctx.config.read().present_delay;
                    let plugged = ctx
                        .runtime
                        .borrow_mut()
                        .plug_in(*bundle, roms, present_delay);
                    match plugged {
                        Ok(()) => {
                            LOBBY_CMDS.with(|c| *c.borrow_mut() = None);
                            *LOBBY_UI.write() = None;
                            *LINK_NOTICE.write() = None;
                        }
                        Err(e) => link_failed(&ctx, format!("couldn't plug in: {e:#}")),
                    }
                    return;
                }
            }
        }
        // The lobby task ended without a session (server closed, etc.).
        LOBBY_CMDS.with(|c| *c.borrow_mut() = None);
        if LOBBY_UI.peek().is_some() {
            *LOBBY_UI.write() = None;
        }
    });
}

/// Keep room-code entry aligned with the server's six-character,
/// ambiguity-free alphabet.
fn sanitize_room_code(input: &str) -> String {
    input
        .chars()
        .flat_map(char::to_uppercase)
        .filter(|c| c.is_ascii() && gbaroll_signaling::ROOM_CODE_ALPHABET.contains(&(*c as u8)))
        .take(gbaroll_signaling::ROOM_CODE_LEN)
        .collect()
}

/// The panel body while the cable is out: create/join, then the roster.
/// (The connected state is `telemetry::CableOverlay`'s card.)
#[component]
pub fn CableBody() -> Element {
    let ctx = use_ctx();
    let mut code_entry = use_signal(String::new);

    let _ = SESSION_EPOCH.read();
    let notice = LINK_NOTICE.read().clone();
    let lobby_ui = LOBBY_UI.read().clone();

    // My library, for the "you need a copy of every ROM" roster check —
    // and for each seat's friendly name: the wire only carries the
    // header title, but the No-Intro name resolves locally by CRC32.
    let (have_crc, roster_names) = {
        let lib = ctx.library.read();
        let lib = lib.as_ref().and_then(|v| v.as_ref()).map(|(lib, _)| lib);
        let crcs: std::collections::HashSet<u32> = lib
            .map(|lib| lib.roms.iter().map(|r| r.crc32).collect())
            .unwrap_or_default();
        let names: Vec<String> = lobby_ui
            .as_ref()
            .map(|ui| {
                ui.players
                    .iter()
                    .map(|p| {
                        lib.and_then(|l| l.by_crc32(p.rom_crc32))
                            .map(|r| r.display_name().to_string())
                            .unwrap_or_else(|| p.rom_title.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        (crcs, names)
    };

    rsx! {
        div { class: "cable",
            if let Some(notice) = notice {
                p { class: "link-notice", "{notice}" }
            }
            if let Some(ui) = lobby_ui {
                // Lobby: the room code, the roster, waiting for the start.
                if let Some(code) = &ui.code {
                    super::telemetry::RoomCode { code: code.clone() }
                } else {
                    p { class: "sub", "Connecting to the server…" }
                }
                div { class: "roster",
                    for (idx, player) in ui.players.iter().enumerate() {
                        div { class: "roster-row",
                            span {
                                class: if player.ready { "roster-ready ready" } else { "roster-ready" },
                                if player.ready {
                                    icons::Check {}
                                } else {
                                    "·"
                                }
                            }
                            // The seat's identity colour — the same one
                            // its ping trace uses once the cable is in.
                            span {
                                class: "dot",
                                style: "background: {super::telemetry::PEER_COLORS[idx % super::telemetry::PEER_COLORS.len()]}",
                            }
                            div { class: "roster-name",
                                span { "{player.nick}" }
                                span { class: "game-title", "{roster_names[idx]}" }
                            }
                            if idx == ui.my_idx {
                                span { class: "you-badge", "you" }
                            } else if !have_crc.contains(&player.rom_crc32) {
                                span { class: "link-notice", "missing this ROM" }
                            }
                        }
                    }
                }
                if let Some(status) = &ui.status {
                    p { class: "sub", "{status}" }
                }
                div { class: "menu-actions",
                    if ui.my_idx == 0 {
                        // The host's seat is always ready; they just start
                        // the room once everyone else is.
                        button {
                            class: "btn primary",
                            disabled: ui.starting || !ui.players.iter().all(|p| p.ready),
                            onclick: move |_| {
                                if let Some(ui) = LOBBY_UI.write().as_mut() {
                                    ui.starting = true;
                                    ui.status = Some("Starting the room…".to_string());
                                }
                                send_cmd(LobbyCommand::Start);
                            },
                            "Start"
                        }
                    } else {
                        button {
                            class: if ui.my_ready { "btn" } else { "btn primary" },
                            disabled: ui.starting
                                || ui.players.iter().any(|p| !have_crc.contains(&p.rom_crc32)),
                            onclick: {
                                let ready = !ui.my_ready;
                                move |_| {
                                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                                        ui.my_ready = ready;
                                        ui.status = None;
                                    }
                                    send_cmd(LobbyCommand::SetReady { ready });
                                }
                            },
                            if ui.my_ready { "Unready" } else { "Ready up" }
                        }
                    }
                    button {
                        class: "btn",
                        onclick: {
                            let ctx = ctx.clone();
                            move |_| {
                                leave();
                                // If start already froze the machine for its
                                // capture, resume it on a fresh deadline.
                                if let Some(shared) = ctx.runtime.borrow().shared() {
                                    shared.resume();
                                }
                            }
                        },
                        "Leave"
                    }
                }
                if ui.my_idx == 0 && !ui.starting && !ui.players.iter().all(|p| p.ready) {
                    p { class: "hint", "Waiting for everyone to ready up." }
                }
            } else {
                // Offline: host a new room or join a friend's.
                button {
                    class: "btn primary wide",
                    onclick: {
                        let ctx = ctx.clone();
                        move |_| start_lobby(&ctx, LobbyMode::Create)
                    },
                    icons::Cable {}
                    "Create a room"
                }
                p { class: "sub", "You'll get a code to share with the other players." }
                div { class: "or-divider", "or" }
                div { class: "join-row",
                    input {
                        r#type: "text",
                        placeholder: "6-character code",
                        value: "{code_entry}",
                        oninput: move |evt: FormEvent| {
                            code_entry.set(sanitize_room_code(&evt.value()));
                        },
                        // Enter submits once the code is complete.
                        onkeydown: {
                            let ctx = ctx.clone();
                            move |evt: KeyboardEvent| {
                                let code = code_entry.read().clone();
                                if evt.key().to_string() == "Enter"
                                    && code.len() == gbaroll_signaling::ROOM_CODE_LEN
                                {
                                    start_lobby(&ctx, LobbyMode::Join { code });
                                }
                            }
                        },
                    }
                    button {
                        class: "btn primary",
                        disabled: code_entry.read().len() != gbaroll_signaling::ROOM_CODE_LEN,
                        onclick: {
                            let ctx = ctx.clone();
                            move |_| {
                                let code = code_entry.read().clone();
                                start_lobby(&ctx, LobbyMode::Join { code });
                            }
                        },
                        "Join"
                    }
                }
            }
        }
    }
}
