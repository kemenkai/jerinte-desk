//! What a real path did that the scenarios of `capacity_sim` did not: measured on 2026-11-17 with
//! a Windows machine without a GPU as the controlled side, through the relay, the path to the
//! viewer cut from open to 3 Mbps for eighty seconds.
//!
//! The picture was a page scrolled fast, new in every frame: the software encoder could not make
//! a frame of it smaller than some forty kilobytes, so below the bitrate that takes it kept to
//! the bitrate by leaving frames out, and what went out in a second was a whole number of frames.
//! It left them out only once its rate control buffer ran low: with libvpx's own buffer, of up
//! to six seconds, it went on sending two or three times its bitrate for seconds after each cut,
//! and the queue stood again and again (measured once more with the fix that follows, on the
//! same day). The encoder is now given a buffer of a second (`VPX_BUFFER_MS`).
//! The picture's source gave twelve frames a second. At ratio 1 the bitrate was 1074 kbps
//! (1280x800), the target ratio 6, and the relay's limit 10 Mbps.
use super::sim::{run, Content, EncoderModel, Link, Report, Scenario};
use super::*;

/// 1280x800: the bitrate at ratio 1, in kbps.
const BASE_KBPS: f64 = 1074.0;
/// The quality that is the default: 300%.
const TARGET: f32 = 6.0;
/// What the path carried while it was cut, and when it was.
const CUT_KBPS: f64 = 2_700.0;
const CUT_FROM_MS: u32 = 40_000;
const CUT_TO_MS: u32 = 120_000;

/// How long after the path is cut the stream has found it, sent the queue and come back up to
/// just under what the path carries. The cut is found after eight to ten seconds, by the two
/// late replies the parent asks for; by then the relay holds ten to thirteen seconds of the
/// stream, which go out at three quarters of the path's rate in up to eighteen; a clean reply
/// then ends it, and the next step of the way up comes within three more.
const FOUND_AND_SENT_MS: u32 = 35_000;

/// libvpx's own rate control buffer for VP9, which the encoder ran with: four seconds to start
/// with, five where it aims to be, six at most.
const VP9_BUFFER_MS: (f64, f64, f64) = (4000.0, 5000.0, 6000.0);
/// The one the encoder is given now, as AV1's and as WebRTC's: a second at most.
const VPX_BUFFER_MS: (f64, f64, f64) = (600.0, 600.0, 1000.0);

fn relay_squeeze(seed: u64, frame_kbit: f64) -> Scenario {
    Scenario {
        name: "field_relay_squeeze",
        seconds: 300,
        limit: 60,
        quality: Quality::Custom(TARGET),
        abr: true,
        content: Content::Video,
        encoder: EncoderModel::Floor(frame_kbit),
        link: Link {
            capacity_kbps: vec![
                (0, 50_000.0),
                (CUT_FROM_MS, CUT_KBPS),
                (CUT_TO_MS, 50_000.0),
            ],
            wobble: 0.03,
            base_rtt_ms: 1.0,
            jitter_median_ms: 1.0,
            jitter_sigma: 0.3,
            loss_per_s: 0.0,
            stall_mean_interval_s: 0.0,
            stall_ms: (0.0, 0.0),
        },
        seed,
        reports: true,
        // The relay's buffers took seconds of the stream: no limit.
        send_buffer_kbit: None,
        source_fps: Some(12.0),
        relay_cap_kbps: Some(10_000),
        base_kbps: Some(BASE_KBPS),
        // On Windows the loop looks for a change of privacy mode while it waits: a tenth of its
        // time, more or less.
        loop_wait: Some((0.05, 0.15)),
        restart_only: false,
        encoder_sized_for: None,
        encoder_buffer_ms: Some(VPX_BUFFER_MS),
    }
}

/// The second measurement: a Linux machine with an Intel GPU as the controlled side, direct,
/// 1280x720 at sixty frames a second. The encoder is VAAPI's: it takes a new bitrate only by
/// being created again, and its bitrate is not proportional to the ratio. Created for thirty
/// frames a second, as hardware encoders then were, at sixty it put out twice its bitrate,
/// 14.4 Mbps where 7.2 were asked for. The path was cut to 8 Mbps, of which 7.6 are the
/// stream's.
const VAAPI_BASE_KBPS: f64 = 1000.0;
const VAAPI_CUT_KBPS: f64 = 7_600.0;
/// Created for the sixty frames a second the stream runs at, it puts out its 7.2 Mbps: the same
/// path cut to half of that.
const VAAPI_SIZED_CUT_KBPS: f64 = 3_600.0;

fn vaapi_squeeze(seed: u64) -> Scenario {
    vaapi_cut(seed, VAAPI_CUT_KBPS, Some(30.0))
}

fn vaapi_sized_squeeze(seed: u64) -> Scenario {
    vaapi_cut(seed, VAAPI_SIZED_CUT_KBPS, None)
}

fn vaapi_cut(seed: u64, cut_kbps: f64, sized_for: Option<f64>) -> Scenario {
    Scenario {
        name: "field_vaapi_squeeze",
        seconds: 420,
        encoder: EncoderModel::FixedRate,
        encoder_sized_for: sized_for,
        link: Link {
            capacity_kbps: vec![
                (0, 100_000.0),
                (CUT_FROM_MS, cut_kbps),
                (CUT_TO_MS, 100_000.0),
            ],
            wobble: 0.03,
            base_rtt_ms: 1.0,
            jitter_median_ms: 1.0,
            jitter_sigma: 0.3,
            loss_per_s: 0.0,
            stall_mean_interval_s: 0.0,
            stall_ms: (0.0, 0.0),
        },
        // WebRTC's send buffer holds about a second of the stream, then the loop is held up.
        send_buffer_kbit: Some(8_000.0),
        source_fps: None,
        relay_cap_kbps: None,
        base_kbps: Some(VAAPI_BASE_KBPS),
        loop_wait: None,
        restart_only: true,
        ..relay_squeeze(seed, 0.0)
    }
}

/// What the encoder really puts out at the ratio it was created with, at sixty frames a second,
/// created for thirty (`times` 2) or for sixty (1).
fn vaapi_out_kbps(ratio: f64, times: f64) -> f64 {
    let base = VAAPI_BASE_KBPS * ratio;
    times * base * (1.0 + 1.0 / (1.0 + (base - 2000.0).max(0.0) / 1000.0))
}

/// The nominal bitrate over a stretch of the run, in kbps: the least, the mean and the last.
fn kbps(r: &Report, from_ms: u32, to_ms: u32) -> (f64, f64, f64) {
    let ratios: Vec<f64> = r
        .trace
        .iter()
        .filter(|(at, ..)| *at >= from_ms && *at < to_ms)
        .map(|(.., ratio)| *ratio as f64 * BASE_KBPS)
        .collect();
    let least = ratios.iter().copied().fold(f64::MAX, f64::min);
    let mean = ratios.iter().sum::<f64>() / ratios.len().max(1) as f64;
    (least, mean, *ratios.last().unwrap_or(&0.0))
}

fn print_timeline(r: &Report) {
    println!("s  kbps(nominal)  queue_ms  fps");
    for (at, fps, queue_ms, ratio) in r.trace.iter().filter(|(at, ..)| at % 2000 == 0) {
        println!(
            "{:>3} {:>6.0} {:>6} {:>3}",
            at / 1000,
            *ratio as f64 * BASE_KBPS,
            queue_ms,
            fps
        );
    }
}

/// Prints one run second by second, to set against the log of the real one:
/// `QOS_SIM_DEBUG=1 cargo test --lib field_timeline -- --ignored --nocapture` prints the
/// controller's state as well; `QOS_SIM_SCENARIO=vaapi` runs the second measurement (`vaapi60`
/// with the encoder created for sixty frames a second),
/// `QOS_SIM_FRAME=320` the first with smaller frames, `QOS_SIM_SEED=6` another seed.
#[test]
#[ignore]
fn field_timeline() {
    let frame_kbit = std::env::var("QOS_SIM_FRAME")
        .ok()
        .and_then(|v| v.parse().ok());
    let seed = std::env::var("QOS_SIM_SEED")
        .ok()
        .and_then(|v| v.parse().ok());
    let seed = seed.unwrap_or(1);
    let sc = match std::env::var("QOS_SIM_SCENARIO").as_deref() {
        Ok("vaapi") => vaapi_squeeze(seed),
        Ok("vaapi60") => vaapi_sized_squeeze(seed),
        _ => relay_squeeze(seed, frame_kbit.unwrap_or(540.0)),
    };
    print_timeline(&run(&sc));
}

#[test]
fn a_cut_path_is_used_and_an_open_one_is_gone_back_to() {
    // A frame of forty kilobytes, as the encoder made at its lowest quality, and of sixty-seven,
    // as it made at the quality asked for.
    for frame_kbit in [320.0, 540.0] {
        for seed in 1..=10 {
            let r = run(&relay_squeeze(seed, frame_kbit));
            let run = format!("frames of {frame_kbit} kbit, seed {seed}");
            // While the path is cut the stream never goes under a fifth of what it carries: it
            // is at a quarter while the queue is sent, and nothing takes it lower.
            let (least, ..) = kbps(&r, CUT_FROM_MS, CUT_TO_MS);
            assert!(
                least >= CUT_KBPS / 5.0,
                "{run}: down to {least:.0} kbps on a path that carries {CUT_KBPS:.0}"
            );
            // Once the cut is found and the queue sent, it uses two thirds of the path and more.
            // It goes to half of what the path was measured to carry for a second or two each
            // time it has tried for more and met the limit again (measured, that is a little
            // under what the path carries); paced, the queue of such a try is sent in a second
            // or two, and the replies that met it may still take the table one step lower for
            // a few seconds, to a third of the path. Never lower.
            let (least, mean, _) = kbps(&r, CUT_FROM_MS + FOUND_AND_SENT_MS, CUT_TO_MS);
            assert!(
                least >= CUT_KBPS / 3.0,
                "{run}: down to {least:.0} kbps with the queue sent"
            );
            assert!(
                mean >= CUT_KBPS * 2.0 / 3.0,
                "{run}: {mean:.0} kbps on average on a path that carries {CUT_KBPS:.0}"
            );
            // And no more than the path carries, but for the tries past it: a queue a person
            // feels (half a second) stands for a tenth of that time at most.
            let queues: Vec<u32> = r
                .trace
                .iter()
                .filter(|(at, ..)| *at >= CUT_FROM_MS + FOUND_AND_SENT_MS && *at < CUT_TO_MS)
                .map(|(_, _, queue_ms, _)| *queue_ms)
                .collect();
            let felt = queues.iter().filter(|queue_ms| **queue_ms >= 500).count();
            assert!(
                felt * 10 <= queues.len(),
                "{run}: a queue of half a second for {felt} tenths of a second"
            );
            // Two minutes after the path is open again the stream is back at what was asked for.
            let (.., last) = kbps(&r, CUT_TO_MS, CUT_TO_MS + 120_000);
            assert!(
                last >= 0.9 * TARGET as f64 * BASE_KBPS,
                "{run}: at {last:.0} kbps two minutes after the path opened"
            );
        }
    }
}

#[test]
fn an_encoder_created_again_for_a_cut_path_gets_there_and_comes_back() {
    for seed in 1..=10 {
        vaapi_gets_there_and_comes_back(&vaapi_squeeze(seed), VAAPI_CUT_KBPS, 2.0);
    }
}

#[test]
fn an_encoder_created_for_the_frame_rate_gets_there_and_comes_back() {
    for seed in 1..=10 {
        vaapi_gets_there_and_comes_back(&vaapi_sized_squeeze(seed), VAAPI_SIZED_CUT_KBPS, 1.0);
    }
}

/// The VAAPI encoder on a path cut to `cut_kbps`, putting out `times` its bitrate.
fn vaapi_gets_there_and_comes_back(sc: &Scenario, cut_kbps: f64, times: f64) {
    let seed = sc.seed;
    let r = run(sc);
    let out = |from_ms: u32, to_ms: u32| -> Vec<f64> {
        r.trace
            .iter()
            .filter(|(at, ..)| *at >= from_ms && *at < to_ms)
            .map(|(.., ratio)| vaapi_out_kbps(*ratio as f64, times))
            .collect()
    };
    // Twenty seconds after the cut the encoder is where the path carries it: it was created
    // again for what the path carried, and once or twice more where its bitrate did not come
    // down as far as the ratio. From then on it puts out at least half of what the path carries,
    // and a queue a person feels (half a second) stands for a tenth of the time at most: while
    // it tries for more, half a minute after it came down, and is brought down again.
    let settled = out(CUT_FROM_MS + 20_000, CUT_TO_MS);
    let least = settled.iter().copied().fold(f64::MAX, f64::min);
    let queues: Vec<u32> = r
        .trace
        .iter()
        .filter(|(at, ..)| *at >= CUT_FROM_MS + 20_000 && *at < CUT_TO_MS)
        .map(|(_, _, queue_ms, _)| *queue_ms)
        .collect();
    let felt = queues.iter().filter(|queue_ms| **queue_ms >= 500).count();
    assert!(
        felt * 10 <= queues.len(),
        "seed {seed}: a queue of half a second for {felt} tenths of a second"
    );
    assert!(
        least >= cut_kbps * 0.5,
        "seed {seed}: down to {least:.0} kbps on a path of {cut_kbps:.0}"
    );
    let mean = settled.iter().sum::<f64>() / settled.len() as f64;
    assert!(
        mean >= cut_kbps * 0.7,
        "seed {seed}: {mean:.0} kbps on average on a path of {cut_kbps:.0}"
    );
    // Four minutes after the path opened the stream is back at what was asked for.
    let last = *out(CUT_TO_MS, CUT_TO_MS + 240_000).last().unwrap();
    assert!(
        last >= 0.9 * vaapi_out_kbps(TARGET as f64, times),
        "seed {seed}: at {last:.0} kbps four minutes after the path opened"
    );
    // Each time the encoder is created again the picture stops for a tenth of a second: two or
    // three times to get under the path, twice for each try for more that fails, four times on
    // the way back.
    println!("seed {seed}: created again {} times", r.restarts);
    assert!(
        r.restarts <= 16,
        "seed {seed}: created {} times",
        r.restarts
    );
}

/// The relay measurement over every seed with libvpx's own buffer and with the encoder's:
/// `cargo test --lib field_buffers -- --ignored --nocapture`.
#[test]
#[ignore]
fn field_buffers() {
    let felt = |r: &Report, from_ms: u32| {
        let queues: Vec<u32> = r
            .trace
            .iter()
            .filter(|(at, ..)| *at >= from_ms && *at < CUT_TO_MS)
            .map(|(_, _, queue_ms, _)| *queue_ms)
            .collect();
        let felt = queues.iter().filter(|q| **q >= 500).count() as f64;
        let worst = queues.iter().copied().max().unwrap_or(0);
        (felt / queues.len() as f64 * 100.0, worst)
    };
    for (name, buffer) in [
        ("libvpx", Some(VP9_BUFFER_MS)),
        ("2 s", Some((1000.0, 1500.0, 2000.0))),
        ("1.5 s", Some((1000.0, 1000.0, 1500.0))),
        ("encoder", Some(VPX_BUFFER_MS)),
    ] {
        for frame_kbit in [320.0, 540.0] {
            let mut rows = Vec::new();
            for seed in 1..=10 {
                let r = run(&Scenario {
                    encoder_buffer_ms: buffer,
                    ..relay_squeeze(seed, frame_kbit)
                });
                let (all, worst_all) = felt(&r, CUT_FROM_MS);
                let (after, worst_after) = felt(&r, CUT_FROM_MS + FOUND_AND_SENT_MS);
                let (_, mean, _) = kbps(&r, CUT_FROM_MS + FOUND_AND_SENT_MS, CUT_TO_MS);
                rows.push((all, worst_all, after, worst_after, mean));
            }
            let n = rows.len() as f64;
            let avg =
                |f: fn(&(f64, u32, f64, u32, f64)) -> f64| rows.iter().map(f).sum::<f64>() / n;
            let max =
                |f: fn(&(f64, u32, f64, u32, f64)) -> f64| rows.iter().map(f).fold(0.0, f64::max);
            println!(
                "{name:>8}, frames of {frame_kbit}: felt {:.1}% of the cut (worst {:.0} ms); after {} s {:.1}% (most {:.1}%, worst {:.0} ms); {:.0} kbps asked",
                avg(|r| r.0),
                max(|r| r.1 as f64),
                FOUND_AND_SENT_MS / 1000,
                avg(|r| r.2),
                max(|r| r.2),
                max(|r| r.3 as f64),
                avg(|r| r.4),
            );
        }
    }
}

/// Prints, for each seed of the relay scenario, how long after the cut the stream came down, the
/// longest queue on the way, and what was used once the cut was found (mean / least kbps):
/// `cargo test --lib field_onset -- --ignored --nocapture`.
#[test]
#[ignore]
fn field_onset() {
    for frame_kbit in [320.0, 540.0] {
        let mut rows = Vec::new();
        for seed in 1..=10 {
            let r = run(&relay_squeeze(seed, frame_kbit));
            let found = r
                .trace
                .iter()
                .find(|(at, _, _, ratio)| *at >= CUT_FROM_MS && *ratio < TARGET * 0.9)
                .map_or(0, |(at, ..)| at - CUT_FROM_MS);
            let worst = r
                .trace
                .iter()
                .filter(|(at, ..)| *at >= CUT_FROM_MS && *at < CUT_FROM_MS + FOUND_AND_SENT_MS)
                .map(|(_, _, queue_ms, _)| *queue_ms)
                .max()
                .unwrap_or(0);
            let (least, mean, _) = kbps(&r, CUT_FROM_MS + FOUND_AND_SENT_MS, CUT_TO_MS);
            rows.push(format!(
                "{:.1}s/{}ms/{:.0}/{:.0}",
                found as f64 / 1000.0,
                worst,
                mean,
                least
            ));
        }
        println!("frames of {frame_kbit}: {}", rows.join(" "));
    }
}
