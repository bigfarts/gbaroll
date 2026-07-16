//! End-to-end netplay smoke test: an in-process signaling server, two
//! lobby clients rendezvousing over real websockets, a real WebRTC mesh
//! over loopback, two solo machines captured mid-run and exchanged over
//! the mesh (the cable plug-in), two live rollback sessions running the
//! built-in SIO test ROM against each other — asserting input
//! confirmation progress, no desync across the plugged-in link, a clean
//! unplug that hands both sides a solo continuation, and parseable
//! replays carrying the boot captures.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::net::lobby::{self, LobbyArgs, LobbyCommand, LobbyEvent, LobbyMode, SessionBundle};
use crate::net::protocol::BootBlob;
use crate::session::netplay::{self, NetplayArgs};
use crate::session::{local, SessionEnd};

fn wait_event<T>(
    handle: &lobby::LobbyHandle,
    what: &str,
    timeout: Duration,
    mut matcher: impl FnMut(&LobbyEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_else(|| panic!("timed out waiting for {what}"));
        match handle
            .events
            .recv_timeout(remaining.min(Duration::from_millis(100)))
        {
            Ok(event) => {
                if let LobbyEvent::Fatal(e) = &event {
                    panic!("lobby died waiting for {what}: {e}");
                }
                if let Some(v) = matcher(&event) {
                    return v;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    panic!("timed out waiting for {what}");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("lobby task died waiting for {what}");
            }
        }
    }
}

/// Run a solo machine briefly, then freeze it and capture its encoded
/// boot payload — the client-side half of the plug-in handshake.
fn solo_and_capture(
    rom: &[u8],
    crc: u32,
    run_for: Duration,
) -> (crate::session::SessionRuntime, Vec<u8>) {
    let binder = crate::platform::audio::LateBinder::new();
    let session = local::start(
        local::LocalArgs {
            rom: rom.to_vec(),
            rom_crc32: crc,
            save: None,
        },
        &binder,
        Arc::new(tokio::sync::Notify::new()),
    )
    .expect("solo boot");
    std::thread::sleep(run_for);
    session.shared.paused.store(true, Ordering::Relaxed);
    let blob = session
        .link
        .as_ref()
        .expect("solo sessions expose their link")
        .with_link(|link| BootBlob {
            state: link.capture_boot_state(0).unwrap(),
            save: link.export_save(0),
        })
        .unwrap()
        .encode()
        .unwrap();
    (session, blob)
}

#[test]
fn two_player_netplay_smoke() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .try_init();
    mgba::log::install_default_logger();

    // In-process signaling server on an ephemeral port.
    let listener = crate::runtime()
        .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
        .unwrap();
    let addr = listener.local_addr().unwrap();
    crate::runtime().spawn(async move {
        // Loopback: host candidates suffice, no STUN/TURN.
        let _ = gbaroll_signaling_server::serve(listener, vec![]).await;
    });
    let server_url = format!("ws://{addr}");

    let rom = mgba_siolink::testrom::build();
    let crc = crc32fast::hash(&rom);
    let replays_dir = tempfile::tempdir().unwrap();

    // Two solo machines already running, deliberately for different
    // lengths — the cable plugs into whatever state each is in.
    let (host_solo, host_blob) = solo_and_capture(&rom, crc, Duration::from_millis(400));
    let (guest_solo, guest_blob) = solo_and_capture(&rom, crc, Duration::from_millis(150));
    drop(host_solo);
    drop(guest_solo);

    let lobby_args = |mode| LobbyArgs {
        server_url: server_url.clone(),
        nick: "smoke".to_string(),
        rom_crc32: crc,
        rom_title: "TESTROM".to_string(),
        mode,
    };

    // Host creates; guest joins by code; both ready; host starts.
    let host = lobby::spawn(lobby_args(LobbyMode::Create));
    let code = wait_event(&host, "room code", Duration::from_secs(10), |e| match e {
        LobbyEvent::Joined { code } => Some(code.clone()),
        _ => None,
    });
    let guest = lobby::spawn(lobby_args(LobbyMode::Join { code }));
    wait_event(&guest, "join ack", Duration::from_secs(10), |e| match e {
        LobbyEvent::Joined { .. } => Some(()),
        _ => None,
    });
    // The host never readies up — only the guest does; the host's seat
    // must still read ready in the roster.
    guest.send(LobbyCommand::SetReady { ready: true });
    wait_event(
        &host,
        "everyone ready",
        Duration::from_secs(10),
        |e| match e {
            LobbyEvent::Roster { players, .. }
                if players.len() == 2 && players.iter().all(|p| p.ready) =>
            {
                Some(())
            }
            _ => None,
        },
    );
    host.send(LobbyCommand::Start);

    // Mesh + boot exchange (real datachannels over loopback): answer each
    // side's Starting with its prepared capture, then take the bundles.
    // Both lobbies MUST be pumped concurrently — each side's exchange
    // completes only once the other has answered Starting.
    let mut bundles: [Option<Box<SessionBundle>>; 2] = [None, None];
    {
        let sides = [(&host, &host_blob), (&guest, &guest_blob)];
        let deadline = Instant::now() + Duration::from_secs(60);
        while bundles.iter().any(|b| b.is_none()) {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for mesh + exchange"
            );
            for (i, (handle, blob)) in sides.iter().enumerate() {
                while let Ok(event) = handle.events.try_recv() {
                    match event {
                        LobbyEvent::Starting => handle.send(LobbyCommand::Boot((*blob).clone())),
                        LobbyEvent::SessionReady(bundle) => bundles[i] = Some(bundle),
                        LobbyEvent::Fatal(e) => panic!("lobby {i} died before session: {e}"),
                        _ => {}
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let host_bundle = bundles[0].take().unwrap();
    let guest_bundle = bundles[1].take().unwrap();

    // Every peer must hold the identical payload set after the exchange.
    assert_eq!(
        host_bundle.boots, guest_bundle.boots,
        "exchanged payloads differ"
    );
    assert_eq!(
        host_bundle.boots,
        vec![host_blob.clone(), guest_blob.clone()]
    );

    // Boot both sessions. No audio backend in tests; each session gets
    // its own silent binder.
    let start = |bundle: Box<SessionBundle>| {
        let binder = crate::platform::audio::LateBinder::new();
        netplay::start(
            NetplayArgs {
                bundle: *bundle,
                roms: vec![rom.clone(), rom.clone()],
                rom_meta: vec![(crc, "TESTROM".to_string(), "TEST".to_string()); 2],
                replays_dir: replays_dir.path().to_path_buf(),
                present_delay: 2,
            },
            &binder,
            Arc::new(tokio::sync::Notify::new()),
        )
        .expect("session boot")
    };
    let host_session = start(host_bundle);
    let guest_session = start(guest_bundle);

    // Run for a few seconds with wiggling inputs (so repeat-last
    // predictions miss and rollbacks actually happen), watching for
    // desync/disconnect ends. The cross-peer checkpoint digests are
    // live during this — surviving it means both peers rebuilt the
    // identical machine from the exchanged captures.
    for i in 0..40u32 {
        host_session
            .shared
            .joyflags
            .store(if i % 4 < 2 { 0x001 } else { 0x002 }, Ordering::Relaxed);
        guest_session
            .shared
            .joyflags
            .store(if i % 6 < 3 { 0x010 } else { 0x040 }, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(100));
        for (who, session) in [("host", &host_session), ("guest", &guest_session)] {
            if let Some(end) = session.shared.end.lock().unwrap().clone() {
                panic!("{who} session ended early: {end:?}");
            }
        }
    }

    let host_confirmed = host_session.shared.stats.lock().unwrap().confirmed;
    let guest_confirmed = guest_session.shared.stats.lock().unwrap().confirmed;
    assert!(
        host_confirmed > 60 && guest_confirmed > 60,
        "sessions barely progressed: host {host_confirmed}, guest {guest_confirmed}"
    );

    // Host pulls the cable; the guest should see a peer-quit (the
    // control-plane Quit beats the transport EOF), and BOTH sides get a
    // solo continuation handoff.
    host_session.shared.unplug.store(true, Ordering::Relaxed);
    let wait_end = |who: &str, session: &crate::session::SessionRuntime| -> SessionEnd {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(end) = session.shared.end.lock().unwrap().clone() {
                break end;
            }
            assert!(Instant::now() < deadline, "{who} never ended");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let host_end = wait_end("host", &host_session);
    assert!(
        matches!(host_end, SessionEnd::Unplugged),
        "unexpected host end: {host_end:?}"
    );
    let guest_end = wait_end("guest", &guest_session);
    assert!(
        matches!(
            guest_end,
            SessionEnd::PeerQuit { player: 0 } | SessionEnd::PeerDisconnected { player: 0 }
        ),
        "unexpected guest end: {guest_end:?}"
    );

    let host_handoff = host_session.shared.handoff.lock().unwrap().take();
    assert!(host_handoff.is_some(), "host unplug left no handoff");
    let guest_handoff = guest_session
        .shared
        .handoff
        .lock()
        .unwrap()
        .take()
        .expect("guest unplug left no handoff");
    drop(host_session);
    drop(guest_session);

    // The guest's machine continues solo from the unplug.
    let binder = crate::platform::audio::LateBinder::new();
    let resumed = local::resume(
        guest_handoff,
        crc,
        &binder,
        Arc::new(tokio::sync::Notify::new()),
    )
    .expect("resume");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let frontier = resumed.shared.stats.lock().unwrap().frontier;
        if frontier > 30 {
            break;
        }
        if let Some(end) = resumed.shared.end.lock().unwrap().clone() {
            panic!("resumed session ended early: {end:?}");
        }
        assert!(Instant::now() < deadline, "resumed session never advanced");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(resumed);

    // Both sides recorded replays that parse, carry the session, and
    // embed the boot captures playback needs.
    let mut replay_files: Vec<_> = std::fs::read_dir(replays_dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    replay_files.sort();
    assert_eq!(
        replay_files.len(),
        2,
        "expected two replays, got {replay_files:?}"
    );
    for path in replay_files {
        let replay = gbaroll_replay::Replay::parse(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(replay.num_players(), 2);
        assert!(
            replay.is_complete,
            "replay {} not finalized",
            path.display()
        );
        assert!(
            replay.inputs.len() > 60,
            "replay {} too short: {}",
            path.display(),
            replay.inputs.len()
        );
        assert_eq!(replay.metadata.players[0].rom_crc32, crc);
        assert!(
            replay.metadata.players.iter().all(|p| p.boot.is_some()),
            "replay {} is missing boot captures",
            path.display()
        );
    }
}

/// The ported playback engine: prefetch fills the keyframe store, and a
/// snapshot-load + step-forward seek lands on the *identical* state a
/// linear run reaches — the determinism the whole scrub path rests on.
#[test]
fn playback_engine_seek_is_exact() {
    use crate::session::playback::engine;

    let rom = mgba_siolink::testrom::build();
    let config = engine::BootConfig {
        roms: vec![rom.clone(), rom.clone()],
        boots: vec![None, None],
        rtc_unix_micros: Some(1_752_000_000_000_000),
    };
    // A varying input stream so states actually differ tick to tick.
    let inputs: Vec<Vec<u32>> = (0..240u32)
        .map(|i| vec![i & 0x3ff, (i * 3) & 0x3ff])
        .collect();

    let store = engine::SnapshotStore::new(inputs.len() as u32, 2);
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    engine::run_prefetch(&config, &inputs, store.clone(), progress.clone(), cancel).unwrap();
    assert_eq!(progress.load(Ordering::Relaxed), inputs.len() as u32);
    assert!(
        store.best_at_or_before(0).is_some(),
        "boot keyframe missing"
    );

    // Ground truth: a linear run to tick 150.
    let target = 150u32;
    let mut linear = engine::Playback::new(&config, std::sync::Arc::new(inputs.clone())).unwrap();
    while linear.cursor() < target {
        assert!(linear.step());
    }
    let truth = linear.capture().unwrap().state.digest();

    // Seek path: nearest keyframe at or before the target, then step.
    let mut seeked = engine::Playback::new(&config, std::sync::Arc::new(inputs.clone())).unwrap();
    let key = store
        .best_at_or_before(target)
        .expect("keyframe below target");
    assert!(
        key.tick <= target && key.tick > 0,
        "prefetch never keyframed mid-stream"
    );
    seeked.load(&key).unwrap();
    while seeked.cursor() < target {
        assert!(seeked.step());
    }
    assert_eq!(
        seeked.capture().unwrap().state.digest(),
        truth,
        "snapshot-restored seek diverged from the linear run"
    );
}

/// The threaded playback session: drive + prefetch + async seek worker
/// over a synthesized replay. Playback advances, an async backward seek
/// lands, and the scrub previews have snapshots to blit.
#[test]
fn playback_session_scrubs() {
    let rom = mgba_siolink::testrom::build();
    let crc = crc32fast::hash(&rom);
    let replay = {
        let meta = gbaroll_replay::Metadata {
            local_player: 0,
            started_at_unix_micros: Some(1_752_000_000_000_000),
            rtc_unix_micros: Some(1_752_000_000_000_000),
            players: (0..2)
                .map(|i| gbaroll_replay::PlayerMeta {
                    nick: format!("p{i}"),
                    rom_crc32: crc,
                    rom_title: "TESTROM".to_string(),
                    rom_code: "TEST".to_string(),
                    boot: None,
                })
                .collect(),
        };
        let mut w = gbaroll_replay::Writer::new(Vec::new(), &meta).unwrap();
        for i in 0..300u32 {
            w.push(&[i & 0x3ff, (i * 3) & 0x3ff]).unwrap();
        }
        gbaroll_replay::Replay::parse(&w.finish().unwrap()).unwrap()
    };

    let mut binder = crate::platform::audio::LateBinder::new();
    binder.set_sample_rate(48_000);
    let mut audio_tap = binder.clone();
    let session = crate::session::playback::start(
        crate::session::playback::PlaybackArgs {
            replay,
            roms: vec![rom.clone(), rom],
        },
        &binder,
        Arc::new(tokio::sync::Notify::new()),
    )
    .unwrap();
    // Full speed for the test.
    session.shared.speed.store(400, Ordering::Relaxed);

    // Playback advances.
    let deadline = Instant::now() + Duration::from_secs(30);
    while session.shared.position.load(Ordering::Relaxed) < 100 {
        assert!(Instant::now() < deadline, "playback never advanced");
        if let Some(end) = session.shared.end.lock().unwrap().clone() {
            panic!("playback ended early: {end:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Exercise the callback-facing PCM handoff while the drive captures
    // snapshots. This diagnostic ROM does not initialize GBA sound, so
    // an empty read is valid; importantly it must not affect pacing.
    for _ in 0..20 {
        let mut pcm = [[0i16; 2]; 480];
        let _ = crate::platform::audio::Stream::fill(&mut audio_tap, &mut pcm);
        std::thread::sleep(Duration::from_millis(10));
    }

    // Async backward seek: pause (like a scrub press), request, land.
    let handles = session.playback.as_ref().unwrap();
    session.shared.paused.store(true, Ordering::Relaxed);
    let mut paused_pcm = [[0i16; 2]; 480];
    assert_eq!(
        crate::platform::audio::Stream::fill(&mut audio_tap, &mut paused_pcm),
        0,
        "paused replay leaked queued audio"
    );
    handles.seek.request(30, false);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let position = session.shared.position.load(Ordering::Relaxed);
        if position == 30 && handles.seek.pending_target().is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "seek never landed (at {position})"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    // The stores can preview around the playhead.
    assert!(
        handles.nearest_snapshot(30).is_some(),
        "no snapshot near the playhead"
    );

    // Resume-after-seek: forward this time.
    handles.seek.request(120, true);
    let deadline = Instant::now() + Duration::from_secs(20);
    while session.shared.paused.load(Ordering::Relaxed) {
        assert!(Instant::now() < deadline, "seek resume never fired");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(session);
}
