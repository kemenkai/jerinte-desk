//! `capacity` through the controller's own doors, where the simulation does not go: the relay's
//! limit, the encoder that takes a new bitrate only by being created again, the word for the
//! viewer, and who is measured at all.
use super::*;

const DISPLAY: &str = "wiring";
/// As the simulation: the bitrate at ratio 1, in kbps.
const BASE_KBPS: f32 = 6000.0;

/// One viewer of one display at a custom quality of `target`, the stream at its target.
fn session(target: f32) -> VideoQoS {
    let mut qos = super::smoke::session(60, Quality::Custom(target));
    qos.abr_config = true;
    qos.new_display(DISPLAY.to_owned());
    qos.set_support_changing_quality(DISPLAY, true);
    qos.ratio = target;
    sync(&mut qos);
    qos
}

/// What the video loop does on every frame: the encoder takes the ratio and says its bitrate.
fn sync(qos: &mut VideoQoS) {
    let bitrate = (BASE_KBPS * qos.ratio()) as u32;
    qos.store_bitrate(bitrate);
}

/// One second in which the stream put out `share` of its bitrate and the probe came back after
/// `delay` ms, the path having delivered `delivered` of what was sent (1 for all of it).
fn second(qos: &mut VideoQoS, delay: u32, delivered: f32, share: f32) {
    second_of(qos, delay, delivered, share, 60);
}

/// The same, with the bitrate's `share` going out in `frames` frames.
fn second_of(qos: &mut VideoQoS, delay: u32, delivered: f32, share: f32, frames: usize) {
    qos.advance_ms(1000);
    sync(qos);
    let kbps = BASE_KBPS * qos.ratio() * share;
    let bytes = (kbps * 1000.0 / 8.0) as u64;
    if delivered > 0.0 {
        // The bytes of a second, which took the viewer `1 / delivered` seconds to receive.
        qos.user_delivery(
            1,
            bytes,
            Duration::from_secs(1),
            Duration::from_secs_f32(1.0 / delivered),
        );
    }
    qos.user_network_delay(1, delay);
    sync(qos);
    qos.update_display_rates(
        DISPLAY,
        bytes as usize,
        frames,
        Duration::ZERO,
        Duration::from_secs(1),
    );
    // The video loop counts every frame it hands to the encoder, sent or not.
    qos.update_display_data(DISPLAY, 60);
    sync(qos);
}

fn settle(qos: &mut VideoQoS) {
    for _ in 0..30 {
        second(qos, 10, 1.0, 1.0);
    }
}

#[test]
fn a_full_path_is_left_in_one_step_and_the_viewer_is_told() {
    let mut qos = session(3.5);
    settle(&mut qos);
    assert_eq!(qos.ratio(), 3.5);
    assert_eq!(qos.qos_state(), 0);
    // The path delivers 10 of the 21 Mbps: replies come later and later.
    let mut at = Vec::new();
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
        at.push(qos.ratio());
    }
    let after = qos.ratio();
    assert!(
        after < 3.5 * 0.3,
        "well below the 10 Mbps the path carried, to send the queue: {at:?}"
    );
    assert_eq!(qos.qos_state(), 1, "lowered for the network");
    // With the queue sent, just below what the path carried, and no further for half a minute
    // on good replies.
    for _ in 0..20 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    let kbps = BASE_KBPS * qos.ratio();
    assert!(
        (8_000.0..=9_500.0).contains(&kbps),
        "{kbps} kbps under a 10 Mbps path"
    );
    assert_eq!(qos.qos_state(), 1, "still not what was asked for");
    // Then past it by small steps, and with the path carrying all of it, back to the target,
    // which the viewer is told too.
    for _ in 0..60 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert_eq!(qos.ratio(), 3.5);
    assert_eq!(qos.qos_state(), 0);
}

#[test]
fn a_hardware_encoder_braked_on_the_first_late_reply_is_still_cut_by_what_the_path_carried() {
    // Sixty frames a second, twice its bitrate: 42 Mbps into a path that carries 10.
    let mut qos = session(3.5);
    qos.set_encoder_hardware(DISPLAY, true, 30);
    for _ in 0..10 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    assert!((qos.overshoot() - 2.0).abs() < 0.01);
    let before = qos.ratio();
    // The first late reply: the path fell short. The frame rate is braked for it, and in the
    // next second the encoder puts out a fifth of what it did: less than the path carries, and
    // it all arrives, in less time than it was sent in, the reply late again.
    // The cut comes with the second late reply or, the bitrate having been looked at less than
    // three seconds before, with the next look at it.
    second(&mut qos, 1100, 10.0 / 42.0, 2.0);
    let sent = BASE_KBPS * qos.ratio() * 0.4;
    for delay in [1200, 1300] {
        if !qos.capacity.has_ceiling() {
            second(&mut qos, delay, 10_000.0 / sent, 0.4);
        }
    }
    assert!(qos.capacity.has_ceiling(), "cut by the path's measure");
    assert_eq!(qos.qos_state(), 1);
    // With the queue sent the stream settles under the 10 Mbps, at the sixty frames a second
    // it comes back to.
    for _ in 0..12 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    let out = BASE_KBPS * qos.ratio() * 2.0;
    assert!(
        (8_000.0..=9_500.0).contains(&out),
        "{out} kbps out under a 10 Mbps path, from ratio {before}"
    );
}

#[test]
fn a_path_that_falls_short_again_under_its_ceiling_is_measured_again() {
    // Sixty frames a second, twice its bitrate; brought under a path of 10 Mbps.
    let mut qos = session(3.5);
    qos.set_encoder_hardware(DISPLAY, true, 30);
    for _ in 0..10 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 42.0, 2.0);
    }
    for _ in 0..12 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    assert!(qos.capacity.has_ceiling());
    let out = BASE_KBPS * qos.ratio() * 2.0;
    assert!((8_000.0..=9_500.0).contains(&out), "{out} kbps out");
    // The path now carries 4 Mbps. The first late reply brakes the frame rate, the encoder
    // sends a fifth of what it did, and that arrives faster than it was sent: not the queue of
    // the cut before, which is long gone, but a path that fell short anew.
    second(&mut qos, 1100, 4_000.0 / out, 2.0);
    let braked = BASE_KBPS * qos.ratio() * 0.4;
    for delay in [1200, 1300] {
        second(&mut qos, delay, 4_000.0 / braked, 0.4);
    }
    for _ in 0..12 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    let out = BASE_KBPS * qos.ratio() * 2.0;
    assert!(
        (3_000.0..=3_900.0).contains(&out),
        "{out} kbps out under a path of 4 Mbps"
    );
}

#[test]
fn a_clean_reply_ends_the_wait_for_the_queue_on_a_still_screen_too() {
    let mut qos = session(3.5);
    settle(&mut qos);
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
    }
    assert!(qos.capacity.has_ceiling());
    let low = BASE_KBPS * qos.ratio();
    assert!(low < 5_500.0, "{low} kbps while the queue is sent");
    // The screen stops changing: a frame a second. The replies come back clean, and what the
    // stream may put out is what it settles at, without waiting out the time reckoned for a
    // queue that is gone. (The ratio itself is raised when the screen changes again.)
    for _ in 0..3 {
        second_of(&mut qos, 10, 1.0, 0.02, 1);
    }
    let allowed = BASE_KBPS * qos.max_ratio();
    assert!(
        (8_000.0..=9_500.0).contains(&allowed),
        "{allowed} kbps allowed under a 10 Mbps path"
    );
}

#[test]
fn a_pause_the_path_catches_up_from_is_not_a_full_path() {
    let mut qos = session(3.5);
    settle(&mut qos);
    // The path stops for most of a second, then delivers what was held and what is sent, at
    // twice the stream's rate, the reply late once more for the queue on its way out. It
    // carries the stream: no ceiling is made of it.
    second(&mut qos, 900, 0.2, 1.0);
    second(&mut qos, 600, 2.0, 1.0);
    second(&mut qos, 10, 1.0, 1.0);
    assert!(
        !qos.capacity.has_ceiling(),
        "what two late replies come to is left to the table"
    );
}

#[test]
fn late_replies_with_everything_delivered_are_left_to_the_table() {
    let mut qos = session(3.5);
    settle(&mut qos);
    // The replies are late, but all that was sent arrived in the time it was sent in: the path
    // is not full.  The table's cut, and no ceiling.
    for _ in 0..3 {
        second(&mut qos, 400, 1.0, 1.0);
    }
    assert!(
        qos.ratio() < 3.5 && qos.ratio() > 3.5 * 0.4,
        "{}",
        qos.ratio()
    );
    assert!(!qos.capacity.has_ceiling());
}

#[test]
fn a_still_screen_says_nothing_of_the_path() {
    let mut qos = session(3.5);
    settle(&mut qos);
    // Hardly anything to send, and less of it arrives than was sent: not a measure of the path.
    for delay in [300, 400, 500] {
        second(&mut qos, delay, 0.5, 0.02);
    }
    assert!(!qos.capacity.has_ceiling());
}

#[test]
fn a_viewer_that_does_not_say_when_is_as_it_always_was() {
    let mut qos = super::smoke::session(30, Quality::Balanced);
    qos.abr_config = true;
    qos.new_display(DISPLAY.to_owned());
    qos.set_support_changing_quality(DISPLAY, true);
    for _ in 0..30 {
        second(&mut qos, 10, 0.0, 1.0);
    }
    for _ in 0..6 {
        second(&mut qos, 1200, 0.0, 1.0);
    }
    assert!(!qos.capacity.has_ceiling());
    assert_eq!(qos.qos_state(), 1, "a cut by the table is told as well");
}

#[test]
fn two_viewers_or_two_displays_are_not_measured() {
    for extra in ["viewer", "display"] {
        let mut qos = session(3.5);
        settle(&mut qos);
        if extra == "viewer" {
            qos.users.insert(2, qos.users[&1].clone());
        } else {
            qos.new_display("second".to_owned());
            qos.set_support_changing_quality("second", true);
        }
        for delay in [1100, 2200, 3300] {
            second(&mut qos, delay, 10.0 / 21.0, 1.0);
        }
        assert!(!qos.capacity.has_ceiling(), "with a second {extra}");
    }
}

#[test]
fn a_session_through_the_relay_stays_under_its_limit() {
    let mut qos = session(3.5);
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    for _ in 0..5 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    let kbps = BASE_KBPS * qos.ratio();
    assert!(
        (9_500.0..=10_050.0).contains(&kbps),
        "{kbps} kbps under a 10 Mbps relay"
    );
    assert_eq!(
        qos.qos_state(),
        0,
        "the relay's limit is not a cut for the network"
    );

    // A hardware encoder that puts out twice its bitrate is held to the limit by what it really
    // sends.
    qos.set_encoder_hardware(DISPLAY, true, 30);
    for _ in 0..8 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    let kbps = BASE_KBPS * qos.ratio() * 2.0;
    assert!(
        (9_000.0..=10_500.0).contains(&kbps),
        "{kbps} kbps really sent"
    );

    // A direct viewer is not limited.
    let mut qos = session(3.5);
    qos.relay_cap_kbps = 10_000;
    settle(&mut qos);
    assert_eq!(qos.ratio(), 3.5);
}

#[test]
fn a_high_target_is_climbed_to_once_the_frame_rate_has_started() {
    let mut qos = super::smoke::session(60, Quality::Balanced);
    qos.abr_config = true;
    qos.new_display(DISPLAY.to_owned());
    qos.set_support_changing_quality(DISPLAY, true);
    qos.user_image_quality(1, 300 << 8); // custom 300%: ratio 6
    assert_eq!(
        qos.ratio(),
        capacity::START_RATIO,
        "from the best preset's level"
    );
    let mut seconds = 0;
    while qos.ratio() < 6.0 {
        second(&mut qos, 10, 1.0, 1.0);
        seconds += 1;
        assert!(seconds < 30, "stuck at {}", qos.ratio());
        if qos.fps() < 60 {
            assert_eq!(
                qos.ratio(),
                capacity::START_RATIO,
                "not before the frame rate is up"
            );
        }
    }
    assert!(seconds <= 12, "{seconds} s to the target");
    // The same quality set again does not start over; a preset is taken at once.
    qos.user_image_quality(1, 300 << 8);
    assert_eq!(qos.ratio(), 6.0);
    qos.user_image_quality(1, ImageQuality::Best.value());
    assert_eq!(qos.ratio(), BR_BEST);
}

/// One viewer of one display whose encoder, a hardware one, takes a new bitrate only by being
/// created again, and was created for the sixty frames a second the viewer allows.
fn restart_only_session() -> VideoQoS {
    let mut qos = session(3.5);
    qos.set_support_changing_quality(DISPLAY, false);
    qos.set_restartable(DISPLAY, true);
    qos.set_encoder_hardware(DISPLAY, true, 60);
    qos
}

#[test]
fn an_encoder_that_cannot_change_is_created_again_for_a_full_path() {
    let mut qos = restart_only_session();
    // Nothing is asked of an encoder created a moment ago.
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
    assert_eq!(qos.ratio(), 3.5);
    // Later a full path brings it down, straight to where the stream settles.
    for _ in 0..30 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
    }
    assert!(qos.take_restart_request(DISPLAY));
    let kbps = BASE_KBPS * qos.ratio;
    assert!(
        (8_000.0..=9_000.0).contains(&kbps),
        "{kbps} kbps under a 10 Mbps path"
    );
    assert_eq!(qos.qos_state(), 1);
    // The video loop starts over with the new encoder.
    recreate(&mut qos);
    // Late replies without a full path do not bring it down, and good ones do not bring it
    // back before its time.
    for _ in 0..25 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
    // After half a minute: half again, not all the way.
    let before = qos.ratio;
    for _ in 0..6 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(qos.take_restart_request(DISPLAY));
    assert!(!qos.take_restart_request(DISPLAY), "asked once");
    assert!(
        (qos.ratio - before * 1.5).abs() < 1e-3,
        "{} from {before}",
        qos.ratio
    );
    assert_eq!(qos.qos_state(), 1, "not back yet");
    // A viewer coming and going does not let go of an encoder that is held down: the stream
    // is where it is, and stays "lowered".
    recreate(&mut qos);
    qos.viewers_changed();
    for _ in 0..10 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!((qos.ratio - before * 1.5).abs() < 1e-3);
    assert!(!qos.take_restart_request(DISPLAY));
    assert_eq!(qos.qos_state(), 1);
    // After half a minute more, half again would come within a tenth of the target: all the
    // way, by a restart, and the viewer is told.
    for _ in 0..22 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(qos.take_restart_request(DISPLAY));
    assert_eq!(qos.ratio, 3.5);
    assert_eq!(qos.qos_state(), 0);
    recreate(&mut qos);
    for _ in 0..400 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY), "nothing more to ask");
}

#[test]
fn a_try_for_more_that_ends_in_a_cut_is_waited_longer_after() {
    let mut qos = restart_only_session();
    for _ in 0..30 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    // The path carries 10 of the 21 Mbps, and goes on doing so. An encoder is brought down no
    // sooner than five seconds after it was created.
    let cut = |qos: &mut VideoQoS| {
        let mut asked = false;
        for delay in [1100, 2200, 3300, 3300, 3300, 3300] {
            second_at(qos, delay, 10_000.0 / qos.bitrate() as f32);
            asked = qos.take_restart_request(DISPLAY);
            if asked {
                break;
            }
        }
        assert!(asked);
        recreate(qos);
    };
    cut(&mut qos);
    let low = qos.ratio;
    // Half a minute, and it is tried half again higher, which the path does not carry.
    let mut waited = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_at(&mut qos, 10, 1.0);
        waited += 1;
        assert!(waited < 40);
    }
    assert!((30..=32).contains(&waited), "{waited} s");
    recreate(&mut qos);
    assert!(qos.ratio > low * 1.4);
    cut(&mut qos);
    // The next try comes after a minute.
    let mut waited = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_at(&mut qos, 10, 1.0);
        waited += 1;
        assert!(waited < 80);
    }
    assert!((60..=62).contains(&waited), "{waited} s");
    // It ends in a cut again, and the wait would be two minutes. But the viewer goes and
    // another comes, on a path of its own: for that one the wait is the first again.
    recreate(&mut qos);
    cut(&mut qos);
    qos.viewers_changed();
    let mut waited = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_at(&mut qos, 10, 1.0);
        waited += 1;
        assert!(waited < 80);
    }
    assert!((30..=32).contains(&waited), "{waited} s");
}

#[test]
fn an_encoder_that_puts_out_more_than_it_was_brought_down_to_is_created_once_more() {
    // Sixty frames a second: the encoder puts out twice its bitrate, which was seen.
    let mut qos = restart_only_session();
    for _ in 0..30 {
        second_out(&mut qos, 10, 1.0, 2.0);
    }
    assert!((qos.overshoot() - 2.0).abs() < 0.01);
    // The path carries 10 of the 42 Mbps really sent: the encoder is created again for 8.5 of
    // real output.
    for delay in [1100, 2200, 3300] {
        second_out(&mut qos, delay, 10.0 / 42.0, 2.0);
    }
    assert!(qos.take_restart_request(DISPLAY));
    recreate(&mut qos);
    let to = qos.ratio;
    assert!(
        (BASE_KBPS * to * 2.0 - 8_500.0).abs() < 300.0,
        "ratio {to} for 8.5 Mbps out"
    );
    // Its bitrate is not proportional to the ratio: at the new one it puts out 2.6 times it,
    // 11 Mbps. Seen in a second of its own, and corrected two seconds after the encoder was
    // created, without waiting for the path to fill again.
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_out(&mut qos, 10, 1.0, 2.6);
        seconds += 1;
        assert!(seconds < 10);
    }
    assert!((2..=4).contains(&seconds), "{seconds} s");
    assert!(
        (BASE_KBPS * qos.ratio * 2.6 - 8_500.0).abs() < 300.0,
        "ratio {} for 8.5 Mbps out",
        qos.ratio
    );
    assert_eq!(qos.qos_state(), 1);
}

#[test]
fn an_encoder_that_cannot_change_is_held_to_the_relays_limit_too() {
    let mut qos = restart_only_session();
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    // 21 Mbps asked for through a relay of 10: created again for the limit, once it has run
    // five seconds, and not counted as lowered for the network.
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_at(&mut qos, 10, 1.0);
        seconds += 1;
        assert!(seconds < 10);
    }
    recreate(&mut qos);
    assert!((BASE_KBPS * qos.ratio - 10_000.0).abs() < 100.0);
    assert_eq!(qos.qos_state(), 0);
    // At the limit it stays, and is not created again and again for a ratio that hardly moves.
    for _ in 0..60 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
    // Sixty frames a second now: it puts out twice its bitrate, 20 Mbps through a relay of 10.
    // That is seen though the ratio asked for is not quite the one it runs at, and it is
    // created again for the limit.
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_out(&mut qos, 10, 1.0, 2.0);
        seconds += 1;
        assert!(seconds < 10);
    }
    recreate(&mut qos);
    let out = BASE_KBPS * qos.ratio * 2.0;
    assert!((out - 10_000.0).abs() < 100.0, "{out} kbps out");
    // And at thirty again, for good, it is let back up to the limit.
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_out(&mut qos, 10, 1.0, 1.0);
        seconds += 1;
        assert!(seconds < 20);
    }
    recreate(&mut qos);
    assert!((BASE_KBPS * qos.ratio - 10_000.0).abs() < 100.0);
    assert_eq!(qos.qos_state(), 0);
}

/// The video loop starts over with a new encoder of the same kind, at the ratio and for the frame
/// rate it reads.
fn recreate(qos: &mut VideoQoS) {
    qos.remove_display(DISPLAY);
    qos.new_display(DISPLAY.to_owned());
    let ratio = qos.ratio_for_creation();
    qos.store_bitrate_at(ratio, (BASE_KBPS * ratio) as u32);
    qos.set_support_changing_quality(DISPLAY, false);
    qos.set_restartable(DISPLAY, true);
    let fps = qos.asked_fps();
    qos.set_encoder_hardware(DISPLAY, true, fps);
}

#[test]
fn a_custom_quality_moved_is_given_to_an_encoder_that_cannot_change_by_a_restart() {
    let mut qos = restart_only_session();
    recreate(&mut qos);
    for _ in 0..5 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY), "nothing moved");
    // The viewer drags the slider to 300% and on to 250% and back: the ratio follows at once,
    // the encoder once, and when it has run five seconds since it was last created.
    recreate(&mut qos);
    qos.user_image_quality(1, 300 << 8);
    for quality in [250, 300, 300, 300] {
        qos.user_image_quality(1, quality << 8);
        second_at(&mut qos, 10, 1.0);
        assert!(!qos.take_restart_request(DISPLAY));
    }
    assert_eq!(qos.ratio, 6.0);
    second_at(&mut qos, 10, 1.0);
    assert!(qos.take_restart_request(DISPLAY));
    recreate(&mut qos);
    assert_eq!(
        qos.bitrate(),
        (BASE_KBPS * 6.0) as u32,
        "its bitrate is the ratio's again"
    );
    for _ in 0..60 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY), "once");
    assert_eq!(qos.qos_state(), 0);
}

/// For the restart-only encoder: its bitrate is the one it was created with, which the test
/// keeps in step by hand where the video loop would have restarted.
fn sync_at(qos: &mut VideoQoS) {
    let bitrate = (BASE_KBPS * qos.ratio) as u32;
    qos.store_bitrate(bitrate);
}

fn second_at(qos: &mut VideoQoS, delay: u32, delivered: f32) {
    second_out(qos, delay, delivered, 1.0);
}

/// The same, the encoder putting out `share` of its bitrate.
fn second_out(qos: &mut VideoQoS, delay: u32, delivered: f32, share: f32) {
    second_out_of(qos, delay, delivered, share, 60);
}

/// The same, in `frames` frames.
fn second_out_of(qos: &mut VideoQoS, delay: u32, delivered: f32, share: f32, frames: usize) {
    qos.advance_ms(1000);
    let bytes = (qos.bitrate() as f32 * share * 1000.0 / 8.0) as u64;
    qos.user_delivery(
        1,
        bytes,
        Duration::from_secs(1),
        Duration::from_secs_f32(1.0 / delivered),
    );
    qos.user_network_delay(1, delay);
    qos.update_display_rates(
        DISPLAY,
        bytes as usize,
        frames,
        Duration::ZERO,
        Duration::from_secs(1),
    );
    qos.update_display_data(DISPLAY, frames);
}

#[test]
fn an_encoder_that_may_not_be_created_again_keeps_its_bitrate() {
    let mut qos = session(3.5);
    qos.set_support_changing_quality(DISPLAY, false);
    // Not restartable: Wayland, or a recording under way.
    for _ in 0..40 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    for delay in [1100, 2200, 3300, 3300, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
    assert_eq!(qos.ratio(), 3.5);
}

#[test]
fn a_path_a_little_too_narrow_brings_the_encoder_down_as_well() {
    let mut qos = restart_only_session();
    for _ in 0..40 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    // The path delivers 96% of what is sent, second after second. An encoder left a little over
    // what the path carries would stay over it for good: it is created again for just under,
    // which costs a tenth of a second. So small a cut is not one the viewer is told of.
    for delay in [300, 400, 500] {
        second(&mut qos, delay, 0.96, 1.0);
    }
    assert!(qos.take_restart_request(DISPLAY));
    let kbps = BASE_KBPS * qos.ratio;
    assert!(
        (0.80 * 21_000.0..=0.83 * 21_000.0).contains(&kbps),
        "{kbps} kbps under a path that carries 96% of 21 Mbps"
    );
    assert_eq!(qos.qos_state(), 0);
}

#[test]
fn an_encoder_at_the_floor_is_not_created_again_and_again() {
    // Brought down as far as the ratio goes, a hardware encoder still puts out more than the
    // path was seen to carry: another one at the same ratio would put out the same.
    let mut qos = restart_only_session();
    for _ in 0..30 {
        second_out(&mut qos, 10, 1.0, 2.0);
    }
    for delay in [1100, 2200, 3300] {
        second_out(&mut qos, delay, 0.02, 2.0);
    }
    assert!(qos.take_restart_request(DISPLAY));
    recreate(&mut qos);
    let floor = qos.ratio;
    let mut asked = 0;
    for _ in 0..30 {
        second_out(&mut qos, 10, 1.0, 2.0);
        if qos.take_restart_request(DISPLAY) {
            asked += 1;
            recreate(&mut qos);
        }
    }
    assert!(qos.ratio >= floor, "{} from {floor}", qos.ratio);
    assert!(asked <= 2, "created again {asked} times in half a minute");
}

#[test]
fn a_preset_is_held_to_the_same_distance_between_restarts() {
    // The best preset on an encoder that takes a new bitrate only by being created again: the
    // controller says when, as for a custom quality, and the video loop does not create it
    // again for every ratio the controller passes through (`check_qos` asks `restart_only`).
    let mut qos = super::smoke::session(60, Quality::Best);
    qos.abr_config = true;
    qos.new_display(DISPLAY.to_owned());
    qos.ratio = BR_BEST;
    sync(&mut qos);
    qos.set_support_changing_quality(DISPLAY, false);
    qos.set_restartable(DISPLAY, true);
    qos.set_encoder_hardware(DISPLAY, true, 60);
    assert!(qos.restart_only() && !qos.in_vbr_state());
    for _ in 0..10 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
    // The viewer takes the balanced preset: the ratio follows, and the encoder is asked for
    // once.
    qos.user_image_quality(1, ImageQuality::Balanced.value());
    second_at(&mut qos, 10, 1.0);
    assert_eq!(qos.ratio, BR_BALANCED);
    assert!(qos.take_restart_request(DISPLAY));
    assert!(!qos.take_restart_request(DISPLAY));
}

#[test]
fn every_display_is_asked_for_itself() {
    let mut qos = restart_only_session();
    for _ in 0..40 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
    }
    // A second display's loop started meanwhile: both are asked, each once.
    qos.new_display("second".to_owned());
    qos.set_support_changing_quality("second", false);
    qos.set_restartable("second", true);
    let before = qos.ratio;
    sync_at(&mut qos);
    for _ in 0..40 {
        second_at(&mut qos, 10, 1.0);
        qos.update_display_data("second", 60);
    }
    assert!(qos.ratio > before);
    for name in [DISPLAY, "second"] {
        assert!(qos.take_restart_request(name), "{name}");
        assert!(!qos.take_restart_request(name), "{name} once");
    }
}

#[test]
fn an_encoder_held_down_is_let_go_when_it_may_no_longer_be_created_again() {
    let mut qos = restart_only_session();
    for _ in 0..40 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
    }
    assert!(qos.take_restart_request(DISPLAY));
    assert!(qos.ratio < 3.5);
    // A recording starts: the loop starts over with an encoder that may not be created again
    // for the network.  It is created once more, at the target, and left alone.
    qos.remove_display(DISPLAY);
    qos.new_display(DISPLAY.to_owned());
    qos.set_support_changing_quality(DISPLAY, false);
    qos.set_restartable(DISPLAY, false);
    sync_at(&mut qos);
    second_at(&mut qos, 10, 1.0);
    assert_eq!(qos.ratio, 3.5);
    assert!(qos.take_restart_request(DISPLAY));
    assert_eq!(qos.qos_state(), 0);
    for _ in 0..40 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
}

#[test]
fn a_quality_that_comes_while_the_encoder_is_created_is_climbed_to_as_well() {
    // The display is there, its encoder not yet: what it can do is not known.
    for follows in [true, false] {
        let mut qos = super::smoke::session(60, Quality::Balanced);
        qos.abr_config = true;
        qos.new_display(DISPLAY.to_owned());
        qos.user_image_quality(1, 300 << 8); // custom 300%: ratio 6
        assert_eq!(qos.ratio(), capacity::START_RATIO);
        // The encoder is created at the target, whatever it turns out to be.
        assert_eq!(qos.ratio_for_creation(), 6.0);
        qos.set_support_changing_quality(DISPLAY, follows);
        if follows {
            // It follows the ratio: brought to where the climb is, which goes on.
            assert_eq!(qos.ratio(), capacity::START_RATIO);
            assert!(qos.capacity.starting());
        } else {
            // It does not: it has its target, as it always did, and there is no climb.
            assert_eq!(qos.ratio(), 6.0);
            assert!(!qos.capacity.starting());
            assert_eq!(qos.ratio_for_creation(), 6.0);
        }
    }
}

#[test]
fn a_viewer_coming_or_going_is_not_the_network_coming_back() {
    let mut qos = session(3.5);
    settle(&mut qos);
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
    }
    assert_eq!(qos.qos_state(), 1);
    let lowered = qos.ratio();
    // Another viewer opens and closes: the stream is where it was, and still lowered.
    qos.users.insert(2, qos.users[&1].clone());
    qos.viewers_changed();
    assert_eq!((qos.ratio(), qos.qos_state()), (lowered, 1));
    qos.users.remove(&2);
    qos.viewers_changed();
    assert_eq!((qos.ratio(), qos.qos_state()), (lowered, 1));
    // What the path carried is forgotten with it, so the way back is the plain one, and once
    // the stream is back the viewer is told.
    for _ in 0..60 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert_eq!((qos.ratio(), qos.qos_state()), (3.5, 0));
}

#[test]
fn a_hardware_encoder_is_held_to_a_limit_by_what_it_puts_out() {
    let mut qos = session(3.5);
    qos.set_encoder_hardware(DISPLAY, true, 30);
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    // A hardware encoder is taken to put out a thirtieth of its bitrate a frame, twice its
    // bitrate at these sixty frames a second, until two seconds show that this one keeps to
    // its bitrate: once its frame rate is seen the stream is at half the relay's limit, and
    // comes up to it.
    second(&mut qos, 10, 1.0, 1.0);
    second(&mut qos, 10, 1.0, 1.0);
    let started = BASE_KBPS * qos.ratio();
    assert!((started - 5_000.0).abs() < 100.0, "{started} kbps at first");
    for _ in 0..25 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    let settled = qos.ratio();
    assert!((BASE_KBPS * settled - 10_000.0).abs() < 100.0);
    // One second a third over its bitrate, as one of many key frames is: not taken for the
    // encoder, and nothing moves for it.
    second(&mut qos, 10, 1.0, 1.3);
    assert_eq!(qos.overshoot(), 1.0);
    assert_eq!(qos.ratio(), settled);
    for _ in 0..3 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert_eq!(qos.ratio(), settled);
    // Sixty frames a second: twice its bitrate, second after second. The stream comes under
    // the relay's limit by what is really sent, and stays there.
    for _ in 0..8 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    assert!((qos.overshoot() - 2.0).abs() < 0.01);
    let out = BASE_KBPS * qos.ratio() * 2.0;
    assert!(
        (out - 10_000.0).abs() < 300.0,
        "{out} kbps out under 10 Mbps"
    );
    // Thirty again: it keeps to its bitrate, which is believed with the first second that says
    // so, and the stream is back by the ordinary way up.
    for _ in 0..18 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert_eq!(qos.overshoot(), 1.0);
    assert!(
        (qos.ratio() - settled).abs() < settled * 0.02,
        "{} of {settled}",
        qos.ratio()
    );
}

#[test]
fn a_software_encoder_is_never_taken_to_put_out_more_than_its_bitrate() {
    // It keeps to its bitrate over time. A second over it (a change of scene, whole frames
    // where others were left out) is not held against it, under a limit or not.
    let mut qos = session(3.5);
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    for _ in 0..6 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    let settled = qos.ratio();
    for share in [2.0, 0.4, 1.6, 2.0, 2.0, 2.0] {
        second(&mut qos, 10, 1.0, share);
        assert_eq!(qos.ratio(), settled);
        assert_eq!(qos.overshoot(), 1.0);
    }
    // And what a hardware encoder was seen to put out is not carried over to it.
    qos.set_encoder_hardware(DISPLAY, true, 30);
    for _ in 0..4 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    assert!((qos.overshoot() - 2.0).abs() < 0.01);
    qos.set_encoder_hardware(DISPLAY, false, 30);
    assert_eq!(qos.overshoot(), 1.0);
}

#[test]
fn an_encoder_that_leaves_frames_out_is_not_brought_down_for_its_whole_frames() {
    // Under a limit of 1 Mbps an encoder makes of a picture that is new in every frame one
    // frame of 500 kbit in some seconds, two or three in others: 1 Mbps on the whole. No second
    // of it is taken for what the encoder puts out, hardware or not, and the ratio stays where
    // the limit put it.
    let mut qos = session(3.5);
    qos.set_encoder_hardware(DISPLAY, true, 30);
    qos.relay_cap_kbps = 1_000;
    qos.user_relayed(1, true);
    for _ in 0..25 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    let settled = qos.ratio();
    assert!((BASE_KBPS * settled - 1_000.0).abs() < 20.0);
    for frames in [3, 1, 2, 3, 1, 2, 2, 3, 1, 2, 2, 2] {
        second_of(&mut qos, 10, 1.0, frames as f32 * 0.5, frames);
        assert_eq!(qos.ratio(), settled, "after a second of {frames} frames");
        assert_eq!(qos.overshoot(), 1.0);
    }
}

#[test]
fn a_cut_under_the_relays_limit_is_told_as_any_other() {
    // The default through the relay: the stream rides the relay's limit, not the target.
    let mut qos = session(3.5);
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    for _ in 0..10 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert!((BASE_KBPS * qos.ratio() - 10_000.0).abs() < 300.0);
    assert_eq!(qos.qos_state(), 0);
    // The path carries 4 of the 10 Mbps.
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 0.4, 1.0);
    }
    assert!(BASE_KBPS * qos.ratio() < 4_000.0);
    assert_eq!(qos.qos_state(), 1, "lowered below what the relay allows");
}

#[test]
fn the_climb_that_ends_on_a_queue_is_not_raised_in_the_same_breath() {
    let mut qos = super::smoke::session(60, Quality::Balanced);
    qos.abr_config = true;
    qos.new_display(DISPLAY.to_owned());
    qos.set_support_changing_quality(DISPLAY, true);
    qos.user_image_quality(1, 300 << 8);
    let mut seconds = 0;
    while qos.ratio() < 3.0 {
        second(&mut qos, 10, 1.0, 1.0);
        seconds += 1;
        assert!(seconds < 30);
    }
    // Replies 70 ms late: a queue is forming.  With the second of them the average says so, and
    // the climb ends where it is.
    second(&mut qos, 80, 1.0, 1.0);
    second(&mut qos, 80, 1.0, 1.0);
    assert!(!qos.capacity.starting());
    let reached = qos.ratio();
    assert!(reached < 6.0);
    // For the next three seconds the ordinary way up, which such replies still allow, does not
    // add to it.
    for _ in 0..2 {
        second(&mut qos, 80, 1.0, 1.0);
        assert_eq!(qos.ratio(), reached);
    }
    // With good replies again it goes on, by the ordinary way.
    for _ in 0..12 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert!(qos.ratio() > reached);
}

#[test]
fn what_was_sent_over_a_limit_brings_the_ratio_down_once() {
    let mut qos = session(3.5);
    qos.set_encoder_hardware(DISPLAY, true, 30);
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    for _ in 0..10 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    let settled = qos.ratio();
    // A second at 1.3 times its bitrate: 13 Mbps for a limit of 10.  The ratio comes down by
    // 10/13, whether the way up is due in the same second or not, and not by its square.
    for _ in 0..4 {
        second(&mut qos, 10, 1.0, 1.3);
        let least = settled * (10.0 / 13.0) * 0.97;
        assert!(qos.ratio() >= least, "{} of {settled}", qos.ratio());
    }
}

#[test]
fn lowered_stays_said_when_more_is_asked_of_a_full_path() {
    let mut qos = session(3.5);
    settle(&mut qos);
    for delay in [1100, 2200, 3300] {
        second(&mut qos, delay, 10.0 / 21.0, 1.0);
    }
    assert_eq!(qos.qos_state(), 1);
    // The viewer asks for still more: the stream is as short of it as it was.
    qos.user_image_quality(1, 300 << 8);
    assert_eq!(qos.qos_state(), 1);
    // The viewer settles for a preset the stream is above: nothing is lowered any more.
    qos.user_image_quality(1, ImageQuality::Low.value());
    assert_eq!(qos.qos_state(), 0);
}

#[test]
fn a_hardware_encoder_is_set_to_put_out_its_bitrate_at_the_frame_rate_allowed() {
    use scrap::codec::FpsSizing;
    // Created for sixty frames a second, it is set to its bitrate there.
    let sizing = FpsSizing::new(60);
    assert_eq!(sizing.bitrate(6_000), 6_000);
    // Thirty are allowed: twice its bitrate, every frame getting a thirtieth of the bitrate it
    // puts out at thirty; and so for every bitrate after that, until another frame rate.
    let sizing = sizing.resized(30);
    assert_eq!((sizing.sized(), sizing.bitrate(6_000)), (30, 12_000));
    assert_eq!(sizing.bitrate(3_000), 6_000);
    // Ten: no more than twice its bitrate, so it is sized for thirty and puts out a third of its
    // bitrate at ten.
    let sizing = sizing.resized(10);
    assert_eq!((sizing.sized(), sizing.bitrate(6_000)), (30, 12_000));
    // Sixty again, and none asked for: its bitrate.
    let sizing = sizing.resized(60);
    assert_eq!((sizing.sized(), sizing.bitrate(6_000)), (60, 6_000));
    assert_eq!(sizing.resized(0), sizing);
    // Created for thirty and asked for sixty: half.
    let sizing = FpsSizing::new(30).resized(60);
    assert_eq!((sizing.sized(), sizing.bitrate(6_000)), (60, 3_000));
}

#[test]
fn a_hardware_encoder_sized_for_the_frame_rate_is_taken_at_its_bitrate() {
    let mut qos = session(3.5);
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    // Sized for the sixty frames a second the viewer allows, it puts out its bitrate: from the
    // first second the stream is held to the relay's limit, and not to half of it.
    qos.set_encoder_hardware(DISPLAY, true, 60);
    for _ in 0..3 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert_eq!(qos.overshoot(), 1.0);
    assert!((BASE_KBPS * qos.ratio() - 10_000.0).abs() < 100.0);
}

#[test]
fn a_lower_frame_rate_allowed_is_not_reckoned_at_the_frames_before_it() {
    let mut qos = session(3.5);
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    // An encoder sized for thirty at sixty frames a second: twice its bitrate.
    qos.set_encoder_hardware(DISPLAY, true, 30);
    for _ in 0..5 {
        second(&mut qos, 10, 1.0, 2.0);
    }
    assert!((qos.overshoot() - 2.0).abs() < 0.01);
    // The viewer allows thirty now: the sixty frames of the seconds before do not count, and it
    // is taken at its bitrate at once.
    qos.user_custom_fps(1, 30);
    assert_eq!(qos.overshoot(), 1.0);
}

#[test]
fn an_encoder_that_cannot_change_is_created_again_for_a_higher_frame_rate_once() {
    let mut qos = restart_only_session();
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    // Created for thirty frames a second, as through a relay the viewer first asks for, and
    // brought to the relay's limit for it; then the viewer allows sixty, and it puts out twice
    // its bitrate.
    qos.user_custom_fps(1, 30);
    recreate(&mut qos);
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_out_of(&mut qos, 10, 1.0, 1.0, 30);
        seconds += 1;
        assert!(seconds < 10);
    }
    recreate(&mut qos);
    assert!((BASE_KBPS * qos.ratio - 10_000.0).abs() < 100.0);
    qos.user_custom_fps(1, 60);
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_out(&mut qos, 10, 1.0, 2.0);
        seconds += 1;
        assert!(seconds < 10);
    }
    // Created again for sixty, at the same ratio: it puts out its bitrate again, the relay's
    // limit, and is not created once more for the limit.
    recreate(&mut qos);
    assert!((BASE_KBPS * qos.ratio - 10_000.0).abs() < 100.0);
    for _ in 0..30 {
        second_out(&mut qos, 10, 1.0, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
}

#[test]
fn an_encoder_that_cannot_change_follows_a_frame_rate_that_moves_far_and_not_near() {
    let mut qos = restart_only_session();
    for _ in 0..10 {
        second_at(&mut qos, 10, 1.0);
    }
    // The viewer's decoder allows a little less now and then: not worth a restart.
    for fps in [54, 58, 50, 60] {
        qos.user_custom_fps(1, fps);
        for _ in 0..10 {
            second_at(&mut qos, 10, 1.0);
        }
        assert!(!qos.take_restart_request(DISPLAY), "{fps} fps");
    }
    // It allows thirty: the encoder created for sixty puts out half its bitrate, and is created
    // again for thirty.
    qos.user_custom_fps(1, 30);
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_out_of(&mut qos, 10, 1.0, 0.5, 30);
        seconds += 1;
        assert!(seconds < 10);
    }
    recreate(&mut qos);
    for _ in 0..30 {
        second_out_of(&mut qos, 10, 1.0, 1.0, 30);
    }
    assert!(!qos.take_restart_request(DISPLAY));
    // And a little more than it was created for, now and then: not worth a restart either.
    qos.user_custom_fps(1, 36);
    for _ in 0..10 {
        second_out_of(&mut qos, 10, 1.0, 1.0, 36);
    }
    assert!(!qos.take_restart_request(DISPLAY));
}

#[test]
fn an_encoder_held_down_is_not_created_again_for_a_lower_frame_rate() {
    let mut qos = restart_only_session();
    for _ in 0..40 {
        second_at(&mut qos, 10, 1.0);
    }
    // Brought down for a path that carries 10 of the 21 Mbps.
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_at(&mut qos, 1100 + seconds * 300, 10.0 / 21.0);
        seconds += 1;
        assert!(seconds < 15);
    }
    recreate(&mut qos);
    assert_eq!(qos.qos_state(), 1);
    // The viewer allows thirty frames a second: held down, the encoder putting out less does no
    // harm, and the wait to step back up is not started anew for a restart.
    qos.user_custom_fps(1, 30);
    for _ in 0..20 {
        second_out_of(&mut qos, 10, 1.0, 0.5, 30);
        assert!(!qos.take_restart_request(DISPLAY));
    }
}

#[test]
fn a_congested_path_does_not_keep_an_encoder_from_being_created_for_a_higher_frame_rate() {
    let mut qos = restart_only_session();
    // Created for thirty frames a second; the viewer allows sixty and it puts out twice its
    // bitrate, 42 Mbps on a path of 30: replies come late. Brought down to where an encoder
    // created for sixty would settle, it would hardly come down at all (no restart for the
    // path is worth it); it is created again for the frame rate all the same.
    qos.user_custom_fps(1, 30);
    recreate(&mut qos);
    qos.user_custom_fps(1, 60);
    for _ in 0..6 {
        second_out(&mut qos, 10, 1.0, 2.0);
    }
    let _ = qos.take_restart_request(DISPLAY);
    let mut seconds = 0;
    while !qos.take_restart_request(DISPLAY) {
        second_out(&mut qos, 1100 + seconds * 300, 30.0 / 42.0, 2.0);
        seconds += 1;
        assert!(seconds < 10);
    }
    recreate(&mut qos);
    assert_eq!(qos.capacity.overshoot_at(60, qos.asked_fps()), 1.0);
}

#[test]
fn a_second_display_with_a_software_encoder_is_not_reckoned_with() {
    let mut qos = session(3.5);
    qos.set_encoder_hardware(DISPLAY, true, 60);
    // Another display's encoder is a software one, created when thirty were allowed: it keeps to
    // its bitrate at any frame rate, and says nothing of the hardware one's.
    qos.new_display("second".to_owned());
    qos.set_support_changing_quality("second", true);
    qos.set_encoder_hardware("second", false, 30);
    assert!(!qos.fps_risen());
    assert_eq!(qos.overshoot(), 1.0);
}

#[test]
fn the_second_an_encoder_is_sized_anew_in_says_nothing_of_it() {
    let mut qos = session(3.5);
    qos.set_encoder_hardware(DISPLAY, true, 60);
    for _ in 0..3 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert_eq!(qos.overshoot(), 1.0);
    // Sized for thirty in the middle of a second that ran at sixty and so put out its bitrate:
    // not taken for an encoder that, sized for thirty, puts out half at sixty frames.
    qos.set_encoder_hardware(DISPLAY, true, 30);
    second(&mut qos, 10, 1.0, 1.0);
    second(&mut qos, 10, 1.0, 2.0);
    assert!((qos.overshoot() - 2.0).abs() < 0.01);
}
