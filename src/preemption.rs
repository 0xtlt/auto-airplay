//! Another AirPlay source (typically an Apple TV) taking over the speaker.
//!
//! When the receiver closes our session mid-stream, the bridge must not grab
//! the speaker back at the next noise: it waits for the input to go quiet and
//! then for a real new start of sound, and backs off exponentially when this
//! keeps happening. Everything here is pure (time is injected) and unit-tested;
//! the daemon only wires it up.

use std::time::{Duration, Instant};

/// Environment variable carrying the consecutive preemption count across the
/// `exec` that starts the next session.
pub const PREEMPTIONS_ENV: &str = "AUTO_AIRPLAY_PREEMPTIONS";

/// Upper bound of the tracked count; keeps the backoff arithmetic far from
/// overflow and a corrupted environment value harmless.
const MAX_TRACKED: u32 = 1_000;

/// Whether an error kind from `classify_error` means the receiver closed the
/// session (on a feedback request or any other RTSP read), as opposed to
/// timeouts, crypto failures or our own errors.
pub fn is_preemption_kind(error_kind: &str) -> bool {
    matches!(error_kind, "eof" | "reset-or-broken-pipe")
}

/// Pause before the speaker may be re-acquired after the `consecutive`-th
/// preemption in a row: `base * 2^(consecutive - 1)`, capped at `max`.
pub fn backoff_delay(base: Duration, max: Duration, consecutive: u32) -> Duration {
    if consecutive == 0 {
        return Duration::ZERO;
    }
    let factor = 1u32 << (consecutive - 1).min(31);
    base.saturating_mul(factor).min(max)
}

/// The consecutive preemption count after a new preemption. It starts over at
/// 1 when the preempted session had streamed for at least `reset_after`.
pub fn next_consecutive(previous: u32, streamed: Duration, reset_after: Duration) -> u32 {
    if streamed >= reset_after {
        1
    } else {
        previous.saturating_add(1).min(MAX_TRACKED)
    }
}

/// Value of [`PREEMPTIONS_ENV`] for the next process; `None` means the
/// variable must be removed (no preemption state).
pub fn encode_preemptions(consecutive: u32) -> Option<String> {
    (consecutive > 0).then(|| consecutive.min(MAX_TRACKED).to_string())
}

/// Reads [`PREEMPTIONS_ENV`]. Anything that is not a positive integer (unset,
/// empty, negative, garbage) means no preemption state, i.e. `0`.
pub fn decode_preemptions(raw: Option<&str>) -> u32 {
    raw.and_then(|raw| raw.trim().parse::<u32>().ok())
        .map_or(0, |count| count.min(MAX_TRACKED))
}

/// Holds the output back after a preemption until all of these hold:
///
/// 1. the backoff elapsed (`now >= not_before`),
/// 2. the input stayed continuously silent for `quiet_required`,
/// 3. (checked by the caller's sound confirmation) new sound followed.
///
/// Silence counts while the backoff is still running. Once reached, the quiet
/// condition is kept: the next confirmed sound is a new start. Sound that never
/// stops, such as a record still playing or constant noise, keeps the gate
/// closed forever.
#[derive(Debug, Clone)]
pub struct PreemptionGate {
    quiet_required: Duration,
    not_before: Instant,
    silent_since: Option<Instant>,
    quiet_reached: bool,
}

impl PreemptionGate {
    pub fn new(quiet_required: Duration, not_before: Instant) -> Self {
        Self {
            quiet_required,
            not_before,
            silent_since: None,
            quiet_reached: false,
        }
    }

    /// Records one level observation; `silent` means below the silence
    /// threshold.
    pub fn observe(&mut self, silent: bool, now: Instant) {
        if self.quiet_reached {
            return;
        }
        if !silent {
            self.silent_since = None;
            return;
        }
        let since = *self.silent_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= self.quiet_required {
            self.quiet_reached = true;
        }
    }

    /// The input has been quiet long enough; only new sound is missing.
    pub fn quiet_reached(&self) -> bool {
        self.quiet_reached
    }

    /// Whether acquiring is allowed at `now`.
    pub fn is_open(&self, now: Instant) -> bool {
        self.quiet_reached && now >= self.not_before
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn only_receiver_closed_errors_are_preemptions() {
        assert!(is_preemption_kind("eof"));
        assert!(is_preemption_kind("reset-or-broken-pipe"));
        for other in ["timeout", "crypto", "other", ""] {
            assert!(!is_preemption_kind(other), "{other}");
        }
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let (base, max) = (secs(30), secs(600));
        let delays: Vec<u64> = (1..=7)
            .map(|count| backoff_delay(base, max, count).as_secs())
            .collect();
        assert_eq!(delays, [30, 60, 120, 240, 480, 600, 600]);
        assert_eq!(backoff_delay(base, max, 0), Duration::ZERO);
    }

    #[test]
    fn backoff_never_overflows() {
        let (base, max) = (secs(86_400), secs(86_400));
        assert_eq!(backoff_delay(base, max, 1_000), max);
        assert_eq!(backoff_delay(base, max, u32::MAX), max);
        assert_eq!(backoff_delay(secs(u64::MAX), secs(600), 40), secs(600));
        // A cap below the base still wins.
        assert_eq!(backoff_delay(secs(30), secs(10), 1), secs(10));
    }

    #[test]
    fn consecutive_count_grows_and_resets() {
        let reset_after = secs(600);
        assert_eq!(next_consecutive(0, secs(5), reset_after), 1);
        assert_eq!(next_consecutive(1, secs(5), reset_after), 2);
        assert_eq!(next_consecutive(4, secs(599), reset_after), 5);
        // A long stable session forgives the earlier preemptions.
        assert_eq!(next_consecutive(4, secs(600), reset_after), 1);
        assert_eq!(next_consecutive(4, secs(3_600), reset_after), 1);
        assert_eq!(
            next_consecutive(u32::MAX, secs(1), reset_after),
            MAX_TRACKED
        );
    }

    #[test]
    fn environment_value_round_trips() {
        for count in [1, 2, 5, 31, 999, MAX_TRACKED] {
            let encoded = encode_preemptions(count);
            assert_eq!(decode_preemptions(encoded.as_deref()), count);
        }
    }

    #[test]
    fn no_preemption_removes_the_variable() {
        assert_eq!(encode_preemptions(0), None);
        assert_eq!(decode_preemptions(None), 0);
    }

    #[test]
    fn garbage_environment_values_mean_no_preemption() {
        for raw in [
            "",
            " ",
            "abc",
            "-1",
            "1.5",
            "0",
            "1 2",
            "0x10",
            "99999999999999999999",
        ] {
            assert_eq!(decode_preemptions(Some(raw)), 0, "{raw:?}");
        }
        assert_eq!(decode_preemptions(Some(" 3\n")), 3);
        assert_eq!(decode_preemptions(Some("4000000000")), MAX_TRACKED);
    }

    #[test]
    fn gate_needs_continuous_silence_then_the_backoff() {
        let start = Instant::now();
        let mut gate = PreemptionGate::new(secs(5), start + secs(30));

        gate.observe(true, start);
        gate.observe(true, start + secs(4));
        assert!(!gate.quiet_reached());
        gate.observe(true, start + secs(5));
        assert!(gate.quiet_reached());

        // Quiet is reached but the backoff is still running.
        assert!(!gate.is_open(start + secs(29)));
        assert!(gate.is_open(start + secs(30)));
        assert!(gate.is_open(start + secs(300)));
    }

    #[test]
    fn noise_restarts_the_required_silence() {
        let start = Instant::now();
        let mut gate = PreemptionGate::new(secs(5), start);

        gate.observe(true, start);
        gate.observe(true, start + secs(4));
        gate.observe(false, start + secs(4) + Duration::from_millis(250));
        gate.observe(true, start + secs(5));
        gate.observe(true, start + secs(9));
        assert!(!gate.quiet_reached());
        gate.observe(true, start + secs(10));
        assert!(gate.quiet_reached());
        assert!(gate.is_open(start + secs(10)));
    }

    #[test]
    fn quiet_is_kept_once_reached() {
        let start = Instant::now();
        let mut gate = PreemptionGate::new(secs(5), start);
        gate.observe(true, start);
        gate.observe(true, start + secs(5));

        gate.observe(false, start + secs(6));
        assert!(gate.quiet_reached());
        assert!(gate.is_open(start + secs(6)));
    }

    #[test]
    fn constant_sound_never_opens_the_gate() {
        let start = Instant::now();
        let mut gate = PreemptionGate::new(secs(5), start);
        for step in 0..14_400 {
            gate.observe(false, start + Duration::from_millis(250) * step);
        }
        assert!(!gate.is_open(start + secs(86_400)));
    }
}
