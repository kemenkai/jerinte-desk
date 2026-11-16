//! What the controlling side keeps of the stream's quality: the `[CONN-QOS]` log line, one every
//! ten seconds next to `[CONN-PATH]` and `[CONN-RTT]`, and the notice shown when the controlled
//! side reports that it lowered the quality for the network, or brought it back. And what it
//! tells the controlled side: when each of its probes arrived.

use std::time::{Duration, Instant};

/// `TestDelay.qos_state`: the stream runs below the quality asked for because the path could not
/// carry it.
pub const QOS_STATE_LOWERED: u32 = 1;

/// One line of the log every this long.
const LOG_EVERY: Duration = Duration::from_secs(10);
/// A notice no sooner than this after the one before: a path at its limit goes back and forth.
const NOTICE_EVERY: Duration = Duration::from_secs(30);

/// When a probe of the controlled side arrived here, for `TestDelay.received_at`: milliseconds of
/// a clock that only goes forward, counted from this process's first probe. Never 0, which says
/// the peer does not tell.
pub fn probe_received_at() -> u64 {
    lazy_static::lazy_static! {
        static ref ORIGIN: Instant = Instant::now();
    }
    ORIGIN.elapsed().as_millis() as u64 + 1
}

/// What to tell the person at the controlling side.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Notice {
    /// The quality went down for the network.
    Lowered,
    /// The quality is back at what was asked for.
    Restored,
}

impl Notice {
    /// The key of its text in the translation tables, for the UI to translate. No bitrate in it:
    /// right after it comes down the stream is lower still for some seconds, to send the queue
    /// that built up, and that is not where it stays.
    pub fn key(&self) -> &'static str {
        match self {
            Notice::Lowered => "qos-lowered-tip",
            Notice::Restored => "qos-restored-tip",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct QosReport {
    // The controlled side's last word, from its `TestDelay`.
    delay_ms: u32,
    target_kbps: u32,
    state: u32,
    // What arrived since the last log line.
    bytes: u64,
    frames: u64,
    window: Duration,
    // The state the person was last told of, and when.
    told_state: u32,
    told_at: Option<Instant>,
}

impl QosReport {
    /// A `TestDelay` of the controlled side arrived. Returns the notice to show, if any.
    pub fn on_test_delay(
        &mut self,
        delay_ms: u32,
        target_kbps: u32,
        state: u32,
        now: Instant,
    ) -> Option<Notice> {
        self.delay_ms = delay_ms;
        self.target_kbps = target_kbps;
        self.state = state;
        let lowered = state == QOS_STATE_LOWERED;
        if lowered == (self.told_state == QOS_STATE_LOWERED) {
            return None;
        }
        // Too soon after the last one: said later, if it still holds then.
        if self
            .told_at
            .is_some_and(|at| now.saturating_duration_since(at) < NOTICE_EVERY)
        {
            return None;
        }
        self.told_state = if lowered { QOS_STATE_LOWERED } else { 0 };
        self.told_at = Some(now);
        Some(if lowered {
            Notice::Lowered
        } else {
            Notice::Restored
        })
    }

    /// The status timer measured `bytes_per_sec` and `fps` over `elapsed`. Returns the fields of a
    /// `[CONN-QOS]` line once enough time has gathered, and starts over.
    pub fn on_status(
        &mut self,
        bytes_per_sec: usize,
        fps: i32,
        elapsed: Duration,
    ) -> Option<String> {
        let secs = elapsed.as_secs_f64();
        self.bytes += (bytes_per_sec as f64 * secs) as u64;
        self.frames += (fps.max(0) as f64 * secs).round() as u64;
        self.window += elapsed;
        if self.window < LOG_EVERY {
            return None;
        }
        let secs = self.window.as_secs_f64();
        let line = format!(
            "fps={:.0} kbps={:.0} target_kbps={} delay_ms={} state={}",
            self.frames as f64 / secs,
            self.bytes as f64 * 8.0 / 1000.0 / secs,
            self.target_kbps,
            self.delay_ms,
            self.state,
        );
        self.bytes = 0;
        self.frames = 0;
        self.window = Duration::ZERO;
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_change_of_state_is_told_once() {
        let t0 = Instant::now();
        let mut r = QosReport::default();
        assert_eq!(r.on_test_delay(20, 21_000, 0, t0), None);
        assert_eq!(
            r.on_test_delay(400, 8_000, 1, t0 + Duration::from_secs(1)),
            Some(Notice::Lowered)
        );
        for s in 2..40 {
            assert_eq!(
                r.on_test_delay(30, 8_000, 1, t0 + Duration::from_secs(s)),
                None
            );
        }
        assert_eq!(
            r.on_test_delay(20, 21_000, 0, t0 + Duration::from_secs(40)),
            Some(Notice::Restored)
        );
    }

    #[test]
    fn a_state_that_flaps_is_not_told_every_time() {
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        let mut r = QosReport::default();
        assert_eq!(r.on_test_delay(400, 8_000, 1, at(0)), Some(Notice::Lowered));
        // Back and down again within the half minute: neither is told.
        assert_eq!(r.on_test_delay(20, 21_000, 0, at(5)), None);
        assert_eq!(r.on_test_delay(400, 9_000, 1, at(10)), None);
        // Back for good: told once the half minute is over, not before.
        assert_eq!(r.on_test_delay(20, 21_000, 0, at(20)), None);
        assert_eq!(
            r.on_test_delay(20, 21_000, 0, at(31)),
            Some(Notice::Restored)
        );
    }

    #[test]
    fn a_peer_that_never_sets_the_field_tells_nothing() {
        let t0 = Instant::now();
        let mut r = QosReport::default();
        for s in 0..120 {
            let kbps = if s < 60 { 4_000 } else { 1_000 };
            assert_eq!(
                r.on_test_delay(30, kbps, 0, t0 + Duration::from_secs(s)),
                None
            );
        }
    }

    #[test]
    fn the_time_a_probe_arrived_only_goes_forward_and_is_never_zero() {
        let first = probe_received_at();
        assert!(first > 0, "0 says the peer does not tell");
        std::thread::sleep(Duration::from_millis(5));
        assert!(probe_received_at() >= first + 5);
    }

    #[test]
    fn the_notice_is_a_sentence_in_every_language() {
        // The tables the UI translates the keys with: English, which every other language falls
        // back to, and the two Chinese ones.
        for lang in ["en", "zh-cn", "zh-tw"] {
            for notice in [Notice::Lowered, Notice::Restored] {
                let text = crate::lang::text_in_table(lang, notice.key()).unwrap_or_default();
                assert!(
                    text.chars().count() > 10 && !text.contains('{'),
                    "{lang}: {text}"
                );
            }
        }
    }

    #[test]
    fn the_log_line_averages_ten_seconds() {
        let mut r = QosReport::default();
        r.on_test_delay(35, 12_000, 1, Instant::now());
        for _ in 0..9 {
            // 1 MB/s is 8000 kbps.
            assert_eq!(r.on_status(1_000_000, 60, Duration::from_secs(1)), None);
        }
        assert_eq!(
            r.on_status(1_000_000, 60, Duration::from_secs(1))
                .as_deref(),
            Some("fps=60 kbps=8000 target_kbps=12000 delay_ms=35 state=1")
        );
        // The window starts over.
        assert_eq!(r.on_status(500_000, 30, Duration::from_secs(1)), None);
    }
}
