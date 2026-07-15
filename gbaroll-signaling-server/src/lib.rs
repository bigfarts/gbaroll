//! The gbaroll signaling server: room rendezvous + opaque signal relay
//! for 2–4 player sessions. See the `gbaroll-signaling` crate for the
//! protocol. Run behind a TLS-terminating proxy for `wss://`.
//!
//! The binary is a thin wrapper over [`serve`]; the library form exists
//! so integration tests can run an in-process server.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use gbaroll_signaling::{
    decode, encode, normalize_room_code, ClientMessage, ErrorKind, IceServer, PlayerInfo, ServerMessage, StartPlayer,
    MAX_PLAYERS, MAX_SAVE_SIZE, PROTOCOL_VERSION, ROOM_CODE_ALPHABET, ROOM_CODE_LEN,
};
use rand::Rng;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

struct Player {
    nick: String,
    ready: bool,
    rom_crc32: u32,
    rom_title: String,
    save: Option<Vec<u8>>,
    tx: mpsc::UnboundedSender<ServerMessage>,
}

struct Room {
    started: bool,
    players: Vec<Player>,
}

impl Room {
    fn roster(&self) -> Vec<PlayerInfo> {
        self.players
            .iter()
            .map(|p| PlayerInfo {
                nick: p.nick.clone(),
                ready: p.ready,
                rom_crc32: p.rom_crc32,
                rom_title: p.rom_title.clone(),
            })
            .collect()
    }

    /// Occupancy changed: whatever anyone agreed to ready up for no
    /// longer describes the room, so ready state (and the saves behind
    /// it) resets for everyone.
    fn reset_ready(&mut self) {
        for p in &mut self.players {
            p.ready = false;
            p.save = None;
        }
    }

    /// Send each member the roster, stamped with their own index.
    fn broadcast_roster(&self) {
        let players = self.roster();
        for (i, p) in self.players.iter().enumerate() {
            let _ = p.tx.send(ServerMessage::Roster {
                players: players.clone(),
                your_idx: i as u8,
            });
        }
    }

    fn broadcast(&self, msg: ServerMessage) {
        for p in &self.players {
            let _ = p.tx.send(msg.clone());
        }
    }
}

type Rooms = Arc<Mutex<HashMap<String, Room>>>;

/// The ICE servers handed to every client when none are configured.
pub fn default_ice_servers() -> Vec<IceServer> {
    vec![IceServer {
        urls: vec![
            "stun:stun.l.google.com:19302".to_string(),
            "stun:stun1.l.google.com:19302".to_string(),
        ],
        username: None,
        credential: None,
    }]
}

/// Accept-and-serve loop over an already-bound listener. Runs forever.
/// `ice_servers` is handed to every client on connect (the deployment
/// knows its own STUN/TURN infrastructure).
pub async fn serve(listener: TcpListener, ice_servers: Vec<IceServer>) -> anyhow::Result<()> {
    let rooms: Rooms = Arc::new(Mutex::new(HashMap::new()));
    let ice_servers = Arc::new(ice_servers);
    log::info!("listening on {}", listener.local_addr()?);
    loop {
        let (stream, addr) = listener.accept().await?;
        let rooms = rooms.clone();
        let ice_servers = ice_servers.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, addr, rooms, ice_servers).await {
                log::debug!("{addr}: {e:#}");
            }
        });
    }
}

/// Where this connection sits, once its create/join was accepted.
struct Membership {
    code: String,
    /// The player's *current* index. Lobby departures compact indices
    /// downward, so this must be re-derived under the rooms lock — we
    /// track identity by the sender handle instead.
    tx: mpsc::UnboundedSender<ServerMessage>,
}

fn player_idx(room: &Room, tx: &mpsc::UnboundedSender<ServerMessage>) -> Option<usize> {
    room.players.iter().position(|p| p.tx.same_channel(tx))
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    rooms: Rooms,
    ice_servers: Arc<Vec<IceServer>>,
) -> anyhow::Result<()> {
    let config = WebSocketConfig {
        max_message_size: Some(2 * 1024 * 1024),
        max_frame_size: Some(2 * 1024 * 1024),
        ..Default::default()
    };
    let ws = tokio_tungstenite::accept_async_with_config(stream, Some(config)).await?;
    let (mut sink, mut source) = ws.split();

    // Outbound messages flow through a queue so room broadcasts (made
    // under the rooms lock) never block on a slow socket.
    let (tx, mut rx) = mpsc::unbounded_channel::<ServerMessage>();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let bytes = match encode(&msg) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if sink.send(Message::Binary(bytes)).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
    });

    // Greet with the ICE servers the mesh should use.
    let _ = tx.send(ServerMessage::Hello {
        ice_servers: ice_servers.as_ref().clone(),
    });

    let mut membership: Option<Membership> = None;
    let result: anyhow::Result<()> = async {
        while let Some(msg) = source.next().await {
            let msg = msg?;
            let bytes = match msg {
                Message::Binary(b) => b,
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) => continue,
                _ => {
                    send_error(&tx, ErrorKind::Malformed, "expected a binary message");
                    continue;
                }
            };
            let msg: ClientMessage = match decode(&bytes) {
                Ok(m) => m,
                Err(_) => {
                    send_error(&tx, ErrorKind::Malformed, "undecodable message");
                    continue;
                }
            };
            if !handle_message(msg, &tx, &mut membership, &rooms, addr) {
                break;
            }
        }
        Ok(())
    }
    .await;

    if let Some(m) = membership.take() {
        leave_room(&rooms, &m);
    }
    drop(tx);
    let _ = writer.await;
    result
}

fn send_error(tx: &mpsc::UnboundedSender<ServerMessage>, kind: ErrorKind, message: &str) {
    let _ = tx.send(ServerMessage::Error {
        kind,
        message: message.to_string(),
    });
}

/// Handle one message. Returns false to end the connection.
fn handle_message(
    msg: ClientMessage,
    tx: &mpsc::UnboundedSender<ServerMessage>,
    membership: &mut Option<Membership>,
    rooms: &Rooms,
    addr: SocketAddr,
) -> bool {
    match msg {
        ClientMessage::CreateRoom {
            protocol_version,
            nick,
            rom_crc32,
            rom_title,
        } => {
            if membership.is_some() {
                send_error(tx, ErrorKind::Malformed, "already in a room");
                return true;
            }
            if protocol_version != PROTOCOL_VERSION {
                send_error(tx, ErrorKind::ProtocolVersionMismatch, "update your client");
                return false;
            }
            let mut rooms = rooms.lock().unwrap();
            let code = loop {
                let code = random_code();
                if !rooms.contains_key(&code) {
                    break code;
                }
            };
            rooms.insert(
                code.clone(),
                Room {
                    started: false,
                    players: vec![Player {
                        nick: sanitize_nick(&nick),
                        ready: false,
                        rom_crc32,
                        rom_title: rom_title.clone(),
                        save: None,
                        tx: tx.clone(),
                    }],
                },
            );
            log::info!("{addr}: created room {code} ({rom_title})");
            let _ = tx.send(ServerMessage::RoomCreated { code: code.clone() });
            rooms.get(&code).unwrap().broadcast_roster();
            *membership = Some(Membership { code, tx: tx.clone() });
            true
        }
        ClientMessage::JoinRoom {
            protocol_version,
            code,
            nick,
            rom_crc32,
            rom_title,
        } => {
            if membership.is_some() {
                send_error(tx, ErrorKind::Malformed, "already in a room");
                return true;
            }
            if protocol_version != PROTOCOL_VERSION {
                send_error(tx, ErrorKind::ProtocolVersionMismatch, "update your client");
                return false;
            }
            let code = normalize_room_code(&code);
            let mut rooms = rooms.lock().unwrap();
            let Some(room) = rooms.get_mut(&code) else {
                send_error(tx, ErrorKind::RoomNotFound, "no such room");
                return true;
            };
            if room.started {
                send_error(tx, ErrorKind::RoomAlreadyStarted, "room already started");
                return true;
            }
            if room.players.len() >= MAX_PLAYERS {
                send_error(tx, ErrorKind::RoomFull, "room is full");
                return true;
            }
            room.reset_ready();
            room.players.push(Player {
                nick: sanitize_nick(&nick),
                ready: false,
                rom_crc32,
                rom_title,
                save: None,
                tx: tx.clone(),
            });
            log::info!("{addr}: joined room {code}");
            let _ = tx.send(ServerMessage::RoomJoined { code: code.clone() });
            room.broadcast_roster();
            *membership = Some(Membership { code, tx: tx.clone() });
            true
        }
        ClientMessage::SetReady { ready, save } => {
            let Some(m) = membership.as_ref() else { return true };
            if save.as_ref().is_some_and(|s| s.len() > MAX_SAVE_SIZE) {
                send_error(tx, ErrorKind::Malformed, "save image too large");
                return true;
            }
            let mut rooms = rooms.lock().unwrap();
            let Some(room) = rooms.get_mut(&m.code) else { return true };
            if room.started {
                return true;
            }
            let Some(i) = player_idx(room, &m.tx) else { return true };
            room.players[i].ready = ready;
            room.players[i].save = if ready { save } else { None };
            room.broadcast_roster();
            true
        }
        ClientMessage::Chat { text } => {
            let Some(m) = membership.as_ref() else { return true };
            let text: String = text.chars().take(512).collect();
            let rooms = rooms.lock().unwrap();
            let Some(room) = rooms.get(&m.code) else { return true };
            let Some(i) = player_idx(room, &m.tx) else { return true };
            room.broadcast(ServerMessage::Chat {
                from: i as u8,
                nick: room.players[i].nick.clone(),
                text,
            });
            true
        }
        ClientMessage::Start => {
            let Some(m) = membership.as_ref() else { return true };
            let mut rooms = rooms.lock().unwrap();
            let Some(room) = rooms.get_mut(&m.code) else { return true };
            if room.started {
                return true;
            }
            let Some(i) = player_idx(room, &m.tx) else { return true };
            if i != 0 {
                send_error(tx, ErrorKind::NotHost, "only the host can start");
                return true;
            }
            if room.players.len() < 2 || !room.players.iter().all(|p| p.ready) {
                send_error(tx, ErrorKind::NotEveryoneReady, "need 2+ players, all ready");
                return true;
            }
            room.started = true;
            let clock_unix_micros = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);
            let players: Vec<StartPlayer> = room
                .players
                .iter()
                .map(|p| StartPlayer {
                    nick: p.nick.clone(),
                    rom_crc32: p.rom_crc32,
                    save: p.save.clone(),
                })
                .collect();
            log::info!("room {} starting with {} players", m.code, players.len());
            for (idx, p) in room.players.iter().enumerate() {
                let _ = p.tx.send(ServerMessage::Starting {
                    clock_unix_micros,
                    players: players.clone(),
                    your_idx: idx as u8,
                });
            }
            // The saves have been handed out; no need to keep them.
            for p in &mut room.players {
                p.save = None;
            }
            true
        }
        ClientMessage::Signal { to, payload } => {
            let Some(m) = membership.as_ref() else { return true };
            let rooms = rooms.lock().unwrap();
            let Some(room) = rooms.get(&m.code) else { return true };
            if !room.started {
                send_error(tx, ErrorKind::Malformed, "room hasn't started");
                return true;
            }
            let Some(from) = player_idx(room, &m.tx) else { return true };
            let Some(target) = room.players.get(to as usize) else {
                return true;
            };
            let _ = target.tx.send(ServerMessage::Signal {
                from: from as u8,
                payload,
            });
            true
        }
        ClientMessage::Leave => false,
    }
}

fn leave_room(rooms: &Rooms, m: &Membership) {
    let mut rooms = rooms.lock().unwrap();
    let Some(room) = rooms.get_mut(&m.code) else { return };
    let Some(i) = player_idx(room, &m.tx) else { return };
    if room.started {
        // Indices are frozen once started (they're core indices); mark
        // the seat gone so surviving peers can tear down.
        room.players.remove(i);
        room.broadcast(ServerMessage::PeerLeft { player_idx: i as u8 });
    } else {
        room.players.remove(i);
        room.reset_ready();
        room.broadcast_roster();
    }
    if room.players.is_empty() {
        rooms.remove(&m.code);
        log::info!("room {} closed", m.code);
    }
}

fn sanitize_nick(nick: &str) -> String {
    let n: String = nick.trim().chars().take(24).collect();
    if n.is_empty() {
        "player".to_string()
    } else {
        n
    }
}

fn random_code() -> String {
    let mut rng = rand::thread_rng();
    (0..ROOM_CODE_LEN)
        .map(|_| ROOM_CODE_ALPHABET[rng.gen_range(0..ROOM_CODE_ALPHABET.len())] as char)
        .collect()
}
