//! Silence detection: signal level helpers and the state machine that decides
//! when the AirPlay output should be held or released.
//!
//! Everything here is pure (time is injected), so it is fully unit-testable.

use std::time::{Duration, Instant};

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
/// Two thresholds give hysteresis. While the output is held, any level below
/// `silence_threshold_dbfs` counts as silence and starts the idle countdown.
/// While it is released, sound only counts once the level reaches the higher
/// `sound_threshold_dbfs` on every observation for a whole `confirm` window;
/// one observation below it restarts the window. Noise that hovers between the
/// two thresholds, or pops above the sound threshold briefly, never acquires.
///
/// The monitor only suggests actions; the caller performs the I/O and then
/// confirms with [`mark_active`](Self::mark_active) or
/// [`mark_released`](Self::mark_released). Until confirmed, the same action is
/// suggested again on every observation, so failed attempts can be retried.
#[derive(Debug, Clone)]
pub struct SilenceMonitor {
    silence_threshold_dbfs: f32,
    sound_threshold_dbfs: f32,
    idle: Duration,
    confirm: Duration,
    state: OutputState,
    /// Start of the current uninterrupted run of observations at or above the
    /// sound threshold, with the lowest and highest level seen in it.
    sound_window: Option<SoundWindow>,
    silent_since: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
struct SoundWindow {
    since: Instant,
    min_dbfs: f32,
    max_dbfs: f32,
}

impl SilenceMonitor {
    /// Creates a monitor in the [`OutputState::Released`] state. A zero `idle`
    /// duration disables silence detection: the output is acquired right away
    /// and never released.
    pub fn new(
        silence_threshold_dbfs: f32,
        sound_threshold_dbfs: f32,
        idle: Duration,
        confirm: Duration,
    ) -> Self {
        Self {
            silence_threshold_dbfs,
            sound_threshold_dbfs: sound_threshold_dbfs.max(silence_threshold_dbfs),
            idle,
            confirm,
            state: OutputState::Released,
            sound_window: None,
            silent_since: None,
        }
    }

    pub fn state(&self) -> OutputState {
        self.state
    }

    /// Lowest and highest level of the sound window in progress, if any. Right
    /// after an [`Action::Acquire`] this is what justified acquiring.
    pub fn sound_levels(&self) -> Option<(f32, f32)> {
        self.sound_window
            .map(|window| (window.min_dbfs, window.max_dbfs))
    }

    pub fn observe(&mut self, level_dbfs: f32, now: Instant) -> Option<Action> {
        match self.state {
            OutputState::Released => {
                if self.idle.is_zero() {
                    return Some(Action::Acquire);
                }
                if level_dbfs < self.sound_threshold_dbfs {
                    self.sound_window = None;
                    return None;
                }
                let window = self.sound_window.get_or_insert(SoundWindow {
                    since: now,
                    min_dbfs: level_dbfs,
                    max_dbfs: level_dbfs,
                });
                window.min_dbfs = window.min_dbfs.min(level_dbfs);
                window.max_dbfs = window.max_dbfs.max(level_dbfs);
                let confirmed = now.saturating_duration_since(window.since) >= self.confirm;
                confirmed.then_some(Action::Acquire)
            }
            OutputState::Active => {
                if self.idle.is_zero() {
                    return None;
                }
                if level_dbfs >= self.silence_threshold_dbfs {
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
        self.sound_window = None;
        self.silent_since = None;
    }

    /// The output was released; start watching for sound.
    pub fn mark_released(&mut self, _now: Instant) {
        self.state = OutputState::Released;
        self.sound_window = None;
        self.silent_since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: f32 = -40.0;
    const SOUND_THRESHOLD: f32 = -34.0;
    const SOUND: f32 = -20.0;
    const SILENT: f32 = -70.0;
    const IDLE: Duration = Duration::from_secs(30);
    const CONFIRM: Duration = Duration::from_secs(2);

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    fn monitor() -> SilenceMonitor {
        SilenceMonitor::new(THRESHOLD, SOUND_THRESHOLD, IDLE, CONFIRM)
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
            monitor.observe(SOUND, start + CONFIRM - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            monitor.observe(SOUND, start + CONFIRM),
            Some(Action::Acquire)
        );
    }

    #[test]
    fn acquire_is_repeated_while_released_and_sound_continues() {
        let start = Instant::now();
        let mut monitor = monitor();

        monitor.observe(SOUND, start);
        assert_eq!(
            monitor.observe(SOUND, start + secs(2)),
            Some(Action::Acquire)
        );
        // The caller failed to connect and did not call `mark_active`.
        assert_eq!(
            monitor.observe(SOUND, start + secs(3)),
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
        monitor.observe(SOUND, start + secs(2));
        monitor.mark_active(start + secs(2));
        assert_eq!(monitor.state(), OutputState::Active);

        assert_eq!(monitor.observe(SILENT, start + secs(2)), None);
        assert_eq!(monitor.observe(SILENT, start + secs(31)), None);
        assert_eq!(
            monitor.observe(SILENT, start + secs(32)),
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
            now += CONFIRM;
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
        let mut monitor = SilenceMonitor::new(THRESHOLD, SOUND_THRESHOLD, Duration::ZERO, CONFIRM);

        assert_eq!(monitor.observe(SILENT, start), Some(Action::Acquire));
        assert_eq!(
            monitor.observe(f32::NEG_INFINITY, start),
            Some(Action::Acquire)
        );
    }

    #[test]
    fn zero_idle_never_releases() {
        let start = Instant::now();
        let mut monitor = SilenceMonitor::new(THRESHOLD, SOUND_THRESHOLD, Duration::ZERO, CONFIRM);
        monitor.mark_active(start);

        for step in 0..10 {
            assert_eq!(monitor.observe(SILENT, start + secs(step * 3_600)), None);
        }
    }

    const STEP: Duration = Duration::from_millis(250);

    /// Feeds `levels` one per `STEP`, starting at `start`, and returns the
    /// first action together with the time it was suggested.
    fn feed(
        monitor: &mut SilenceMonitor,
        start: Instant,
        levels: impl IntoIterator<Item = f32>,
    ) -> Option<(Duration, Action)> {
        levels.into_iter().enumerate().find_map(|(index, level)| {
            let offset = STEP * index as u32;
            monitor
                .observe(level, start + offset)
                .map(|action| (offset, action))
        })
    }

    #[test]
    fn noise_just_below_the_sound_threshold_never_acquires() {
        let mut monitor = monitor();
        let noise = SOUND_THRESHOLD - 0.1;

        // Well above the silence threshold, for an hour.
        assert_eq!(
            feed(&mut monitor, Instant::now(), vec![noise; 14_400]),
            None
        );
    }

    #[test]
    fn noise_oscillating_around_the_sound_threshold_never_acquires() {
        let mut monitor = monitor();
        // Up to the threshold and just under it, every other observation.
        let levels = (0..14_400).map(|index| {
            if index % 2 == 0 {
                SOUND_THRESHOLD + 3.0
            } else {
                SOUND_THRESHOLD - 0.1
            }
        });

        assert_eq!(feed(&mut monitor, Instant::now(), levels), None);
    }

    #[test]
    fn brief_pops_above_the_sound_threshold_never_acquire() {
        let mut monitor = monitor();
        // 1.75 s above the threshold (8 observations), then one dip, repeated.
        let levels = (0..4_000).map(|index| {
            if index % 8 == 7 {
                SOUND_THRESHOLD - 1.0
            } else {
                SOUND
            }
        });

        assert_eq!(feed(&mut monitor, Instant::now(), levels), None);
    }

    #[test]
    fn sound_above_the_threshold_for_the_whole_window_acquires() {
        let mut monitor = monitor();
        let acquired = feed(&mut monitor, Instant::now(), vec![SOUND; 20]);

        assert_eq!(acquired, Some((CONFIRM, Action::Acquire)));
    }

    #[test]
    fn level_exactly_at_the_sound_threshold_counts_as_sound() {
        let mut monitor = monitor();
        let acquired = feed(&mut monitor, Instant::now(), vec![SOUND_THRESHOLD; 20]);

        assert_eq!(acquired, Some((CONFIRM, Action::Acquire)));
    }

    #[test]
    fn a_dip_restarts_the_confirm_window() {
        let start = Instant::now();
        let mut monitor = monitor();

        // 1.75 s of sound, one dip, then sound again: the clock starts over.
        let before = vec![SOUND; 8].into_iter().chain([SOUND_THRESHOLD - 0.1]);
        assert_eq!(feed(&mut monitor, start, before), None);
        let resumed = start + STEP * 9;
        for step in 0..8 {
            assert_eq!(monitor.observe(SOUND, resumed + STEP * step), None);
        }
        assert_eq!(
            monitor.observe(SOUND, resumed + CONFIRM),
            Some(Action::Acquire)
        );
    }

    #[test]
    fn levels_between_the_thresholds_do_not_count_as_sound_but_keep_the_output() {
        let start = Instant::now();
        let between = (THRESHOLD + SOUND_THRESHOLD) / 2.0;

        // Released: not sound.
        let mut released = monitor();
        assert_eq!(feed(&mut released, start, vec![between; 100]), None);

        // Active: not silence either, so the idle countdown never completes.
        let mut active = active_monitor(start);
        assert_eq!(feed(&mut active, start, vec![between; 14_400]), None);
    }

    #[test]
    fn reports_the_levels_seen_during_the_confirm_window() {
        let start = Instant::now();
        let mut monitor = monitor();
        assert_eq!(monitor.sound_levels(), None);

        let levels = [
            -30.0, -22.0, -33.5, -25.0, -30.0, -30.0, -30.0, -30.0, -30.0,
        ];
        let acquired = feed(&mut monitor, start, levels);

        assert_eq!(acquired, Some((CONFIRM, Action::Acquire)));
        assert_eq!(monitor.sound_levels(), Some((-33.5, -22.0)));

        // A dip clears the report.
        monitor.observe(SILENT, start + secs(3));
        assert_eq!(monitor.sound_levels(), None);
    }

    #[test]
    fn a_sound_threshold_below_the_silence_threshold_is_raised_to_it() {
        let start = Instant::now();
        let mut monitor = SilenceMonitor::new(THRESHOLD, THRESHOLD - 10.0, IDLE, CONFIRM);

        // Between the (raised) thresholds nothing acquires.
        assert_eq!(feed(&mut monitor, start, vec![THRESHOLD - 1.0; 100]), None);
        assert_eq!(
            feed(&mut monitor, start, vec![THRESHOLD; 20]),
            Some((CONFIRM, Action::Acquire))
        );
    }
}
