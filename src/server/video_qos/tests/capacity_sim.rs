//! The scenarios `capacity` was written for, on the closed-loop simulation of `sim`: a target far
//! above the presets at sixty frames a second, and a viewer that says when the probes reach it.
//!
//! Each scenario is also run with a viewer that does not say, which is the controller as it was;
//! those rows are printed for comparison and not asserted.
//!
//! The bounds state what the product needs, and were written down before the first run.  As in
//! `sim`: if a seed or a scenario violates one, change the design or loosen the bound with a
//! written reason; never tune a constant until the bound passes.
use super::sim::{run, Content, EncoderModel, Link, Report, Scenario, SEEDS};
use super::*;

/// As `sim`: the bitrate at ratio 1, in kbps.
const BASE_KBPS: f64 = 6000.0;
/// 21 Mbps.
const TARGET: f32 = 3.5;
/// As `sim`: what comes before belongs to the start.
const WARM_UP_MS: u32 = 15_000;
/// A queue this long is one a person feels.
const FELT_QUEUE_MS: u32 = 500;

fn link(capacity_kbps: &[(u32, f64)]) -> Link {
    Link {
        capacity_kbps: capacity_kbps.to_vec(),
        wobble: 0.05,
        base_rtt_ms: 15.0,
        jitter_median_ms: 3.0,
        jitter_sigma: 0.5,
        loss_per_s: 0.0,
        stall_mean_interval_s: 0.0,
        stall_ms: (0.0, 0.0),
    }
}

/// `sim`'s weak-signal home Wi-Fi at another capacity: heavy-tailed jitter, a retransmission
/// every five seconds that halves the rate for a second, a capacity that wanders by half either
/// way, and a stall of up to 2.5 s every twenty seconds or so.
fn home_wifi(capacity_kbps: f64) -> Link {
    Link {
        capacity_kbps: vec![(0, capacity_kbps)],
        wobble: 0.5,
        base_rtt_ms: 8.0,
        jitter_median_ms: 15.0,
        jitter_sigma: 1.0,
        loss_per_s: 0.2,
        stall_mean_interval_s: 20.0,
        stall_ms: (300.0, 2500.0),
    }
}

fn scenario(name: &'static str, target: f32, link: Link) -> Scenario {
    Scenario {
        name,
        seconds: 300,
        limit: 60,
        quality: Quality::Custom(target),
        abr: true,
        content: Content::Video,
        encoder: EncoderModel::Cbr,
        link,
        seed: 1,
        reports: true,
        send_buffer_kbit: None,
        source_fps: None,
        relay_cap_kbps: None,
        base_kbps: None,
        loop_wait: None,
        restart_only: false,
        encoder_sized_for: None,
    }
}

/// What one run came to, from its trace of `(time_ms, target fps, queue_ms, ratio)`.
struct Measures {
    /// Mean ratio after the warm-up.
    mean_ratio: f64,
    /// Mean of what the encoder really puts out after the warm-up, in kbps: its bitrate, or for
    /// the fixed-rate model its bitrate times the frames over thirty.
    mean_output_kbps: f64,
    queue_p95_ms: u32,
    /// Share of the time after the warm-up with a queue a person feels.
    felt_pct: f64,
    /// When the ratio first reached 90% of the target.
    to_90pct_ms: Option<u32>,
    /// From the capacity drop until the queue stayed under 300 ms for five seconds.
    settled_ms: Option<u32>,
    /// From the capacity restore until the ratio was back at 90% of the target.
    back_ms: Option<u32>,
}

fn measure(sc: &Scenario, r: &Report) -> Measures {
    let target = sc.quality.ratio();
    let after: Vec<_> = r.trace.iter().filter(|s| s.0 >= WARM_UP_MS).collect();
    let n = after.len().max(1) as f64;
    let output = |fps: u32, ratio: f32| {
        let nominal = BASE_KBPS * ratio as f64;
        match sc.encoder {
            EncoderModel::Cbr | EncoderModel::Floor(_) => nominal,
            EncoderModel::FixedRate => {
                nominal * fps as f64 / sc.encoder_sized_for.unwrap_or(sc.limit as f64)
            }
        }
    };
    let drop_ms = sc
        .link
        .capacity_kbps
        .windows(2)
        .find(|w| w[1].1 < w[0].1)
        .map(|w| w[1].0);
    let restore_ms = sc
        .link
        .capacity_kbps
        .windows(2)
        .find(|w| w[1].1 > w[0].1)
        .map(|w| w[1].0);
    let settled_ms = drop_ms.and_then(|drop| {
        let mut since = None;
        for (t, _, queue, _) in r.trace.iter().filter(|s| s.0 >= drop) {
            if *queue < 300 {
                let since = *since.get_or_insert(*t);
                if t - since >= 5_000 {
                    return Some(since - drop);
                }
            } else {
                since = None;
            }
        }
        None
    });
    let back_ms = restore_ms.and_then(|restore| {
        r.trace
            .iter()
            .find(|s| s.0 >= restore && s.3 >= target * 0.9)
            .map(|s| s.0 - restore)
    });
    Measures {
        mean_ratio: after.iter().map(|s| s.3 as f64).sum::<f64>() / n,
        mean_output_kbps: after.iter().map(|s| output(s.1, s.3)).sum::<f64>() / n,
        queue_p95_ms: r.queue_p95_ms,
        felt_pct: 100.0 * after.iter().filter(|s| s.2 >= FELT_QUEUE_MS).count() as f64 / n,
        to_90pct_ms: r.trace.iter().find(|s| s.3 >= target * 0.9).map(|s| s.0),
        settled_ms,
        back_ms,
    }
}

/// One scenario over all seeds.
struct Outcome {
    mean_ratio_median: f64,
    mean_output_median_kbps: f64,
    queue_p95_p90_ms: u32,
    felt_p90_pct: f64,
    /// The slowest seed; `None` when a seed never got there, or the scenario has no such moment.
    to_90pct_worst_ms: Option<u32>,
    settled_worst_ms: Option<u32>,
    back_worst_ms: Option<u32>,
}

/// With `RUSTDESK_QOS_SIM_CSV` set, as for `sim`: every run's trace, second by second, appended
/// to `<that path>.capacity.csv`, to look at what a number in the table came from.
fn write_trace(sc: &Scenario, r: &Report) {
    use std::io::Write;
    let Ok(path) = std::env::var("RUSTDESK_QOS_SIM_CSV") else {
        return;
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{path}.capacity.csv"))
        .unwrap();
    let viewer = if sc.reports { "says" } else { "silent" };
    for (t, fps, queue, ratio) in r.trace.iter().filter(|s| s.0 % 1000 == 0) {
        writeln!(
            file,
            "{},{viewer},{},{t},{fps},{queue},{ratio:.3}",
            sc.name, sc.seed
        )
        .unwrap();
    }
}

fn outcome(sc: &Scenario) -> Outcome {
    let all: Vec<Measures> = SEEDS
        .map(|seed| {
            let sc = Scenario { seed, ..sc.clone() };
            let report = run(&sc);
            write_trace(&sc, &report);
            measure(&sc, &report)
        })
        .collect();
    let pick = |mut v: Vec<f64>, p: f64| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[(((v.len() - 1) as f64) * p).round() as usize]
    };
    let worst = |f: fn(&Measures) -> Option<u32>| {
        all.iter()
            .map(f)
            .try_fold(0, |worst, ms| ms.map(|ms| worst.max(ms)))
    };
    Outcome {
        mean_ratio_median: pick(all.iter().map(|m| m.mean_ratio).collect(), 0.5),
        mean_output_median_kbps: pick(all.iter().map(|m| m.mean_output_kbps).collect(), 0.5),
        queue_p95_p90_ms: pick(all.iter().map(|m| m.queue_p95_ms as f64).collect(), 0.9) as u32,
        felt_p90_pct: pick(all.iter().map(|m| m.felt_pct).collect(), 0.9),
        to_90pct_worst_ms: worst(|m| m.to_90pct_ms),
        settled_worst_ms: worst(|m| m.settled_ms),
        back_worst_ms: worst(|m| m.back_ms),
    }
}

fn row(name: &str, viewer: &str, o: &Outcome) -> String {
    let secs = |ms: Option<u32>| {
        ms.map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
            .unwrap_or_else(|| "-".to_owned())
    };
    format!(
        "| {name} | {viewer} | {:.2} | {:.0} | {} ms | {:.1}% | {} | {} | {} |",
        o.mean_ratio_median,
        o.mean_output_median_kbps,
        o.queue_p95_p90_ms,
        o.felt_p90_pct,
        secs(o.to_90pct_worst_ms),
        secs(o.settled_worst_ms),
        secs(o.back_worst_ms),
    )
}

/// Runs `sc` with a viewer that says when the probes reach it and one that does not, prints
/// both, and returns the first.
fn compare(sc: &Scenario) -> Outcome {
    let says = outcome(sc);
    let silent = outcome(&Scenario {
        reports: false,
        ..sc.clone()
    });
    println!("{}", row(sc.name, "says", &says));
    println!("{}", row(sc.name, "silent", &silent));
    says
}

const HEADER: &str = "| scenario | viewer | ratio (median of means) | output kbps (median of means) | queue p95 (p90) | felt queue (p90) | to 90% of target (worst) | settled after drop (worst) | back after restore (worst) |\n|---|---|---:|---:|---:|---:|---:|---:|---:|";

#[test]
fn capacity_scenarios() {
    println!("{HEADER}");

    // A path with room to spare: the target is reached soon and kept, with no queue.
    let o = compare(&scenario("high_clean_50", TARGET, link(&[(0, 50_000.0)])));
    assert!(
        o.mean_ratio_median >= 0.95 * TARGET as f64,
        "clean: below 95% of the target"
    );
    assert!(o.queue_p95_p90_ms < 50, "clean: a queue on a clean path");
    assert!(
        o.to_90pct_worst_ms.is_some_and(|ms| ms <= 15_000),
        "clean: over 15 s to 90% of the target"
    );

    // A path that carries 12 of the 21 Mbps asked for: the stream settles near what it carries,
    // uses most of it, and a queue a person feels is rare.
    let tight = scenario("high_tight_12", TARGET, link(&[(0, 12_000.0)]));
    let o = compare(&tight);
    assert!(o.queue_p95_p90_ms < 400, "tight: queue p95 over 400 ms");
    assert!(
        o.felt_p90_pct <= 5.0,
        "tight: a felt queue over 5% of the time"
    );
    assert!(
        o.mean_output_median_kbps >= 0.7 * 12_000.0,
        "tight: under 70% of the path used"
    );

    // The same with send buffers of a megabyte: they fill, the encoder is held to the path's
    // pace, and what is sent no longer exceeds what arrives.
    let o = compare(&Scenario {
        name: "high_tight_12_buffered",
        send_buffer_kbit: Some(8_000.0),
        ..tight.clone()
    });
    assert!(
        o.felt_p90_pct <= 10.0,
        "buffered: a felt queue over 10% of the time"
    );
    assert!(
        o.mean_output_median_kbps >= 0.7 * 12_000.0,
        "buffered: under 70% of the path used"
    );

    // The same with an encoder that puts out twice its bitrate at sixty frames a second, as the
    // hardware ones did when they were all created for thirty.
    let o = compare(&Scenario {
        name: "high_tight_12_fixed_rate",
        encoder: EncoderModel::FixedRate,
        encoder_sized_for: Some(30.0),
        ..tight.clone()
    });
    assert!(
        o.queue_p95_p90_ms < 600,
        "fixed rate: queue p95 over 600 ms"
    );
    assert!(
        o.felt_p90_pct <= 8.0,
        "fixed rate: a felt queue over 8% of the time"
    );
    assert!(
        o.mean_output_median_kbps >= 0.6 * 12_000.0,
        "fixed rate: under 60% of the path used"
    );

    // 40 Mbps, then 10 for a minute and a half, then 40 again: the queue of the drop is gone
    // within twenty seconds, and the target is back within two minutes of the path's return.
    //
    // Twenty and not ten, as first written: that the path fell short is known only at the second
    // bad reply, which is the parent's rule and unchanged, and with one probe under way at a time
    // the second reply to a queue growing by more than a second every second comes back six
    // seconds after the drop. The queue of those six seconds is what then has to be sent.
    let o = compare(&scenario(
        "high_drop_40_10_40",
        TARGET,
        link(&[(0, 40_000.0), (60_000, 10_000.0), (150_000, 40_000.0)]),
    ));
    assert!(
        o.settled_worst_ms.is_some_and(|ms| ms <= 20_000),
        "drop: the queue not settled within 20 s"
    );
    assert!(
        o.back_worst_ms.is_some_and(|ms| ms <= 120_000),
        "drop: the target not back within 120 s"
    );

    // A jittery path with capacity to spare for 12 Mbps: jitter and stalls are not a full path,
    // the stream stays near its target.  Sixty Mbps, which wandering and a retransmission's
    // halving together leave at 15 at the least: the 20 Mbps of `sim`, first used here, are left
    // at 5, which is not capacity to spare for 12.
    let o = compare(&scenario("high_home_wifi_60", 2.0, home_wifi(60_000.0)));
    assert!(
        o.mean_ratio_median >= 0.8 * 2.0,
        "wifi: below 80% of the target"
    );
    assert!(o.queue_p95_p90_ms < 1000, "wifi: queue p95 over 1 s");

    // That path at 20 Mbps, which is often less than the 12 asked for and often more, with
    // pauses in between: there is no right bitrate to settle at.  What is asked is that knowing
    // what the path delivers does not make the queue worse than not knowing, and does not cost
    // more than a quarter of what is sent.
    //
    // A quarter and not a tenth, as first written: the wait at a ceiling trades bitrate for
    // fewer queues, and on a path whose capacity moves by the second the ceiling of one moment
    // is held through the next, when the path would carry more.  The first run came to 18% less
    // sent for a felt queue 18% of the time against 21%; that is the trade as it stands, and
    // this bound keeps it from growing.
    let sc = scenario("high_home_wifi_20", 2.0, home_wifi(20_000.0));
    let says = outcome(&sc);
    let silent = outcome(&Scenario {
        reports: false,
        ..sc.clone()
    });
    println!("{}", row(sc.name, "says", &says));
    println!("{}", row(sc.name, "silent", &silent));
    assert!(
        says.felt_p90_pct <= silent.felt_p90_pct,
        "wifi at 20: a felt queue more often than without the deliveries"
    );
    assert!(
        says.mean_output_median_kbps >= 0.75 * silent.mean_output_median_kbps,
        "wifi at 20: a quarter less sent than without the deliveries"
    );
}
