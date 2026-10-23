//! The PTP follower of the vendored `airplay-client`: how a buffered stream
//! learns the speaker's clock.
//!
//! A buffered AirPlay 2 stream is anchored to the speaker's PTP timeline, so
//! the sender must know which timeline that is and what time it is on it. The
//! follower is driven here against a fake grandmaster on the loopback
//! interface (ephemeral ports instead of 319/320): it must lock onto the
//! grandmaster's clock and timeline, track drift, shrug off delay spikes (the
//! Wi-Fi of the speaker this is for has 470 ms ping spikes), and report a
//! silent, replaced or jumping grandmaster.
//!
//! The messages the follower sends are compared, byte for byte, with what the
//! pinned upstream `airplay-timing` sends in the flow upstream used with
//! HomePods (where port 320 can be bound: upstream always sends there). The
//! estimator and the clock model are tested on their own.

#[path = "support/grandmaster.rs"]
mod grandmaster;

use std::net::UdpSocket;
use std::thread;
use std::time::Duration;

use airplay_audio::timeline::mono_now_ns;
use airplay_client::ptp_follower::wire::{self, AnnounceBody, Message, PortIdentity};
use airplay_client::ptp_follower::{
    ClockModel, DIVERGENCE_LIMIT_NS, FOLLOWER_PRIORITY1, FollowerConfig, ModelUpdate,
    OffsetEstimator, PtpFollower, SLEW_PPM,
};
use grandmaster::{Behaviour, FakeGrandmaster, GM_IDENTITY, LOCALHOST, LOCK_TIMEOUT};

/// How far the follower's clock is from the grandmaster's, in ms, worst of a
/// few readings over `over`.
fn worst_error_ms(gm: &FakeGrandmaster, follower: &PtpFollower, over: Duration) -> f64 {
    let clock = follower.clock();
    let readings = 10;
    let mut worst = 0.0f64;
    for _ in 0..readings {
        let error = i128::from(clock.now_ns()) - gm.now_ns();
        worst = worst.max((error as f64 / 1e6).abs());
        thread::sleep(over / readings);
    }
    worst
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

fn wait_locked(follower: &PtpFollower) {
    if let Err(error) = runtime().block_on(follower.wait_until_locked(LOCK_TIMEOUT)) {
        panic!(
            "the follower never locked: {error} ({:?})",
            follower.status()
        );
    }
}

#[test]
fn follows_the_grandmaster_clock_and_timeline() {
    let gm = FakeGrandmaster::start(Behaviour::default());
    let mut follower = gm.start_follower();
    wait_locked(&follower);

    assert_eq!(
        follower.timeline_id(),
        Some(u64::from_be_bytes(GM_IDENTITY)),
        "the timeline is the grandmaster's clock identity"
    );
    let error = worst_error_ms(&gm, &follower, Duration::from_secs(1));
    assert!(error < 2.0, "clock off by {error:.3} ms");

    let status = follower.status();
    assert!(status.locked);
    assert!(status.delay_responses > 0, "{status:?}");
    assert!(status.lost.is_none());
    follower.stop();
}

#[test]
fn tracks_a_drifting_grandmaster() {
    for ppm in [150.0, -150.0] {
        let gm = FakeGrandmaster::start(Behaviour {
            ppm,
            ..Behaviour::default()
        });
        let follower = gm.start_follower();
        wait_locked(&follower);
        // 3 s at 150 ppm is 0.45 ms of drift: it must not accumulate.
        thread::sleep(Duration::from_secs(3));
        let error = worst_error_ms(&gm, &follower, Duration::from_secs(1));
        assert!(error < 2.0, "{ppm} ppm: clock off by {error:.3} ms");
    }
}

#[test]
fn ignores_delay_spikes() {
    // Every third Sync and Delay_Req held up by 300 ms, as on the speaker's
    // Wi-Fi: the fastest messages still bound the offset.
    let gm = FakeGrandmaster::start(Behaviour {
        spike_every: 3,
        spike: Duration::from_millis(300),
        ..Behaviour::default()
    });
    let follower = gm.start_follower();
    wait_locked(&follower);
    let error = worst_error_ms(&gm, &follower, Duration::from_secs(2));
    assert!(error < 2.0, "clock off by {error:.3} ms");
}

#[test]
fn locks_without_delay_responses() {
    let gm = FakeGrandmaster::start(Behaviour {
        answer_delay_req: false,
        ..Behaviour::default()
    });
    let follower = gm.start_follower();
    wait_locked(&follower);
    // Only the lower bound is known: off by the one-way delay (0.3 ms).
    let error = worst_error_ms(&gm, &follower, Duration::from_secs(1));
    assert!(error < 2.0, "clock off by {error:.3} ms");
    assert_eq!(follower.status().delay_responses, 0);
}

#[test]
fn never_goes_backwards_and_never_jumps() {
    // The estimate moves (spikes, drift), the published clock must not jump.
    let gm = FakeGrandmaster::start(Behaviour {
        ppm: 120.0,
        spike_every: 2,
        spike: Duration::from_millis(80),
        ..Behaviour::default()
    });
    let follower = gm.start_follower();
    wait_locked(&follower);
    let clock = follower.clock();
    // A reading between two local readings, retried if preempted between them.
    let read = || loop {
        let before = mono_now_ns();
        let media = clock.now_ns();
        let after = mono_now_ns();
        if after - before < 20_000 {
            return (before, media, after);
        }
    };
    let mut previous = read();
    for _ in 0..2_000 {
        thread::sleep(Duration::from_micros(500));
        let now = read();
        assert!(now.1 >= previous.1, "the clock went backwards");
        // The clock runs within 500 ppm of the local one: between two
        // readings it moves as much as the local clock could have, give or
        // take 500 ppm.
        let media = now.1 - previous.1;
        let longest = now.2 - previous.0;
        let shortest = now.0 - previous.2;
        assert!(
            media <= longest + longest / 2_000 + 1,
            "jumped: {media} ns in at most {longest} ns"
        );
        assert!(
            media + shortest / 2_000 + 1 >= shortest,
            "stalled: {media} ns in at least {shortest} ns"
        );
        previous = now;
    }
}

#[test]
fn reports_a_silent_grandmaster() {
    let gm = FakeGrandmaster::start(Behaviour::default());
    let follower = gm.start_follower_with(FollowerConfig {
        silence_limit: Duration::from_millis(800),
        ..gm.follower_config()
    });
    wait_locked(&follower);
    gm.change(|behaviour| behaviour.silent = true);
    thread::sleep(Duration::from_millis(1_500));
    let lost = follower
        .lost()
        .expect("a silent grandmaster must be reported");
    assert!(lost.contains("no PTP Sync"), "{lost}");
    assert!(!follower.is_locked());
}

#[test]
fn reports_a_replaced_grandmaster() {
    let gm = FakeGrandmaster::start(Behaviour::default());
    let follower = gm.start_follower();
    wait_locked(&follower);
    gm.change(|behaviour| behaviour.identity = [9, 8, 7, 6, 5, 4, 3, 2]);
    thread::sleep(Duration::from_millis(600));
    let lost = follower.lost().expect("a new grandmaster must be reported");
    assert!(lost.contains("grandmaster changed"), "{lost}");
}

#[test]
fn reports_a_clock_jump() {
    let gm = FakeGrandmaster::start(Behaviour::default());
    let follower = gm.start_follower_with(FollowerConfig {
        divergence_persist: Duration::from_millis(500),
        ..gm.follower_config()
    });
    wait_locked(&follower);
    gm.change(|behaviour| behaviour.step_ns = 1_000_000_000);
    thread::sleep(Duration::from_millis(1_500));
    let lost = follower.lost().expect("a clock jump must be reported");
    assert!(lost.contains("jumped"), "{lost}");
}

#[test]
fn waiting_fails_without_a_grandmaster() {
    let gm = FakeGrandmaster::start(Behaviour {
        silent: true,
        ..Behaviour::default()
    });
    let follower = gm.start_follower();
    let error = runtime()
        .block_on(follower.wait_until_locked(Duration::from_millis(600)))
        .unwrap_err();
    assert!(error.contains("no PTP Sync"), "{error}");
}

#[test]
fn introduces_itself_like_a_mac_then_yields() {
    let gm = FakeGrandmaster::start(Behaviour::default());
    let follower = gm.start_follower();
    wait_locked(&follower);
    let identity = follower.identity();
    let ours: Vec<Message> = gm
        .received()
        .into_iter()
        .filter(|(from, _)| {
            from.port() == follower.event_addr().port()
                || from.port() == follower.general_addr().port()
        })
        .map(|(_, message)| message)
        .collect();

    let syncs = ours
        .iter()
        .filter(|message| matches!(message, Message::Sync { header, .. } if header.flags & wire::TWO_STEP != 0))
        .count();
    assert_eq!(syncs, 3, "three introductory two-step Syncs");
    let announces: Vec<&AnnounceBody> = ours
        .iter()
        .filter_map(|message| match message {
            Message::Announce { body, .. } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(announces.len(), 2);
    for body in announces {
        assert_eq!(body.priority1, FOLLOWER_PRIORITY1);
        assert_eq!(body.grandmaster, identity);
    }
    let signaling = ours
        .iter()
        .filter(|message| matches!(message, Message::Signaling { .. }))
        .count();
    assert_eq!(signaling, 2, "the Mac-style Signaling, then the yield");
    let delay_requests = ours
        .iter()
        .filter(|message| matches!(message, Message::DelayReq { header, .. } if header.source.clock == identity))
        .count();
    assert!(delay_requests >= 8, "a Delay_Req per Follow_Up");
}

#[test]
fn does_not_take_a_grandmaster_that_ranks_below_it() {
    // A clock with priority1 255 would lose to us: we warn and keep listening,
    // but a speaker that sends Syncs anyway is still followed.
    let gm = FakeGrandmaster::start(Behaviour {
        priority1: 255,
        ..Behaviour::default()
    });
    let follower = gm.start_follower();
    wait_locked(&follower);
    let yields = gm
        .received()
        .iter()
        .filter(|(_, message)| matches!(message, Message::Signaling { .. }))
        .count();
    assert_eq!(yields, 1, "no yield Signaling to a worse clock");
}

// ----- estimator and clock model -----

#[test]
fn estimator_takes_the_middle_of_the_tightest_bounds() {
    let mut estimator = OffsetEstimator::new(Duration::from_secs(4));
    let offset = 5_000_000_000i128;
    let mut local = 1_000_000_000u64;
    // Forward delays 3, 1 (fastest), 250 ms; reverse 2 (fastest), 40 ms.
    for (forward, reverse) in [(3, 40), (1, 2), (250, 40)] {
        let ms = 1_000_000i128;
        estimator.add_forward(local, i128::from(local) + offset - forward * ms);
        estimator.add_reverse(local, i128::from(local) + offset + reverse * ms);
        local += 125_000_000;
    }
    let estimate = estimator.estimate().unwrap();
    assert_eq!(estimate.lower_ns, offset - 1_000_000);
    assert_eq!(estimate.upper_ns, Some(offset + 2_000_000));
    assert_eq!(estimate.offset_ns, offset + 500_000);
    assert_eq!(estimate.round_trip_ns(), Some(3_000_000));
}

#[test]
fn estimator_forgets_samples_outside_its_window() {
    let mut estimator = OffsetEstimator::new(Duration::from_secs(4));
    estimator.add_forward(1_000_000_000, 1_000_000_000 + 100);
    assert_eq!(estimator.estimate().unwrap().offset_ns, 100);
    // Five seconds later the old (faster) sample is gone.
    estimator.add_forward(6_000_000_000, 6_000_000_000 + 50);
    assert_eq!(estimator.forward_samples(), 1);
    assert_eq!(estimator.estimate().unwrap().offset_ns, 50);
    assert!(
        OffsetEstimator::new(Duration::from_secs(1))
            .estimate()
            .is_none()
    );
}

#[test]
fn clock_model_locks_then_only_slews() {
    let mut model = ClockModel::default();
    assert_eq!(model.offset_ns(), None);
    assert_eq!(model.update(1_000_000_000, 7_000), ModelUpdate::Locked);
    assert_eq!(model.offset_ns(), Some(7_000));

    // A 10 ms jump of the estimate moves the clock 500 ppm × 125 ms at most.
    let step = 125_000_000u64;
    assert_eq!(
        model.update(1_000_000_000 + step, 10_007_000),
        ModelUpdate::Tracking
    );
    let max_step = i128::from(step) * SLEW_PPM / 1_000_000;
    assert_eq!(model.offset_ns(), Some(7_000 + max_step));

    // Small errors are taken whole.
    let mut model = ClockModel::default();
    model.update(0, 0);
    model.update(125_000_000, 20_000);
    assert_eq!(model.offset_ns(), Some(20_000));
}

#[test]
fn clock_model_reports_a_lasting_divergence_only() {
    let mut model = ClockModel::new(Duration::from_secs(3));
    model.update(0, 0);
    // Far enough that slewing (0.5 ms/s) does not bring it under the limit.
    let far = DIVERGENCE_LIMIT_NS * 4;
    let second = 1_000_000_000u64;
    // A short excursion is not a jump.
    assert_eq!(model.update(second, far), ModelUpdate::Tracking);
    assert_eq!(model.update(2 * second, 0), ModelUpdate::Tracking);
    // A lasting one is.
    assert_eq!(model.update(3 * second, far), ModelUpdate::Tracking);
    assert_eq!(model.update(5 * second, far), ModelUpdate::Tracking);
    assert!(matches!(
        model.update(6 * second, far),
        ModelUpdate::Diverged { .. }
    ));
}

// ----- wire format -----

fn port(clock: [u8; 8]) -> PortIdentity {
    PortIdentity { clock, port: 1 }
}

#[test]
fn wire_round_trips() {
    let source = port(GM_IDENTITY);
    let requester = port([1, 2, 3, 4, 5, 6, 7, 8]);
    let time = 188_748_500_000_123u128;

    let Some(Message::Sync { header, origin_ns }) =
        wire::parse(&wire::sync(source, 7, false, time))
    else {
        panic!("not a Sync")
    };
    assert_eq!(
        (header.sequence, header.source, origin_ns),
        (7, source, time)
    );
    assert_eq!(header.flags & wire::TWO_STEP, 0);

    let Some(Message::FollowUp {
        header,
        precise_origin_ns,
    }) = wire::parse(&wire::follow_up(source, 8, time, 3 << 16))
    else {
        panic!("not a Follow_Up")
    };
    assert_eq!((header.sequence, precise_origin_ns), (8, time));
    assert_eq!(header.correction_ns(), 3);

    let Some(Message::DelayResp {
        header,
        receive_ns,
        requesting,
    }) = wire::parse(&wire::delay_resp(source, 9, time, requester, -(5 << 16)))
    else {
        panic!("not a Delay_Resp")
    };
    assert_eq!(
        (header.sequence, receive_ns, requesting),
        (9, time, requester)
    );
    assert_eq!(header.correction_ns(), -5);

    let body = AnnounceBody::follower(GM_IDENTITY);
    let Some(Message::Announce { body: parsed, .. }) =
        wire::parse(&wire::announce(source, 1, &body, 1))
    else {
        panic!("not an Announce")
    };
    assert_eq!(parsed, body);

    assert!(wire::parse(&[0u8; 20]).is_none(), "too short");
    let mut version1 = wire::sync(source, 1, true, 0);
    version1[1] = 1;
    assert!(wire::parse(&version1).is_none(), "not PTPv2");
}

#[test]
fn timestamps_use_48_bit_seconds() {
    let mut out = Vec::new();
    let time = (0x1234_5678_9ABCu128) * 1_000_000_000 + 999_999_999;
    wire::write_timestamp(&mut out, time);
    assert_eq!(
        out,
        [0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0x3B, 0x9A, 0xC9, 0xFF]
    );
    assert_eq!(wire::read_timestamp(&out), time);
}

/// Binds the port upstream always sends general messages to, or skips.
fn bind_upstream_general_port() -> Option<std::net::UdpSocket> {
    match UdpSocket::bind((LOCALHOST, airplay_timing::PTP_GENERAL_PORT)) {
        Ok(socket) => {
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            Some(socket)
        }
        Err(error) => {
            eprintln!(
                "skipped: cannot bind 127.0.0.1:{} ({error})",
                airplay_timing::PTP_GENERAL_PORT
            );
            None
        }
    }
}

fn next_datagram(socket: &UdpSocket) -> Vec<u8> {
    let mut buffer = [0u8; 512];
    let (length, _) = socket.recv_from(&mut buffer).unwrap();
    buffer[..length].to_vec()
}

#[tokio::test]
async fn sends_what_upstream_sends_to_a_homepod() {
    let Some(general) = bind_upstream_general_port() else {
        return;
    };
    let event = UdpSocket::bind((LOCALHOST, 0)).unwrap();
    event
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let identity = [0xAB, 1, 2, 3, 4, 5, 6, 7];
    let to = event.local_addr().unwrap();
    let ours = port(identity);
    let upstream_event = tokio::net::UdpSocket::bind((LOCALHOST, 0)).await.unwrap();
    let upstream_general = tokio::net::UdpSocket::bind((LOCALHOST, 0)).await.unwrap();

    // Announce (upstream: clock class 248, priority1 250, like a Mac).
    let mut sequence = 0u16;
    airplay_timing::send_ptp_announce(
        &upstream_general,
        to,
        &identity,
        &mut sequence,
        248,
        FOLLOWER_PRIORITY1,
    )
    .await
    .unwrap();
    assert_eq!(
        next_datagram(&general),
        wire::announce(ours, 1, &AnnounceBody::follower(identity), 1)
    );

    // Mac-style Signaling and the yield.
    let mut sequence = 0u16;
    airplay_timing::send_mac_style_signaling(&upstream_general, to, &identity, &mut sequence)
        .await
        .unwrap();
    assert_eq!(next_datagram(&general), wire::signaling_mac_style(ours, 1));
    airplay_timing::send_stop_signaling(&upstream_general, to, &identity, &mut sequence)
        .await
        .unwrap();
    assert_eq!(
        next_datagram(&general),
        wire::signaling_initial_intervals(ours, 2)
    );

    // Sync and Follow_Up (upstream stamps them with the wall clock).
    let mut sequence = 0u16;
    airplay_timing::send_ptp_sync(
        &upstream_event,
        &upstream_general,
        to,
        &identity,
        &mut sequence,
    )
    .await
    .unwrap();
    assert_eq!(next_datagram(&event), wire::sync(ours, 1, true, 0));
    let follow_up = next_datagram(&general);
    let mut expected = wire::follow_up(ours, 1, 0, 0);
    expected[34..44].copy_from_slice(&follow_up[34..44]);
    assert_eq!(follow_up, expected);
}
