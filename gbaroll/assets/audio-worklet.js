// The gbaroll audio sink: an interleaved-i16 ring buffer the main
// thread fills via port.postMessage (no SharedArrayBuffer anywhere).
// Every 4th render quantum (~10.7ms at 48kHz) it reports its queue
// depth back — which the main thread also uses as a tick source when
// the tab is hidden and requestAnimationFrame stops.
class GbarollSink extends AudioWorkletProcessor {
    constructor() {
        super();
        this.capacity = 16384; // frames
        this.ring = new Int16Array(this.capacity * 2);
        this.readPos = 0;
        this.len = 0;
        this.sinceReport = 0;
        this.port.onmessage = (e) => this.push(e.data);
    }

    // chunk: Int16Array of interleaved stereo frames.
    push(chunk) {
        let frames = chunk.length >> 1;
        // Overflow: drop oldest so latency stays bounded.
        const overflow = this.len + frames - this.capacity;
        if (overflow > 0) {
            this.readPos = (this.readPos + overflow) % this.capacity;
            this.len -= overflow;
        }
        let writePos = (this.readPos + this.len) % this.capacity;
        for (let i = 0; i < frames; i++) {
            const w = writePos * 2;
            this.ring[w] = chunk[i * 2];
            this.ring[w + 1] = chunk[i * 2 + 1];
            writePos = (writePos + 1) % this.capacity;
        }
        this.len += frames;
    }

    process(inputs, outputs) {
        const out = outputs[0];
        const left = out[0];
        const right = out[1] || out[0];
        const n = left.length;
        for (let i = 0; i < n; i++) {
            if (this.len > 0) {
                const r = this.readPos * 2;
                left[i] = this.ring[r] / 32768;
                right[i] = this.ring[r + 1] / 32768;
                this.readPos = (this.readPos + 1) % this.capacity;
                this.len--;
            } else {
                left[i] = 0;
                right[i] = 0;
            }
        }
        if (++this.sinceReport >= 4) {
            this.sinceReport = 0;
            this.port.postMessage(this.len);
        }
        return true;
    }
}

registerProcessor("gbaroll-sink", GbarollSink);
