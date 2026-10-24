//! The AirPlay output chain, from the playout chunks to the packets on the
//! wire, checked for anything on the sender's side that could be heard as a
//! crackle.
//!
//! * Offline: the 48 kHz to 44.1 kHz resampler and the cut into 352-frame
//!   packets (`LiveAudioDecoder::decode_resampled`) keep a sine continuous,
//!   and the ALAC encoder is lossless: every packet decodes back bit for bit
//!   (symphonia's ALAC decoder), uncompressed ("escape") packets included.
//! * On loopback, with the daemon's input path (jitter buffer and pacer) and
//!   the real sender (`AudioStreamer::start_live`): the packets arrive in
//!   order, and the audio decoded from them is exactly what the sender was
//!   given, resampled (to the dither, from the first frame on); retransmit
//!   requests are answered with the packets asked for (payload type 86) and
//!   counted by `take_output_stats`. (Which requests count as late, and how
//!   old, is tested on a controlled clock in `tests/output_stats.rs`.) A
//!   packet whose send fails is neither counted nor dated.
//!
//! A sine shows any splice: once a sine is fitted to a second of audio, what
//! is left is the dither (about 2 LSB) unless a frame is missing, repeated or
//! altered, which leaves hundreds to thousands.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use airplay_audio::{
    AlacEncoder, AudioEncoder, AudioStreamer, LiveAudioDecoder, LivePcmFrame, RetransmitRequest,
    RtpSender,
};
use airplay_client::{AudioFormat, ClockOffset, StreamConfig};
use airplay_resampler::Resampler;
use auto_airplay::plc::{JitterBuffer, PLAYOUT_CHUNK_FRAMES};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_ALAC, CodecParameters, Decoder, DecoderOptions};
use symphonia::core::formats::Packet;
use symphonia::default::codecs::AlacDecoder;

const CH: usize = 2;
const INPUT_RATE: u32 = 48_000;
const RTP_RATE: usize = 44_100;
const FRAMES_PER_PACKET: usize = 352;
/// One tone per channel, unrelated to either sample rate.
const HZ: [f64; CH] = [997.0, 1_499.0];
const AMPLITUDE: f64 = 0.5 * 32_767.0;
/// Dither and rounding stay within about 2.5 LSB of the fitted sine; a splice
/// leaves far more.
const MAX_RESIDUAL: f64 = 16.0;
/// Two resamplings of the same audio differ by their dither (TPDF, 1 LSB
/// each way) and its rounding: 2 LSB measured; a lost or repeated frame
/// leaves thousands.
const MAX_DITHER_DIFFERENCE: i32 = 4;

/// `frames` stereo frames of the 48 kHz test tone from frame `first` on.
fn tone(first: usize, frames: usize) -> Vec<i16> {
    let mut samples = Vec::with_capacity(frames * CH);
    for frame in first..first + frames {
        let t = frame as f64 / f64::from(INPUT_RATE);
        for hz in HZ {
            samples.push((AMPLITUDE * (std::f64::consts::TAU * hz * t).sin()).round() as i16);
        }
    }
    samples
}

/// What is left of `y` once the best `a sin(wn) + b cos(wn) + c` is removed.
fn residuals(y: &[f64], w: f64) -> Vec<f64> {
    let basis = |n: usize| [(w * n as f64).sin(), (w * n as f64).cos(), 1.0];
    let mut normal = [[0.0_f64; 3]; 3];
    let mut rhs = [0.0_f64; 3];
    for (n, &value) in y.iter().enumerate() {
        let row = basis(n);
        for i in 0..3 {
            for j in 0..3 {
                normal[i][j] += row[i] * row[j];
            }
            rhs[i] += row[i] * value;
        }
    }
    let det = |m: [[f64; 3]; 3]| {
        m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
            - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
            + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
    };
    let whole = det(normal);
    let mut coefficients = [0.0; 3];
    for (k, coefficient) in coefficients.iter_mut().enumerate() {
        let mut replaced = normal;
        for i in 0..3 {
            replaced[i][k] = rhs[i];
        }
        *coefficient = det(replaced) / whole;
    }
    y.iter()
        .enumerate()
        .map(|(n, &value)| {
            let row = basis(n);
            value - (0..3).map(|i| coefficients[i] * row[i]).sum::<f64>()
        })
        .collect()
}

/// The largest residual of 44.1 kHz audio after `skip` frames, fitted one
/// second at a time, with the frame it is at.
fn worst_residual(pcm: &[i16], skip: usize) -> (f64, usize) {
    let mut worst = (0.0_f64, 0_usize);
    for (channel, hz) in HZ.iter().enumerate() {
        let y: Vec<f64> = pcm
            .iter()
            .skip(channel)
            .step_by(CH)
            .skip(skip)
            .map(|&sample| f64::from(sample))
            .collect();
        let w = std::f64::consts::TAU * hz / RTP_RATE as f64;
        for (second, window) in y.as_chunks::<RTP_RATE>().0.iter().enumerate() {
            for (n, value) in residuals(window, w).into_iter().enumerate() {
                if value.abs() > worst.0 {
                    worst = (value.abs(), skip + second * RTP_RATE + n);
                }
            }
        }
    }
    worst
}

/// An ALAC decoder set up with the encoder's own parameters.
fn alac_decoder() -> AlacDecoder {
    let cookie = AlacEncoder::new(AudioFormat::default())
        .unwrap()
        .magic_cookie();
    let mut params = CodecParameters::new();
    params
        .for_codec(CODEC_TYPE_ALAC)
        .with_extra_data(cookie.into_boxed_slice());
    AlacDecoder::try_new(&params, &DecoderOptions::default()).unwrap()
}

fn decode(decoder: &mut AlacDecoder, payload: &[u8], timestamp: u64) -> Vec<i16> {
    let packet = Packet::new_from_slice(0, timestamp, FRAMES_PER_PACKET as u64, payload);
    let decoded = decoder.decode(&packet).unwrap();
    let mut samples = SampleBuffer::<i16>::new(decoded.capacity() as u64, *decoded.spec());
    samples.copy_interleaved_ref(decoded);
    samples.samples().to_vec()
}

// ----- offline -----

#[test]
fn the_resampler_and_the_packet_cut_keep_a_sine_continuous() {
    let (sender, mut decoder) = LiveAudioDecoder::create_pair(INPUT_RATE, CH as u8, 64);
    let format = AudioFormat::default();
    let chunks = 20 * INPUT_RATE as usize / PLAYOUT_CHUNK_FRAMES;
    let mut out = Vec::new();
    for chunk in 0..chunks {
        let frame = LivePcmFrame {
            samples: tone(chunk * PLAYOUT_CHUNK_FRAMES, PLAYOUT_CHUNK_FRAMES),
            channels: CH as u8,
            sample_rate: INPUT_RATE,
        };
        assert!(sender.try_send(frame));
        while let Some(packet) = decoder
            .decode_resampled(&format, FRAMES_PER_PACKET)
            .unwrap()
        {
            assert_eq!(packet.samples.len(), FRAMES_PER_PACKET * CH);
            out.extend_from_slice(&packet.samples);
        }
    }

    // Nothing lost or added: 20 s in, 20 s out, less the residue of the last
    // packet and the resampler's delay.
    let frames = out.len() / CH;
    let expected = (chunks * PLAYOUT_CHUNK_FRAMES) * RTP_RATE / INPUT_RATE as usize;
    assert!(
        (expected - 2 * FRAMES_PER_PACKET..=expected).contains(&frames),
        "{frames} frames out, {expected} expected"
    );
    // Half a second for the resampler's start-up (silence, then the tone).
    let (residual, at) = worst_residual(&out, RTP_RATE / 2);
    assert!(
        residual <= MAX_RESIDUAL,
        "a step of {residual:.1} LSB at frame {at} (packet boundary: {})",
        at % FRAMES_PER_PACKET == 0
    );
}

/// Each packet's worth of `signal`, through the encoder and back.
fn assert_lossless(name: &str, signal: &[i16]) -> usize {
    let mut encoder = AlacEncoder::new(AudioFormat::default()).unwrap();
    let mut decoder = alac_decoder();
    let mut escapes = 0;
    let (packets, _) = signal.as_chunks::<{ FRAMES_PER_PACKET * CH }>();
    for (index, samples) in packets.iter().enumerate() {
        let packet = encoder.encode(samples).unwrap();
        assert_eq!(packet.samples as usize, FRAMES_PER_PACKET);
        // An uncompressed packet: 16 bits per sample and a few header bytes.
        if packet.data.len() >= FRAMES_PER_PACKET * CH * 2 {
            escapes += 1;
        }
        let decoded = decode(&mut decoder, &packet.data, packet.timestamp);
        assert!(
            decoded == samples.as_slice(),
            "{name}: packet {index} does not decode back"
        );
    }
    escapes
}

/// A deterministic noise source (xorshift64*), in -1..1.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let bits = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11;
        bits as f64 / (1_u64 << 52) as f64 - 1.0
    }
}

#[test]
fn alac_packets_decode_back_bit_for_bit() {
    let frames = 2 * RTP_RATE;
    let mut noise = Noise(0x5eed_1234_abcd_ef01);

    let sine: Vec<i16> = (0..frames * CH)
        .map(|n| (AMPLITUDE * ((n / CH) as f64 * 0.142).sin()) as i16)
        .collect();
    assert_lossless("sine", &sine);

    // Full-scale white noise does not compress: every packet is sent as is.
    let white: Vec<i16> = (0..frames * CH)
        .map(|_| (noise.next() * 32_767.0) as i16)
        .collect();
    let escapes = assert_lossless("white noise", &white);
    assert_eq!(escapes, frames / FRAMES_PER_PACKET, "every packet escapes");

    let extremes: Vec<i16> = (0..frames * CH)
        .map(|n| {
            if (n / CH).is_multiple_of(2) {
                i16::MAX
            } else {
                i16::MIN
            }
        })
        .collect();
    assert_lossless("full-scale square", &extremes);

    // Loud music-like audio with clicks, clipped: what a hot turntable sends.
    let mut low = [0.0_f64; CH];
    let mut loud = Vec::with_capacity(frames * CH);
    for frame in 0..frames {
        let t = frame as f64 / RTP_RATE as f64;
        for (channel, low) in low.iter_mut().enumerate() {
            *low = 0.97 * *low + 0.03 * noise.next();
            let click = if noise.next() > 0.9995 { 0.9 } else { 0.0 };
            let music = 0.6 * (std::f64::consts::TAU * (220.0 + channel as f64) * t).sin()
                + 0.3 * (std::f64::consts::TAU * 3_520.0 * t).sin();
            let value = 3.0 * (music + 2.0 * *low) + click;
            loud.push((value.clamp(-1.0, 1.0) * 32_767.0) as i16);
        }
    }
    assert_lossless("loud clipped music", &loud);
}

// ----- on loopback -----

/// What arrived on the receiver's ports.
#[derive(Default)]
struct Wire {
    /// Audio packets in arrival order: sequence number, RTP timestamp, payload.
    audio: Vec<(u16, u32, Vec<u8>)>,
    /// Retransmit responses (payload type 86): the sequence number in their
    /// header, and the one of the packet they carry.
    resent: Vec<(u16, u16)>,
}

fn listen(socket: UdpSocket, wire: Arc<Mutex<Wire>>, running: Arc<AtomicBool>) -> JoinHandle<()> {
    socket
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    thread::spawn(move || {
        let mut buffer = [0_u8; 2048];
        while running.load(Ordering::Acquire) {
            let Ok((len, _)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            let packet = &buffer[..len];
            let seq = |at: usize| u16::from_be_bytes([packet[at], packet[at + 1]]);
            let mut wire = wire.lock().unwrap();
            match packet[1] & 0x7f {
                96 => wire.audio.push((
                    seq(2),
                    u32::from_be_bytes(packet[4..8].try_into().unwrap()),
                    packet[12..].to_vec(),
                )),
                86 => wire.resent.push((seq(2), seq(6))),
                _ => {}
            }
        }
    })
}

/// The daemon's input path: a 48 kHz capture into the jitter buffer, and a
/// playout thread handing paced chunks to the sender's live queue while
/// `feeding` is set.
struct Input {
    /// Every chunk the live queue took, in order: what the sender was given.
    fed: Arc<Mutex<Vec<i16>>>,
    /// Chunks the live queue refused (none expected: a chunk is only pulled
    /// from the jitter buffer when the queue has room).
    refused: Arc<AtomicUsize>,
    feeding: Arc<AtomicBool>,
    capture: JoinHandle<()>,
    playout: JoinHandle<()>,
}

fn input(running: &Arc<AtomicBool>) -> (LiveAudioDecoder, Input) {
    let buffer = Arc::new(Mutex::new(JitterBuffer::new(INPUT_RATE, CH as u8, 400)));
    let epoch = Instant::now();
    let capture = {
        let (buffer, running) = (Arc::clone(&buffer), Arc::clone(running));
        thread::spawn(move || {
            let period = PLAYOUT_CHUNK_FRAMES as f64 / f64::from(INPUT_RATE);
            let mut index = 0;
            while running.load(Ordering::Acquire) {
                let due = epoch + Duration::from_secs_f64(index as f64 * period);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    thread::sleep(wait);
                }
                let samples = tone(index * PLAYOUT_CHUNK_FRAMES, PLAYOUT_CHUNK_FRAMES);
                buffer.lock().unwrap().push_at(epoch.elapsed(), &samples, 0);
                index += 1;
            }
        })
    };
    // The capture runs all along: the cushion is full when the stream attaches.
    thread::sleep(Duration::from_millis(600));
    let (sender, decoder) = LiveAudioDecoder::create_pair(INPUT_RATE, CH as u8, 64);
    let fed = Arc::new(Mutex::new(Vec::new()));
    let refused = Arc::new(AtomicUsize::new(0));
    let feeding = Arc::new(AtomicBool::new(true));
    let playout = {
        let (fed, refused, feeding) =
            (Arc::clone(&fed), Arc::clone(&refused), Arc::clone(&feeding));
        thread::spawn(move || {
            let mut pacer = {
                let mut buffer = buffer.lock().unwrap();
                buffer.start_playout();
                buffer.pacer()
            };
            let mut chunk = vec![0_i16; PLAYOUT_CHUNK_FRAMES * CH];
            while feeding.load(Ordering::Acquire) {
                let now = epoch.elapsed();
                let sent = if sender.is_full() {
                    pacer.blocked(now);
                    false
                } else if buffer
                    .lock()
                    .unwrap()
                    .pull_chunk(&mut pacer, now, &mut chunk)
                {
                    let frame = LivePcmFrame {
                        samples: chunk.clone(),
                        channels: CH as u8,
                        sample_rate: INPUT_RATE,
                    };
                    if sender.try_send(frame) {
                        fed.lock().unwrap().extend_from_slice(&chunk);
                        pacer.sent(PLAYOUT_CHUNK_FRAMES);
                    } else {
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                    true
                } else {
                    false
                };
                if !sent {
                    thread::sleep(Duration::from_millis(3));
                }
            }
        })
    };
    thread::sleep(Duration::from_millis(500));
    let input = Input {
        fed,
        refused,
        feeding,
        capture,
        playout,
    };
    (decoder, input)
}

/// The first frame of `sent` more than `tolerance` off the start of
/// `reference`, with the difference there.
fn first_difference(sent: &[i16], reference: &[i16], tolerance: i32) -> Option<(usize, i32)> {
    sent.iter()
        .zip(reference)
        .enumerate()
        .map(|(n, (&a, &b))| (n / CH, (i32::from(a) - i32::from(b)).abs()))
        .find(|&(_, difference)| difference > tolerance)
}

#[test]
fn the_sender_sends_what_it_is_given_and_answers_retransmits() {
    let running = Arc::new(AtomicBool::new(true));
    let wire = Arc::new(Mutex::new(Wire::default()));
    let data = UdpSocket::bind("127.0.0.1:0").unwrap();
    let control = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (data_addr, control_addr) = (data.local_addr().unwrap(), control.local_addr().unwrap());
    let mut threads = vec![
        listen(data, Arc::clone(&wire), Arc::clone(&running)),
        listen(control, Arc::clone(&wire), Arc::clone(&running)),
    ];
    let (decoder, input) = input(&running);
    let Input {
        fed,
        refused,
        feeding,
        capture,
        playout,
    } = input;
    threads.push(capture);

    // Two workers, as in the daemon.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (stats, last) = runtime.block_on(async {
        let mut rtp = RtpSender::new(data_addr, 0x0a1b_2c3d);
        rtp.set_control_dest(control_addr);
        rtp.bind(0).unwrap();
        rtp.set_control_socket(UdpSocket::bind("127.0.0.1:0").unwrap());
        let mut streamer = AudioStreamer::new(StreamConfig::default());
        streamer.set_rtp_sender(rtp).await;
        streamer.set_render_delay_ms(250).await;
        streamer.set_timing_offset(ClockOffset::default()).await;
        streamer.start_live(decoder).await.unwrap();
        tokio::time::sleep(Duration::from_secs(6)).await;

        // Stop feeding, then the stream (in that order: once stopped, the
        // stream's queue is gone). From here on the retransmit history holds
        // every packet prepared, and nothing changes it.
        feeding.store(false, Ordering::Release);
        tokio::task::spawn_blocking(move || playout.join().unwrap())
            .await
            .unwrap();
        streamer.stop().await.unwrap();

        // What a receiver asks for when packets did not reach it, picked in
        // that history from a packet that arrived: it and the one before,
        // one 100 packets (0.8 s) back, and three 600 back, whose slots newer
        // packets took.
        let last = wire.lock().unwrap().audio.last().unwrap().0;
        let ask = |first_sequence: u16, count: u16| RetransmitRequest {
            first_sequence,
            count,
        };
        let recent = streamer
            .handle_retransmit(&ask(last.wrapping_sub(1), 2))
            .await
            .unwrap();
        let old = streamer
            .handle_retransmit(&ask(last.wrapping_sub(100), 1))
            .await
            .unwrap();
        let gone = streamer
            .handle_retransmit(&ask(last.wrapping_sub(600), 3))
            .await
            .unwrap();
        assert_eq!((recent, old, gone), (2, 1, 0));
        let stats = streamer.take_output_stats();
        // Until the packets sent again have arrived (5 s at most).
        let asked = Instant::now();
        while wire.lock().unwrap().resent.len() < 3 && asked.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        (stats, last)
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    running.store(false, Ordering::Release);
    for thread in threads {
        thread.join().unwrap();
    }
    let wire = std::mem::take(&mut *wire.lock().unwrap());

    // Every packet, in order, 352 frames apart.
    assert!(wire.audio.len() > 600, "{} packets", wire.audio.len());
    for pair in wire.audio.windows(2) {
        assert_eq!(pair[1].0, pair[0].0.wrapping_add(1), "sequence numbers");
        assert_eq!(pair[1].1, pair[0].1.wrapping_add(352), "RTP timestamps");
    }
    // The audio on the wire is what the sender was given, resampled, from
    // its first frame on: nothing lost, repeated or altered on the way. (The
    // simulated capture may be concealed in the jitter buffer when the test
    // machine is loaded; that happens before the sender, in what it is given.)
    assert_eq!(
        refused.load(Ordering::Relaxed),
        0,
        "the live queue refused chunks"
    );
    let mut alac = alac_decoder();
    let sent: Vec<i16> = wire
        .audio
        .iter()
        .flat_map(|(_, rtp, payload)| decode(&mut alac, payload, u64::from(*rtp)))
        .collect();
    let mut resampler = Resampler::new(INPUT_RATE, RTP_RATE as u32, CH as u8).unwrap();
    let reference: Vec<i16> = fed
        .lock()
        .unwrap()
        .as_chunks::<{ PLAYOUT_CHUNK_FRAMES * CH }>()
        .0
        .iter()
        .flat_map(|chunk| resampler.process(chunk).unwrap())
        .collect();
    assert!(sent.len() <= reference.len(), "more audio sent than given");
    if let Some((at, difference)) = first_difference(&sent, &reference, MAX_DITHER_DIFFERENCE) {
        panic!(
            "the sent audio is {difference} LSB off what was given at frame {at} (packet {})",
            at / FRAMES_PER_PACKET
        );
    }

    // The packets asked for, sent again whole on the control port.
    let mut resent: Vec<u16> = wire.resent.iter().map(|&(_, inner)| inner).collect();
    resent.sort_unstable();
    let mut expected = vec![last.wrapping_sub(1), last, last.wrapping_sub(100)];
    expected.sort_unstable();
    assert_eq!(resent, expected);
    assert!(wire.resent.iter().all(|&(outer, inner)| outer == inner));

    // ... and counted. (Which requests came late and how old the packets
    // were depends on when things ran here: see tests/output_stats.rs, on a
    // controlled clock.)
    assert_eq!(
        (
            stats.rtx_requests,
            stats.rtx_requested,
            stats.rtx_served,
            stats.rtx_missed
        ),
        (3, 6, 3, 3),
        "{stats:?}"
    );
}

#[test]
fn failed_sends_are_neither_counted_nor_dated() {
    // An IPv6 speaker for the sender's IPv4 sockets: every send fails at once.
    let speaker: SocketAddr = "[::1]:9".parse().unwrap();
    let (sender, decoder) = LiveAudioDecoder::create_pair(INPUT_RATE, CH as u8, 64);
    // More than the second of audio `start_live` waits for, queued up front.
    for chunk in 0..64 {
        let frame = LivePcmFrame {
            samples: tone(chunk * PLAYOUT_CHUNK_FRAMES, PLAYOUT_CHUNK_FRAMES),
            channels: CH as u8,
            sample_rate: INPUT_RATE,
        };
        assert!(sender.try_send(frame));
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (handed_over, resend_failed, stats) = runtime.block_on(async {
        let mut rtp = RtpSender::new(speaker, 0x0a1b_2c3d);
        rtp.set_control_dest(speaker);
        rtp.bind(0).unwrap();
        rtp.set_control_socket(UdpSocket::bind("127.0.0.1:0").unwrap());
        let mut streamer = AudioStreamer::new(StreamConfig::default());
        streamer.set_rtp_sender(rtp).await;
        streamer.set_render_delay_ms(250).await;
        streamer.set_timing_offset(ClockOffset::default()).await;
        streamer.start_live(decoder).await.unwrap();
        // The sender thread's queue holds 8 packets: once more than 16 were
        // handed over, it has tried to send some (5 s at most).
        let started = Instant::now();
        while streamer.packets_sent() <= 16 && started.elapsed() < Duration::from_secs(5) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        streamer.stop().await.unwrap();
        // Packet 0 was prepared and kept for resending, but never went out.
        let resend = streamer
            .handle_retransmit(&RetransmitRequest {
                first_sequence: 0,
                count: 1,
            })
            .await;
        (
            streamer.packets_sent(),
            resend.is_err(),
            streamer.take_output_stats(),
        )
    });
    runtime.shutdown_timeout(Duration::from_secs(2));
    drop(sender);

    assert!(handed_over > 16, "{handed_over} packets handed over");
    assert!(resend_failed, "the resend cannot go out either");
    assert_eq!(
        (stats.packets_sent, stats.late_packets, stats.max_late_us),
        (0, 0, 0),
        "{stats:?}"
    );
    assert_eq!(
        (stats.rtx_requested, stats.rtx_served, stats.rtx_missed),
        (1, 0, 1),
        "{stats:?}"
    );
    // Never sent: no age, and not judged late.
    assert_eq!((stats.rtx_late, stats.rtx_max_age_us), (0, 0), "{stats:?}");
}
