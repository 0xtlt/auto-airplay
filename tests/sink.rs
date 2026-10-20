//! The capture and playout threads against an `AudioSink` with no stream
//! attached (the state the daemon is in while it waits for sound): the level
//! must be reported, the stats must tick, and neither thread may ever block.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use airplay_client::LiveAudioDecoder;
use auto_airplay::playout::AudioSink;
use auto_airplay::plc::{JitterBuffer, StatsSnapshot};

const RATE: u32 = 48_000;
const CH: usize = 2;
const PERIOD: usize = 1024;

fn block(index: usize) -> Vec<i16> {
    (0..PERIOD * CH)
        .map(|n| {
            let frame = index * PERIOD + n / CH;
            ((frame as f32 * 0.06).sin() * 8_000.0) as i16 + ((frame * 7919) % 400) as i16 - 200
        })
        .collect()
}

fn shared(jitter_ms: u32) -> Arc<Mutex<JitterBuffer>> {
    Arc::new(Mutex::new(JitterBuffer::new(RATE, CH as u8, jitter_ms)))
}

fn arrival(index: usize) -> Duration {
    Duration::from_secs_f64(index as f64 * PERIOD as f64 / f64::from(RATE))
}

#[test]
fn the_capture_side_reports_the_level_for_minutes_with_nothing_attached() {
    let sink = AudioSink::new();
    let buffer = shared(400);
    sink.install(Arc::clone(&buffer));

    let blocks = 70 * RATE as usize / PERIOD; // 70 s of audio
    let mut worst = Duration::ZERO;
    let mut silent_checks = 0;
    let mut stats = Vec::new();
    for index in 0..blocks {
        let started = Instant::now();
        sink.deliver(&buffer, arrival(index), &block(index), 0);
        worst = worst.max(started.elapsed());
        // The daemon looks every 250 ms.
        if index % 12 == 11 && sink.take_peak_level() < 0.05 {
            silent_checks += 1;
        }
        if index % (60 * RATE as usize / PERIOD) == 60 * RATE as usize / PERIOD - 1 {
            stats.push(buffer.lock().unwrap().take_stats());
        }
    }

    assert_eq!(silent_checks, 0, "the level was lost");
    assert!(worst < Duration::from_millis(100), "a push took {worst:?}");
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].losses, 0, "{}", stats[0]);
    assert_eq!(stats[0].buffer_fill_ms, 400);
    assert_eq!(stats[0].reads, 60 * u64::from(RATE) / PERIOD as u64);
}

/// Runs `body`, failing the test if it takes more than `limit`.
fn within(limit: Duration, body: impl FnOnce() + Send + 'static) {
    let (done, finished) = mpsc::channel();
    thread::spawn(move || {
        body();
        let _ = done.send(());
    });
    finished
        .recv_timeout(limit)
        .expect("the threads deadlocked or blocked");
}

#[test]
fn the_playout_thread_reports_stats_while_capture_pushes_and_nothing_is_attached() {
    within(Duration::from_secs(60), || {
        let sink = AudioSink::new();
        let buffer = shared(400);
        sink.install(Arc::clone(&buffer));
        let running = Arc::new(AtomicBool::new(true));
        let (reports, received) = mpsc::channel::<StatsSnapshot>();

        let playout = {
            let (sink, buffer, running) = (Arc::clone(&sink), Arc::clone(&buffer), running.clone());
            thread::spawn(move || {
                auto_airplay::playout::run_playout(
                    &sink,
                    &buffer,
                    &running,
                    Duration::from_millis(50),
                    |stats| {
                        let _ = reports.send(stats);
                    },
                );
            })
        };
        // The capture thread: 90 simulated seconds as fast as it can go, with
        // a pause every so often so the playout thread gets its turns.
        let capture = {
            let (sink, buffer) = (Arc::clone(&sink), Arc::clone(&buffer));
            thread::spawn(move || {
                let mut loud = 0;
                for index in 0..90 * RATE as usize / PERIOD {
                    sink.deliver(&buffer, arrival(index), &block(index), 0);
                    if sink.take_peak_level() > 0.05 {
                        loud += 1;
                    }
                    if index % 64 == 0 {
                        thread::sleep(Duration::from_millis(1));
                    }
                }
                loud
            })
        };

        let loud = capture.join().unwrap();
        // Stats keep coming after the capture is done, too.
        let mut ticks = 0;
        while received.recv_timeout(Duration::from_secs(2)).is_ok() {
            ticks += 1;
            if ticks >= 5 {
                break;
            }
        }
        running.store(false, Ordering::Release);
        playout.join().unwrap();

        assert!(loud > 4_000, "the level was reported {loud} times");
        assert!(ticks >= 5, "only {ticks} stats reports");
    });
}

#[test]
fn attaching_detaching_and_capturing_at_once_never_deadlock() {
    within(Duration::from_secs(60), || {
        let sink = AudioSink::new();
        let buffer = shared(400);
        sink.install(Arc::clone(&buffer));
        let running = Arc::new(AtomicBool::new(true));

        let playout = {
            let (sink, buffer, running) = (Arc::clone(&sink), Arc::clone(&buffer), running.clone());
            thread::spawn(move || {
                auto_airplay::playout::run_playout(
                    &sink,
                    &buffer,
                    &running,
                    Duration::from_millis(5),
                    |_| {},
                );
            })
        };
        let capture = {
            let (sink, buffer, running) = (Arc::clone(&sink), Arc::clone(&buffer), running.clone());
            thread::spawn(move || {
                let mut index = 0;
                while running.load(Ordering::Acquire) {
                    sink.deliver(&buffer, arrival(index), &block(index), 0);
                    index += 1;
                    thread::sleep(Duration::from_micros(200));
                }
                index
            })
        };

        // The daemon's side: attach, arm, detach, over and over.
        let mut decoders = Vec::new();
        for round in 0..300 {
            let (sender, decoder) = LiveAudioDecoder::create_pair(RATE, CH as u8, 64);
            sink.attach(sender);
            if round % 2 == 0 {
                sink.arm();
            }
            thread::sleep(Duration::from_millis(2));
            sink.take_failed();
            sink.take_peak_level();
            if round % 3 == 0 {
                sink.detach();
            }
            decoders.push(decoder);
        }
        sink.detach();
        running.store(false, Ordering::Release);
        let blocks = capture.join().unwrap();
        playout.join().unwrap();

        assert!(blocks > 100, "capture pushed only {blocks} blocks");
    });
}
