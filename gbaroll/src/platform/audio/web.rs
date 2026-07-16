//! The web audio sink: an `AudioContext` + `AudioWorkletNode` whose
//! processor holds a short ring buffer (assets/audio-worklet.js). The
//! worklet runs on the browser's audio rendering thread and cannot call
//! into this wasm module, so the flow inverts relative to a native
//! callback backend: the runtime pump *pushes* — it computes the sink's
//! deficit against a fixed latency target, pulls that many frames
//! through the [`LateBinder`](super::LateBinder), and postMessages the
//! chunk over. The worklet reports its queue depth back every ~10.7ms;
//! that report is also the pump's hidden-tab tick source, since it
//! keeps firing when requestAnimationFrame stops.

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

pub struct WebAudio {
    ctx: web_sys::AudioContext,
    node: web_sys::AudioWorkletNode,
    /// Frames queued in the worklet as of its last report.
    reported_queued: Rc<Cell<u32>>,
    /// Frames we've posted since that report (the estimate's other half).
    sent_since_report: Cell<u32>,
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
        let node = web_sys::AudioWorkletNode::new(&ctx, "gbaroll-sink")?;
        node.connect_with_audio_node(&ctx.destination())?;

        let reported_queued = Rc::new(Cell::new(0u32));
        let onmessage = {
            let reported_queued = reported_queued.clone();
            Closure::new(move |e: web_sys::MessageEvent| {
                if let Some(n) = e.data().as_f64() {
                    reported_queued.set(n as u32);
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
            sent_since_report: Cell::new(0),
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
