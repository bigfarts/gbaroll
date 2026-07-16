//! WebRTC peer transport, browser flavor: the two negotiated
//! fixed-stream-id datachannels the protocol expects ("gbaroll-ctl"
//! id 0 reliable+ordered, "gbaroll-data" id 1 unordered, zero
//! retransmits — rennet's redundancy replaces reliability), over
//! `web_sys::RtcPeerConnection`. Sends are synchronous; receives pull
//! from unbounded channels fed by the `onmessage` callbacks; channel
//! opens are explicit awaitable barriers (the web has no
//! blocks-until-open first send).

use std::cell::RefCell;
use std::rc::Rc;

use futures::channel::{mpsc, oneshot};
use futures::StreamExt;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelType, RtcIceCandidateInit,
    RtcPeerConnection, RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit,
};

fn jserr(what: &str, e: JsValue) -> anyhow::Error {
    anyhow::anyhow!("{what}: {e:?}")
}

/// Connection-level events the mesh loop consumes.
pub enum PeerEvent {
    /// A trickled local ICE candidate to relay to the peer.
    Candidate(String),
    /// The connection failed or closed.
    Failed,
}

/// One live peer connection. Dropping it tears the transport down.
pub struct PeerConnection {
    pc: RtcPeerConnection,
    _closures: Vec<Closure<dyn FnMut(web_sys::Event)>>,
}

/// The sending half of a datachannel. `send` is synchronous — the
/// browser buffers; `buffered_amount` exposes the backlog for teardown
/// draining.
#[derive(Clone)]
pub struct ChannelSender {
    dc: RtcDataChannel,
}

impl ChannelSender {
    pub fn send(&self, bytes: &[u8]) -> anyhow::Result<()> {
        self.dc
            .send_with_u8_array(bytes)
            .map_err(|e| jserr("datachannel send", e))
    }

    pub fn buffered_amount(&self) -> u32 {
        self.dc.buffered_amount()
    }
}

/// The receiving half: `None` once the channel has closed.
pub struct ChannelReceiver {
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl ChannelReceiver {
    pub async fn receive(&mut self) -> Option<Vec<u8>> {
        self.rx.next().await
    }
}

/// Everything `new` hands back for one peer.
pub struct PeerParts {
    pub pc: PeerConnection,
    pub events: mpsc::UnboundedReceiver<PeerEvent>,
    pub ctl_tx: ChannelSender,
    pub ctl_rx: ChannelReceiver,
    pub data_tx: ChannelSender,
    pub data_rx: ChannelReceiver,
    /// Resolves when the control channel opens (the handshake barrier).
    pub ctl_open: oneshot::Receiver<()>,
}

pub fn new(ice_servers: &[gbaroll_signaling::IceServer]) -> anyhow::Result<PeerParts> {
    let config = RtcConfiguration::new();
    let servers = js_sys::Array::new();
    for server in ice_servers {
        let entry = web_sys::RtcIceServer::new();
        let urls = js_sys::Array::new();
        for url in &server.urls {
            urls.push(&JsValue::from_str(url));
        }
        entry.set_urls(&urls);
        if let Some(username) = &server.username {
            entry.set_username(username);
        }
        if let Some(credential) = &server.credential {
            entry.set_credential(credential);
        }
        servers.push(&entry);
    }
    config.set_ice_servers(&servers);
    let pc = RtcPeerConnection::new_with_configuration(&config)
        .map_err(|e| jserr("create peer connection", e))?;

    let mut closures = Vec::new();
    let (event_tx, events) = mpsc::unbounded::<PeerEvent>();

    // Both channels are negotiated on fixed stream ids, so both sides
    // just create them — no in-band open announcement.
    let ctl_init = RtcDataChannelInit::new();
    ctl_init.set_negotiated(true);
    ctl_init.set_id(0);
    let ctl = pc.create_data_channel_with_data_channel_dict("gbaroll-ctl", &ctl_init);

    let data_init = RtcDataChannelInit::new();
    data_init.set_negotiated(true);
    data_init.set_id(1);
    data_init.set_ordered(false);
    data_init.set_max_retransmits(0);
    let data = pc.create_data_channel_with_data_channel_dict("gbaroll-data", &data_init);

    let (ctl_rx, ctl_open) = wire_channel(&ctl, &mut closures, true);
    let (data_rx, _) = wire_channel(&data, &mut closures, false);

    {
        let event_tx = event_tx.clone();
        let onicecandidate: Closure<dyn FnMut(web_sys::Event)> =
            Closure::new(move |e: web_sys::Event| {
                let e: web_sys::RtcPeerConnectionIceEvent = e.unchecked_into();
                if let Some(candidate) = e.candidate() {
                    let candidate = candidate.candidate();
                    // The empty candidate is end-of-candidates; peers
                    // don't need it.
                    if !candidate.is_empty() {
                        let _ = event_tx.unbounded_send(PeerEvent::Candidate(candidate));
                    }
                }
            });
        pc.set_onicecandidate(Some(onicecandidate.as_ref().unchecked_ref()));
        closures.push(onicecandidate);
    }
    {
        let event_tx = event_tx.clone();
        let pc2 = pc.clone();
        let onstate: Closure<dyn FnMut(web_sys::Event)> = Closure::new(move |_| {
            match pc2.connection_state() {
                RtcPeerConnectionState::Failed | RtcPeerConnectionState::Closed => {
                    let _ = event_tx.unbounded_send(PeerEvent::Failed);
                }
                // "disconnected" can self-heal; let ICE keep trying.
                _ => {}
            }
        });
        pc.set_onconnectionstatechange(Some(onstate.as_ref().unchecked_ref()));
        closures.push(onstate);
    }

    Ok(PeerParts {
        pc: PeerConnection {
            pc,
            _closures: closures,
        },
        events,
        ctl_tx: ChannelSender { dc: ctl },
        ctl_rx,
        data_tx: ChannelSender { dc: data },
        data_rx,
        ctl_open,
    })
}

/// Hook one datachannel's callbacks up: messages into an unbounded
/// channel (closed on channel close), plus an open barrier.
fn wire_channel(
    dc: &RtcDataChannel,
    closures: &mut Vec<Closure<dyn FnMut(web_sys::Event)>>,
    want_open: bool,
) -> (ChannelReceiver, oneshot::Receiver<()>) {
    dc.set_binary_type(RtcDataChannelType::Arraybuffer);
    let (tx, rx) = mpsc::unbounded::<Vec<u8>>();
    {
        let tx = tx.clone();
        let onmessage: Closure<dyn FnMut(web_sys::Event)> = Closure::new(move |e: web_sys::Event| {
            let e: web_sys::MessageEvent = e.unchecked_into();
            if let Ok(buf) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
                let _ = tx.unbounded_send(js_sys::Uint8Array::new(&buf).to_vec());
            }
        });
        dc.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        closures.push(onmessage);
    }
    {
        let onclose: Closure<dyn FnMut(web_sys::Event)> = Closure::new(move |_| {
            tx.close_channel();
        });
        dc.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        closures.push(onclose);
    }
    let (open_tx, open_rx) = oneshot::channel::<()>();
    if want_open {
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));
        let onopen: Closure<dyn FnMut(web_sys::Event)> = Closure::new(move |_| {
            if let Some(tx) = open_tx.borrow_mut().take() {
                let _ = tx.send(());
            }
        });
        dc.set_onopen(Some(onopen.as_ref().unchecked_ref()));
        closures.push(onopen);
    }
    (ChannelReceiver { rx }, open_rx)
}

impl PeerConnection {
    /// Create and install the local offer; returns its SDP for the
    /// signaling relay (candidates trickle separately).
    pub async fn create_offer(&self) -> anyhow::Result<String> {
        let offer = JsFuture::from(self.pc.create_offer())
            .await
            .map_err(|e| jserr("create offer", e))?;
        JsFuture::from(
            self.pc
                .set_local_description(offer.unchecked_ref::<RtcSessionDescriptionInit>()),
        )
        .await
        .map_err(|e| jserr("set local offer", e))?;
        self.local_sdp()
    }

    /// Install the peer's offer and produce our installed answer's SDP.
    pub async fn accept_offer(&self, sdp: &str) -> anyhow::Result<String> {
        self.set_remote(RtcSdpType::Offer, sdp).await?;
        let answer = JsFuture::from(self.pc.create_answer())
            .await
            .map_err(|e| jserr("create answer", e))?;
        JsFuture::from(
            self.pc
                .set_local_description(answer.unchecked_ref::<RtcSessionDescriptionInit>()),
        )
        .await
        .map_err(|e| jserr("set local answer", e))?;
        self.local_sdp()
    }

    pub async fn accept_answer(&self, sdp: &str) -> anyhow::Result<()> {
        self.set_remote(RtcSdpType::Answer, sdp).await
    }

    async fn set_remote(&self, sdp_type: RtcSdpType, sdp: &str) -> anyhow::Result<()> {
        let desc = RtcSessionDescriptionInit::new(sdp_type);
        desc.set_sdp(sdp);
        JsFuture::from(self.pc.set_remote_description(&desc))
            .await
            .map_err(|e| jserr("set remote description", e))?;
        Ok(())
    }

    fn local_sdp(&self) -> anyhow::Result<String> {
        Ok(self
            .pc
            .local_description()
            .ok_or_else(|| anyhow::anyhow!("local description missing"))?
            .sdp())
    }

    pub async fn add_remote_candidate(&self, candidate: &str) -> anyhow::Result<()> {
        let init = RtcIceCandidateInit::new(candidate);
        // Datachannel-only SDPs have a single m-line.
        init.set_sdp_m_line_index(Some(0));
        JsFuture::from(
            self.pc
                .add_ice_candidate_with_opt_rtc_ice_candidate_init(Some(&init)),
        )
        .await
        .map_err(|e| jserr("add ice candidate", e))?;
        Ok(())
    }

    pub fn close(&self) {
        self.pc.close();
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        self.pc.set_onicecandidate(None);
        self.pc.set_onconnectionstatechange(None);
        self.pc.close();
    }
}
