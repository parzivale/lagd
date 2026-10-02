//! An interleaved `f32` ring buffer whose read head sits a settable distance
//! behind the write head.
//!
//! The read position is *derived* from the write position on every pull rather
//! than tracked independently. That makes "the delay is exactly N ms" literally
//! true and leaves no drift to accumulate; the cost is that if the playback
//! node ever runs twice between two capture cycles it re-reads the same window
//! for one quantum. In a normal graph both nodes run once per driver cycle, so
//! that does not happen.

use std::mem;

/// Bytes per sample on the wire. Both streams negotiate `F32LE`.
const SAMPLE_BYTES: usize = 4;

/// A delay line for audio, with a click-free delay change.
///
/// A delay change moves the read head, which is a step discontinuity in the
/// signal — audible as a click. Crossfading between the old and new read
/// positions is what makes live adjustment usable rather than merely possible.
pub struct DelayRing {
    buf: Vec<f32>,
    capacity_frames: usize,
    channels: usize,
    /// Total frames ever written. Monotonic; the modulo into `buf` happens at
    /// access time.
    written: u64,
    /// The delay currently in force, in frames.
    delay_frames: usize,
    fade: Option<Fade>,
    fade_len: usize,
    underruns: u64,
    /// Decode and encode scratch. The `PipeWire` callbacks run in real-time
    /// context, so they must not allocate; these are sized once and reused.
    in_scratch: Vec<f32>,
    out_scratch: Vec<f32>,
}

struct Fade {
    from: usize,
    to: usize,
    done: usize,
}

impl DelayRing {
    /// `max_delay_frames` sets the buffer size; `fade_frames` is how long a
    /// delay change is smeared over.
    ///
    /// # Panics
    ///
    /// If `channels` is zero.
    #[must_use]
    pub fn new(channels: usize, max_delay_frames: usize, fade_frames: usize) -> Self {
        assert!(channels > 0, "a ring needs at least one channel");
        // Headroom beyond the maximum delay so a pull that spans a quantum can
        // always reach back far enough without wrapping onto itself.
        let capacity_frames = (max_delay_frames + fade_frames)
            .next_power_of_two()
            .max(4096);
        Self {
            buf: vec![0.0; capacity_frames * channels],
            capacity_frames,
            channels,
            written: 0,
            delay_frames: 0,
            fade: None,
            fade_len: fade_frames.max(1),
            underruns: 0,
            in_scratch: Vec::new(),
            out_scratch: Vec::new(),
        }
    }

    /// The longest delay this buffer can serve without reading overwritten
    /// audio.
    #[must_use]
    pub fn max_delay_frames(&self) -> usize {
        // Leave a quantum's worth of slack between the heads.
        self.capacity_frames.saturating_sub(self.fade_len + 2048)
    }

    /// Times a pull asked for audio that had already been overwritten.
    #[must_use]
    pub fn underruns(&self) -> u64 {
        self.underruns
    }

    /// Appends interleaved frames.
    pub fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(self.channels) {
            let base = (self.written as usize % self.capacity_frames) * self.channels;
            self.buf[base..base + self.channels].copy_from_slice(frame);
            self.written += 1;
        }
    }

    /// Appends interleaved little-endian `f32` frames straight from a
    /// `PipeWire` buffer.
    ///
    /// Decoded sample by sample rather than by casting the slice: a mapped SPA
    /// buffer carries no alignment guarantee for `f32`, and a misaligned
    /// reinterpret is undefined behaviour however well it happens to work.
    pub fn push_le_bytes(&mut self, bytes: &[u8]) {
        // Moved out and back so `push` can take `&mut self`; the allocation
        // survives across callbacks either way.
        let mut scratch = mem::take(&mut self.in_scratch);
        scratch.clear();
        scratch.extend(
            bytes
                .chunks_exact(SAMPLE_BYTES)
                .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
        );
        self.push(&scratch);
        self.in_scratch = scratch;
    }

    /// Writes `frames` delayed frames into a `PipeWire` buffer as interleaved
    /// little-endian `f32`.
    pub fn pull_le_bytes(&mut self, out: &mut [u8], frames: usize, want_delay: usize) {
        let mut scratch = mem::take(&mut self.out_scratch);
        scratch.clear();
        scratch.resize(frames * self.channels, 0.0);
        self.pull(&mut scratch, frames, want_delay);
        for (slot, sample) in out.chunks_exact_mut(SAMPLE_BYTES).zip(scratch.iter()) {
            slot.copy_from_slice(&sample.to_le_bytes());
        }
        self.out_scratch = scratch;
    }

    /// Fills `out` with `frames` interleaved frames taken `want_delay` frames
    /// behind the write head, crossfading if the delay has just changed.
    pub fn pull(&mut self, out: &mut [f32], frames: usize, want_delay: usize) {
        let want = want_delay.min(self.max_delay_frames());

        if want != self.delay_frames && self.fade.is_none() {
            self.fade = Some(Fade {
                from: self.delay_frames,
                to: want,
                done: 0,
            });
        }

        // Slot arithmetic is signed because a requested slot can legitimately
        // predate the start of the stream; `sample` reads those as silence.
        // `written` is a frame counter, so wrapping i64 is not reachable.
        let newest = i64::try_from(self.written).unwrap_or(i64::MAX) - 1;

        for i in 0..frames {
            // Counting back from the write head keeps the output exactly
            // `delay` behind the input with no independent read cursor to
            // drift.
            let slot = newest - i64::try_from(frames - 1 - i).unwrap_or(0);

            let (d_from, d_to, t) = match &self.fade {
                Some(fade) => {
                    let progress = (fade.done + i).min(self.fade_len);
                    (fade.from, fade.to, progress as f32 / self.fade_len as f32)
                }
                None => (self.delay_frames, self.delay_frames, 0.0),
            };

            let out_base = i * self.channels;
            for ch in 0..self.channels {
                let a = self.sample(slot - as_i64(d_from), ch);
                let b = self.sample(slot - as_i64(d_to), ch);
                out[out_base + ch] = a * (1.0 - t) + b * t;
            }
        }

        if let Some(fade) = &mut self.fade {
            fade.done += frames;
            if fade.done >= self.fade_len {
                self.delay_frames = fade.to;
                self.fade = None;
            }
        }
    }

    /// One sample, or silence for a slot that has not been written yet.
    fn sample(&mut self, slot: i64, channel: usize) -> f32 {
        if slot < 0 {
            // Start-up, or a delay longer than the audio seen so far.
            return 0.0;
        }
        let age = i64::try_from(self.written).unwrap_or(i64::MAX) - slot;
        if age > as_i64(self.capacity_frames) {
            // Asked for audio that has already been overwritten. Only reachable
            // if the delay exceeds the buffer, which `max_delay_frames` rules
            // out — count it rather than return garbage.
            self.underruns += 1;
            return 0.0;
        }
        let idx = (slot as usize % self.capacity_frames) * self.channels + channel;
        self.buf[idx]
    }
}

/// Frame counts are bounded by the buffer size, far below `i64::MAX`.
fn as_i64(frames: usize) -> i64 {
    i64::try_from(frames).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp(frames: usize) -> Vec<f32> {
        (0..frames).map(|i| i as f32).collect()
    }

    /// Exact equality is the right assertion here: the ring copies samples, it
    /// does not compute on them, so anything but a bit-identical result is a
    /// bug in the indexing.
    #[test]
    #[allow(clippy::float_cmp)]
    fn zero_delay_is_passthrough() {
        let mut ring = DelayRing::new(1, 4096, 1);
        ring.push(&ramp(256));
        let mut out = vec![0.0; 256];
        ring.pull(&mut out, 256, 0);
        assert_eq!(out, ramp(256));
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn a_delay_shifts_the_output_back() {
        let mut ring = DelayRing::new(1, 4096, 1);
        ring.push(&ramp(256));
        let mut out = vec![0.0; 128];
        // Fade length is one frame, so the new delay is in force immediately.
        ring.pull(&mut out, 128, 64);
        // Output frame i corresponds to input frame (256 - 128 + i) - 64.
        assert_eq!(out[64], 128.0);
        assert_eq!(out[127], 191.0);
    }

    /// The bytes path is what production uses, so it gets the same check.
    #[test]
    #[allow(clippy::float_cmp)]
    fn the_byte_path_round_trips() {
        let mut ring = DelayRing::new(2, 4096, 1);
        let samples = ramp(64);
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        ring.push_le_bytes(&bytes);

        let mut out = vec![0u8; 64 * SAMPLE_BYTES];
        ring.pull_le_bytes(&mut out, 32, 0);
        let decoded: Vec<f32> = out
            .chunks_exact(SAMPLE_BYTES)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        assert_eq!(decoded, samples);
    }

    #[test]
    #[allow(clippy::float_cmp)]
    fn unwritten_audio_reads_as_silence_not_garbage() {
        let mut ring = DelayRing::new(2, 4096, 1);
        let mut out = vec![1.0; 64];
        ring.pull(&mut out, 32, 480);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(ring.underruns(), 0);
    }

    /// The reason the crossfade exists: a hard jump between read positions is a
    /// step discontinuity, and a step in a signal is a click.
    #[test]
    fn a_delay_change_is_smeared_not_stepped() {
        let fade = 64;
        let mut ring = DelayRing::new(1, 4096, fade);
        // A constant 1.0 region followed by a constant 0.0 region, so any
        // instant jump between them shows up as a single-sample step.
        let mut input = vec![1.0; 1024];
        input.extend(std::iter::repeat_n(0.0, 1024));
        ring.push(&input);

        let mut out = vec![0.0; fade];
        ring.pull(&mut out, fade, 1500);

        let stepped = out.windows(2).any(|w| (w[1] - w[0]).abs() > 0.5);
        assert!(!stepped, "delay change produced a step: {out:?}");
    }

    #[test]
    fn the_delay_is_clamped_to_what_the_buffer_holds() {
        let mut ring = DelayRing::new(1, 4096, 16);
        let max = ring.max_delay_frames();
        ring.push(&ramp(8192));
        let mut out = vec![0.0; 32];
        // Far beyond the buffer; must clamp rather than read overwritten audio.
        ring.pull(&mut out, 32, max * 4);
        assert_eq!(ring.underruns(), 0);
    }
}
