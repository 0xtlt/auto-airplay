//! A buffered AirPlay 2 session (stream type 103) of the vendored client, end
//! to end against a fake speaker on the loopback interface: plaintext RTSP
//! (what follows pairing), a fake PTP grandmaster, and a TCP data port that
//! decrypts and decodes every packet the way a receiver does.
//!
//! Checks what goes on the wire (SETUP payloads, SETPEERS, the anchor, the
//! packets), that the audio decodes back to what went in, that every packet
//! arrives about `latency` before it plays and that this margin does not
//! drift, and that a broken data connection or a lost clock fails the session
//! where the daemon watches it (`send_feedback`).

#[path = "support/grandmaster.rs"]
mod grandmaster;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use airplay_audio::timeline::mono_now_ns;
use airplay_client::ptp_follower::FollowerConfig;
use airplay_client::{
    Connection, Device, DeviceId, LiveAudioDecoder, LiveFrameSender, LivePcmFrame, MediaClock,
    StreamConfig,
};
use airplay_crypto::chacha::AudioCipher;
use airplay_rtsp::RtspSession;
use grandmaster::{Behaviour, FakeGrandmaster, GM_IDENTITY};
use plist::{Dictionary, Value};

const LOCALHOST: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
/// The receiver holds this much (the setting under test; 2 s by default).
const LATENCY: Duration = Duration::from_millis(1_000);
const SOURCE_RATE: u32 = 48_000;
const TONE_HZ: f64 = 1_000.0;
const TONE_AMPLITUDE: f64 = 8_000.0;
const CHUNK_FRAMES: usize = 1024;
const RTP_RATE: f64 = 44_100.0;
/// What Apple senders put in the SSRC of AAC-LC 44.1 kHz stereo packets.
const SSRC_AAC: u32 = 0x1600_0000;

#[derive(Debug, Clone)]
struct Request {
    method: String,
    uri: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    /// Grandmaster time when it arrived.
    arrived_gm_ns: i128,
    /// Grandmaster time just before the reply was written.
    replied_gm_ns: i128,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn plist(&self) -> Value {
        Value::from_reader(std::io::Cursor::new(&self.body)).expect("a plist body")
    }

    fn dictionary(&self) -> Dictionary {
        self.plist().into_dictionary().expect("a plist dictionary")
    }
}

#[derive(Debug, Clone)]
struct Packet {
    arrived_ns: u64,
    first_byte: u8,
    sequence: u32,
    timestamp: u32,
    ssrc: u32,
    payload: Vec<u8>,
}

#[derive(Default)]
struct Seen {
    requests: Vec<Request>,
    packets: Vec<Packet>,
    data_error: Option<String>,
}

#[derive(Clone, Copy, Default)]
struct Options {
    /// Answer the buffered stream's SETUP with 415.
    refuse_buffered: bool,
}

/// A HomePod, as far as a buffered session goes, minus pairing.
struct FakeSpeaker {
    rtsp_addr: SocketAddr,
    seen: Arc<Mutex<Seen>>,
    gm: Arc<FakeGrandmaster>,
    data: Arc<Mutex<Option<TcpStream>>>,
    running: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl FakeSpeaker {
    fn start(options: Options) -> Self {
        let gm = Arc::new(FakeGrandmaster::start(Behaviour::default()));
        let listener = TcpListener::bind((LOCALHOST, 0)).unwrap();
        let rtsp_addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let data = Arc::new(Mutex::new(None));
        let running = Arc::new(AtomicBool::new(true));
        let rtsp = {
            let seen = Arc::clone(&seen);
            let gm = Arc::clone(&gm);
            let data = Arc::clone(&data);
            let running = Arc::clone(&running);
            thread::spawn(move || serve_rtsp(&listener, &seen, &gm, &data, &running, options))
        };
        Self {
            rtsp_addr,
            seen,
            gm,
            data,
            running,
            threads: vec![rtsp],
        }
    }

    fn device(&self) -> Device {
        fake_device(self.rtsp_addr)
    }

    /// A follower configuration that reaches the fake grandmaster.
    fn ptp_config(&self) -> FollowerConfig {
        self.gm.follower_config()
    }

    fn requests(&self) -> Vec<Request> {
        self.seen.lock().unwrap().requests.clone()
    }

    fn packets(&self) -> Vec<Packet> {
        self.seen.lock().unwrap().packets.clone()
    }

    /// Closes the data connection, as a receiver that drops the stream.
    fn drop_data_connection(&self) {
        if let Some(stream) = self.data.lock().unwrap().take() {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }
}

impl Drop for FakeSpeaker {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.drop_data_connection();
        // The RTSP thread ends when the client closes its connection.
        for thread in self.threads.drain(..) {
            if thread.is_finished() {
                let _ = thread.join();
            }
        }
    }
}

fn fake_device(addr: SocketAddr) -> Device {
    Device {
        id: DeviceId::from_mac_string("AA:BB:CC:DD:EE:FF").unwrap(),
        name: "Salon".into(),
        addresses: vec![addr.ip()],
        port: addr.port(),
        model: "AudioAccessory5,1".into(),
        manufacturer: None,
        serial_number: None,
        features: Default::default(),
        required_sender_features: None,
        public_key: None,
        source_version: Default::default(),
        firmware_version: None,
        os_version: None,
        protocol_version: None,
        requires_password: false,
        status_flags: 0,
        access_control: None,
        pairing_identity: None,
        system_pairing_identity: None,
        bluetooth_address: None,
        homekit_home_id: None,
        group_id: None,
        is_group_leader: false,
        group_public_name: None,
        group_contains_discoverable_leader: false,
        home_group_id: None,
        household_id: None,
        parent_group_id: None,
        parent_group_contains_discoverable_leader: false,
        tight_sync_id: None,
        raop_port: None,
        raop_encryption_types: None,
        raop_codecs: None,
        raop_transport: None,
        raop_metadata_types: None,
        raop_digest_auth: false,
        vodka_version: None,
    }
}

/// The next request (times left at 0), `None` once the client is gone.
fn read_request(reader: &mut impl BufRead) -> Option<Request> {
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_string();
    let uri = parts.next()?.to_string();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once(':')?;
        headers.push((key.trim().to_string(), value.trim().to_string()));
    }
    let length = headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case("Content-Length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    Some(Request {
        method,
        uri,
        headers,
        body,
        arrived_gm_ns: 0,
        replied_gm_ns: 0,
    })
}

fn plist_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    value.to_writer_binary(&mut out).unwrap();
    out
}

fn dictionary(entries: Vec<(&str, Value)>) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

fn serve_rtsp(
    listener: &TcpListener,
    seen: &Arc<Mutex<Seen>>,
    gm: &FakeGrandmaster,
    data: &Arc<Mutex<Option<TcpStream>>>,
    running: &AtomicBool,
    options: Options,
) {
    let Ok((stream, _)) = listener.accept() else {
        return;
    };
    let events = TcpListener::bind((LOCALHOST, 0)).unwrap();
    let event_port = events.local_addr().unwrap().port();
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    let mut data_threads = Vec::new();

    while running.load(Ordering::Acquire) {
        let Some(mut request) = read_request(&mut reader) else {
            break;
        };
        request.arrived_gm_ns = gm.now_ns();
        let mut status = (200, "OK");
        let mut session = false;
        let mut reply: Option<Value> = None;
        match request.method.as_str() {
            "GET" => {
                reply = Some(dictionary(vec![("name", Value::String("Salon".into()))]));
            }
            "SETUP" => {
                let setup = request.dictionary();
                if setup.contains_key("timingProtocol") {
                    session = true;
                    reply = Some(dictionary(vec![
                        ("eventPort", Value::Integer(u64::from(event_port).into())),
                        ("timingPort", Value::Integer(0u64.into())),
                        (
                            "timingPeerInfo",
                            dictionary(vec![
                                (
                                    "Addresses",
                                    Value::Array(vec![Value::String("127.0.0.1".into())]),
                                ),
                                ("ID", Value::String("fake-speaker".into())),
                            ]),
                        ),
                    ]));
                } else if options.refuse_buffered {
                    status = (415, "Unsupported Media Type");
                } else {
                    let stream = setup["streams"].as_array().unwrap()[0]
                        .as_dictionary()
                        .unwrap();
                    let key: [u8; 32] = stream["shk"].as_data().unwrap().try_into().unwrap();
                    let listener = TcpListener::bind((LOCALHOST, 0)).unwrap();
                    let data_port = listener.local_addr().unwrap().port();
                    let slot = Arc::clone(data);
                    let seen = Arc::clone(seen);
                    data_threads.push(thread::spawn(move || {
                        receive_data(&listener, key, &seen, &slot)
                    }));
                    // No controlPort: a buffered stream has no use for one.
                    reply = Some(dictionary(vec![(
                        "streams",
                        Value::Array(vec![dictionary(vec![
                            ("type", Value::Integer(103u64.into())),
                            ("dataPort", Value::Integer(u64::from(data_port).into())),
                            ("audioBufferSize", Value::Integer(8_388_608u64.into())),
                            ("streamID", Value::Integer(1u64.into())),
                        ])]),
                    )]));
                }
            }
            _ => {}
        }
        let cseq = request.header("CSeq").unwrap_or("0").to_string();
        let teardown_session = request.method == "TEARDOWN" && request.body.is_empty();
        request.replied_gm_ns = gm.now_ns();
        seen.lock().unwrap().requests.push(request);

        let body = reply.as_ref().map(plist_bytes).unwrap_or_default();
        let mut response = format!(
            "RTSP/1.0 {} {}\r\nCSeq: {cseq}\r\nServer: AirTunes/870.14.1\r\n",
            status.0, status.1
        );
        if session {
            response.push_str("Session: 1\r\n");
        }
        if !body.is_empty() {
            response.push_str("Content-Type: application/x-apple-binary-plist\r\n");
        }
        response.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        let mut bytes = response.into_bytes();
        bytes.extend_from_slice(&body);
        if writer.write_all(&bytes).is_err() || teardown_session {
            break;
        }
    }
    drop(events);
    if let Some(stream) = data.lock().unwrap().take() {
        let _ = stream.shutdown(Shutdown::Both);
    }
    for thread in data_threads {
        let _ = thread.join();
    }
}

/// Reads, checks and decrypts every packet of the buffered stream.
fn receive_data(
    listener: &TcpListener,
    key: [u8; 32],
    seen: &Mutex<Seen>,
    slot: &Mutex<Option<TcpStream>>,
) {
    let Ok((mut stream, _)) = listener.accept() else {
        return;
    };
    *slot.lock().unwrap() = Some(stream.try_clone().unwrap());
    let cipher = AudioCipher::new(key);
    loop {
        let mut length = [0u8; 2];
        if stream.read_exact(&mut length).is_err() {
            return;
        }
        let length = usize::from(u16::from_be_bytes(length));
        let mut rest = vec![0u8; length - 2];
        if stream.read_exact(&mut rest).is_err() {
            return;
        }
        let arrived_ns = mono_now_ns();
        let sequence = u32::from_be_bytes([0, rest[1], rest[2], rest[3]]);
        let timestamp = u32::from_be_bytes(rest[4..8].try_into().unwrap());
        let ssrc = u32::from_be_bytes(rest[8..12].try_into().unwrap());
        let nonce: [u8; 8] = rest[rest.len() - 8..].try_into().unwrap();
        let tag: [u8; 16] = rest[rest.len() - 24..rest.len() - 8].try_into().unwrap();
        let ciphertext = &rest[12..rest.len() - 24];
        match cipher.decrypt(ciphertext, &nonce, &tag, timestamp, ssrc) {
            Ok(payload) => seen.lock().unwrap().packets.push(Packet {
                arrived_ns,
                first_byte: rest[0],
                sequence,
                timestamp,
                ssrc,
                payload,
            }),
            Err(error) => {
                seen.lock().unwrap().data_error = Some(format!("packet {sequence}: {error}"));
                return;
            }
        }
    }
}

/// A 1 kHz tone at 48 kHz (a Bluetooth source's rate), handed over in
/// 1024-frame chunks at the pace of the speaker's clock, as the daemon's
/// playout does.
struct Feeder {
    running: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// When each chunk was handed over, on the feeder's clock.
    handoffs: Arc<Mutex<Vec<u64>>>,
    /// Chunks the live queue refused (it was full).
    refused: Arc<AtomicUsize>,
}

impl Feeder {
    fn start(sender: LiveFrameSender, clock: MediaClock) -> Self {
        let running = Arc::new(AtomicBool::new(true));
        let handoffs = Arc::new(Mutex::new(Vec::new()));
        let refused = Arc::new(AtomicUsize::new(0));
        let thread = {
            let running = Arc::clone(&running);
            let handoffs = Arc::clone(&handoffs);
            let refused = Arc::clone(&refused);
            thread::spawn(move || {
                let start = clock.now_ns();
                let mut frame = 0u64;
                while running.load(Ordering::Acquire) {
                    let due = start + frame * 1_000_000_000 / u64::from(SOURCE_RATE);
                    if clock.now_ns() < due {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    let mut samples = Vec::with_capacity(CHUNK_FRAMES * 2);
                    for index in 0..CHUNK_FRAMES as u64 {
                        let t = (frame + index) as f64 / f64::from(SOURCE_RATE);
                        let value = (TONE_AMPLITUDE
                            * (2.0 * std::f64::consts::PI * TONE_HZ * t).sin())
                            as i16;
                        samples.extend_from_slice(&[value, value]);
                    }
                    let handed = clock.now_ns();
                    let accepted = sender.try_send(LivePcmFrame {
                        samples,
                        channels: 2,
                        sample_rate: SOURCE_RATE,
                    });
                    if !accepted {
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                    handoffs.lock().unwrap().push(handed);
                    frame += CHUNK_FRAMES as u64;
                }
            })
        };
        Self {
            running,
            thread: Some(thread),
            handoffs,
            refused,
        }
    }

    /// When each chunk was handed over; panics if any was refused (the
    /// times would not match the stream).
    fn handoffs(&self) -> Vec<u64> {
        assert_eq!(
            self.refused.load(Ordering::Relaxed),
            0,
            "the live queue overflowed"
        );
        self.handoffs.lock().unwrap().clone()
    }

    fn stop(&mut self) {
        self.running.store(false, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Connects, sets up a buffered session and starts streaming the tone.
async fn start_session(speaker: &FakeSpeaker, ptp: FollowerConfig) -> (Connection, Feeder) {
    let mut connection =
        Connection::connect_unpaired(speaker.device(), StreamConfig::airplay2_buffered())
            .await
            .unwrap();
    connection.set_ptp_config(ptp);
    connection.set_buffered_latency(LATENCY);
    connection.setup().await.unwrap();
    let clock = connection
        .media_clock()
        .expect("the speaker's clock after SETUP");
    let (sender, decoder) = LiveAudioDecoder::create_pair(SOURCE_RATE, 2, 64);
    let feeder = Feeder::start(sender, clock);
    connection.start_streaming_live(decoder).await.unwrap();
    (connection, feeder)
}

/// A raw AAC-LC access unit with an ADTS header (44.1 kHz, stereo), the way
/// receivers hand them to a decoder.
fn adts(unit: &[u8]) -> Vec<u8> {
    let length = unit.len() + 7;
    let mut frame = vec![
        0xFF,
        0xF1,
        (1 << 6) | (4 << 2),
        (2 << 6) | ((length >> 11) as u8 & 0x03),
        ((length >> 3) & 0xFF) as u8,
        (((length & 0x07) << 5) as u8) | 0x1F,
        0xFC,
    ];
    frame.extend_from_slice(unit);
    frame
}

fn decode(packets: &[Packet]) -> Vec<i16> {
    let mut decoder = fdk_aac::dec::Decoder::new(fdk_aac::dec::Transport::Adts);
    let mut pcm = Vec::new();
    let mut frame = vec![0i16; 2048 * 2];
    for packet in packets {
        let data = adts(&packet.payload);
        assert_eq!(decoder.fill(&data).unwrap(), data.len());
        decoder
            .decode_frame(&mut frame)
            .expect("a decodable AAC-LC access unit");
        let size = decoder.decoded_frame_size();
        assert_eq!(size, 2048, "1024 stereo frames per packet");
        pcm.extend_from_slice(&frame[..size]);
    }
    pcm
}

fn network_time_ns(anchor: &Dictionary) -> i128 {
    let secs = anchor["networkTimeSecs"].as_unsigned_integer().unwrap();
    let frac = anchor["networkTimeFrac"].as_unsigned_integer().unwrap();
    i128::from(secs) * 1_000_000_000 + ((u128::from(frac) * 1_000_000_000) >> 64) as i128
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_a_buffered_session_to_a_fake_speaker() {
    let speaker = FakeSpeaker::start(Options::default());
    let (mut connection, mut feeder) = start_session(&speaker, speaker.ptp_config()).await;

    // The clock the audio is paced on is the speaker's.
    let clock = connection.media_clock().unwrap();
    let error_ms = speaker.gm.error_ms(&clock);
    assert!(error_ms < 2.0, "media clock off by {error_ms:.3} ms");

    tokio::time::sleep(Duration::from_secs(3)).await;
    connection.send_feedback().await.expect("a healthy session");
    let stats = connection.buffered_stats().expect("buffered stream stats");
    feeder.stop();
    assert!(!feeder.handoffs().is_empty(), "the tone was handed over");
    connection.stop().await.unwrap();
    connection.disconnect().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The RTSP exchange, in order.
    let requests = speaker.requests();
    let methods: Vec<&str> = requests
        .iter()
        .map(|request| request.method.as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "GET",
            "SETUP",
            "SETPEERS",
            "SETUP",
            "RECORD",
            "SETRATEANCHORTIME",
            "POST",
            "TEARDOWN",
            "TEARDOWN"
        ],
        "{requests:#?}"
    );
    let session_uri = &requests[1].uri;
    assert!(
        session_uri.starts_with("rtsp://127.0.0.1/"),
        "{session_uri}"
    );

    // SETUP of the timing channel: PTP, with us as a timing peer.
    let timing = requests[1].dictionary();
    assert_eq!(timing["timingProtocol"].as_string(), Some("PTP"));
    let peer = timing["timingPeerInfo"].as_dictionary().unwrap();
    let ours = peer["Addresses"].as_array().unwrap();
    assert!(
        ours.contains(&Value::String("127.0.0.1".into())),
        "{peer:?}"
    );
    assert!(timing.contains_key("timingPeerList"));

    // SETPEERS as Apple senders send it: the speaker and us.
    let setpeers = &requests[2];
    assert_eq!(&setpeers.uri, session_uri);
    assert_eq!(setpeers.header("Content-Type"), Some("/peer-list-changed"));
    let peers = setpeers.plist().into_array().unwrap();
    assert_eq!(
        peers,
        vec![Value::String("127.0.0.1".into())],
        "speaker and us share an address here"
    );

    // SETUP of the stream: buffered AAC-LC 44.1 kHz stereo, 1024 frames.
    let setup = requests[3].dictionary();
    let stream = setup["streams"].as_array().unwrap()[0]
        .as_dictionary()
        .unwrap();
    assert_eq!(stream["type"].as_unsigned_integer(), Some(103));
    assert_eq!(stream["ct"].as_unsigned_integer(), Some(4), "AAC-LC");
    assert_eq!(stream["audioFormat"].as_unsigned_integer(), Some(0x40_0000));
    assert_eq!(stream["spf"].as_unsigned_integer(), Some(1024));
    assert_eq!(stream["sr"].as_unsigned_integer(), Some(44_100));
    assert_eq!(stream["shk"].as_data().map(<[u8]>::len), Some(32));
    assert!(!stream.contains_key("asc"), "Apple senders send no asc");

    // The anchor: frame 0 plays LATENCY after the client took it, on the
    // grandmaster's timeline.
    let anchor_request = &requests[5];
    assert_eq!(
        anchor_request.header("Content-Type"),
        Some("application/x-apple-binary-plist")
    );
    let anchor = anchor_request.dictionary();
    assert_eq!(anchor["rate"].as_unsigned_integer(), Some(1));
    assert_eq!(anchor["rtpTime"].as_unsigned_integer(), Some(0));
    assert_eq!(anchor["networkTimeFlags"].as_unsigned_integer(), Some(0));
    assert_eq!(
        anchor["networkTimeTimelineID"].as_unsigned_integer(),
        Some(u64::from_be_bytes(GM_IDENTITY))
    );
    let plays_at = network_time_ns(&anchor);
    let latency_ms = LATENCY.as_millis() as f64;
    // The client took it once RECORD was answered and before its request
    // arrived, on its clock (within 2 ms of the grandmaster's): true however
    // the test is scheduled.
    let taken = plays_at - LATENCY.as_nanos() as i128;
    let record_answered = requests[4].replied_gm_ns;
    let anchor_arrived = anchor_request.arrived_gm_ns;
    let slack = 2_000_000;
    assert!(
        (record_answered - slack..=anchor_arrived + slack).contains(&taken),
        "anchor taken {:.1} ms after RECORD was answered, {:.1} ms before it arrived",
        (taken - record_answered) as f64 / 1e6,
        (anchor_arrived - taken) as f64 / 1e6
    );

    assert!(requests[6].uri.ends_with("/feedback"));
    let teardown = requests[7].dictionary();
    let removed = teardown["streams"].as_array().unwrap()[0]
        .as_dictionary()
        .unwrap();
    assert_eq!(removed["type"].as_unsigned_integer(), Some(103));
    assert!(requests[8].body.is_empty());

    // The packets: framing, numbering, format tag, encryption.
    assert!(speaker.seen.lock().unwrap().data_error.is_none());
    let packets = speaker.packets();
    assert!(packets.len() >= 100, "{} packets in 3 s", packets.len());
    for (index, packet) in packets.iter().enumerate() {
        assert_eq!(packet.first_byte, 0x80);
        assert_eq!(packet.sequence, index as u32);
        assert_eq!(packet.timestamp, 1024 * index as u32);
        assert_eq!(packet.ssrc, SSRC_AAC);
    }
    assert_eq!(stats.dropped, 0);

    // Packets arrive about LATENCY before they play, and that margin does
    // not drift (the audio is paced on the speaker's clock). Medians: a
    // machine that stalls now and then delays a few packets, not the bulk.
    let margins_ms: Vec<f64> = packets
        .iter()
        .map(|packet| {
            let plays = plays_at + (f64::from(packet.timestamp) / RTP_RATE * 1e9) as i128;
            (plays - speaker.gm.master_ns(packet.arrived_ns)) as f64 / 1e6
        })
        .collect();
    let median = |values: &[f64]| {
        let mut sorted = values.to_vec();
        sorted.sort_by(f64::total_cmp);
        sorted[sorted.len() / 2]
    };
    let (lowest, typical, highest) = (
        margins_ms.iter().cloned().fold(f64::MAX, f64::min),
        median(&margins_ms),
        margins_ms.iter().cloned().fold(f64::MIN, f64::max),
    );
    eprintln!(
        "margin before play: {lowest:.1} / {typical:.1} / {highest:.1} ms (min / median / max, latency {latency_ms} ms, {} packets)",
        packets.len()
    );
    assert!(
        (typical - latency_ms).abs() < 100.0,
        "packets arrive {typical:.1} ms before they play"
    );
    let third = margins_ms.len() / 3;
    let early = median(&margins_ms[..third]);
    let late = median(&margins_ms[margins_ms.len() - third..]);
    assert!(
        (late - early).abs() < 15.0,
        "margin drifted from {early:.1} to {late:.1} ms"
    );

    // The audio decodes back to the tone, resampled to 44.1 kHz.
    let pcm = decode(&packets);
    let left: Vec<f64> = pcm
        .iter()
        .step_by(2)
        .skip(8_192)
        .map(|&sample| f64::from(sample))
        .collect();
    let rms = (left.iter().map(|sample| sample * sample).sum::<f64>() / left.len() as f64).sqrt();
    let expected_rms = TONE_AMPLITUDE / 2f64.sqrt();
    assert!(
        (rms / expected_rms - 1.0).abs() < 0.1,
        "rms {rms:.0}, expected {expected_rms:.0}"
    );
    let crossings = left
        .windows(2)
        .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
        .count();
    let frequency = crossings as f64 / (left.len() as f64 / RTP_RATE);
    assert!(
        (frequency / TONE_HZ - 1.0).abs() < 0.01,
        "decoded a {frequency:.1} Hz tone"
    );
}

/// The frequency of the tone in `pcm` (interleaved stereo at 44.1 kHz),
/// from its rising zero crossings on the left channel, and its RMS level.
fn tone_of(pcm: &[i16]) -> (f64, f64) {
    let left: Vec<f64> = pcm
        .iter()
        .step_by(2)
        .map(|&sample| f64::from(sample))
        .collect();
    let rms = (left.iter().map(|sample| sample * sample).sum::<f64>() / left.len() as f64).sqrt();
    let crossings = left
        .windows(2)
        .filter(|pair| pair[0] < 0.0 && pair[1] >= 0.0)
        .count();
    (crossings as f64 / (left.len() as f64 / RTP_RATE), rms)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pauses_and_resumes_a_buffered_stream() {
    let speaker = FakeSpeaker::start(Options::default());
    let (mut connection, mut feeder) = start_session(&speaker, speaker.ptp_config()).await;
    tokio::time::sleep(Duration::from_millis(1_000)).await;

    connection.pause().await.expect("pause");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let resumed_at = mono_now_ns();
    connection.resume().await.expect("resume");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    connection
        .send_feedback()
        .await
        .expect("healthy after the resume");
    feeder.stop();
    connection.stop().await.unwrap();
    connection.disconnect().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let requests = speaker.requests();
    let methods: Vec<&str> = requests
        .iter()
        .map(|request| request.method.as_str())
        .collect();
    assert_eq!(
        methods,
        [
            "GET",
            "SETUP",
            "SETPEERS",
            "SETUP",
            "RECORD",
            "SETRATEANCHORTIME",
            "SETRATEANCHORTIME",
            "FLUSHBUFFERED",
            "SETRATEANCHORTIME",
            "POST",
            "TEARDOWN",
            "TEARDOWN"
        ],
        "pause and resume must not use FLUSH or RECORD"
    );

    // Pause: rate 0 alone (the anchor is kept), then a flush of everything
    // the speaker holds, up to the first packet after the pause.
    let pause = requests[6].dictionary();
    assert_eq!(pause.len(), 1, "{pause:?}");
    assert_eq!(pause["rate"].as_unsigned_integer(), Some(0));
    let flush = requests[7].dictionary();
    assert_eq!(flush.len(), 2, "flush from now: {flush:?}");
    let until_seq = flush["flushUntilSeq"].as_unsigned_integer().unwrap();
    let until_ts = flush["flushUntilTS"].as_unsigned_integer().unwrap();
    assert!(until_seq > 30, "paused after {until_seq} packets");
    assert_eq!(until_ts, until_seq * 1024);

    // Resume: a new anchor for that first packet, LATENCY after the client
    // took it (between the flush's answer and the anchor's arrival).
    let resume_request = &requests[8];
    let resume = resume_request.dictionary();
    assert_eq!(resume["rate"].as_unsigned_integer(), Some(1));
    assert_eq!(resume["rtpTime"].as_unsigned_integer(), Some(until_ts));
    assert_eq!(
        resume["networkTimeTimelineID"].as_unsigned_integer(),
        Some(u64::from_be_bytes(GM_IDENTITY))
    );
    let plays_at = network_time_ns(&resume);
    let taken = plays_at - LATENCY.as_nanos() as i128;
    let slack = 2_000_000;
    assert!(
        (requests[7].replied_gm_ns - slack..=resume_request.arrived_gm_ns + slack).contains(&taken),
        "resume anchor taken outside the resume"
    );

    // The packets: one numbering across the pause, nothing made during it.
    assert!(speaker.seen.lock().unwrap().data_error.is_none());
    let packets = speaker.packets();
    for (index, packet) in packets.iter().enumerate() {
        assert_eq!(packet.sequence, index as u32);
        assert_eq!(packet.timestamp, 1024 * index as u32);
    }
    let (before, after) = packets.split_at(until_seq as usize);
    assert!(after.len() > 40, "{} packets after the resume", after.len());
    assert!(
        after.iter().all(|packet| packet.arrived_ns >= resumed_at),
        "a packet from after the pause was sent before the resume"
    );

    // After the resume: the tone again (fresh encoder, so skip its priming),
    // about LATENCY before it plays.
    let decoded = decode(&packets);
    let resumed_pcm = &decoded[(before.len() + 4) * 2048..];
    let (frequency, rms) = tone_of(resumed_pcm);
    assert!(
        (frequency / TONE_HZ - 1.0).abs() < 0.01,
        "{frequency:.1} Hz"
    );
    let expected_rms = TONE_AMPLITUDE / 2f64.sqrt();
    assert!((rms / expected_rms - 1.0).abs() < 0.1, "rms {rms:.0}");
    let mut margins_ms: Vec<f64> = after
        .iter()
        .map(|packet| {
            let frames = f64::from(packet.timestamp) - until_ts as f64;
            let plays = plays_at + (frames / RTP_RATE * 1e9) as i128;
            (plays - speaker.gm.master_ns(packet.arrived_ns)) as f64 / 1e6
        })
        .collect();
    margins_ms.sort_by(f64::total_cmp);
    let typical = margins_ms[margins_ms.len() / 2];
    let latency_ms = LATENCY.as_millis() as f64;
    assert!(
        (typical - latency_ms).abs() < 100.0,
        "after the resume, packets arrive {typical:.1} ms before they play"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_data_connection_fails_the_session() {
    let speaker = FakeSpeaker::start(Options::default());
    let (mut connection, _feeder) = start_session(&speaker, speaker.ptp_config()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    connection.send_feedback().await.expect("healthy before");

    speaker.drop_data_connection();
    let mut failure = None;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Err(error) = connection.send_feedback().await {
            failure = Some(error);
            break;
        }
    }
    let failure = failure.expect("the session must fail once the speaker drops the stream");
    let airplay_client::Error::Connection(io) = &failure else {
        panic!("not a connection error: {failure}");
    };
    assert!(
        matches!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
        ),
        "{io:?}"
    );
    let _ = connection.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lost_clock_fails_the_session() {
    let speaker = FakeSpeaker::start(Options::default());
    let ptp = FollowerConfig {
        silence_limit: Duration::from_millis(500),
        ..speaker.ptp_config()
    };
    let (mut connection, _feeder) = start_session(&speaker, ptp).await;
    connection.send_feedback().await.expect("healthy before");

    speaker.gm.change(|behaviour| behaviour.silent = true);
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    let failure = connection
        .send_feedback()
        .await
        .expect_err("the clock is gone");
    assert!(failure.to_string().contains("PTP"), "{failure}");
    let _ = connection.disconnect().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_buffered_stream_fails_setup_clearly() {
    let speaker = FakeSpeaker::start(Options {
        refuse_buffered: true,
    });
    let mut connection =
        Connection::connect_unpaired(speaker.device(), StreamConfig::airplay2_buffered())
            .await
            .unwrap();
    connection.set_ptp_config(speaker.ptp_config());
    let failure = connection.setup().await.expect_err("the speaker refused");
    assert!(
        failure
            .to_string()
            .contains("SETUP (buffered stream) returned status 415"),
        "{failure}"
    );
    let _ = connection.disconnect().await;
}

#[test]
fn anchors_encode_network_time_and_timeline() {
    assert_eq!(
        airplay_rtsp::split_network_time(1_500_000_000),
        (1, 1 << 63)
    );
    assert_eq!(airplay_rtsp::split_network_time(7_000_000_000), (7, 0));

    let session = RtspSession::new(
        fake_device("127.0.0.1:7000".parse().unwrap()),
        StreamConfig::airplay2_buffered(),
    );
    // A timeline ID with the top bit set must survive the plist encoding.
    let timeline = 0xF414_7DE8_5498_0008u64;
    let body = session
        .build_rate_anchor(0xFFFF_FFF0, 987_654_321_250_000_000, timeline, 1)
        .unwrap();
    let anchor = Value::from_reader(std::io::Cursor::new(body))
        .unwrap()
        .into_dictionary()
        .unwrap();
    assert_eq!(
        anchor["networkTimeTimelineID"].as_unsigned_integer(),
        Some(timeline)
    );
    assert_eq!(
        anchor["networkTimeSecs"].as_unsigned_integer(),
        Some(987_654_321)
    );
    assert_eq!(
        anchor["networkTimeFrac"].as_unsigned_integer(),
        Some(1 << 62)
    );
    assert_eq!(anchor["rtpTime"].as_unsigned_integer(), Some(0xFFFF_FFF0));
    assert_eq!(anchor["rate"].as_unsigned_integer(), Some(1));
}

// ----- latency, against realtime mode -----

/// What realtime mode (the default) gives a chunk from handoff to the
/// AirPlay client to its announced play time, in ms: the vendored realtime
/// sender as the daemon drives it (500 ms prefill, render delay 250 ms, NTP
/// timing), fed the same paced tone, against a loopback receiver that reads
/// the sync packets.
async fn realtime_handoff_to_play(run: Duration) -> Vec<f64> {
    use airplay_audio::timeline::wall_minus_mono_ns;
    use airplay_audio::{AudioStreamer, RtpSender};
    use airplay_client::ClockOffset;

    let data = std::net::UdpSocket::bind((LOCALHOST, 0)).unwrap();
    let control = std::net::UdpSocket::bind((LOCALHOST, 0)).unwrap();
    control
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    data.set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let (data_addr, control_addr) = (data.local_addr().unwrap(), control.local_addr().unwrap());
    let running = Arc::new(AtomicBool::new(true));
    let anchors = Arc::new(Mutex::new(Vec::<(u32, i128)>::new()));
    let listeners = [(data, false), (control, true)].map(|(socket, is_control)| {
        let (running, anchors) = (Arc::clone(&running), Arc::clone(&anchors));
        thread::spawn(move || {
            let mut buffer = [0u8; 2048];
            while running.load(Ordering::Acquire) {
                let Ok((length, _)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                // NTP sync: [8..16] NTP time, [16..20] the RTP timestamp
                // that plays then.
                if is_control && length >= 20 && buffer[1] & 0x7F == 84 {
                    let word =
                        |at: usize| u32::from_be_bytes(buffer[at..at + 4].try_into().unwrap());
                    let unix_ns = (i128::from(word(8)) - 2_208_988_800) * 1_000_000_000
                        + ((i128::from(word(12)) * 1_000_000_000) >> 32);
                    anchors.lock().unwrap().push((word(16), unix_ns));
                }
            }
        })
    });

    let (sender, decoder) = LiveAudioDecoder::create_pair(SOURCE_RATE, 2, 64);
    let mut feeder = Feeder::start(sender, MediaClock::from_fn(mono_now_ns));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut rtp = RtpSender::new(data_addr, 0);
    rtp.set_control_dest(control_addr);
    rtp.bind(0).unwrap();
    rtp.set_control_socket(std::net::UdpSocket::bind("0.0.0.0:0").unwrap());
    let mut streamer = AudioStreamer::new(StreamConfig::default());
    streamer.set_rtp_sender(rtp).await;
    streamer.set_render_delay_ms(250).await;
    streamer.set_timing_offset(ClockOffset::default()).await;
    streamer.start_live(decoder).await.unwrap();
    tokio::time::sleep(run).await;
    feeder.stop();
    streamer.stop().await.unwrap();
    running.store(false, Ordering::Release);
    for listener in listeners {
        listener.join().unwrap();
    }

    let (rtp, plays_unix) = *anchors.lock().unwrap().last().expect("sync packets");
    let wall_minus_mono = wall_minus_mono_ns();
    let handoffs = feeder.handoffs();
    let skip = handoffs.len() / 3;
    handoffs[skip..handoffs.len() - 20]
        .iter()
        .enumerate()
        .map(|(index, &handed)| {
            let frame = ((skip + index) * CHUNK_FRAMES) as f64 * RTP_RATE / f64::from(SOURCE_RATE);
            let plays = plays_unix + ((frame - f64::from(rtp)) / RTP_RATE * 1e9) as i128;
            (plays - (i128::from(handed) + wall_minus_mono)) as f64 / 1e6
        })
        .collect()
}

/// The same for buffered mode with `buffer` held by the speaker: the fake
/// speaker's anchor gives the play times, on the grandmaster's clock (the
/// feeder's too). AAC delays the audio by 2048 frames in the stream.
async fn buffered_handoff_to_play(buffer: Duration, run: Duration) -> Vec<f64> {
    let speaker = FakeSpeaker::start(Options::default());
    let mut connection =
        Connection::connect_unpaired(speaker.device(), StreamConfig::airplay2_buffered())
            .await
            .unwrap();
    connection.set_ptp_config(speaker.ptp_config());
    connection.set_buffered_latency(buffer);
    connection.setup().await.unwrap();
    let (sender, decoder) = LiveAudioDecoder::create_pair(SOURCE_RATE, 2, 64);
    let mut feeder = Feeder::start(sender, connection.media_clock().unwrap());
    connection.start_streaming_live(decoder).await.unwrap();
    tokio::time::sleep(run).await;
    feeder.stop();
    let _ = connection.disconnect().await;

    let anchor = speaker
        .requests()
        .into_iter()
        .find(|request| request.method == "SETRATEANCHORTIME")
        .unwrap()
        .dictionary();
    let plays_at = network_time_ns(&anchor);
    let handoffs = feeder.handoffs();
    let skip = handoffs.len() / 3;
    handoffs[skip..handoffs.len() - 20]
        .iter()
        .enumerate()
        .map(|(index, &handed)| {
            let frame = ((skip + index) * CHUNK_FRAMES) as f64 * RTP_RATE / f64::from(SOURCE_RATE)
                + f64::from(airplay_audio::buffered::AAC_ENCODER_DELAY_FRAMES);
            let plays = plays_at + (frame / RTP_RATE * 1e9) as i128;
            (plays - i128::from(handed)) as f64 / 1e6
        })
        .collect()
}

fn summary(values: &[f64]) -> (f64, f64, f64) {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    (
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1],
    )
}

/// Prints how much later than realtime mode buffered mode plays the same
/// audio. The jitter buffer before the handoff (400 ms by default) and the
/// pacer's lead (up to 50 ms) come on top of both, and the speaker's own
/// output latency too.
///
/// ```text
/// cargo test --test buffered_session -- --ignored --nocapture
/// ```
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "timed comparison with realtime mode; run with --ignored --nocapture"]
async fn compare_latency_with_realtime() {
    let run = Duration::from_secs(8);
    let realtime = summary(&realtime_handoff_to_play(run).await);
    let buffered = summary(&buffered_handoff_to_play(Duration::from_millis(2_000), run).await);
    eprintln!("handoff to the AirPlay client -> announced play time (min / median / max):");
    eprintln!(
        "  realtime (render_delay_ms 250): {:.0} / {:.0} / {:.0} ms",
        realtime.0, realtime.1, realtime.2
    );
    eprintln!(
        "  buffered (speaker_buffer_ms 2000): {:.0} / {:.0} / {:.0} ms",
        buffered.0, buffered.1, buffered.2
    );
    eprintln!(
        "  added by buffered mode: {:.0} ms (medians)",
        buffered.1 - realtime.1
    );
    assert!(buffered.1 > realtime.1);
}
