//! Replay-playback audio (the PCM queue the playback drive fills and
//! the callback-facing stream that drains it). Not compiled on web
//! yet — this returns when the replay playback port lands.

/// Replay audio is produced by the playback drive thread after each
/// emulated tick. The host callback only drains this short queue, so a
/// snapshot capture or seek holding the playback mutex cannot turn into
/// a callback-sized hole in the output.
#[derive(Clone)]
pub struct ReplayAudioQueue {
    inner: std::sync::Arc<std::sync::Mutex<ReplayAudioState>>,
    sample_rate: u32,
}

struct ReplayAudioState {
    frames: std::collections::VecDeque<[i16; NUM_CHANNELS]>,
    generation: u64,
    prime_frames: usize,
    max_frames: usize,
}

impl ReplayAudioQueue {
    pub fn new(sample_rate: u32) -> Self {
        let prime_frames = ((sample_rate as f64 * TARGET_QUEUED_SECS).ceil() as usize).max(1);
        let max_frames = ((sample_rate as f64 * 0.25).ceil() as usize).max(prime_frames);
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(ReplayAudioState {
                frames: std::collections::VecDeque::with_capacity(max_frames),
                generation: 0,
                prime_frames,
                max_frames,
            })),
            sample_rate,
        }
    }

    /// Drop everything already published. The generation makes a
    /// producer that was concurrently resampling stale audio discard its
    /// result instead of appending it after the reset.
    pub fn reset(&self) -> u64 {
        let mut state = self.inner.lock().unwrap();
        state.frames.clear();
        state.generation = state.generation.wrapping_add(1);
        state.generation
    }

    fn reset_for_rate(&self, fps_target: f32) -> u64 {
        let mut state = self.inner.lock().unwrap();
        state.frames.clear();
        state.generation = state.generation.wrapping_add(1);
        // At very slow playback one emulated frame produces more than
        // 50ms of host audio. Require roughly one and a half ticks so
        // the callback never empties the only chunk while waiting for
        // the next slow tick.
        let seconds = TARGET_QUEUED_SECS.max(1.5 / fps_target.max(1.0) as f64);
        state.prime_frames = ((self.sample_rate as f64 * seconds).ceil() as usize)
            .max(1)
            .min(state.max_frames);
        state.generation
    }

    fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
    }

    fn push(&self, generation: u64, frames: &[[i16; NUM_CHANNELS]]) -> bool {
        let mut state = self.inner.lock().unwrap();
        if state.generation != generation {
            return false;
        }
        let overflow = state
            .frames
            .len()
            .saturating_add(frames.len())
            .saturating_sub(state.max_frames);
        if overflow > 0 {
            let drain = overflow.min(state.frames.len());
            state.frames.drain(..drain);
        }
        let start = frames.len().saturating_sub(state.max_frames);
        state.frames.extend(frames[start..].iter().copied());
        true
    }

    #[cfg(test)]
    fn needs_prebuffer(&self) -> bool {
        if self.sample_rate == 0 {
            return false;
        }
        let state = self.inner.lock().unwrap();
        state.frames.len() < state.prime_frames
    }
}

/// Callback-facing half of replay audio. It deliberately has no handle
/// to the playback link: all it can do is consume published PCM.
pub struct ReplayStream {
    queue: ReplayAudioQueue,
    shared: std::sync::Arc<crate::session::SharedSession>,
    generation: u64,
    primed: bool,
    silent: bool,
    config: (usize, u32),
}

impl ReplayStream {
    pub fn new(
        queue: ReplayAudioQueue,
        shared: std::sync::Arc<crate::session::SharedSession>,
    ) -> Self {
        let generation = queue.generation();
        let config = (
            shared
                .view_player
                .load(std::sync::atomic::Ordering::Relaxed),
            shared
                .speed
                .load(std::sync::atomic::Ordering::Relaxed)
                .max(25),
        );
        Self {
            queue,
            shared,
            generation,
            primed: false,
            silent: true,
            config,
        }
    }
}

impl Stream for ReplayStream {
    fn fill(&mut self, buf: &mut [[i16; NUM_CHANNELS]]) -> usize {
        let config = (
            self.shared
                .view_player
                .load(std::sync::atomic::Ordering::Relaxed),
            self.shared
                .speed
                .load(std::sync::atomic::Ordering::Relaxed)
                .max(25),
        );
        if config != self.config {
            self.config = config;
            self.generation = self.queue.reset();
            self.primed = false;
            return 0;
        }

        let paused = self
            .shared
            .paused
            .load(std::sync::atomic::Ordering::Acquire);
        let fps_target = f32::from_bits(
            self.shared
                .fps_target
                .load(std::sync::atomic::Ordering::Relaxed),
        );
        if paused || fps_target <= 0.0 {
            if !self.silent {
                self.generation = self.queue.reset();
                self.silent = true;
            }
            self.primed = false;
            return 0;
        }
        self.silent = false;

        let mut state = self.queue.inner.lock().unwrap();
        if state.generation != self.generation {
            self.generation = state.generation;
            self.primed = false;
        }
        if !self.primed {
            if state.frames.len() < state.prime_frames {
                return 0;
            }
            self.primed = true;
        }

        let available = state.frames.len().min(buf.len());
        for dst in &mut buf[..available] {
            *dst = state.frames.pop_front().unwrap();
        }
        if available < buf.len() {
            self.primed = false;
        }
        available
    }
}

/// Drive-thread half of replay audio. Resampling here is inexpensive and
/// keeps both the emulator mutex and mGBA's buffers out of the real-time
/// callback.
pub struct ReplayAudioProducer {
    queue: ReplayAudioQueue,
    sample_rate: u32,
    resampler: mgba::audio::AudioResampler,
    dest_buffer: mgba::audio::AudioBuffer,
    dest_capacity: usize,
    scratch: Vec<[i16; NUM_CHANNELS]>,
    generation: u64,
    config: Option<(usize, u32)>,
    discard_source: bool,
}

impl ReplayAudioProducer {
    pub fn new(queue: ReplayAudioQueue) -> Self {
        let dest_capacity = (queue.sample_rate as usize / 4).max(SAMPLES * 2);
        let generation = queue.generation();
        Self {
            sample_rate: queue.sample_rate,
            queue,
            resampler: mgba::audio::AudioResampler::new(),
            dest_buffer: mgba::audio::AudioBuffer::new(dest_capacity, NUM_CHANNELS as u32),
            dest_capacity,
            scratch: Vec::new(),
            generation,
            config: None,
            discard_source: true,
        }
    }

    pub fn reset(&mut self) {
        self.generation = self.queue.reset();
        self.reset_resampler();
        self.discard_source = true;
    }

    fn reset_resampler(&mut self) {
        self.resampler = mgba::audio::AudioResampler::new();
        self.dest_buffer = mgba::audio::AudioBuffer::new(self.dest_capacity, NUM_CHANNELS as u32);
        self.scratch.clear();
    }

    fn clear_sources(link: &mut mgba_siolink::Link) {
        for i in 0..link.num_players() {
            link.core_mut(i).audio_buffer().clear();
        }
    }

    pub fn publish(
        &mut self,
        link: &mut mgba_siolink::Link,
        player: usize,
        speed_percent: u32,
        fps_target: f32,
    ) {
        let player = player.min(link.num_players() - 1);
        let config = (player, speed_percent);
        if self.config != Some(config) {
            self.config = Some(config);
            self.generation = self.queue.reset_for_rate(fps_target);
            self.reset_resampler();
            self.discard_source = true;
        }

        let generation = self.queue.generation();
        if generation != self.generation {
            self.generation = generation;
            self.reset_resampler();
            self.discard_source = true;
        }

        if self.discard_source || self.sample_rate == 0 {
            Self::clear_sources(link);
            self.discard_source = false;
            return;
        }

        for i in 0..link.num_players() {
            if i != player {
                link.core_mut(i).audio_buffer().clear();
            }
        }

        let rate = link.core(player).audio_sample_rate() as f64;
        let faux_clock = link
            .core(player)
            .calculate_framerate_ratio(fps_target as f64);
        let mut core = link.core_mut(player);
        let mut source = core.audio_buffer();
        self.resampler.set_source(&mut source, rate, true);
        self.resampler
            .set_destination(&mut self.dest_buffer, self.sample_rate as f64 * faux_clock);
        self.resampler.process();

        let available = self.dest_buffer.available();
        self.scratch.resize(available, [0, 0]);
        let linear: &mut [i16] = bytemuck::cast_slice_mut(&mut self.scratch[..]);
        let read = self.dest_buffer.read(linear, available);
        self.queue.push(self.generation, &self.scratch[..read]);
    }
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn playing_shared() -> std::sync::Arc<crate::session::SharedSession> {
        let shared =
            crate::session::SharedSession::new(0, std::sync::Arc::new(tokio::sync::Notify::new()));
        shared.set_fps_target(crate::session::EXPECTED_FPS);
        shared
    }

    #[test]
    fn replay_stream_primes_and_rebuffers_after_an_underrun() {
        let shared = playing_shared();
        let queue = ReplayAudioQueue::new(1_000);
        let generation = queue.generation();
        let mut stream = ReplayStream::new(queue.clone(), shared);

        assert!(queue.push(generation, &[[1, 1]; 49]));
        assert_eq!(stream.fill(&mut [[0, 0]; 20]), 0);
        assert!(queue.push(generation, &[[2, 2]; 1]));
        assert_eq!(stream.fill(&mut [[0, 0]; 40]), 40);
        assert_eq!(stream.fill(&mut [[0, 0]; 20]), 10);
        assert!(queue.push(generation, &[[3, 3]; 39]));
        assert_eq!(stream.fill(&mut [[0, 0]; 20]), 0);
        assert!(queue.push(generation, &[[4, 4]; 11]));
        assert_eq!(stream.fill(&mut [[0, 0]; 20]), 20);
    }

    #[test]
    fn replay_queue_rejects_audio_from_before_a_reset() {
        let queue = ReplayAudioQueue::new(1_000);
        let stale_generation = queue.generation();
        queue.reset();

        assert!(!queue.push(stale_generation, &[[1, 1]; 50]));
        assert!(queue.needs_prebuffer());
    }

    #[test]
    fn replay_stream_resets_immediately_when_paused() {
        let shared = playing_shared();
        let queue = ReplayAudioQueue::new(1_000);
        let generation = queue.generation();
        assert!(queue.push(generation, &[[1, 1]; 50]));
        let mut stream = ReplayStream::new(queue.clone(), shared.clone());
        assert_eq!(stream.fill(&mut [[0, 0]; 10]), 10);

        shared.paused.store(true, Ordering::Release);
        assert_eq!(stream.fill(&mut [[0, 0]; 10]), 0);
        assert!(queue.needs_prebuffer());
        assert!(!queue.push(generation, &[[2, 2]; 50]));
    }
}
