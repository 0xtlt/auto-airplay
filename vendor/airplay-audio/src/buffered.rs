//! Buffered audio over TCP: AirPlay 2 stream type 103 (local addition, see
//! VENDORED.md).
//!
//! A realtime stream (type 96) sends each packet over UDP just in time, so the
//! receiver only holds what its render delay allows (about 170 ms here): a
//! Wi-Fi stall longer than that is heard. A buffered stream, what Apple TVs
//! and iPhones send to HomePods, goes over TCP and is anchored once to the
//! receiver's PTP clock (`SETRATEANCHORTIME`); the sender can run seconds
//! ahead and the receiver keeps everything until it is due. Nothing is lost,
//! and a stall only eats into that lead.
//!
//! Each packet on the TCP connection:
//!
//! ```text
//! u16 BE   length of the whole packet, these two bytes included
//! u8       0x80 (RTP version 2)
//! u24 BE   sequence number (the RTP payload type byte is part of it)
//! u32 BE   RTP timestamp
//! u32 BE   SSRC (Apple senders give the audio format here)
//! ...      AAC-LC access unit (1024 frames, raw, no ADTS), encrypted
//! [16]     Poly1305 tag
//! [8]      nonce
//! ```
//!
//! The payload is sealed with ChaCha20-Poly1305 under the stream key sent in
//! SETUP (`shk`), with the RTP timestamp and SSRC as additional data and a
//! 12-byte nonce made of four zero bytes and the eight sent at the end, like
//! the realtime stream. The nonce counts packets, so it never repeats.

use std::io::{self, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use airplay_core::codec::{AudioCodec, AudioFormat, SampleRate};
use airplay_core::error::{Result, StreamingError};
use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};

use crate::LiveAudioDecoder;

/// Frames per packet of a buffered AAC stream (`spf` in SETUP).
pub const FRAMES_PER_PACKET: u32 = 1024;
/// The SSRC Apple senders put on AAC-LC, 44.1 kHz, stereo buffered packets.
pub const SSRC_AAC_44100_STEREO: u32 = 0x1600_0000;
/// AAC bit rate: what Apple senders use for AirPlay.
pub const AAC_BIT_RATE: u32 = 256_000;
/// fdk-aac's AAC-LC encoder delay: the audio of input frame `n` is in the
/// access units around frame `n + 2048` (about 46 ms).
pub const AAC_ENCODER_DELAY_FRAMES: u32 = 2048;
/// Packets queued for the TCP connection: about 12 s of audio. A longer
/// stall drops the oldest new packets (counted) rather than the stream.
pub const SEND_QUEUE_PACKETS: usize = 512;
/// A write blocked this long fails the stream.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the encoder waits for input before looking at `stop`.
const INPUT_POLL: Duration = Duration::from_millis(5);
const RTP_HEADER_LEN: usize = 12;
const TAG_LEN: usize = 16;
const NONCE_LEN: usize = 8;

/// The format of a buffered AAC stream.
pub fn aac_format() -> AudioFormat {
    AudioFormat {
        codec: AudioCodec::Aac,
        sample_rate: SampleRate::Hz44100,
        bit_depth: 16,
        channels: 2,
        frames_per_packet: FRAMES_PER_PACKET,
    }
}

/// Seals AAC access units into buffered packets.
pub struct Packetizer {
    cipher: ChaCha20Poly1305,
    ssrc: u32,
    sequence: u32,
    nonce: u64,
}

impl Packetizer {
    /// `key` is the stream key sent in SETUP; sequence numbers start at
    /// `first_sequence` (24 bits).
    pub fn new(key: [u8; 32], ssrc: u32, first_sequence: u32) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(&key.into()),
            ssrc,
            sequence: first_sequence & 0x00FF_FFFF,
            nonce: 0,
        }
    }

    /// The sequence number the next packet gets.
    pub fn next_sequence(&self) -> u32 {
        self.sequence
    }

    /// The packet carrying `payload` at RTP `timestamp`, length prefix
    /// included.
    pub fn packet(&mut self, timestamp: u32, payload: &[u8]) -> Result<Vec<u8>> {
        let total = 2 + RTP_HEADER_LEN + payload.len() + TAG_LEN + NONCE_LEN;
        let length = u16::try_from(total).map_err(|_| {
            StreamingError::Encoding(format!("buffered packet too long ({total} bytes)"))
        })?;
        let mut packet = Vec::with_capacity(total);
        packet.extend_from_slice(&length.to_be_bytes());
        packet.push(0x80);
        packet.extend_from_slice(&self.sequence.to_be_bytes()[1..]);
        packet.extend_from_slice(&timestamp.to_be_bytes());
        packet.extend_from_slice(&self.ssrc.to_be_bytes());
        let aad: [u8; 8] = packet[6..14].try_into().expect("timestamp and SSRC");

        let nonce_bytes = self.nonce.to_le_bytes();
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&nonce_bytes);
        let mut sealed = payload.to_vec();
        let tag = self
            .cipher
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &aad, &mut sealed)
            .map_err(|_| StreamingError::Encoding("cannot encrypt a buffered packet".into()))?;
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&tag);
        packet.extend_from_slice(&nonce_bytes);

        self.sequence = (self.sequence + 1) & 0x00FF_FFFF;
        self.nonce = self.nonce.wrapping_add(1);
        Ok(packet)
    }
}

/// AAC-LC encoder for buffered streams: 1024 frames in, one raw access unit
/// out. The access units lag the input by [`AAC_ENCODER_DELAY_FRAMES`]
/// (fdk-aac primes with silence), a constant added to the latency.
pub struct AacLcEncoder {
    encoder: fdk_aac::enc::Encoder,
    output: Vec<u8>,
}

impl AacLcEncoder {
    /// Stereo, 44.1 kHz, constant `bit_rate`.
    pub fn new(bit_rate: u32) -> Result<Self> {
        let encoder = fdk_aac::enc::Encoder::new(fdk_aac::enc::EncoderParams {
            bit_rate: fdk_aac::enc::BitRate::Cbr(bit_rate),
            sample_rate: 44_100,
            transport: fdk_aac::enc::Transport::Raw,
            channels: fdk_aac::enc::ChannelMode::Stereo,
        })
        .map_err(|error| {
            StreamingError::Encoding(format!("cannot create the AAC encoder: {error}"))
        })?;
        Ok(Self {
            encoder,
            output: vec![0u8; 8192],
        })
    }

    /// The AudioSpecificConfig of the stream (2 bytes for AAC-LC).
    pub fn audio_specific_config(&self) -> Result<Vec<u8>> {
        let info = self
            .encoder
            .info()
            .map_err(|error| StreamingError::Encoding(format!("AAC encoder info: {error}")))?;
        Ok(info.confBuf[..info.confSize as usize].to_vec())
    }

    /// Encodes 1024 interleaved stereo frames into one access unit (empty if
    /// the encoder had nothing to emit yet).
    pub fn encode(&mut self, samples: &[i16]) -> Result<&[u8]> {
        let info = self
            .encoder
            .encode(samples, &mut self.output)
            .map_err(|error| StreamingError::Encoding(format!("AAC encoding failed: {error}")))?;
        if info.input_consumed != samples.len() {
            return Err(StreamingError::Encoding(format!(
                "the AAC encoder took {} of {} samples",
                info.input_consumed,
                samples.len()
            ))
            .into());
        }
        Ok(&self.output[..info.output_size])
    }
}

/// Why a buffered stream stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFailure {
    pub message: String,
    /// Set when the connection failed (reset, broken pipe, timeout...).
    pub kind: Option<io::ErrorKind>,
}

impl StreamFailure {
    /// As an I/O error of the same kind (`Other` for encoding failures).
    pub fn to_io_error(&self) -> io::Error {
        io::Error::new(
            self.kind.unwrap_or(io::ErrorKind::Other),
            self.message.clone(),
        )
    }
}

/// Counters of a buffered stream.
#[derive(Debug, Default)]
pub struct BufferedCounters {
    /// Access units produced by the encoder (and packets built).
    pub packets: AtomicU64,
    /// Packets written to the connection.
    pub sent: AtomicU64,
    /// Bytes written to the connection.
    pub bytes: AtomicU64,
    /// Packets dropped because the send queue was full.
    pub dropped: AtomicU64,
    /// Longest single write, in µs (a stalled connection).
    pub longest_write_us: AtomicU64,
}

/// A snapshot of [`BufferedCounters`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BufferedStats {
    pub packets: u64,
    pub sent: u64,
    pub bytes: u64,
    pub dropped: u64,
    pub queued: usize,
    pub longest_write_us: u64,
}

/// Where a buffered stream is: the sequence number and RTP timestamp of the
/// next packet it will send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamPosition {
    pub sequence: u32,
    pub timestamp: u32,
}

/// What the encoding thread works on; `pause` and `resume` take it too, so a
/// packet is either made before a pause or after the resume.
struct Encoding {
    encoder: AacLcEncoder,
    packetizer: Packetizer,
    timestamp: u32,
    paused: bool,
}

impl Encoding {
    fn position(&self) -> StreamPosition {
        StreamPosition {
            sequence: self.packetizer.next_sequence(),
            timestamp: self.timestamp,
        }
    }
}

/// Encodes a live source into a buffered stream on a TCP connection.
///
/// Two threads: one encodes what the decoder delivers, as soon as it does,
/// and queues the packets; the other writes them to the connection. A slow
/// connection therefore never holds up the encoder (nor whatever paces the
/// input).
pub struct BufferedStreamer {
    running: Arc<AtomicBool>,
    counters: Arc<BufferedCounters>,
    error: Arc<Mutex<Option<StreamFailure>>>,
    encoding: Arc<Mutex<Encoding>>,
    queue: Receiver<Vec<u8>>,
    connection: TcpStream,
    threads: Vec<JoinHandle<()>>,
}

impl BufferedStreamer {
    /// Starts streaming `decoder` to `connection` (connected to the
    /// receiver's data port). RTP timestamps start at `first_timestamp` and
    /// count frames at 44.1 kHz; packets are sealed with `key`.
    pub fn start(
        connection: TcpStream,
        mut decoder: LiveAudioDecoder,
        key: [u8; 32],
        first_timestamp: u32,
    ) -> Result<Self> {
        connection.set_nodelay(true)?;
        connection.set_write_timeout(Some(WRITE_TIMEOUT))?;
        decoder.set_recv_timeout(INPUT_POLL);
        let encoding = Arc::new(Mutex::new(Encoding {
            encoder: AacLcEncoder::new(AAC_BIT_RATE)?,
            packetizer: Packetizer::new(key, SSRC_AAC_44100_STEREO, 0),
            timestamp: first_timestamp,
            paused: false,
        }));

        let running = Arc::new(AtomicBool::new(true));
        let counters = Arc::new(BufferedCounters::default());
        let error = Arc::new(Mutex::new(None));
        let (sender, queue) = bounded::<Vec<u8>>(SEND_QUEUE_PACKETS);

        let encode = {
            let running = Arc::clone(&running);
            let counters = Arc::clone(&counters);
            let error = Arc::clone(&error);
            let encoding = Arc::clone(&encoding);
            let queue = queue.clone();
            thread::Builder::new()
                .name("ap2-encode".into())
                .spawn(move || {
                    if let Err(failure) = encode_loop(
                        &running,
                        &counters,
                        &queue,
                        &sender,
                        &mut decoder,
                        &encoding,
                    ) {
                        fail(
                            &error,
                            &running,
                            format!("buffered stream encoding failed: {failure}"),
                            None,
                        );
                    }
                })?
        };
        let write = {
            let running = Arc::clone(&running);
            let counters = Arc::clone(&counters);
            let error = Arc::clone(&error);
            let queue = queue.clone();
            let mut connection = connection.try_clone()?;
            thread::Builder::new()
                .name("ap2-send".into())
                .spawn(move || {
                    if let Err(failure) = write_loop(&running, &counters, &queue, &mut connection) {
                        fail(
                            &error,
                            &running,
                            format!("buffered stream connection failed: {failure}"),
                            Some(failure.kind()),
                        );
                    }
                })?
        };
        Ok(Self {
            running,
            counters,
            error,
            encoding,
            queue,
            connection,
            threads: vec![encode, write],
        })
    }

    fn encoding(&self) -> std::sync::MutexGuard<'_, Encoding> {
        self.encoding
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The next packet's sequence number and RTP timestamp.
    pub fn position(&self) -> StreamPosition {
        self.encoding().position()
    }

    /// Whether the stream is paused.
    pub fn is_paused(&self) -> bool {
        self.encoding().paused
    }

    /// Stops making packets: the input that arrives while paused is dropped
    /// (a live source plays on). Returns where the stream stopped: no packet
    /// from that position on is made before [`Self::resume`]. Packets
    /// already queued are still sent.
    pub fn pause(&self) -> StreamPosition {
        let mut encoding = self.encoding();
        encoding.paused = true;
        encoding.position()
    }

    /// Makes packets again, from the input that arrives from now on, with a
    /// fresh encoder (the audio before the pause is not continued).
    /// Sequence numbers and RTP timestamps carry on where they stopped;
    /// returns that position.
    pub fn resume(&self) -> Result<StreamPosition> {
        let mut encoding = self.encoding();
        if encoding.paused {
            encoding.encoder = AacLcEncoder::new(AAC_BIT_RATE)?;
            encoding.paused = false;
        }
        Ok(encoding.position())
    }

    /// Why the stream stopped, if it failed.
    pub fn error(&self) -> Option<StreamFailure> {
        self.error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Whether both threads still run.
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// The counters so far.
    pub fn stats(&self) -> BufferedStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        BufferedStats {
            packets: load(&self.counters.packets),
            sent: load(&self.counters.sent),
            bytes: load(&self.counters.bytes),
            dropped: load(&self.counters.dropped),
            queued: self.queue.len(),
            longest_write_us: load(&self.counters.longest_write_us),
        }
    }

    /// Stops both threads and closes the connection.
    pub fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        // Unblocks a write stuck on a stalled connection.
        let _ = self.connection.shutdown(Shutdown::Both);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Drop for BufferedStreamer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn fail(
    error: &Mutex<Option<StreamFailure>>,
    running: &AtomicBool,
    message: String,
    kind: Option<io::ErrorKind>,
) {
    if running.swap(false, Ordering::AcqRel) {
        tracing::warn!(error = %message, "AirPlay buffered stream stopped");
    }
    error
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert(StreamFailure { message, kind });
}

fn encode_loop(
    running: &AtomicBool,
    counters: &BufferedCounters,
    queue: &Receiver<Vec<u8>>,
    sender: &Sender<Vec<u8>>,
    decoder: &mut LiveAudioDecoder,
    encoding: &Mutex<Encoding>,
) -> Result<()> {
    let format = aac_format();
    while running.load(Ordering::Acquire) {
        let Some(frame) = decoder.decode_resampled(&format, FRAMES_PER_PACKET as usize)? else {
            if decoder.is_eof() {
                tracing::debug!("buffered stream: input closed");
                break;
            }
            continue;
        };
        let packet = {
            let mut guard = encoding
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let encoding = &mut *guard;
            if encoding.paused {
                continue;
            }
            let unit = encoding.encoder.encode(&frame.samples)?;
            if unit.is_empty() {
                continue;
            }
            let packet = encoding.packetizer.packet(encoding.timestamp, unit)?;
            encoding.timestamp = encoding.timestamp.wrapping_add(FRAMES_PER_PACKET);
            packet
        };
        counters.packets.fetch_add(1, Ordering::Relaxed);
        match sender.try_send(packet) {
            Ok(()) => {}
            Err(TrySendError::Full(packet)) => {
                // The connection is stalled for good: drop the oldest packet,
                // it would play too late anyway.
                let _ = queue.try_recv();
                counters.dropped.fetch_add(1, Ordering::Relaxed);
                let _ = sender.try_send(packet);
            }
            Err(TrySendError::Disconnected(_)) => break,
        }
    }
    Ok(())
}

fn write_loop(
    running: &AtomicBool,
    counters: &BufferedCounters,
    queue: &Receiver<Vec<u8>>,
    connection: &mut TcpStream,
) -> io::Result<()> {
    while running.load(Ordering::Acquire) {
        let packet = match queue.recv_timeout(Duration::from_millis(100)) {
            Ok(packet) => packet,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let started = Instant::now();
        connection.write_all(&packet)?;
        let took = started.elapsed().as_micros() as u64;
        counters.longest_write_us.fetch_max(took, Ordering::Relaxed);
        counters.sent.fetch_add(1, Ordering::Relaxed);
        counters
            .bytes
            .fetch_add(packet.len() as u64, Ordering::Relaxed);
    }
    Ok(())
}
