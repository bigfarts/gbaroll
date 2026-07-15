//! The lobby client: one tokio task owning the signaling websocket.
//! Commands come in over a channel from the UI; state changes flow back
//! as [`LobbyEvent`]s the UI drains each frame. When the room starts,
//! the same task builds the WebRTC mesh (the websocket doubles as the
//! signal relay) and hands the finished [`SessionBundle`] to the UI.

use futures::{SinkExt, StreamExt};
use gbaroll_signaling::{ClientMessage, ErrorKind, PlayerInfo, ServerMessage, StartPlayer, PROTOCOL_VERSION};
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::net::mesh::{self, PeerLink};

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
    Roster { players: Vec<PlayerInfo>, your_idx: usize },
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

/// Everything a netplay session needs to boot.
pub struct SessionBundle {
    pub room_code: String,
    pub clock_unix_micros: u64,
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
    pub events: std::sync::mpsc::Receiver<LobbyEvent>,
    cmd_tx: tokio::sync::mpsc::UnboundedSender<LobbyCommand>,
}

impl LobbyHandle {
    pub fn send(&self, cmd: LobbyCommand) {
        let _ = self.cmd_tx.send(cmd);
    }
}

impl Drop for LobbyHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(LobbyCommand::Leave);
    }
}

pub fn spawn(args: LobbyArgs) -> LobbyHandle {
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::runtime().spawn(async move {
        if let Err(e) = run(args, event_tx.clone(), cmd_rx).await {
            let _ = event_tx.send(LobbyEvent::Fatal(format!("{e:#}")));
        }
    });
    LobbyHandle {
        events: event_rx,
        cmd_tx,
    }
}

async fn run(
    args: LobbyArgs,
    events: std::sync::mpsc::Sender<LobbyEvent>,
    mut cmd_rx: tokio::sync::mpsc::UnboundedReceiver<LobbyCommand>,
) -> anyhow::Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async(&args.server_url)
        .await
        .map_err(|e| anyhow::anyhow!("can't reach signaling server at {}: {e}", args.server_url))?;
    let (mut sink, mut stream) = ws.split();

    let first = match &args.mode {
        LobbyMode::Create => ClientMessage::CreateRoom {
            protocol_version: PROTOCOL_VERSION,
            nick: args.nick.clone(),
            rom_crc32: args.rom_crc32,
            rom_title: args.rom_title.clone(),
        },
        LobbyMode::Join { code } => ClientMessage::JoinRoom {
            protocol_version: PROTOCOL_VERSION,
            code: code.clone(),
            nick: args.nick.clone(),
            rom_crc32: args.rom_crc32,
            rom_title: args.rom_title.clone(),
        },
    };
    send(&mut sink, &first).await?;

    let mut room_code = String::new();
    // The mesh's STUN/TURN list, from the server's greeting.
    let mut ice_servers: Vec<gbaroll_signaling::IceServer> = Vec::new();

    // Lobby phase: relay commands and server messages until Starting.
    let starting = loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(LobbyCommand::SetReady { ready }) => {
                        send(&mut sink, &ClientMessage::SetReady { ready }).await?;
                    }
                    Some(LobbyCommand::Start) => {
                        send(&mut sink, &ClientMessage::Start).await?;
                    }
                    // A boot capture belongs to the start phase; stray
                    // ones in the lobby phase mean nothing.
                    Some(LobbyCommand::Boot(_)) => {}
                    Some(LobbyCommand::Leave) | None => {
                        let _ = send(&mut sink, &ClientMessage::Leave).await;
                        let _ = sink.send(Message::Close(None)).await;
                        return Ok(());
                    }
                }
            }
            msg = stream.next() => {
                let msg = match msg {
                    Some(Ok(m)) => m,
                    Some(Err(e)) => anyhow::bail!("signaling connection lost: {e}"),
                    None => anyhow::bail!("signaling connection closed"),
                };
                let Message::Binary(bytes) = msg else { continue };
                let msg: ServerMessage = match gbaroll_signaling::decode(&bytes) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                match msg {
                    ServerMessage::Hello { ice_servers: servers } => {
                        ice_servers = servers;
                    }
                    ServerMessage::RoomCreated { code } | ServerMessage::RoomJoined { code } => {
                        room_code = code.clone();
                        let _ = events.send(LobbyEvent::Joined { code });
                    }
                    ServerMessage::Roster { players, your_idx } => {
                        let _ = events.send(LobbyEvent::Roster { players, your_idx: your_idx as usize });
                    }
                    ServerMessage::Error { kind, message } => {
                        let fatal = matches!(
                            kind,
                            ErrorKind::ProtocolVersionMismatch
                                | ErrorKind::RoomNotFound
                                | ErrorKind::RoomFull
                                | ErrorKind::RoomAlreadyStarted
                        );
                        if fatal {
                            anyhow::bail!("{message}");
                        }
                        let _ = events.send(LobbyEvent::Error(message));
                    }
                    ServerMessage::Starting { clock_unix_micros, players, your_idx } => {
                        break (clock_unix_micros, players, your_idx as usize);
                    }
                    _ => {}
                }
            }
        }
    };

    // Mesh phase: the websocket becomes the signal relay. The UI captures
    // the local machine as soon as it sees Starting, so the boot payload
    // rides the command queue while the mesh comes up.
    let (clock_unix_micros, players, local_player) = starting;
    let _ = events.send(LobbyEvent::Starting);
    let _ = events.send(LobbyEvent::Connecting(format!(
        "connecting to {} peer(s)…",
        players.len() - 1
    )));
    let mut peers = mesh::build(&mut sink, &mut stream, local_player, players.len(), &ice_servers).await?;

    let blob = loop {
        match cmd_rx.recv().await {
            Some(LobbyCommand::Boot(blob)) => break blob,
            Some(LobbyCommand::Leave) | None => {
                let _ = sink.send(Message::Close(None)).await;
                return Ok(());
            }
            Some(_) => {}
        }
    };

    // The plug-in exchange: every side's capture crosses the mesh.
    let _ = events.send(LobbyEvent::Connecting("exchanging machine state…".to_string()));
    let boots = mesh::exchange_boots(&mut peers, local_player, players.len(), blob).await?;

    let _ = events.send(LobbyEvent::SessionReady(Box::new(SessionBundle {
        room_code,
        clock_unix_micros,
        players,
        local_player,
        peers,
        boots,
    })));

    // The room is done with the server; close politely.
    let _ = send(&mut sink, &ClientMessage::Leave).await;
    let _ = sink.send(Message::Close(None)).await;
    Ok(())
}

async fn send<Sink>(sink: &mut Sink, msg: &ClientMessage) -> anyhow::Result<()>
where
    Sink: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    sink.send(Message::Binary(gbaroll_signaling::encode(msg)?)).await?;
    Ok(())
}
