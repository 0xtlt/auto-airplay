//! Silence detection: signal level helpers and the state machine that decides
//! when the AirPlay output should be held or released.
//!
//! Everything here is pure (time is injected), so it is fully unit-testable.

use std::time::{Duration, Instant};

/// How long sound must be sustained before the output is (re)acquired. This
/// keeps a single needle pop or static click from taking over the speaker.
pub const DEFAULT_RESUME_DURATION: Duration = Duration::from_secs(1);

const FULL_SCALE: f64 = 32_768.0;

/// RMS level of interleaved 16-bit samples, normalized to `0.0..=1.0`.
pub fn rms_linear(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_of_squares: f64 = samples
        .iter()
        .map(|&sample| {
            let value = f64::from(sample);
            value * value
        })
        .sum();
    let rms = (sum_of_squares / samples.len() as f64).sqrt() / FULL_SCALE;
    rms.min(1.0) as f32
}

/// Converts a linear level (`1.0` = full scale) to dBFS. Zero (or negative)
/// levels map to negative infinity.
pub fn linear_to_dbfs(level: f32) -> f32 {
    if level <= 0.0 {
        f32::NEG_INFINITY
    } else {
        20.0 * level.log10()
    }
}

/// RMS level of interleaved 16-bit samples in dBFS; digital silence (or no
/// samples) is negative infinity.
pub fn rms_dbfs(samples: &[i16]) -> f32 {
    linear_to_dbfs(rms_linear(samples))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    /// The AirPlay speaker is not held by this bridge.
    Released,
    /// The bridge is connected to the speaker and streaming.
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Acquire,
    Release,
}

/// Decides, from observed input levels, when to acquire or release the output.
///
/// The monitor only suggests actions; the caller performs the I/O and then
/// confirms with [`mark_active`](Self::mark_active) or
/// [`mark_released`](Self::mark_released). Until confirmed, the same action is
/// suggested again on every observation, so failed attempts can be retried.
#[derive(Debug, Clone)]
pub struct SilenceMonitor {
    threshold_dbfs: f32,
    idle: Duration,
    resume: Duration,
    state: OutputState,
    sound_since: Option<Instant>,
    silent_since: Option<Instant>,
}

impl SilenceMonitor {
    /// Creates a monitor in the [`OutputState::Released`] state. A zero `idle`
    /// duration disables silence detection: the output is acquired right away
    /// and never released.
    pub fn new(threshold_dbfs: f32, idle: Duration, resume: Duration) -> Self {
        Self {
            threshold_dbfs,
            idle,
            resume,
            state: OutputState::Released,
            sound_since: None,
            silent_since: None,
        }
    }

    pub fn state(&self) -> OutputState {
        self.state
    }

    pub fn observe(&mut self, level_dbfs: f32, now: Instant) -> Option<Action> {
        let has_sound = level_dbfs >= self.threshold_dbfs;
        match self.state {
            OutputState::Released => {
                if self.idle.is_zero() {
                    return Some(Action::Acquire);
                }
                if !has_sound {
                    self.sound_since = None;
                    return None;
                }
                let since = *self.sound_since.get_or_insert(now);
                (now.saturating_duration_since(since) >= self.resume).then_some(Action::Acquire)
            }
            OutputState::Active => {
                if self.idle.is_zero() {
                    return None;
                }
                if has_sound {
                    self.silent_since = None;
                    return None;
                }
                let since = *self.silent_since.get_or_insert(now);
                (now.saturating_duration_since(since) >= self.idle).then_some(Action::Release)
            }
        }
    }

    /// The output was acquired; start watching for silence.
    pub fn mark_active(&mut self, _now: Instant) {
        self.state = OutputState::Active;
        self.sound_since = None;
        self.silent_since = None;
    }

    /// The output was released; start watching for sound.
    pub fn mark_released(&mut self, _now: Instant) {
        self.state = OutputState::Released;
        self.sound_since = None;
        self.silent_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: f32 = -50.0;
    const SOUND: f32 = -20.0;
    const SILENT: f32 = -70.0;
    const IDLE: Duration = Duration::from_secs(30);
    const RESUME: Duration = Duration::from_secs(1);

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    fn monitor() -> SilenceMonitor {
        SilenceMonitor::new(THRESHOLD, IDLE, RESUME)
    }

    fn active_monitor(start: Instant) -> SilenceMonitor {
        let mut monitor = monitor();
        monitor.mark_active(start);
        monitor
    }

    fn sine(amplitude: f32, length: usize) -> Vec<i16> {
        (0..length)
            .map(|index| {
                let phase = 2.0 * std::f32::consts::PI * index as f32 / 100.0;
                (amplitude * phase.sin()).round() as i16
            })
            .collect()
    }

    #[test]
    fn empty_and_zero_samples_are_negative_infinity() {
        assert_eq!(rms_dbfs(&[]), f32::NEG_INFINITY);
        assert_eq!(rms_dbfs(&[0; 512]), f32::NEG_INFINITY);
        assert_eq!(rms_linear(&[]), 0.0);
    }

    #[test]
    fn full_scale_square_wave_is_zero_dbfs() {
        let samples = [i16::MIN, i16::MAX, i16::MIN, i16::MAX];
        let level = rms_dbfs(&samples);
        assert!(level.abs() < 0.01, "got {level}");
    }

    #[test]
    fn most_negative_sample_does_not_exceed_full_scale() {
        assert_eq!(rms_linear(&[i16::MIN; 8]), 1.0);
        assert_eq!(rms_dbfs(&[i16::MIN; 8]), 0.0);
    }

    #[test]
    fn full_scale_sine_is_about_minus_three_dbfs() {
        let level = rms_dbfs(&sine(32_767.0, 10_000));
        assert!((level + 3.01).abs() < 0.05, "got {level}");
    }

    #[test]
    fn half_amplitude_is_about_six_db_lower() {
        let full = rms_dbfs(&sine(30_000.0, 10_000));
        let half = rms_dbfs(&sine(15_000.0, 10_000));
        assert!((full - half - 6.02).abs() < 0.05);
    }

    #[test]
    fn linear_to_dbfs_matches_known_values() {
        assert_eq!(linear_to_dbfs(1.0), 0.0);
        assert!((linear_to_dbfs(0.1) + 20.0).abs() < 1e-4);
        assert_eq!(linear_to_dbfs(0.0), f32::NEG_INFINITY);
    }

    #[test]
    fn starts_released() {
        assert_eq!(monitor().state(), OutputState::Released);
    }

    #[test]
    fn releases_exactly_after_idle_duration() {
        let start = Instant::now();
        let mut monitor = active_monitor(start);

        assert_eq!(monitor.observe(SILENT, start), None);
        assert_eq!(
            monitor.observe(SILENT, start + IDLE - Duration::from_millis(1)),
            None
        );
        assert_eq!(monitor.observe(SILENT, start + IDLE), Some(Action::Release));
    }

    #[test]
    fn level_exactly_at_threshold_counts_as_sound() {
        let start = Instant::now();
        let mut monitor = active_monitor(start);

        assert_eq!(monitor.observe(THRESHOLD, start), None);
        assert_eq!(monitor.observe(THRESHOLD, start + secs(120)), None);
        assert_eq!(monitor.observe(f32::NEG_INFINITY, start + secs(121)), None);
    }

    #[test]
    fn level_just_below_threshold_counts_as_silence() {
        let start = Instant::now();
        let mut monitor = active_monitor(start);
        let below = THRESHOLD - 0.01;

        assert_eq!(monitor.observe(below, start), None);
        assert_eq!(monitor.observe(below, start + IDLE), Some(Action::Release));
    }

    #[test]
    fn sound_resets_the_silence_timer() {
        let start = Instant::now();
        let mut monitor = active_monitor(start);

        assert_eq!(monitor.observe(SILENT, start), None);
        assert_eq!(monitor.observe(SOUND, start + secs(29)), None);
        assert_eq!(monitor.observe(SILENT, start + secs(30)), None);
        assert_eq!(monitor.observe(SILENT, start + secs(59)), None);
        assert_eq!(
            monitor.observe(SILENT, start + secs(60)),
            Some(Action::Release)
        );
    }

    #[test]
    fn keeps_suggesting_release_until_confirmed() {
        let start = Instant::now();
        let mut monitor = active_monitor(start);

        monitor.observe(SILENT, start);
        assert_eq!(monitor.observe(SILENT, start + IDLE), Some(Action::Release));
        assert_eq!(
            monitor.observe(SILENT, start + IDLE + secs(1)),
            Some(Action::Release)
        );
    }

    #[test]
    fn silence_while_released_does_nothing() {
        let start = Instant::now();
        let mut monitor = monitor();

        for step in 0..100 {
            assert_eq!(monitor.observe(SILENT, start + secs(step)), None);
        }
    }

    #[test]
    fn short_pop_does_not_acquire() {
        let start = Instant::now();
        let mut monitor = monitor();

        assert_eq!(monitor.observe(SOUND, start), None);
        assert_eq!(
            monitor.observe(SOUND, start + Duration::from_millis(250)),
            None
        );
        assert_eq!(
            monitor.observe(SILENT, start + Duration::from_millis(500)),
            None
        );
        // The debounce restarts: sound again does not inherit the earlier pop.
        assert_eq!(
            monitor.observe(SOUND, start + Duration::from_millis(750)),
            None
        );
        assert_eq!(
            monitor.observe(SOUND, start + Duration::from_millis(1_500)),
            None
        );
    }

    #[test]
    fn sustained_sound_acquires() {
        let start = Instant::now();
        let mut monitor = monitor();

        assert_eq!(monitor.observe(SOUND, start), None);
        assert_eq!(
            monitor.observe(SOUND, start + RESUME - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            monitor.observe(SOUND, start + RESUME),
            Some(Action::Acquire)
        );
    }

    #[test]
    fn acquire_is_repeated_while_released_and_sound_continues() {
        let start = Instant::now();
        let mut monitor = monitor();

        monitor.observe(SOUND, start);
        assert_eq!(
            monitor.observe(SOUND, start + secs(1)),
            Some(Action::Acquire)
        );
        // The caller failed to connect and did not call `mark_active`.
        assert_eq!(
            monitor.observe(SOUND, start + secs(2)),
            Some(Action::Acquire)
        );
        assert_eq!(
            monitor.observe(SOUND, start + secs(10)),
            Some(Action::Acquire)
        );
    }

    #[test]
    fn mark_active_starts_a_fresh_silence_timer() {
        let start = Instant::now();
        let mut monitor = monitor();

        monitor.observe(SOUND, start);
        monitor.observe(SOUND, start + secs(1));
        monitor.mark_active(start + secs(1));
        assert_eq!(monitor.state(), OutputState::Active);

        assert_eq!(monitor.observe(SILENT, start + secs(1)), None);
        assert_eq!(monitor.observe(SILENT, start + secs(30)), None);
        assert_eq!(
            monitor.observe(SILENT, start + secs(31)),
            Some(Action::Release)
        );
    }

    #[test]
    fn repeated_release_and_acquire_cycles() {
        let start = Instant::now();
        let mut monitor = monitor();
        let mut now = start;

        for _ in 0..3 {
            assert_eq!(monitor.observe(SOUND, now), None);
            now += RESUME;
            assert_eq!(monitor.observe(SOUND, now), Some(Action::Acquire));
            monitor.mark_active(now);

            assert_eq!(monitor.observe(SILENT, now), None);
            now += IDLE;
            assert_eq!(monitor.observe(SILENT, now), Some(Action::Release));
            monitor.mark_released(now);
            assert_eq!(monitor.state(), OutputState::Released);
        }
    }

    #[test]
    fn sound_before_release_does_not_count_as_resume_debounce() {
        let start = Instant::now();
        let mut monitor = active_monitor(start);

        monitor.observe(SOUND, start);
        monitor.observe(SILENT, start + secs(1));
        monitor.observe(SILENT, start + IDLE + secs(1));
        monitor.mark_released(start + IDLE + secs(1));

        let resumed = start + IDLE + secs(2);
        assert_eq!(monitor.observe(SOUND, resumed), None);
    }

    #[test]
    fn zero_idle_acquires_immediately_regardless_of_level() {
        let start = Instant::now();
        let mut monitor = SilenceMonitor::new(THRESHOLD, Duration::ZERO, RESUME);

        assert_eq!(monitor.observe(SILENT, start), Some(Action::Acquire));
        assert_eq!(
            monitor.observe(f32::NEG_INFINITY, start),
            Some(Action::Acquire)
        );
    }

    #[test]
    fn zero_idle_never_releases() {
        let start = Instant::now();
        let mut monitor = SilenceMonitor::new(THRESHOLD, Duration::ZERO, RESUME);
        monitor.mark_active(start);

        for step in 0..10 {
            assert_eq!(monitor.observe(SILENT, start + secs(step * 3_600)), None);
        }
    }
}
