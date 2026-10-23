//! Buffered streams, AirPlay 2 type 103 (local addition, see VENDORED.md).
//!
//! What an Apple TV does with a HomePod, as far as a sender goes:
//!
//! 1. SETUP the timing channel with PTP, naming our address as a timing peer,
//!    then SETPEERS (the speaker and us), so the speaker's PTP talks to us.
//! 2. Follow the speaker's PTP clock (`ptp_follower`): it stays the
//!    grandmaster, its clock identity is the timeline.
//! 3. SETUP a buffered stream (type 103, AAC-LC 44.1 kHz stereo, 1024 frames
//!    per packet) and RECORD.
//! 4. Connect to the stream's data port (TCP) and send packets as soon as the
//!    audio is there (`airplay_audio::buffered`).
//! 5. SETRATEANCHORTIME once: the first frame plays `latency` from now on the
//!    speaker's clock. From then on the speaker plays on its own clock, and
//!    everything it has received before a frame is due is played.
//!
//! The audio has to be produced at the pace of that same clock, or the
//! speaker's buffer slowly fills or drains: [`Connection::media_clock`] hands
//! it to whoever paces the input.

use std::io;
use std::time::Duration;

use airplay_audio::{BufferedStats, BufferedStreamer, LiveAudioDecoder};
use airplay_core::error::{Error as CoreError, Result, RtspError};
use airplay_rtsp::{RtspConnection, RtspRequest, RtspSession, SessionState};
use tokio::net::TcpStream;

use super::{generate_device_id, select_best_address, Connection};
use crate::media_clock::MediaClock;
use crate::ptp_follower::{FollowerConfig, PtpFollower};
use crate::PlaybackState;

/// Default lead of the audio over its play time.
pub(super) const DEFAULT_LATENCY: Duration = Duration::from_millis(2_000);
/// How long the speaker's PTP clock may take to settle after SETUP.
const PTP_LOCK_TIMEOUT: Duration = Duration::from_secs(8);
/// Connecting to the stream's data port.
const DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The first RTP timestamp (also the anchor's).
const FIRST_TIMESTAMP: u32 = 0;

impl Connection {
    /// How long before it plays a buffered stream's audio is sent: what the
    /// speaker holds to ride out network stalls, and the latency the stream
    /// adds. Must be called before streaming starts.
    pub fn set_buffered_latency(&mut self, latency: Duration) {
        self.buffered_latency = latency;
    }

    /// The speaker's clock as followed for a buffered stream, once known.
    /// Audio for the stream should be produced at its pace.
    pub fn media_clock(&self) -> Option<MediaClock> {
        self.ptp_follower
            .as_ref()
            .filter(|follower| follower.is_locked())
            .map(PtpFollower::clock)
    }

    /// Counters of the buffered stream, while there is one.
    pub fn buffered_stats(&self) -> Option<BufferedStats> {
        self.buffered.as_ref().map(BufferedStreamer::stats)
    }

    /// Where the PTP follower talks; the standard ports otherwise. Public only
    /// so the host crate's tests can use unprivileged ports.
    #[doc(hidden)]
    pub fn set_ptp_config(&mut self, config: FollowerConfig) {
        self.ptp_config = Some(config);
    }

    /// Connects without pairing or encryption. Public only so the host
    /// crate's tests can run a session against a fake receiver; real
    /// receivers require pairing.
    #[doc(hidden)]
    pub async fn connect_unpaired(
        device: airplay_core::Device,
        config: airplay_core::StreamConfig,
    ) -> Result<Self> {
        let client_device_id = generate_device_id();
        let ip_addr =
            *select_best_address(&device.addresses).ok_or(RtspError::ConnectionRefused)?;
        let mut rtsp = RtspConnection::new(std::net::SocketAddr::new(ip_addr, device.port));
        rtsp.connect().await?;

        let mut session = RtspSession::new(device.clone(), config.clone());
        session.set_client_device_id(client_device_id.clone());
        session.set_connected()?;
        session.set_request_host(ip_addr.to_string());
        let client_instance = client_device_id.replace(':', "");
        rtsp.add_session_header("User-Agent", "AirPlay/745.83");
        rtsp.add_session_header("X-Apple-Device-ID", client_device_id);
        rtsp.add_session_header("DACP-ID", client_instance.clone());
        rtsp.add_session_header("Client-Instance", client_instance);
        rtsp.add_session_header("Active-Remote", "1234567890");
        rtsp.send(RtspRequest::get_info()).await?;
        session.set_paired()?;

        Ok(Self {
            device,
            rtsp,
            session,
            streamer: None,
            playback_state: PlaybackState::Stopped,
            volume: 1.0,
            stream_config: config,
            timing_offset: None,
            timing_tx: None,
            timing_task: None,
            timing_server: None,
            ptp_master: None,
            ptp_master_sync_task: None,
            ptp_master_clock_id: None,
            control_receiver: None,
            control_task: None,
            events_stream: None,
            events_secret: None,
            events_task: None,
            render_delay_ms: 0,
            eq_config: None,
            eq_params: None,
            stream_stats: crate::stats::StreamStats::new(),
            ptp_follower: None,
            ptp_config: None,
            buffered: None,
            buffered_latency: DEFAULT_LATENCY,
        })
    }

    /// SETUP of a buffered stream (steps 1 to 3 above).
    pub(super) async fn setup_buffered(&mut self) -> Result<()> {
        let speaker =
            *select_best_address(&self.device.addresses).ok_or(RtspError::ConnectionRefused)?;
        let local = self
            .rtsp
            .local_addr()
            .ok_or_else(|| {
                RtspError::SetupFailed("no local address for the RTSP connection".into())
            })?
            .ip();
        tracing::info!(%speaker, "RTSP setup start (buffered stream, PTP timing)");

        // The PTP ports first: without them there is no clock, so no stream,
        // and the speaker is not bothered with a SETUP for nothing.
        let config = self
            .ptp_config
            .clone()
            .unwrap_or_else(|| FollowerConfig::new(speaker));
        let follower = PtpFollower::start(config.clone()).map_err(|error| {
            CoreError::Connection(io::Error::new(
                error.kind(),
                format!("a buffered stream needs the PTP ports: {error}"),
            ))
        })?;
        self.ptp_follower = Some(follower);

        // SETUP phase 1: PTP timing, our address as a timing peer.
        let local_addresses = vec![local.to_string()];
        let body = self
            .session
            .build_setup_phase1(config.local_event_port, Some(local_addresses.clone()))?;
        let response = self
            .rtsp
            .send(RtspRequest::setup(self.session.request_uri(), body))
            .await?;
        expect_ok("SETUP (timing)", response.status_code)?;
        self.session
            .process_setup_phase1_response(response.body.as_deref().unwrap_or(&[]))?;
        let session_id = response
            .headers
            .get("Session")
            .cloned()
            .unwrap_or_else(|| "1".to_string());
        self.rtsp.add_session_header("Session", session_id);

        // The events connection, before SETUP phase 2 (as in `setup`).
        let event_port = self
            .session
            .ports()
            .map(|ports| ports.event_port)
            .ok_or_else(|| RtspError::InvalidResponse("No ports in SETUP response".into()))?;
        let events_addr = std::net::SocketAddr::new(speaker, event_port);
        match TcpStream::connect(events_addr).await {
            Ok(stream) => self.start_events_responder(stream, events_addr),
            Err(error) => {
                tracing::warn!(%error, %events_addr, "Could not connect to the events port (proceeding anyway)")
            }
        }

        // SETPEERS: the speaker's timing addresses and ours.
        let mut peers = self.session.receiver_timing_addresses();
        if peers.is_empty() {
            peers.push(speaker.to_string());
        }
        for address in &local_addresses {
            if !peers.contains(address) {
                peers.push(address.clone());
            }
        }
        let body = self.session.build_setpeers(&peers)?;
        let response = self
            .rtsp
            .send(RtspRequest::setpeers_for_session(
                self.session.request_uri(),
                body,
            ))
            .await?;
        if response.status_code == 200 {
            tracing::info!(?peers, "SETPEERS acknowledged");
        } else {
            tracing::warn!(
                status = response.status_code,
                ?peers,
                "SETPEERS refused (continuing)"
            );
        }

        // Introduce ourselves to the speaker's PTP, the way a Mac does.
        if let Some(follower) = self.ptp_follower.as_ref() {
            follower.introduce();
        }

        // A control port for SETUP phase 2 (a buffered stream does not use
        // it, but the receiver must not get ICMP errors if it tries).
        let mut control_receiver = airplay_audio::RtpReceiver::new();
        let control_port = control_receiver.bind(0)?;
        self.session.set_local_control_port(control_port);
        self.control_receiver = Some(std::sync::Arc::new(control_receiver));

        // SETUP phase 2: the buffered stream.
        let body = self.session.build_setup_phase2()?;
        let response = self
            .rtsp
            .send(RtspRequest::setup(self.session.request_uri(), body))
            .await?;
        expect_ok("SETUP (buffered stream)", response.status_code)?;
        self.session
            .process_setup_phase2_response(response.body.as_deref().unwrap_or(&[]))?;
        tracing::info!(
            data_port = self.session.ports().map(|ports| ports.data_port),
            audio_buffer_size = self.session.audio_buffer_size(),
            "the speaker accepted a buffered stream"
        );

        // RECORD (as in `setup`).
        let record = RtspRequest::record_with_info(self.session.request_uri(), 0, 0);
        match tokio::time::timeout(Duration::from_secs(2), self.rtsp.send(record)).await {
            Ok(Ok(response)) if response.status_code == 200 => {
                tracing::info!("RECORD acknowledged")
            }
            Ok(Ok(response)) => tracing::warn!(
                status = response.status_code,
                "RECORD returned an error (continuing anyway)"
            ),
            Ok(Err(error)) => tracing::warn!(%error, "RECORD error (continuing anyway)"),
            Err(_) => tracing::warn!("RECORD timeout (continuing anyway)"),
        }

        // The speaker's clock. Introduce ourselves again if its PTP did not
        // answer yet (it may only start once the stream is set up).
        let follower = self
            .ptp_follower
            .as_ref()
            .ok_or_else(|| RtspError::SetupFailed("no PTP follower".into()))?;
        if follower.status().syncs == 0 {
            follower.introduce();
        }
        follower
            .wait_until_locked(PTP_LOCK_TIMEOUT)
            .await
            .map_err(|reason| {
                CoreError::Connection(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("PTP: {reason}"),
                ))
            })?;
        let status = follower.status();
        tracing::info!(
            timeline = %status.timeline_id.map(|id| format!("{id:016x}")).unwrap_or_default(),
            round_trip_ms = status.round_trip_ns.map(|ns| ns as f64 / 1e6),
            "RTSP setup complete (buffered stream)"
        );
        Ok(())
    }

    /// Starts sending a buffered stream and anchors it (steps 4 and 5 above).
    pub(super) async fn start_streaming_buffered(
        &mut self,
        decoder: LiveAudioDecoder,
    ) -> Result<()> {
        if self.session.state() != SessionState::Ready {
            self.setup().await?;
        }
        let (clock, timeline) = {
            let follower = self
                .ptp_follower
                .as_ref()
                .ok_or_else(|| RtspError::SetupFailed("no PTP follower".into()))?;
            if let Some(reason) = follower.lost() {
                return Err(CoreError::Connection(io::Error::other(format!(
                    "PTP: {reason}"
                ))));
            }
            let timeline = follower.timeline_id().ok_or_else(|| {
                RtspError::SetupFailed("the speaker's PTP timeline is unknown".into())
            })?;
            (follower.clock(), timeline)
        };

        let speaker =
            *select_best_address(&self.device.addresses).ok_or(RtspError::ConnectionRefused)?;
        let data_port = self
            .session
            .ports()
            .map(|ports| ports.data_port)
            .filter(|&port| port != 0)
            .ok_or_else(|| RtspError::SetupFailed("no data port for the buffered stream".into()))?;
        let data_addr = std::net::SocketAddr::new(speaker, data_port);
        let connection = tokio::time::timeout(DATA_CONNECT_TIMEOUT, TcpStream::connect(data_addr))
            .await
            .map_err(|_| CoreError::Timeout)??
            .into_std()?;
        connection.set_nonblocking(false)?;

        let mut streamer = BufferedStreamer::start(
            connection,
            decoder,
            *self.session.stream_key(),
            FIRST_TIMESTAMP,
        )?;

        // The anchor: the first frame plays `latency` from now, on the
        // speaker's clock.
        let latency = self.buffered_latency;
        let plays_at = clock.now_ns().saturating_add(latency.as_nanos() as u64);
        let body = self
            .session
            .build_rate_anchor(FIRST_TIMESTAMP, plays_at, timeline, 1)?;
        let anchored = self
            .rtsp
            .send(RtspRequest::set_rate_anchor_time(
                self.session.request_uri(),
                body,
            ))
            .await
            .and_then(|response| expect_ok("SETRATEANCHORTIME", response.status_code));
        if let Err(error) = anchored {
            streamer.stop();
            return Err(error);
        }

        self.session.start_playing()?;
        self.buffered = Some(streamer);
        self.playback_state = PlaybackState::Playing;
        tracing::info!(
            latency_ms = latency.as_millis() as u64,
            timeline = %format!("{timeline:016x}"),
            %data_addr,
            "buffered stream started"
        );
        Ok(())
    }

    /// Stops the buffered stream and removes it from the session.
    pub(super) async fn stop_buffered(&mut self) {
        if let Some(mut streamer) = self.buffered.take() {
            let stats = streamer.stats();
            streamer.stop();
            tracing::info!(
                packets = stats.packets,
                sent = stats.sent,
                dropped = stats.dropped,
                longest_write_ms = stats.longest_write_us / 1_000,
                "buffered stream stopped"
            );
            if let Ok(body) = self.session.build_teardown_streams() {
                let request = RtspRequest::teardown_streams(self.session.request_uri(), body);
                if let Err(error) = self.rtsp.send(request).await {
                    tracing::debug!(%error, "TEARDOWN of the buffered stream failed");
                }
            }
        }
        self.playback_state = PlaybackState::Stopped;
    }

    /// An error if the buffered stream failed or its clock was lost.
    pub(super) fn buffered_health(&self) -> Result<()> {
        if let Some(failure) = self.buffered.as_ref().and_then(BufferedStreamer::error) {
            return Err(CoreError::Connection(failure.to_io_error()));
        }
        if let Some(reason) = self.ptp_follower.as_ref().and_then(PtpFollower::lost) {
            return Err(CoreError::Connection(io::Error::other(format!(
                "PTP: {reason}"
            ))));
        }
        Ok(())
    }
}

fn expect_ok(what: &str, status: u16) -> Result<()> {
    if status == 200 {
        Ok(())
    } else {
        Err(RtspError::SetupFailed(format!("{what} returned status {status}")).into())
    }
}
