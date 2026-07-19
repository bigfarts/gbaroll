//! The link panel's room body: create/join, then the roster for as long
//! as this machine is in the room — rooms hold dynamic membership, so
//! the roster (and the code, for inviting more players) stays live
//! while a merged session runs; `telemetry.rs` embeds [`RoomSection`]
//! for that. One spawned drain task per room owns the event stream and
//! drives every merge — walk out of a running link, capture on
//! `Starting`, ROM resolution + `Runtime::plug_in` on `SessionReady` —
//! repeatedly, as membership changes bring re-merges.

use std::cell::RefCell;

use dioxus::dioxus_core::spawn_forever;
use dioxus::prelude::*;

use super::{icons, use_ctx, Ctx};
use crate::net::lobby::{self, LobbyCommand, LobbyEvent, LobbyMode};
use crate::runtime::{LINK_NOTICE, PANEL_OPEN, SESSION_EPOCH};
use crate::session::SessionKind;

/// The roster mirror the panel renders; `None` = not in a room.
pub static LOBBY_UI: GlobalSignal<Option<LobbyUi>> = Signal::global(|| None);

#[derive(Clone, Default)]
pub struct LobbyUi {
    pub code: Option<String>,
    pub players: Vec<gbaroll_signaling::PlayerInfo>,
    pub my_idx: usize,
    /// A merge is in flight (capture/mesh/exchange).
    pub starting: bool,
    pub status: Option<String>,
    /// This client created the room (vs. joined one) — drives the code
    /// auto-copy when the server assigns it. Known at lobby start,
    /// unlike `my_idx`, which is a default 0 until the first roster.
    pub created: bool,
}

thread_local! {
    /// The live room's command sender (the drain task owns the rest).
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

/// Leave any running room (also called by Quit game).
pub fn leave() {
    send_cmd(LobbyCommand::Leave);
    LOBBY_CMDS.with(|c| *c.borrow_mut() = None);
    *LOBBY_UI.write() = None;
}

/// Whether this machine holds a room with anyone else still in it —
/// i.e. a re-merge is worth waiting for.
pub fn room_has_company() -> bool {
    LOBBY_UI
        .peek()
        .as_ref()
        .is_some_and(|ui| ui.players.len() >= 2)
}

/// The merged session died without our say-so (a peer walked out of
/// range). If this machine still holds the room, the lobby dances the
/// ready bit so the server voids the dead merge and re-merges the room
/// once it converges. Called by the runtime's unplug-continue path.
/// `held` reports that the runtime froze the continuation for the
/// re-merge (wireless: running it would sever the survivors' live
/// connections on the first RF tick) — surface why the game is
/// standing still.
pub fn notify_session_dropped(held: bool) {
    send_cmd(LobbyCommand::Dropped);
    if held {
        if let Some(ui) = LOBBY_UI.write().as_mut() {
            ui.status = Some("Player left — waiting to relink…".to_string());
        }
    }
}

/// A room or merge failure that ends our membership: let the (possibly
/// frozen) game run again and surface the reason in the panel.
fn link_failed(ctx: &Ctx, message: String) {
    log::warn!("{message}");
    if let Some(shared) = ctx.runtime.borrow().shared() {
        shared.resume();
    }
    *LINK_NOTICE.write() = Some(message);
    LOBBY_CMDS.with(|c| *c.borrow_mut() = None);
    *LOBBY_UI.write() = None;
}

/// Open a room from the running solo session's link panel. The game
/// keeps running; the link merges as members arrive.
fn start_lobby(ctx: &Ctx, mode: LobbyMode) {
    if LOBBY_UI.peek().is_some() {
        return;
    }
    let server_url = crate::config::signaling_server();
    let nick = ctx.config.read().nick.clone();
    let (rom_crc32, rom_title, wireless) = {
        let rt = ctx.runtime.borrow();
        let Some(desc) = rt.descriptor() else {
            *LINK_NOTICE.write() = Some("this session can't host a link".to_string());
            return;
        };
        let link = desc.link;
        let Some(crc) = desc.rom_crc32 else {
            *LINK_NOTICE.write() = Some("this session can't host a link".to_string());
            return;
        };
        let lib = ctx.library.read();
        let Some(info) = lib
            .as_ref()
            .and_then(|v| v.as_ref())
            .and_then(|lib| lib.by_crc32(crc))
        else {
            *LINK_NOTICE.write() =
                Some("this game's ROM is missing from the library".to_string());
            return;
        };
        // The wire carries stable cartridge metadata. Every client
        // resolves the CRC32 through its own DAT-backed library.
        (
            info.crc32,
            info.title.clone(),
            link == crate::session::LinkKind::Wireless,
        )
    };

    let created = matches!(mode, LobbyMode::Create);
    let handle = lobby::spawn(lobby::LobbyArgs {
        server_url,
        nick,
        rom_crc32,
        rom_title,
        wireless,
        mode,
    });
    *LINK_NOTICE.write() = None;
    *LOBBY_UI.write() = Some(LobbyUi {
        created,
        ..Default::default()
    });
    *PANEL_OPEN.write() = true;
    drain(ctx.clone(), handle);
}

/// How long a merge waits for the pump to swap a running link out for
/// its solo continuation before the capture can happen (50ms tries).
const UNPLUG_WAIT_TRIES: usize = 100;

/// The per-room event loop: mirror roster state into [`LOBBY_UI`],
/// answer every `Starting` with a machine capture (walking out of a
/// running link first), and hand each `SessionReady` to
/// `Runtime::plug_in` — the room outlives its merges.
///
/// `spawn_forever`, not `spawn`: a scope-bound task dies with the
/// component whose event handler spawned it, and this one is started
/// from CableBody — which unmounts the moment a merge swaps the panel
/// to the telemetry card (or the panel just collapses). Cancelling the
/// task drops the handle, whose Drop sends Leave: every member would
/// quit the room at the first merge, deleting it out from under later
/// joiners. The room must outlive any component, so the task hangs off
/// the root scope and ends only when the lobby's event stream does.
fn drain(ctx: Ctx, handle: lobby::LobbyHandle) {
    // Keep the sender for the panel's buttons; the handle (and its
    // Drop-sends-Leave) lives in the task.
    let mut handle = handle;
    LOBBY_CMDS.with(|c| *c.borrow_mut() = Some(handle.sender()));
    spawn_forever(async move {
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
                    // Auto-report whether this side holds every ROM in
                    // the roster (the wire's ready bit): the room
                    // merges once every member reports complete. The
                    // server resets the bit on every occupancy change,
                    // so each roster is re-asserted; sending only on
                    // mismatch keeps the echo from looping.
                    let have_all = {
                        let lib = ctx.library.read();
                        let lib = lib.as_ref().and_then(|v| v.as_ref());
                        players
                            .iter()
                            .all(|p| lib.is_some_and(|l| l.by_crc32(p.rom_crc32).is_some()))
                    };
                    if players.get(your_idx).map(|p| p.ready) != Some(have_all) {
                        handle.send(LobbyCommand::SetReady { ready: have_all });
                    }
                    // A wireless continuation may be frozen against the
                    // re-merge (see `notify_session_dropped`): a merge
                    // isn't in flight, yet the machine is paused. A
                    // roster with company keeps that hold — and its
                    // status line — while the room converges; one
                    // shrunk to just us means nobody is coming, so the
                    // machine plays on solo and its departed peers
                    // fall out of range for real.
                    let held = !LOBBY_UI.peek().as_ref().is_some_and(|ui| ui.starting)
                        && ctx.runtime.borrow().shared().is_some_and(|s| {
                            s.paused.load(std::sync::atomic::Ordering::Acquire)
                        });
                    if held && players.len() < 2 {
                        if let Some(shared) = ctx.runtime.borrow().shared() {
                            shared.resume();
                        }
                    }
                    let keep_status = held && players.len() >= 2;
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        ui.players = players;
                        ui.my_idx = your_idx;
                        if !ui.starting && !keep_status {
                            ui.status = None;
                        }
                    }
                }
                LobbyEvent::Error(message) => {
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        ui.status = Some(message);
                    }
                }
                LobbyEvent::Starting => {
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        ui.starting = true;
                        ui.status = Some("Linking up…".to_string());
                    }
                    // The merge is beginning: freeze the machine and
                    // ship its capture. A running link is walked out of
                    // first — captures come only from a solo machine —
                    // waiting for the pump to swap in the continuation.
                    let netplay = ctx
                        .runtime
                        .borrow()
                        .descriptor()
                        .map(|d| d.kind == SessionKind::Netplay)
                        .unwrap_or(false);
                    if netplay {
                        ctx.runtime.borrow().unplug();
                    }
                    let mut captured = None;
                    for _ in 0..UNPLUG_WAIT_TRIES {
                        match ctx.runtime.borrow().descriptor().map(|d| d.kind) {
                            Some(SessionKind::Local) => break,
                            Some(SessionKind::Netplay) => {}
                            None => break,
                        }
                        gloo_timers::future::TimeoutFuture::new(50).await;
                    }
                    if ctx.runtime.borrow().descriptor().map(|d| d.kind)
                        == Some(SessionKind::Local)
                    {
                        captured = Some(ctx.runtime.borrow_mut().capture_boot_blob());
                    }
                    match captured {
                        Some(Ok(blob)) => handle.send(LobbyCommand::Boot(blob)),
                        Some(Err(e)) => {
                            link_failed(&ctx, format!("couldn't capture the machine: {e:#}"));
                            return;
                        }
                        None => {
                            link_failed(
                                &ctx,
                                "the game ended before the room linked up".to_string(),
                            );
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
                LobbyEvent::MergeFailed(message) => {
                    // The room stands; the lobby is arranging a retry.
                    // Let the frozen machine play solo meanwhile.
                    log::warn!("{message}");
                    if let Some(shared) = ctx.runtime.borrow().shared() {
                        shared.resume();
                    }
                    if let Some(ui) = LOBBY_UI.write().as_mut() {
                        ui.starting = false;
                        ui.status = Some(message);
                    }
                }
                LobbyEvent::Kicked => {
                    // Walked out of range by decree: the link goes too.
                    if ctx
                        .runtime
                        .borrow()
                        .descriptor()
                        .map(|d| d.kind == SessionKind::Netplay)
                        .unwrap_or(false)
                    {
                        ctx.runtime.borrow().unplug();
                    }
                    link_failed(&ctx, "The room's creator removed you.".to_string());
                    return;
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
                            .unwrap_or_default();
                        (storage, lib)
                    };
                    let Some(storage) = storage else {
                        link_failed(&ctx, "storage unavailable".to_string());
                        return;
                    };
                    let mut missing = None;
                    for player in &bundle.players {
                        let Some(info) = lib.by_crc32(player.rom_crc32) else {
                            missing = Some(format!(
                                "missing a copy of {}'s ROM (crc32 {:08x})",
                                player.nick, player.rom_crc32
                            ));
                            break;
                        };
                        match crate::library::read_rom(&storage, info).await {
                            Ok(bytes) => roms.push(bytes),
                            Err(e) => {
                                missing = Some(format!("{e:#}"));
                                break;
                            }
                        }
                    }
                    if let Some(message) = missing {
                        link_failed(&ctx, message);
                        return;
                    }
                    let present_delay = ctx.config.read().present_delay;
                    let plugged = ctx
                        .runtime
                        .borrow_mut()
                        .plug_in(*bundle, roms, present_delay);
                    match plugged {
                        Ok(()) => {
                            // The room lives on — the next membership
                            // change brings the next merge.
                            if let Some(ui) = LOBBY_UI.write().as_mut() {
                                ui.starting = false;
                                ui.status = None;
                            }
                            *LINK_NOTICE.write() = None;
                        }
                        Err(e) => {
                            link_failed(&ctx, format!("couldn't link up: {e:#}"));
                            return;
                        }
                    }
                }
            }
        }
        // The room task ended without a session (left, server closed,
        // etc.). However it went, never leave the machine frozen: a
        // wireless continuation held for a re-merge that can no longer
        // come must play on solo.
        if let Some(shared) = ctx.runtime.borrow().shared() {
            shared.resume();
        }
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

/// The dropped link card while no session is merged: header + body.
/// (The merged state is `telemetry::TelemetryCard`, which embeds
/// [`RoomSection`] so the room stays visible mid-session.)
#[component]
pub fn CableCard() -> Element {
    let ctx = use_ctx();
    // The session's peripheral was fixed at launch; the card names it.
    let link = ctx
        .runtime
        .borrow()
        .descriptor()
        .map(|d| d.link)
        .unwrap_or_default();
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
            CableBody {}
        }
    }
}

/// The room's code and roster — everything another player needs to be
/// invited, and everything the creator needs to manage seats. Shared
/// between the offline panel body and the connected telemetry card;
/// renders nothing when this machine isn't in a room.
#[component]
pub fn RoomSection() -> Element {
    let ctx = use_ctx();
    let Some(ui) = LOBBY_UI.read().clone() else {
        return rsx! {};
    };
    let wireless = ctx
        .runtime
        .borrow()
        .descriptor()
        .map(|d| d.link == crate::session::LinkKind::Wireless)
        .unwrap_or(false);
    // Every seat the room's peripheral offers, the unfilled ones drawn
    // as open slots: four for a cable chain, five (a host and its four
    // clients — one full RFU group) for a wireless room.
    let seats = gbaroll_signaling::max_players(wireless);

    // My library, for the "you need a copy of every ROM" roster check —
    // and for each seat's friendly name: the wire only carries the
    // header title, but the No-Intro name resolves locally by CRC32.
    // The DAT knows names for ROMs the library doesn't hold, so even a
    // missing ROM reads by its proper name.
    let (have_crc, roster_names) = {
        let lib = ctx.library.read();
        let lib = lib.as_ref().and_then(|v| v.as_ref());
        let dat = ctx.dat.read();
        let crcs: std::collections::HashSet<u32> = lib
            .map(|lib| lib.roms.iter().map(|r| r.crc32).collect())
            .unwrap_or_default();
        let names: Vec<String> = ui
            .players
            .iter()
            .map(|p| {
                lib.and_then(|l| l.by_crc32(p.rom_crc32))
                    .map(|r| r.display_name().to_string())
                    .or_else(|| {
                        dat.as_ref()
                            .and_then(|d| d.lookup(p.rom_crc32))
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| p.rom_title.clone())
            })
            .collect();
        (crcs, names)
    };

    rsx! {
        if let Some(code) = &ui.code {
            super::telemetry::RoomCode { code: code.clone(), auto_copy: ui.created }
        } else {
            // The code button's exact footprint, so the panel
            // doesn't reflow when the connection lands.
            button {
                class: "btn room-code-btn",
                disabled: true,
                span { class: "room-code-label",
                    span { class: "sub", "LINK CODE" }
                    code { class: "room-code", "······" }
                }
                "Connecting…"
            }
        }
        div { class: "roster",
            for idx in 0..seats {
                if let Some(player) = ui.players.get(idx) {
                    div { class: "roster-row",
                        // The seat's identity colour — the same one
                        // its ping trace uses while a session is
                        // merged.
                        span {
                            class: "dot",
                            style: "background: {super::telemetry::PEER_COLORS[idx % super::telemetry::PEER_COLORS.len()]}",
                        }
                        div { class: "roster-name",
                            span {
                                if player.nick.is_empty() {
                                    "Player {idx + 1}"
                                } else {
                                    "{player.nick}"
                                }
                            }
                            span { class: "game-title", "{roster_names[idx]}" }
                        }
                        if idx == ui.my_idx {
                            span { class: "you-badge", "you" }
                        } else if !have_crc.contains(&player.rom_crc32) {
                            // *I* lack this seat's game.
                            span { class: "link-notice", "missing this ROM" }
                        } else if !player.ready {
                            // The seat hasn't reported holding
                            // every ROM in the roster.
                            span { class: "link-notice", "missing a ROM" }
                        }
                        if ui.my_idx == 0 && idx != 0 && !ui.starting {
                            // The creator's eject control. Kicks
                            // address the stable seat token, so
                            // one racing a departure bounces
                            // rather than hitting whoever slid
                            // into this row.
                            button {
                                class: "btn ghost icon-btn kick-btn",
                                title: "Remove this player",
                                onclick: {
                                    let seat = player.seat;
                                    move |_| send_cmd(LobbyCommand::Kick { seat })
                                },
                                icons::X {}
                            }
                        }
                    }
                } else {
                    div { class: "roster-row open",
                        span { class: "dot hollow" }
                        span { "Open slot" }
                        // A zero-width ghost of the two-line name
                        // block props the row to a seat's exact
                        // height (font metrics vary across the
                        // Comic fallbacks, so no fixed min-height
                        // holds); dot and label centre against it.
                        div { class: "roster-name",
                            span { "\u{a0}" }
                            span { class: "game-title", "\u{a0}" }
                        }
                    }
                }
            }
        }
    }
}

/// Position 0's link-up control for a cable room — the explicit merge
/// trigger (wireless rooms merge on their own, so this renders nothing
/// there). Shared with the telemetry card, where the same press is the
/// re-link that folds a late joiner in once everyone is back at a link
/// menu. Renders nothing for other members or without a live room.
#[component]
pub fn LinkUpButton() -> Element {
    let ctx = use_ctx();
    let cable = ctx
        .runtime
        .borrow()
        .descriptor()
        .map(|d| d.link == crate::session::LinkKind::Cable)
        .unwrap_or(false);
    let Some(ui) = LOBBY_UI.read().clone() else {
        return rsx! {};
    };
    if !cable || ui.my_idx != 0 {
        return rsx! {};
    }
    let have_crc: std::collections::HashSet<u32> = {
        let lib = ctx.library.read();
        lib.as_ref()
            .and_then(|v| v.as_ref())
            .map(|lib| lib.roms.iter().map(|r| r.crc32).collect())
            .unwrap_or_default()
    };
    // Linkable once anyone else is in and every seat holds every ROM.
    // Until then the button is simply gray — including while the
    // server connection is still coming up.
    let blocked = ui.code.is_none()
        || ui.starting
        || ui.players.len() < 2
        || !ui.players.iter().all(|p| p.ready)
        || !ui.players.iter().all(|p| have_crc.contains(&p.rom_crc32));
    rsx! {
        button {
            class: "btn primary",
            disabled: blocked,
            onclick: move |_| {
                if let Some(ui) = LOBBY_UI.write().as_mut() {
                    ui.starting = true;
                    ui.status = Some("Linking up…".to_string());
                }
                send_cmd(LobbyCommand::Start);
            },
            "Link up"
        }
    }
}

/// The panel body while no session is merged: create/join, then the
/// room. Wireless rooms merge as players arrive; cable rooms gather
/// until the creator links them up.
#[component]
pub fn CableBody() -> Element {
    let ctx = use_ctx();
    let mut code_entry = use_signal(String::new);
    // The running session's peripheral, fixed at launch.
    let link = ctx
        .runtime
        .borrow()
        .descriptor()
        .map(|d| d.link)
        .unwrap_or_default();

    let _ = SESSION_EPOCH.read();
    let notice = LINK_NOTICE.read().clone();
    let lobby_ui = LOBBY_UI.read().clone();

    // The "someone is missing a ROM" readout needs my own copy check
    // alongside the roster's ready bits.
    let have_crc: std::collections::HashSet<u32> = {
        let lib = ctx.library.read();
        lib.as_ref()
            .and_then(|v| v.as_ref())
            .map(|lib| lib.roms.iter().map(|r| r.crc32).collect())
            .unwrap_or_default()
    };

    rsx! {
        div { class: "cable",
            if let Some(notice) = notice {
                p { class: "link-notice", "{notice}" }
            }
            if let Some(ui) = lobby_ui {
                RoomSection {}
                // One reserved line for room status, always rendered:
                // text swapping in and out must never shift the buttons
                // below.
                p { class: "sub cable-status",
                    {
                        let connected = ui.code.is_some();
                        let cable = link == crate::session::LinkKind::Cable;
                        let complete = ui.players.iter().all(|p| p.ready)
                            && ui.players.iter().all(|p| have_crc.contains(&p.rom_crc32));
                        ui.status.clone().unwrap_or_else(|| {
                            if connected && !ui.starting {
                                if cable && ui.my_idx != 0 {
                                    "Waiting for the room's creator to link up.".to_string()
                                } else if ui.players.len() < 2 {
                                    if cable {
                                        "Share the code — link up when everyone's in."
                                            .to_string()
                                    } else {
                                        "Share the code — players merge in as they arrive."
                                            .to_string()
                                    }
                                } else if !complete {
                                    "Someone is missing a ROM.".to_string()
                                } else {
                                    String::new()
                                }
                            } else {
                                String::new()
                            }
                        })
                    }
                }
                div { class: "menu-actions",
                    LinkUpButton {}
                    button {
                        class: "btn",
                        onclick: {
                            let ctx = ctx.clone();
                            move |_| {
                                leave();
                                // If a merge already froze the machine for
                                // its capture, resume it on a fresh
                                // deadline.
                                if let Some(shared) = ctx.runtime.borrow().shared() {
                                    shared.resume();
                                }
                            }
                        },
                        "Leave"
                    }
                }
            } else {
                // Not in a room: host a new one or join a friend's. The
                // room runs whatever this machine booted with — the
                // link-port pick on the Play screen.
                button {
                    class: "btn primary wide",
                    onclick: {
                        let ctx = ctx.clone();
                        move |_| start_lobby(&ctx, LobbyMode::Create)
                    },
                    if link == crate::session::LinkKind::Wireless {
                        icons::Wifi {}
                    } else {
                        icons::Cable {}
                    }
                    "Create a room"
                }
                p { class: "sub",
                    if link == crate::session::LinkKind::Wireless {
                        "You'll get a code to share with the other players. They merge in as they arrive, over your wireless adapter."
                    } else {
                        "You'll get a code to share with the other players. Once everyone's in, link the cable up."
                    }
                }
                div { class: "or-divider", "or" }
                form { class: "join-row",
                    // Enter and the Join button both land here; an
                    // incomplete code is a no-op (the submit button is
                    // disabled, which also blocks Enter's implicit
                    // submission).
                    onsubmit: {
                        let ctx = ctx.clone();
                        move |evt: FormEvent| {
                            evt.prevent_default();
                            let code = code_entry.read().clone();
                            if code.len() == gbaroll_signaling::ROOM_CODE_LEN {
                                start_lobby(&ctx, LobbyMode::Join { code });
                            }
                        }
                    },
                    input {
                        r#type: "text",
                        placeholder: "6-character code",
                        value: "{code_entry}",
                        spellcheck: "false",
                        autocomplete: "off",
                        autocapitalize: "characters",
                        oninput: move |evt: FormEvent| {
                            code_entry.set(sanitize_room_code(&evt.value()));
                        },
                    }
                    button {
                        class: "btn primary",
                        r#type: "submit",
                        disabled: code_entry.read().len() != gbaroll_signaling::ROOM_CODE_LEN,
                        "Join"
                    }
                }
            }
        }
    }
}
