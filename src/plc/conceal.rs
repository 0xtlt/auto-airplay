//! Waveform continuation that hides missing audio.

use std::f32::consts::PI;

/// Continues a signal past its last real sample.
///
/// The continuation is the recent past played backwards, mirrored around the
/// last real sample and bouncing between the ends of the history. Mirroring
/// keeps the waveform continuous at the junction: every step of the output is
/// a step the real signal already took, so there is no jump and no new
/// high-frequency content. An optional raised-cosine fade takes it to silence.
/// Every channel is continued on its own.
pub(crate) struct Concealment {
    /// Interleaved history, oldest frame first.
    history: Vec<i16>,
    channels: usize,
    /// Frames of history. Fewer than two leaves nothing to mirror: silence.
    frames: usize,
    /// Length of the fade to silence, `None` for a continuation that never fades.
    fade_frames: Option<usize>,
    /// Frames generated since the continuation started.
    t: usize,
}

impl Concealment {
    pub(crate) fn new(history: &[i16], channels: usize, fade_frames: Option<usize>) -> Self {
        let frames = history.len() / channels;
        Self {
            history: history[..frames * channels].to_vec(),
            channels,
            frames,
            fade_frames,
            t: 0,
        }
    }

    /// Frames generated so far.
    pub(crate) fn elapsed(&self) -> usize {
        self.t
    }

    fn gain(&self, t: usize) -> f32 {
        match self.fade_frames {
            None => 1.0,
            Some(fade) if t >= fade => 0.0,
            Some(fade) => 0.5 * (1.0 + (PI * t as f32 / fade as f32).cos()),
        }
    }

    /// Index of the history frame that frame `t` of the continuation mirrors:
    /// `m - 1, m - 2, … 0, 1, … m, m - 1, …` with `m` the last frame.
    fn source_frame(&self, t: usize) -> usize {
        let m = self.frames - 1;
        let r = (t + 1) % (2 * m);
        m.abs_diff(r)
    }

    fn value(&self, t: usize, channel: usize) -> f32 {
        if self.frames < 2 {
            return 0.0;
        }
        let gain = self.gain(t);
        if gain == 0.0 {
            return 0.0;
        }
        gain * f32::from(self.history[self.source_frame(t) * self.channels + channel])
    }

    /// Writes the next `out.len() / channels` frames of the continuation.
    pub(crate) fn fill(&mut self, out: &mut [i16]) {
        for frame in out.chunks_exact_mut(self.channels) {
            for (channel, sample) in frame.iter_mut().enumerate() {
                *sample = to_i16(self.value(self.t, channel));
            }
            self.t += 1;
        }
    }

    /// Crossfades the continuation into `real`: the continuation weighs
    /// `1 - w` and `real` weighs `w`, with `w` a raised cosine rising from
    /// about 0 to about 1 over `ramp_len` frames, `ramp_pos` of them already
    /// done. Advances the continuation by the frames of `real`.
    pub(crate) fn blend_into(&mut self, real: &mut [i16], ramp_pos: usize, ramp_len: usize) {
        for (offset, frame) in real.chunks_exact_mut(self.channels).enumerate() {
            let weight = ramp(ramp_pos + offset, ramp_len);
            for (channel, sample) in frame.iter_mut().enumerate() {
                let hidden = self.value(self.t, channel);
                *sample = to_i16(hidden * (1.0 - weight) + f32::from(*sample) * weight);
            }
            self.t += 1;
        }
    }
}

/// Raised-cosine weight of frame `pos` in a ramp of `len` frames: strictly
/// between 0 and 1, and smooth at both ends.
pub(crate) fn ramp(pos: usize, len: usize) -> f32 {
    if pos >= len {
        return 1.0;
    }
    0.5 * (1.0 - (PI * (pos + 1) as f32 / (len + 1) as f32).cos())
}

pub(crate) fn to_i16(value: f32) -> i16 {
    value
        .round()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16
}
