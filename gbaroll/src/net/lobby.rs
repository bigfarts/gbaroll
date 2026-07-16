//! The lobby client: one spawned task owning the signaling websocket.
//! Commands come in over a channel from the UI; state changes flow back
//! as [`LobbyEvent`]s the runtime pump drains. When the room starts,
//! the same task builds the WebRTC mesh (the websocket doubles as the
//! signal relay) and hands the finished [`SessionBundle`] to the UI.

use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};
use gbaroll_signaling::{server_message, ClientMessage, ErrorKind, PlayerInfo, ServerMessage, StartPlayer};

use crate::net::mesh::{self, PeerLink};
use crate::net::ws::SignalSocket;

#[derive(Debug)]
pub enum LobbyCommand {
    SetReady { ready: bool },
    /// Host only.
    Start,
    /// The local machine's encoded boot payload, captured by the UI in
    /// response to [`LobbyEvent::Starting`].
    Boot(Vec<u8>),
    Leave,
}

pub enum LobbyEvent {
    /// The room was created/joined; here's its code.
    Joined { code: String },
    Roster {
        players: Vec<PlayerInfo>,
        your_idx: usize,
    },
    /// Non-fatal problem (e.g. "not everyone is ready").
    Error(String),
    /// The lobby is dead; the UI should drop it.
    Fatal(String),
    /// The room is starting: capture the local machine and send it back
    /// as [`LobbyCommand::Boot`] — the cable is being plugged in.
    Starting,
    /// Mesh progress line for the connecting overlay.
    Connecting(String),
    /// Everything is up: hand off to the session.
    SessionReady(Box<SessionBundle>),
}

/// Everything a netplay session needs to boot. The shared RTC seed
/// isn't here: it rides the host's boot payload over the peer protocol.
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

async fn run(
    args: LobbyArgs,
    events: mpsc::UnboundedSender<LobbyEvent>,
    mut cmd_rx: mpsc::UnboundedReceiver<LobbyCommand>,
) -> anyhow::Result<()> {
    let mut socket = SignalSocket::connect(&args.server_url).await?;

    let first = match &args.mode {
        LobbyMode::Create => ClientMessage::create_room(&args.nick, args.rom_crc32, &args.rom_title),
        LobbyMode::Join { code } => {
            ClientMessage::join_room(code, &args.nick, args.rom_crc32, &args.rom_title)
        }
    };
    send(&socket, &first)?;

    let mut room_code = String::new();
    // The mesh's STUN/TURN list, from the server's greeting.
    let mut ice_servers: Vec<gbaroll_signaling::IceServer> = Vec::new();

    // Lobby phase: relay commands and server messages until Starting.
    let starting = loop {
        futures::select! {
            cmd = cmd_rx.next() => {
                match cmd {
                    Some(LobbyCommand::SetReady { ready }) => {
                        send(&socket, &ClientMessage::set_ready(ready))?;
                    }
                    Some(LobbyCommand::Start) => {
                        send(&socket, &ClientMessage::start())?;
                    }
                    // A boot capture belongs to the start phase; stray
                    // ones in the lobby phase mean nothing.
                    Some(LobbyCommand::Boot(_)) => {}
                    Some(LobbyCommand::Leave) | None => {
                        let _ = send(&socket, &ClientMessage::leave());
                        socket.close();
                        return Ok(());
                    }
                }
            }
            msg = socket.next().fuse() => {
                let Some(bytes) = msg else {
                    anyhow::bail!("signaling connection closed");
                };
                let msg = match gbaroll_signaling::decode::<ServerMessage>(&bytes) {
                    Ok(ServerMessage { msg: Some(m) }) => m,
                    _ => continue,
                };
                match msg {
                    server_message::Msg::Hello(hello) => {
                        ice_servers = hello.ice_servers;
                    }
                    server_message::Msg::RoomCreated(m) => {
                        room_code = m.code.clone();
                        let _ = events.unbounded_send(LobbyEvent::Joined { code: m.code });
                    }
                    server_message::Msg::RoomJoined(m) => {
                        room_code = m.code.clone();
                        let _ = events.unbounded_send(LobbyEvent::Joined { code: m.code });
                    }
                    server_message::Msg::Roster(roster) => {
                        let _ = events.unbounded_send(LobbyEvent::Roster {
                            players: roster.players,
                            your_idx: roster.your_idx as usize,
                        });
                    }
                    server_message::Msg::Error(e) => {
                        let fatal = matches!(
                            e.kind(),
                            ErrorKind::ProtocolVersionMismatch
                                | ErrorKind::RoomNotFound
                                | ErrorKind::RoomFull
                                | ErrorKind::RoomAlreadyStarted
                        );
                        if fatal {
                            anyhow::bail!("{}", e.message);
                        }
                        let _ = events.unbounded_send(LobbyEvent::Error(e.message));
                    }
                    server_message::Msg::Starting(s) => {
                        break (s.players, s.your_idx as usize);
                    }
                    _ => {}
                }
            }
        }
    };

    // Mesh phase: the websocket becomes the signal relay. The UI captures
    // the local machine as soon as it sees Starting, so the boot payload
    // rides the command queue while the mesh comes up.
    let (players, local_player) = starting;
    let _ = events.unbounded_send(LobbyEvent::Starting);
    let _ = events.unbounded_send(LobbyEvent::Connecting(format!(
        "connecting to {} peer(s)…",
        players.len() - 1
    )));
    let mut peers = mesh::build(&mut socket, local_player, players.len(), &ice_servers).await?;

    let blob = loop {
        match cmd_rx.next().await {
            Some(LobbyCommand::Boot(blob)) => break blob,
            Some(LobbyCommand::Leave) | None => {
                socket.close();
                return Ok(());
            }
            Some(_) => {}
        }
    };

    // The plug-in exchange: every side's capture crosses the mesh.
    let _ = events.unbounded_send(LobbyEvent::Connecting("exchanging machine state…".to_string()));
    let boots = mesh::exchange_boots(&mut peers, local_player, players.len(), blob).await?;

    let _ = events.unbounded_send(LobbyEvent::SessionReady(Box::new(SessionBundle {
        room_code,
        players,
        local_player,
        peers,
        boots,
    })));

    // The room is done with the server; close politely — the server
    // deletes the room once every member has left.
    let _ = send(&socket, &ClientMessage::leave());
    socket.close();
    Ok(())
}

fn send(socket: &SignalSocket, msg: &ClientMessage) -> anyhow::Result<()> {
    socket.send(&gbaroll_signaling::encode(msg))
}
