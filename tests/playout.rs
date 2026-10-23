//! Feeding the AirPlay sender: pacing, and telling a dead stream from a busy one.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use airplay_client::{LiveAudioDecoder, MediaClock};
use auto_airplay::playout::{Feed, STALL_LIMIT, Stream};
use auto_airplay::plc::{JitterBuffer, PLAYOUT_CHUNK_FRAMES};

const RATE: u32 = 48_000;
const CH: usize = 2;
const STEP: Duration = Duration::from_millis(10);

fn audio(frames: usize) -> Vec<i16> {
    (0..frames * CH)
        .map(|n| ((n / CH) as f32 * 0.05).sin().mul_add(12_000.0, 0.0) as i16)
        .collect()
}

/// A stream on a queue of `capacity` chunks, an armed one if `armed`, fed from
/// a buffer that always has audio. Returns the decoder (the receiving end) too.
struct Fixture {
    stream: Stream,
    buffer: Mutex<JitterBuffer>,
    decoder: Option<LiveAudioDecoder>,
    clock: Instant,
    elapsed: Duration,
    chunk: Vec<i16>,
}

impl Fixture {
    fn new(capacity: usize, armed: bool) -> Self {
        let mut buffer = JitterBuffer::new(RATE, CH as u8, 400);
        buffer.push(&audio(RATE as usize));
        buffer.start_playout();
        let pacer = buffer.pacer();
        let (sender, decoder) = LiveAudioDecoder::create_pair(RATE, CH as u8, capacity);
        let mut stream = Stream::new(sender, pacer);
        if armed {
            stream.arm();
        }
        Self {
            stream,
            buffer: Mutex::new(buffer),
            decoder: Some(decoder),
            clock: Instant::now(),
            elapsed: Duration::ZERO,
            chunk: vec![0; PLAYOUT_CHUNK_FRAMES * CH],
        }
    }

    /// One 10 ms tick: new audio comes in, the stream is offered a chunk.
    fn tick(&mut self) -> Feed {
        self.elapsed += STEP;
        {
            let mut buffer = self.buffer.lock().unwrap();
            buffer.push(&audio(RATE as usize / 100));
        }
        self.stream.feed(
            &self.buffer,
            &mut self.chunk,
            (CH as u8, RATE),
            self.elapsed,
            self.clock + self.elapsed,
        )
    }

    /// Ticks until `limit` has passed; returns the time of the first `Dead`.
    fn run_until(&mut self, limit: Duration) -> Option<Duration> {
        while self.elapsed < limit {
            if self.tick() == Feed::Dead {
                return Some(self.elapsed);
            }
        }
        None
    }

    /// The receiver takes everything queued.
    fn drain(&mut self) {
        let decoder = self.decoder.as_mut().unwrap();
        while decoder.decode_frame().unwrap().is_some() {}
    }
}

#[test]
fn a_healthy_stream_is_fed_in_whole_chunks_and_never_dies() {
    let mut fixture = Fixture::new(64, true);
    let mut sent = 0;
    while fixture.elapsed < Duration::from_secs(10) {
        if fixture.tick() == Feed::Sent {
            sent += 1;
        }
        fixture.drain();
    }

    // Real time: 10 s of 1024-frame chunks, plus the initial lead.
    let expected = 10.0 * f64::from(RATE) / PLAYOUT_CHUNK_FRAMES as f64;
    assert!(
        (f64::from(sent) - expected).abs() < 8.0,
        "{sent} vs {expected}"
    );
}

#[test]
fn a_disconnected_receiver_is_dead_after_the_stall_limit() {
    let mut fixture = Fixture::new(64, true);
    fixture.decoder = None; // the receiving end is gone; the queue is not full

    let dead = fixture
        .run_until(Duration::from_secs(20))
        .expect("a stream nobody listens to must be declared dead");

    // The refusals start with the first chunk (a few ms in) and must last the
    // whole limit.
    assert!(dead >= STALL_LIMIT, "dead after {dead:?}");
    assert!(
        dead < STALL_LIMIT + Duration::from_secs(1),
        "dead after {dead:?}"
    );
}

#[test]
fn a_full_queue_is_not_dead_before_the_stall_limit() {
    let mut fixture = Fixture::new(4, true);

    // Nobody receives: the queue fills up and stays full.
    assert_eq!(
        fixture.run_until(STALL_LIMIT - Duration::from_millis(500)),
        None
    );
    let dead = fixture
        .run_until(Duration::from_secs(20))
        .expect("a queue that never drains is a dead stream");
    assert!(dead >= STALL_LIMIT, "dead after {dead:?}");
}

#[test]
fn a_queue_that_drains_now_and_then_is_alive() {
    let mut fixture = Fixture::new(4, true);

    // Full for 4 s, then the receiver takes a little, then full again: the
    // refusals never last 5 s in a row.
    for _ in 0..4 {
        assert_eq!(
            fixture.run_until(fixture.elapsed + Duration::from_secs(4)),
            None
        );
        fixture.drain();
        let mut accepted = false;
        for _ in 0..200 {
            accepted |= fixture.tick() == Feed::Sent;
            fixture.drain();
            if accepted {
                break;
            }
        }
        assert!(accepted, "a drained queue takes audio again");
    }
}

#[test]
fn a_disconnect_after_a_while_is_noticed_too() {
    let mut fixture = Fixture::new(64, true);
    while fixture.elapsed < Duration::from_secs(3) {
        assert_ne!(fixture.tick(), Feed::Dead);
        fixture.drain();
    }

    fixture.decoder = None;
    let started = fixture.elapsed;
    let dead = fixture
        .run_until(started + Duration::from_secs(20))
        .unwrap();

    assert!(dead - started >= STALL_LIMIT, "{:?}", dead - started);
    assert!(dead - started < STALL_LIMIT + Duration::from_secs(1));
}

#[test]
fn nothing_counts_as_dead_before_the_stream_is_armed() {
    let mut fixture = Fixture::new(4, false);
    fixture.decoder = None;

    assert_eq!(fixture.run_until(Duration::from_secs(30)), None);
    fixture.stream.arm();
    assert_eq!(
        fixture.run_until(fixture.elapsed + STALL_LIMIT - Duration::from_millis(500)),
        None
    );
    assert!(
        fixture
            .run_until(fixture.elapsed + Duration::from_secs(2))
            .is_some()
    );
}

#[test]
fn a_chunk_of_the_wrong_size_is_never_sent() {
    #[cfg(not(debug_assertions))]
    {
        let mut fixture = Fixture::new(64, true);
        let mut short = vec![0_i16; 480 * CH];
        let fed = fixture.stream.feed(
            &fixture.buffer,
            &mut short,
            (CH as u8, RATE),
            Duration::from_millis(1),
            fixture.clock,
        );
        assert_eq!(fed, Feed::Idle);
        let received = fixture.decoder.as_mut().unwrap().decode_frame().unwrap();
        assert!(received.is_none());
    }
}

/// Chunks a stream sends over 10 s of the playout thread's clock while its
/// own clock (a buffered stream: the speaker's) runs `speaker_speed` times as
/// fast, with an input that keeps up with the speaker.
fn chunks_paced_on_the_speaker_clock(speaker_speed: f64) -> usize {
    let mut buffer = JitterBuffer::new(RATE, CH as u8, 400);
    buffer.push(&audio(RATE as usize));
    buffer.start_playout();
    let pacer = buffer.pacer();
    let speaker_ns = Arc::new(AtomicU64::new(5_000_000_000));
    let clock = {
        let speaker_ns = Arc::clone(&speaker_ns);
        MediaClock::from_fn(move || speaker_ns.load(Ordering::Relaxed))
    };
    let (sender, mut decoder) = LiveAudioDecoder::create_pair(RATE, CH as u8, 64);
    let mut stream = Stream::with_clock(sender, pacer, clock);
    stream.arm();
    let buffer = Mutex::new(buffer);
    let mut chunk = vec![0; PLAYOUT_CHUNK_FRAMES * CH];
    let start = Instant::now();
    let tick_ns = (STEP.as_nanos() as f64 * speaker_speed) as u64;
    let input_frames = (RATE as f64 / 100.0 * speaker_speed).round() as usize;
    let mut sent = 0;
    for tick in 1..=1_000u32 {
        speaker_ns.fetch_add(tick_ns, Ordering::Relaxed);
        buffer.lock().unwrap().push(&audio(input_frames));
        // The playout thread's own clock says 10 ms per tick.
        let playout_now = STEP * tick;
        if stream.feed(
            &buffer,
            &mut chunk,
            (CH as u8, RATE),
            playout_now,
            start + playout_now,
        ) == Feed::Sent
        {
            sent += 1;
        }
        while decoder.decode_frame().unwrap().is_some() {}
    }
    sent
}

#[test]
fn a_buffered_stream_is_paced_on_the_speaker_clock() {
    // 10 s of the thread's clock are 10.5 s of a speaker clock 5 % fast:
    // the stream must deliver what the speaker plays, 23 chunks more.
    let per_second = f64::from(RATE) / PLAYOUT_CHUNK_FRAMES as f64;
    for speed in [1.0, 1.05, 0.95] {
        let sent = chunks_paced_on_the_speaker_clock(speed) as f64;
        let expected = 10.0 * speed * per_second;
        assert!(
            (sent - expected).abs() < 4.0,
            "speaker at {speed}: {sent} chunks, expected {expected:.1}"
        );
    }
}
