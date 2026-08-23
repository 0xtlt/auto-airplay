#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
}

impl AudioFormat {
    pub const fn new(sample_rate: u32, channels: u8, bits_per_sample: u8) -> Self {
        Self {
            sample_rate,
            channels,
            bits_per_sample,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatCandidate(AudioFormat);

impl FormatCandidate {
    pub const fn new(sample_rate: u32, channels: u8, bits_per_sample: u8) -> Self {
        Self(AudioFormat::new(sample_rate, channels, bits_per_sample))
    }

    pub const fn format(self) -> AudioFormat {
        self.0
    }
}

pub fn select_best_format(
    supported: &[FormatCandidate],
    preferred_rate: Option<u32>,
) -> Option<AudioFormat> {
    if let Some(rate) = preferred_rate
        && let Some(exact) = supported
            .iter()
            .map(|candidate| candidate.format())
            .filter(|format| format.sample_rate == rate)
            .max_by_key(|format| (format.channels, format.bits_per_sample))
    {
        return Some(exact);
    }

    supported
        .iter()
        .map(|candidate| candidate.format())
        .max_by_key(|format| (format.channels, format.bits_per_sample, format.sample_rate))
}

pub fn sine_wave(sample_rate: u32, channels: u8, frequency_hz: f32, seconds: f32) -> Vec<i16> {
    let frame_count = (sample_rate as f32 * seconds.max(0.0)) as usize;
    let amplitude = i16::MAX as f32 * 0.20;
    let mut samples = Vec::with_capacity(frame_count * channels as usize);

    for frame in 0..frame_count {
        let phase = std::f32::consts::TAU * frequency_hz * frame as f32 / sample_rate as f32;
        let sample = (phase.sin() * amplitude) as i16;
        for _ in 0..channels {
            samples.push(sample);
        }
    }
    samples
}

pub fn write_pcm16_wav(
    writer: &mut impl std::io::Write,
    sample_rate: u32,
    channels: u16,
    samples: &[i16],
) -> std::io::Result<()> {
    let data_size = samples
        .len()
        .checked_mul(std::mem::size_of::<i16>())
        .and_then(|size| u32::try_from(size).ok())
        .ok_or_else(|| std::io::Error::other("WAV data is too large"))?;
    let byte_rate = sample_rate
        .checked_mul(channels as u32)
        .and_then(|rate| rate.checked_mul(2))
        .ok_or_else(|| std::io::Error::other("invalid WAV format"))?;
    let block_align = channels
        .checked_mul(2)
        .ok_or_else(|| std::io::Error::other("invalid channel count"))?;

    writer.write_all(b"RIFF")?;
    writer.write_all(&(36_u32 + data_size).to_le_bytes())?;
    writer.write_all(b"WAVE")?;
    writer.write_all(b"fmt ")?;
    writer.write_all(&16_u32.to_le_bytes())?;
    writer.write_all(&1_u16.to_le_bytes())?;
    writer.write_all(&channels.to_le_bytes())?;
    writer.write_all(&sample_rate.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&block_align.to_le_bytes())?;
    writer.write_all(&16_u16.to_le_bytes())?;
    writer.write_all(b"data")?;
    writer.write_all(&data_size.to_le_bytes())?;
    for sample in samples {
        writer.write_all(&sample.to_le_bytes())?;
    }
    Ok(())
}
