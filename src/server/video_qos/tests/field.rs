//! What a real path did that the scenarios of `capacity_sim` did not: measured on 2026-11-17 with
//! a Windows machine without a GPU as the controlled side, through the relay, the path to the
//! viewer cut from open to 3 Mbps for eighty seconds.
//!
//! The picture was a page scrolled fast, new in every frame: the software encoder could not make
//! a frame of it smaller than some forty kilobytes, so below the bitrate that takes it kept to
//! the bitrate by leaving frames out, and what went out in a second was a whole number of frames.
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
    }
}

/// The second measurement: a Linux machine with an Intel GPU as the controlled side, direct,
/// 1280x720 at sixty frames a second. The encoder is VAAPI's: it takes a new bitrate only by
/// being created again, its bitrate is not proportional to the ratio, and at sixty frames a
/// second it puts out twice its bitrate, 14.4 Mbps where 7.2 are asked for. The path was cut to
/// 8 Mbps, of which 7.6 are the stream's.
const VAAPI_BASE_KBPS: f64 = 1000.0;
const VAAPI_CUT_KBPS: f64 = 7_600.0;

fn vaapi_squeeze(seed: u64) -> Scenario {
    Scenario {
        name: "field_vaapi_squeeze",
        seconds: 420,
        encoder: EncoderModel::FixedRate,
        link: Link {
            capacity_kbps: vec![
                (0, 100_000.0),
                (CUT_FROM_MS, VAAPI_CUT_KBPS),
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

/// What the encoder really puts out at the ratio it was created with, at sixty frames a second.
fn vaapi_out_kbps(ratio: f64) -> f64 {
    let base = VAAPI_BASE_KBPS * ratio;
    2.0 * base * (1.0 + 1.0 / (1.0 + (base - 2000.0).max(0.0) / 1000.0))
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
/// controller's state as well; `QOS_SIM_SCENARIO=vaapi` runs the second measurement,
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
            // under what the path carries), and never lower.
            let (least, mean, _) = kbps(&r, CUT_FROM_MS + FOUND_AND_SENT_MS, CUT_TO_MS);
            assert!(
                least >= CUT_KBPS * 0.45,
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
        let r = run(&vaapi_squeeze(seed));
        let out = |from_ms: u32, to_ms: u32| -> Vec<f64> {
            r.trace
                .iter()
                .filter(|(at, ..)| *at >= from_ms && *at < to_ms)
                .map(|(.., ratio)| vaapi_out_kbps(*ratio as f64))
                .collect()
        };
        // Twenty seconds after the cut the encoder is where the path carries it: it was created
        // again for what the path carried, and once or twice more where its bitrate did not
        // come down as far as the ratio. From then on it puts out at least half of what the
        // path carries, and a queue a person feels (half a second) stands for a tenth of the
        // time at most: while it tries for more, half a minute after it came down, and is
        // brought down again.
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
            least >= VAAPI_CUT_KBPS * 0.5,
            "seed {seed}: down to {least:.0} kbps on a path of {VAAPI_CUT_KBPS:.0}"
        );
        let mean = settled.iter().sum::<f64>() / settled.len() as f64;
        assert!(
            mean >= VAAPI_CUT_KBPS * 0.7,
            "seed {seed}: {mean:.0} kbps on average on a path of {VAAPI_CUT_KBPS:.0}"
        );
        // Four minutes after the path opened the stream is back at what was asked for.
        let last = *out(CUT_TO_MS, CUT_TO_MS + 240_000).last().unwrap();
        assert!(
            last >= 0.9 * vaapi_out_kbps(TARGET as f64),
            "seed {seed}: at {last:.0} kbps four minutes after the path opened"
        );
        // Each time the encoder is created again the picture stops for a tenth of a second:
        // two or three times to get under the path, twice for each try for more that fails,
        // four times on the way back.
        println!("seed {seed}: created again {} times", r.restarts);
        assert!(
            r.restarts <= 16,
            "seed {seed}: created {} times",
            r.restarts
        );
    }
}
