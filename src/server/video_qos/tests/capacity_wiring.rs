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
        60,
        Duration::ZERO,
        Duration::from_secs(1),
    );
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

    // An encoder that puts out twice its bitrate is held to the limit by what it really sends.
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

/// One viewer of one display whose encoder takes a new bitrate only by being created again.
fn restart_only_session() -> VideoQoS {
    let mut qos = session(3.5);
    qos.set_support_changing_quality(DISPLAY, false);
    qos.set_restartable(DISPLAY, true);
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
    // Half a minute after it was created, a full path brings it down, straight to where the
    // stream settles.
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
    for _ in 0..40 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
    // After five minutes: half again, not all the way.
    let before = qos.ratio;
    for _ in 0..300 {
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
    // After five more minutes half again would come within a tenth of the target: all the way,
    // by a restart, and the viewer is told.
    for _ in 0..300 {
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

/// The video loop starts over with a new encoder of the same kind, at the ratio it reads.
fn recreate(qos: &mut VideoQoS) {
    qos.remove_display(DISPLAY);
    qos.new_display(DISPLAY.to_owned());
    let ratio = qos.ratio_for_creation();
    qos.store_bitrate_at(ratio, (BASE_KBPS * ratio) as u32);
    qos.set_support_changing_quality(DISPLAY, false);
    qos.set_restartable(DISPLAY, true);
}

#[test]
fn a_custom_quality_moved_is_given_to_an_encoder_that_cannot_change_by_a_restart() {
    let mut qos = restart_only_session();
    recreate(&mut qos);
    for _ in 0..5 {
        second_at(&mut qos, 10, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY), "nothing moved");
    // The viewer drags the slider to 300%: the ratio follows at once, the encoder not sooner
    // than half a minute after it was created, and once.
    qos.user_image_quality(1, 300 << 8);
    for _ in 0..20 {
        second_at(&mut qos, 10, 1.0);
        assert_eq!(qos.ratio, 6.0);
        assert!(!qos.take_restart_request(DISPLAY));
    }
    for _ in 0..10 {
        second_at(&mut qos, 10, 1.0);
    }
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
    qos.advance_ms(1000);
    let bytes = (qos.bitrate() as f32 * 1000.0 / 8.0) as u64;
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
        60,
        Duration::ZERO,
        Duration::from_secs(1),
    );
    qos.update_display_data(DISPLAY, 60);
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
fn a_cut_not_worth_the_pause_leaves_nothing_behind() {
    let mut qos = restart_only_session();
    for _ in 0..40 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    // The path delivers 96% of what is sent: just under it would be a fifth less at most.
    for delay in [300, 400, 500] {
        second(&mut qos, delay, 0.96, 1.0);
    }
    assert!(!qos.take_restart_request(DISPLAY));
    assert_eq!(qos.ratio(), 3.5);
    assert!(
        !qos.capacity.has_ceiling(),
        "as if it had not been looked at"
    );
    assert_eq!(qos.qos_state(), 0);
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
    for _ in 0..300 {
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
fn a_change_of_scene_over_the_limit_is_not_taken_for_the_encoder() {
    let mut qos = session(3.5);
    qos.relay_cap_kbps = 10_000;
    qos.user_relayed(1, true);
    for _ in 0..6 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    let settled = qos.ratio();
    // One second a third over its bitrate: held to the limit for that second, not remembered,
    // and the stream is back where it was two steps of the way up later.
    second(&mut qos, 10, 1.0, 1.3);
    assert!(qos.ratio() < settled * 0.85);
    assert_eq!(qos.capacity.overshoot(), 1.0);
    for _ in 0..7 {
        second(&mut qos, 10, 1.0, 1.0);
    }
    assert!(
        (qos.ratio() - settled).abs() < settled * 0.02,
        "{} of {settled}",
        qos.ratio()
    );
    // Twice its bitrate is the encoder, and remembered at once.
    second(&mut qos, 10, 1.0, 2.0);
    second(&mut qos, 10, 1.0, 2.0);
    assert!(qos.capacity.overshoot() > 1.5);
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
