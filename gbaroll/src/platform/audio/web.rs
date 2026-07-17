//! The web audio sink: an `AudioContext` + `AudioWorkletNode` whose
//! processor holds a short ring buffer — Rust in gbaroll-worklet's
//! wasm module, wrapped by the registration shell in
//! assets/audio-worklet.js. The worklet runs on the browser's audio
//! rendering thread and cannot call into this wasm module, so the flow
//! inverts relative to a native callback backend: the runtime pump
//! *pushes* — it computes the sink's deficit against a fixed latency
//! target, pulls that many frames through the
//! [`LateBinder`](super::LateBinder), and postMessages the chunk over.
//! The worklet reports its queue depth back every ~10.7ms; that report
//! is also the pump's hidden-tab tick source, since it keeps firing
//! when requestAnimationFrame stops.

use std::cell::Cell;
use std::rc::Rc;

use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use super::{LateBinder, Stream};

/// Steady-state sink depth: ~64ms at 48kHz. Chosen to absorb a full
/// rAF gap plus a catch-up tick burst plus worklet message jitter
/// without underrun (the native SDL backend ran ~30-40ms).
const TARGET_QUEUED_FRAMES: u32 = 3072;

/// Don't bother posting chunks smaller than this (one render quantum).
const MIN_CHUNK_FRAMES: u32 = 128;

/// The processor's DSP module (../gbaroll-worklet), compiled by
/// build.rs. Shipped to the worklet via processorOptions: its scope
/// can't fetch, and the main wasm-bindgen module can't run there.
const WORKLET_WASM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/gbaroll_worklet.wasm"));

pub struct WebAudio {
    ctx: web_sys::AudioContext,
    node: web_sys::AudioWorkletNode,
    /// Frames queued in the worklet as of its last report.
    reported_queued: Rc<Cell<u32>>,
    /// Frames we've posted since that report (the estimate's other
    /// half); the report handler zeroes it, since everything sent
    /// before the report is already counted inside it.
    sent_since_report: Rc<Cell<u32>>,
    scratch: Vec<[i16; super::NUM_CHANNELS]>,
    /// Keeps the report closure alive for the node's lifetime.
    _onmessage: Closure<dyn FnMut(web_sys::MessageEvent)>,
}

impl WebAudio {
    /// Build the sink. Must be called from a user gesture (autoplay
    /// policy); `on_report` fires on every worklet queue report — the
    /// hidden-tab pump source.
    pub async fn create(
        worklet_url: &str,
        on_report: impl Fn() + 'static,
    ) -> Result<WebAudio, JsValue> {
        let opts = web_sys::AudioContextOptions::new();
        opts.set_sample_rate(48_000.0);
        let ctx = web_sys::AudioContext::new_with_context_options(&opts)?;
        JsFuture::from(ctx.audio_worklet()?.add_module(worklet_url)?).await?;
        // Without an explicit outputChannelCount, a worklet node with an
        // unconnected input computes a MONO output — and a mono sink
        // silently drops one side of every pan.
        let node_opts = web_sys::AudioWorkletNodeOptions::new();
        let counts = js_sys::Array::of1(&wasm_bindgen::JsValue::from_f64(2.0));
        node_opts.set_output_channel_count(&counts);
        // The DSP module rides along; the shim compiles and
        // instantiates it in the worklet scope.
        let processor_opts = js_sys::Object::new();
        js_sys::Reflect::set(
            &processor_opts,
            &"wasm".into(),
            &js_sys::Uint8Array::from(WORKLET_WASM).into(),
        )?;
        node_opts.set_processor_options(Some(&processor_opts));
        let node =
            web_sys::AudioWorkletNode::new_with_options(&ctx, "gbaroll-sink", &node_opts)?;
        node.connect_with_audio_node(&ctx.destination())?;

        let reported_queued = Rc::new(Cell::new(0u32));
        let sent_since_report = Rc::new(Cell::new(0u32));
        let onmessage = {
            let reported_queued = reported_queued.clone();
            let sent_since_report = sent_since_report.clone();
            Closure::new(move |e: web_sys::MessageEvent| {
                if let Some(n) = e.data().as_f64() {
                    reported_queued.set(n as u32);
                    sent_since_report.set(0);
                    // Debug probe: sink depth, readable from devtools.
                    let _ = js_sys::Reflect::set(
                        &js_sys::global(),
                        &"gbarollAudioQueue".into(),
                        &n.into(),
                    );
                }
                on_report();
            })
        };
        node.port()?
            .set_onmessage(Some(onmessage.as_ref().unchecked_ref()));

        Ok(WebAudio {
            ctx,
            node,
            reported_queued,
            sent_since_report,
            scratch: Vec::new(),
            _onmessage: onmessage,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.ctx.sample_rate() as u32
    }

    /// Top the sink up to the latency target: estimate its depth from
    /// the last report plus everything sent since, pull the deficit
    /// through the binder, and post it over. `sent_since_report` resets
    /// on each report, so the estimate errs high (frames the worklet
    /// consumed since reporting still count) — the safe direction.
    pub fn pump(&mut self, binder: &mut LateBinder) {
        let estimate = self.reported_queued.get() + self.sent_since_report.get();
        if estimate + MIN_CHUNK_FRAMES > TARGET_QUEUED_FRAMES {
            return;
        }
        let deficit = (TARGET_QUEUED_FRAMES - estimate) as usize;
        self.scratch.resize(deficit, [0, 0]);
        let n = binder.fill(&mut self.scratch[..deficit]);
        if n == 0 {
            return;
        }
        let flat: &[i16] = bytemuck::cast_slice(&self.scratch[..n]);
        let chunk = js_sys::Int16Array::from(flat);
        if let Ok(port) = self.node.port() {
            let transfer = js_sys::Array::of1(&chunk.buffer());
            let _ = port.post_message_with_transferable(&chunk, &transfer);
        }
        self.sent_since_report
            .set(self.sent_since_report.get() + n as u32);
    }

    /// Prime the sink with silence. The deficit pump alone can't build
    /// a cushion — steady-state production exactly matches consumption
    /// (the servo keeps the queue on the core side) — so without this
    /// the ring's floor sits at zero and every pump-scheduling hiccup
    /// is an audible gap. ~43ms of fixed latency buys dropout immunity;
    /// the native SDL path carried a similar total.
    pub fn prime(&mut self, frames: usize) {
        self.scratch.clear();
        self.scratch.resize(frames, [0, 0]);
        let flat: &[i16] = bytemuck::cast_slice(&self.scratch);
        let chunk = js_sys::Int16Array::from(flat);
        if let Ok(port) = self.node.port() {
            let transfer = js_sys::Array::of1(&chunk.buffer());
            let _ = port.post_message_with_transferable(&chunk, &transfer);
        }
        self.sent_since_report
            .set(self.sent_since_report.get() + frames as u32);
    }

    /// The context auto-suspends without a gesture and on some
    /// backgrounding paths; poke it whenever we're pumping.
    pub fn resume_if_suspended(&self) {
        if self.ctx.state() == web_sys::AudioContextState::Suspended {
            let _ = self.ctx.resume();
        }
    }
}

impl Drop for WebAudio {
    fn drop(&mut self) {
        let _ = self.ctx.close();
    }
}
