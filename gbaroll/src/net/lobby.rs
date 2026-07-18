//! The lobby client: one spawned task owning the signaling websocket
//! for as long as this machine is in the room. Commands come in over a
//! channel from the UI; state changes flow back as [`LobbyEvent`]s the
//! runtime pump drains.
//!
//! Rooms hold dynamic membership, so the task's life is a loop: idle in
//! the room until the server broadcasts a merge (`Starting`), build the
//! WebRTC mesh (the websocket doubles as the signal relay), hand the
//! finished [`SessionBundle`] to the UI — and go back to idling for the
//! next merge. Wireless rooms merge on their own as membership changes
//! (a failed merge withdraws the ready bit so the server voids it and
//! re-fires on the next convergence); cable rooms gather until position
//! 0 sends [`LobbyCommand::Start`], and re-link the same way.

use std::collections::VecDeque;

use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};
use gbaroll_signaling::{server_message, ClientMessage, ErrorKind, PlayerInfo, ServerMessage, StartPlayer};
use gloo_timers::future::TimeoutFuture;

use crate::net::mesh::{self, PeerLink};
use crate::net::ws::SignalSocket;

/// How long a merge failure waits before withdrawing ready to arrange
/// the retry — enough to keep a systematically failing mesh from
/// hot-looping the whole room through capture stalls.
const MERGE_RETRY_MS: u32 = 3_000;

/// How long a dead-session report ([`LobbyCommand::Dropped`]) sits
/// before dancing the ready bit. A merge the server already broadcast
/// may still be in flight to us — the drop it recovers from would be
/// stale, and an arriving `Starting` cancels the dance.
const DROP_DANCE_MS: u32 = 2_500;

#[derive(Debug)]
pub enum LobbyCommand {
    /// The automatic "I have every ROM in this roster" report — every
    /// member (position 0 included) keeps it asserted; the room merges
    /// when the whole roster has. Never surfaced as a control.
    SetReady { ready: bool },
    /// Position 0 only, cable rooms only: link the room up. Wireless
    /// rooms merge on their own; a cable room gathers until this — and
    /// takes it again later to fold late joiners in.
    Start,
    /// Position 0 only: throw a player out of the room, addressed by
    /// the roster's stable seat token (positions compact; seats don't,
    /// so a kick racing a departure can't hit the wrong player).
    Kick { seat: u32 },
    /// The local machine's encoded boot payload, captured by the UI in
    /// response to [`LobbyEvent::Starting`].
    Boot(Vec<u8>),
    /// The merged session died under us without a roster change (a peer
    /// walked out of range). In a wireless room: after a beat, withdraw
    /// and re-assert ready so the server voids the dead merge and
    /// re-merges the room once it converges again. Cable rooms sit
    /// solo until position 0 re-links.
    Dropped,
    Leave,
}

pub enum LobbyEvent {
    /// The room was created/joined; here's its code.
    Joined { code: String },
    Roster {
        players: Vec<PlayerInfo>,
        your_idx: usize,
    },
    /// Non-fatal problem.
    Error(String),
    /// The lobby is dead; the UI should drop it.
    Fatal(String),
    /// Position 0 threw us out. The lobby is dead — and the link should
    /// unplug too: we walked (were walked) out of range.
    Kicked,
    /// A merge is beginning: capture the local machine and send it back
    /// as [`LobbyCommand::Boot`] — unplugging any running link first,
    /// captures come only from a solo machine.
    Starting,
    /// Mesh progress line for the connecting overlay.
    Connecting(String),
    /// Everything is up: hand off to the session. The lobby stays alive
    /// — the room's next membership change brings the next merge.
    SessionReady(Box<SessionBundle>),
    /// The merge failed; the machine should resume solo while the lobby
    /// arranges the retry.
    MergeFailed(String),
}

/// Everything a netplay session needs to boot. The shared RTC seed
/// isn't here: it rides position 0's boot payload over the peer
/// protocol.
pub struct SessionBundle {
    pub room_code: String,
    pub players: Vec<StartPlayer>,
    pub local_player: usize,
    pub peers: Vec<PeerLink>,
    /// Every side's encoded boot payload, in player order (exchanged over
    /// the mesh; the local slot is our own capture).
    pub boots: Vec<Vec<u8>>,
}

pub enum LobbyMode {
    Create,
    Join { code: String },
}

pub struct LobbyArgs {
    pub server_url: String,
    pub nick: String,
    pub rom_crc32: u32,
    pub rom_title: String,
    /// The local machine's link peripheral (fixed at launch). Sent on
    /// create — the creator's machine decides what the room's merges
    /// build — and checked against the roster on join: a machine
    /// launched with the other peripheral bows out before a merge
    /// builds it a link its game never booted with.
    pub wireless: bool,
    pub mode: LobbyMode,
}

pub struct LobbyHandle {
    pub events: mpsc::UnboundedReceiver<LobbyEvent>,
    cmd_tx: mpsc::UnboundedSender<LobbyCommand>,
}

impl LobbyHandle {
    pub fn send(&self, cmd: LobbyCommand) {
        let _ = self.cmd_tx.unbounded_send(cmd);
    }

    /// A second command handle for UI controls that outlive the drain
    /// task's ownership of this one.
    pub fn sender(&self) -> mpsc::UnboundedSender<LobbyCommand> {
        self.cmd_tx.clone()
    }
}

impl Drop for LobbyHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.unbounded_send(LobbyCommand::Leave);
    }
}

pub fn spawn(args: LobbyArgs) -> LobbyHandle {
    let (event_tx, event_rx) = mpsc::unbounded();
    let (cmd_tx, cmd_rx) = mpsc::unbounded();
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(e) = run(args, event_tx.clone(), cmd_rx).await {
            let _ = event_tx.unbounded_send(LobbyEvent::Fatal(format!("{e:#}")));
        }
    });
    LobbyHandle {
        events: event_rx,
        cmd_tx,
    }
}

/// The task's room-scoped state, threaded through the phases.
struct Lobby {
    room_code: String,
    /// The mesh's STUN/TURN list, from the server's greeting.
    ice_servers: Vec<gbaroll_signaling::IceServer>,
    /// Server messages a merge phase read off the socket but couldn't
    /// handle (the room's life goes on while a mesh comes up); the idle
    /// phase replays them before touching new traffic.
    stray: VecDeque<server_message::Msg>,
}

/// What the idle phase resolved to: a merge broadcast, or a clean exit.
enum Idle {
    Merge {
        players: Vec<StartPlayer>,
        local_player: usize,
    },
    Left,
    /// Position 0 threw us out; the event is already sent.
    Kicked,
}

enum MergeOutcome {
    Session(Box<SessionBundle>),
    Left,
}

async fn run(
    args: LobbyArgs,
    events: mpsc::UnboundedSender<LobbyEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<LobbyCommand>,
) -> anyhow::Result<()> {
    let mut socket = SignalSocket::connect(&args.server_url).await?;

    let first = match &args.mode {
        LobbyMode::Create => ClientMessage::create_room(
            &args.nick,
            args.rom_crc32,
            &args.rom_title,
            args.wireless,
        ),
        LobbyMode::Join { code } => {
            ClientMessage::join_room(code, &args.nick, args.rom_crc32, &args.rom_title)
        }
    };
    send(&socket, &first)?;

    let mut lobby = Lobby {
        room_code: String::new(),
        ice_servers: Vec::new(),
        stray: VecDeque::new(),
    };

    loop {
        let (players, local_player) =
            match idle_phase(&mut socket, &events, &mut cmd_rx, &mut lobby, &args).await? {
                Idle::Merge {
                    players,
                    local_player,
                } => (players, local_player),
                Idle::Left | Idle::Kicked => {
                    socket.close();
                    return Ok(());
                }
            };

        let _ = events.unbounded_send(LobbyEvent::Starting);
        let _ = events.unbounded_send(LobbyEvent::Connecting(format!(
            "connecting to {} peer(s)…",
            players.len() - 1
        )));
        match merge_phase(
            &mut socket,
            &events,
            &mut cmd_rx,
            &mut lobby,
            players,
            local_player,
        )
        .await
        {
            Ok(MergeOutcome::Session(bundle)) => {
                let _ = events.unbounded_send(LobbyEvent::SessionReady(bundle));
            }
            Ok(MergeOutcome::Left) => {
                socket.close();
                return Ok(());
            }
            Err(e) => {
                // Being kicked mid-merge surfaces as a failed mesh with
                // the KICKED error set aside in the stray queue.
                if lobby.stray.iter().any(|m| {
                    matches!(m, server_message::Msg::Error(err) if err.kind() == ErrorKind::Kicked)
                }) {
                    let _ = events.unbounded_send(LobbyEvent::Kicked);
                    socket.close();
                    return Ok(());
                }
                log::warn!("merge failed: {e:#}");
                let _ = events.unbounded_send(LobbyEvent::MergeFailed(format!(
                    "Couldn't link up: {e:#}"
                )));
                // Wireless: pace the retry, then withdraw ready — the
                // server voids the failed merge, the roster echo has
                // the UI re-assert, and the room re-fires when it
                // converges. Cable: nothing automatic; position 0
                // presses the button again.
                if args.wireless {
                    if !retry_pause(&socket, &mut cmd_rx).await {
                        socket.close();
                        return Ok(());
                    }
                    send(&socket, &ClientMessage::set_ready(false))?;
                }
            }
        }
    }
}

/// Idle in the room: relay commands and server messages until the next
/// merge broadcast (or a departure).
async fn idle_phase(
    socket: &mut SignalSocket,
    events: &mpsc::UnboundedSender<LobbyEvent>,
    cmd_rx: &mut mpsc::UnboundedReceiver<LobbyCommand>,
    lobby: &mut Lobby,
    args: &LobbyArgs,
) -> anyhow::Result<Idle> {
    // The ready dance a Dropped report arms: withdraw after a beat
    // (an in-flight Starting supersedes the drop and cancels it).
    let dance = futures::future::Fuse::<TimeoutFuture>::terminated();
    futures::pin_mut!(dance);

    loop {
        // Replay what a merge phase set aside before touching new
        // traffic.
        if let Some(msg) = lobby.stray.pop_front() {
            if let Some(idle) = handle_server_msg(msg, events, lobby, args)? {
                return Ok(idle);
            }
            continue;
        }
        futures::select! {
            _ = &mut dance => {
                // The dead merge's ready bit comes down; the roster echo
                // has the UI re-assert it, and the server re-merges the
                // room once everyone converges.
                send(socket, &ClientMessage::set_ready(false))?;
            }
            cmd = cmd_rx.next() => {
                match cmd {
                    Some(LobbyCommand::SetReady { ready }) => {
                        send(socket, &ClientMessage::set_ready(ready))?;
                    }
                    Some(LobbyCommand::Start) => {
                        send(socket, &ClientMessage::start())?;
                    }
                    Some(LobbyCommand::Kick { seat }) => {
                        send(socket, &ClientMessage::kick_player(seat))?;
                    }
                    Some(LobbyCommand::Dropped) => {
                        // Only wireless rooms self-heal; a cable room
                        // waits for position 0 to re-link.
                        if args.wireless {
                            dance.set(TimeoutFuture::new(DROP_DANCE_MS).fuse());
                        }
                    }
                    // A boot capture belongs to a merge phase; stray
                    // ones here are stale (a merge that failed under
                    // them) and mean nothing.
                    Some(LobbyCommand::Boot(_)) => {}
                    Some(LobbyCommand::Leave) | None => {
                        let _ = send(socket, &ClientMessage::leave());
                        return Ok(Idle::Left);
                    }
                }
            }
            msg = socket.next().fuse() => {
                let Some(bytes) = msg else {
                    anyhow::bail!("signaling connection closed");
                };
                let Ok(ServerMessage { msg: Some(msg) }) = gbaroll_signaling::decode::<ServerMessage>(&bytes) else {
                    continue;
                };
                if let Some(idle) = handle_server_msg(msg, events, lobby, args)? {
                    return Ok(idle);
                }
            }
        }
    }
}

/// One idle-phase server message. `Some` ends the phase.
fn handle_server_msg(
    msg: server_message::Msg,
    events: &mpsc::UnboundedSender<LobbyEvent>,
    lobby: &mut Lobby,
    args: &LobbyArgs,
) -> anyhow::Result<Option<Idle>> {
    match msg {
        server_message::Msg::Hello(hello) => {
            lobby.ice_servers = hello.ice_servers;
        }
        server_message::Msg::RoomCreated(m) => {
            lobby.room_code = m.code.clone();
            let _ = events.unbounded_send(LobbyEvent::Joined { code: m.code });
        }
        server_message::Msg::RoomJoined(m) => {
            lobby.room_code = m.code.clone();
            let _ = events.unbounded_send(LobbyEvent::Joined { code: m.code });
        }
        server_message::Msg::Roster(roster) => {
            // The room's merges build the creator's peripheral; a
            // machine launched with the other one bows out rather than
            // ending up on a link its game never booted with.
            if roster.wireless != args.wireless {
                anyhow::bail!(
                    "This room plays over the {} — relaunch the game with it to join.",
                    if roster.wireless { "wireless adapter" } else { "link cable" }
                );
            }
            let _ = events.unbounded_send(LobbyEvent::Roster {
                players: roster.players,
                your_idx: roster.your_idx as usize,
            });
        }
        server_message::Msg::Error(e) => {
            let kind = e.kind();
            if kind == ErrorKind::Kicked {
                let _ = events.unbounded_send(LobbyEvent::Kicked);
                return Ok(Some(Idle::Kicked));
            }
            let fatal = matches!(
                kind,
                ErrorKind::ProtocolVersionMismatch
                    | ErrorKind::RoomNotFound
                    | ErrorKind::RoomFull
                    | ErrorKind::RoomAlreadyStarted
            );
            if fatal {
                anyhow::bail!("{}", error_text(kind));
            }
            let _ = events.unbounded_send(LobbyEvent::Error(error_text(kind).to_string()));
        }
        server_message::Msg::Starting(s) => {
            return Ok(Some(Idle::Merge {
                players: s.players,
                local_player: s.your_idx as usize,
            }));
        }
        // Departures between merges: the running session notices over
        // its own transport, and the shrunken roster follows.
        server_message::Msg::PeerLeft(_) => {}
        // Relay leftovers from a finished mesh (late-trickled
        // candidates).
        server_message::Msg::Signal(_) => {}
    }
    Ok(None)
}

/// One merge: mesh with the broadcast roster (the UI's machine capture
/// rides the command queue while the mesh comes up), then exchange
/// every side's boot payload.
async fn merge_phase(
    socket: &mut SignalSocket,
    events: &mpsc::UnboundedSender<LobbyEvent>,
    cmd_rx: &mut mpsc::UnboundedReceiver<LobbyCommand>,
    lobby: &mut Lobby,
    players: Vec<StartPlayer>,
    local_player: usize,
) -> anyhow::Result<MergeOutcome> {
    let mut peers = mesh::build(
        socket,
        local_player,
        players.len(),
        &lobby.ice_servers,
        &mut lobby.stray,
    )
    .await?;

    let blob = loop {
        match cmd_rx.next().await {
            Some(LobbyCommand::Boot(blob)) => break blob,
            Some(LobbyCommand::Leave) | None => {
                let _ = send(socket, &ClientMessage::leave());
                return Ok(MergeOutcome::Left);
            }
            Some(_) => {}
        }
    };

    // The plug-in exchange: every side's capture crosses the mesh.
    let _ = events.unbounded_send(LobbyEvent::Connecting("exchanging machine state…".to_string()));
    let boots = mesh::exchange_boots(&mut peers, local_player, players.len(), blob).await?;

    Ok(MergeOutcome::Session(Box::new(SessionBundle {
        room_code: lobby.room_code.clone(),
        players,
        local_player,
        peers,
        boots,
    })))
}

/// Pace a merge retry, keeping commands flowing so a Leave (or the
/// UI's ready reports) still land promptly. `false` means the user
/// left the room.
async fn retry_pause(
    socket: &SignalSocket,
    cmd_rx: &mut mpsc::UnboundedReceiver<LobbyCommand>,
) -> bool {
    let delay = TimeoutFuture::new(MERGE_RETRY_MS).fuse();
    futures::pin_mut!(delay);
    loop {
        futures::select! {
            _ = &mut delay => return true,
            cmd = cmd_rx.next() => match cmd {
                Some(LobbyCommand::SetReady { ready }) => {
                    let _ = send(socket, &ClientMessage::set_ready(ready));
                }
                Some(LobbyCommand::Start) => {
                    let _ = send(socket, &ClientMessage::start());
                }
                Some(LobbyCommand::Kick { seat }) => {
                    let _ = send(socket, &ClientMessage::kick_player(seat));
                }
                Some(LobbyCommand::Boot(_)) | Some(LobbyCommand::Dropped) => {}
                Some(LobbyCommand::Leave) | None => {
                    let _ = send(socket, &ClientMessage::leave());
                    return false;
                }
            }
        }
    }
}

fn send(socket: &SignalSocket, msg: &ClientMessage) -> anyhow::Result<()> {
    socket.send(&gbaroll_signaling::encode(msg))
}

/// The user-facing text for a server error. The wire carries only the
/// kind; the words are the client's.
fn error_text(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::ProtocolVersionMismatch => "This build is out of date — reload the app.",
        ErrorKind::RoomNotFound => "No room with that code.",
        ErrorKind::RoomFull => "That room is full.",
        ErrorKind::RoomAlreadyStarted => "That room already started.",
        ErrorKind::NotHost => "Only the room's creator can do that.",
        ErrorKind::NotEveryoneReady => "The room can't link up yet.",
        ErrorKind::Kicked => "The room's creator removed you.",
        ErrorKind::Malformed | ErrorKind::Internal | ErrorKind::Unspecified => {
            "The server rejected that."
        }
    }
}
