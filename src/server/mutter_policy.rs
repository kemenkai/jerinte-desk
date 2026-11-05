//! When to record the physical monitors through Mutter (`mutter`), and when to wait: the plain
//! part, built on macOS too for its tests.

use scrap::mutter_model::{Ended, Recording};
use std::time::{Duration, Instant};

// Translation keys, so the key itself is the English text.
pub(super) const LOCKED: &str =
    "The remote device is locked, its screen is shown again once it is unlocked";
pub(super) const STOPPED: &str = "Screen sharing was stopped on the remote device";
pub(super) const CHANGING: &str = "The displays of the remote device are changing";
pub(super) const SOON: &str = "The screen of the remote device is recorded again shortly";
/// Not a translation key: nobody is connected to be told.
pub(super) const IDLE: &str = "No remote connection";

/// How soon a session that failed to open is tried again, and after this many failures in a row,
/// how much later.
const AGAIN: Duration = Duration::from_secs(1);
const FAILURES: u32 = 5;
const AGAIN_SLOWLY: Duration = Duration::from_secs(30);
/// How often a locked screen is looked at again.
const LOCKED_LOOK: Duration = Duration::from_secs(2);

/// What to do about the physical monitors' recording.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Step {
    /// Nobody is connected: nothing is recorded.
    Idle,
    /// It is as wanted.
    Keep,
    /// Record them, in place of what there is.
    Record,
    /// Stop recording them, for good: someone at the device stopped the sharing.
    Stop,
    /// Not now, for this reason.
    Wait(&'static str),
}

#[derive(Default)]
pub(super) struct Policy {
    /// A remote connection logged in, and not all have ended since.
    connected: bool,
    last_open: Option<Instant>,
    failures: u32,
    /// Someone at the device stopped the sharing: not recorded again before the next login.
    stopped: bool,
    /// The screen is locked: recorded again once unlocked.
    locked: bool,
    last_look: Option<Instant>,
    /// What to tell the controlling side, once.
    notice: Option<&'static str>,
    told: Option<&'static str>,
    /// A virtual display has the desktop to itself, the physical monitors off: none is recorded
    /// meanwhile, whoever logs in or out, so none Mutter lights again by itself (a monitor plugged
    /// in) is recorded just as it is turned off again.
    held: bool,
}

impl Policy {
    /// A connection logs in: whoever stopped the sharing before, it is asked for again.
    pub(super) fn login(&mut self) {
        *self = Self {
            connected: true,
            held: self.held,
            ..Self::default()
        };
    }

    /// The last remote connection ended: nothing is recorded until the next login.
    pub(super) fn ended(&mut self) {
        *self = Self {
            held: self.held,
            ..Self::default()
        };
    }

    /// Nothing is recorded while held (`on`), the recording stopped by the caller; or recorded
    /// again as wanted.
    pub(super) fn hold(&mut self, on: bool) {
        self.held = on;
    }

    /// What to do, `recording` as it is now and `wanted` the physical monitors shown, by
    /// connector. `settling`: displays of ours are changing. `inhibited` asks Mutter whether it
    /// refuses sessions now, which it does while the screen is locked.
    pub(super) fn step(
        &mut self,
        recording: &Recording,
        wanted: &[String],
        settling: bool,
        now: Instant,
        inhibited: impl FnOnce() -> bool,
    ) -> Step {
        if !self.connected {
            return Step::Idle;
        }
        if self.held {
            return Step::Wait(CHANGING);
        }
        match recording {
            Recording::Live(recorded) if recorded.as_slice() == wanted => {
                self.locked = false;
                self.told = None;
                self.last_look = None;
                return Step::Keep;
            }
            // GNOME Shell closed it: the screen locked, or someone stopped the sharing in its
            // indicator. Mutter refuses new sessions only while locked.
            Recording::Ended(Ended::Closed) if !self.stopped => {
                let due = self
                    .last_look
                    .map_or(true, |at| now.saturating_duration_since(at) >= LOCKED_LOOK);
                if due {
                    self.last_look = Some(now);
                    if inhibited() {
                        self.locked = true;
                        self.tell(LOCKED);
                        return Step::Wait(LOCKED);
                    }
                    if !self.locked {
                        self.stopped = true;
                        self.tell(STOPPED);
                        return Step::Stop;
                    }
                    // Unlocked.
                    self.locked = false;
                } else {
                    return Step::Wait(if self.locked { LOCKED } else { SOON });
                }
            }
            // Plugged in or out, moved, Mutter gone, a stream failed, or nothing yet: record.
            _ => {}
        }
        if self.stopped {
            return Step::Wait(STOPPED);
        }
        if settling {
            return Step::Wait(CHANGING);
        }
        let again = if self.failures >= FAILURES {
            AGAIN_SLOWLY
        } else {
            AGAIN
        };
        if self
            .last_open
            .map_or(false, |at| now.saturating_duration_since(at) < again)
        {
            return Step::Wait(SOON);
        }
        Step::Record
    }

    /// How recording went.
    pub(super) fn recorded(&mut self, ok: bool, now: Instant) {
        self.last_open = Some(now);
        if ok {
            self.failures = 0;
            self.locked = false;
        } else {
            self.failures += 1;
        }
    }

    /// What to tell the controlling side, if anything new.
    pub(super) fn take_notice(&mut self) -> Option<&'static str> {
        self.notice.take()
    }

    fn tell(&mut self, what: &'static str) {
        if self.told != Some(what) {
            self.told = Some(what);
            self.notice = Some(what);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted() -> Vec<String> {
        vec!["HDMI-1".to_owned()]
    }

    fn live() -> Recording {
        Recording::Live(wanted())
    }

    const CLOSED: Recording = Recording::Ended(Ended::Closed);

    // Logged in.
    fn policy() -> Policy {
        let mut p = Policy::default();
        p.login();
        p
    }

    #[test]
    fn nothing_is_recorded_without_a_connection() {
        let mut p = Policy::default();
        let t = Instant::now();
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Idle);
        p.login();
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Record);
        p.recorded(true, t);
        // The last connection ended: even a session closed meanwhile is not looked into.
        p.ended();
        assert_eq!(p.step(&CLOSED, &wanted(), false, t, || panic!("asked")), Step::Idle);
        assert_eq!(p.take_notice(), None);
        p.login();
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Record);
    }

    // Held, nothing is recorded, a monitor lit again by Mutter included, whoever logs in or out;
    // let go, it is recorded at once.
    #[test]
    fn nothing_is_recorded_while_held() {
        let mut p = policy();
        let t = Instant::now();
        p.hold(true);
        assert_eq!(
            p.step(&Recording::Not, &wanted(), false, t, || panic!("asked")),
            Step::Wait(CHANGING)
        );
        p.login();
        p.ended();
        p.login();
        assert_eq!(
            p.step(&CLOSED, &wanted(), false, t, || panic!("asked")),
            Step::Wait(CHANGING)
        );
        assert_eq!(p.take_notice(), None);
        p.hold(false);
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Record);
    }

    #[test]
    fn records_once_and_keeps_what_is_wanted() {
        let mut p = policy();
        let t = Instant::now();
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Record);
        p.recorded(true, t);
        assert_eq!(p.step(&live(), &wanted(), false, t, || false), Step::Keep);
        // A monitor plugged in: recorded anew, not more than once a second.
        let more = vec!["HDMI-1".to_owned(), "DP-1".to_owned()];
        assert_eq!(p.step(&live(), &more, false, t, || false), Step::Wait(SOON));
        let t = t + AGAIN;
        assert_eq!(p.step(&live(), &more, false, t, || false), Step::Record);
    }

    #[test]
    fn a_move_is_recorded_again_once_the_displays_settle() {
        let mut p = policy();
        let t = Instant::now();
        p.recorded(true, t);
        let t = t + AGAIN;
        let moved = Recording::Ended(Ended::Moved);
        assert_eq!(p.step(&moved, &wanted(), true, t, || false), Step::Wait(CHANGING));
        assert_eq!(p.step(&moved, &wanted(), false, t, || panic!("asked")), Step::Record);
        // Mutter gone, or a stream failed: the same.
        for end in [Ended::Restarted, Ended::Failed] {
            let ended = Recording::Ended(end);
            assert_eq!(p.step(&ended, &wanted(), false, t, || panic!("asked")), Step::Record);
        }
        assert_eq!(p.take_notice(), None);
    }

    #[test]
    fn failures_in_a_row_slow_down() {
        let mut p = policy();
        let mut t = Instant::now();
        for _ in 0..FAILURES {
            assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Record);
            p.recorded(false, t);
            t += AGAIN;
        }
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Wait(SOON));
        t += AGAIN_SLOWLY;
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Record);
        p.recorded(true, t);
        t += AGAIN;
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Record);
    }

    #[test]
    fn a_locked_screen_is_waited_out_and_told_once() {
        let mut p = policy();
        let t = Instant::now();
        p.recorded(true, t);
        let t = t + AGAIN;
        assert_eq!(p.step(&CLOSED, &wanted(), false, t, || true), Step::Wait(LOCKED));
        assert_eq!(p.take_notice(), Some(LOCKED));
        // Not asked again so soon, still locked.
        assert_eq!(p.step(&CLOSED, &wanted(), false, t, || panic!("asked")), Step::Wait(LOCKED));
        let t = t + LOCKED_LOOK;
        assert_eq!(p.step(&CLOSED, &wanted(), false, t, || true), Step::Wait(LOCKED));
        assert_eq!(p.take_notice(), None);
        // Unlocked: recorded again, not taken for a stop.
        let t = t + LOCKED_LOOK;
        assert_eq!(p.step(&CLOSED, &wanted(), false, t, || false), Step::Record);
        p.recorded(true, t);
        assert_eq!(p.step(&live(), &wanted(), false, t, || false), Step::Keep);
        // Locked again later: told again.
        let t = t + AGAIN;
        assert_eq!(p.step(&CLOSED, &wanted(), false, t, || true), Step::Wait(LOCKED));
        assert_eq!(p.take_notice(), Some(LOCKED));
    }

    #[test]
    fn a_stop_at_the_device_holds_until_the_next_login() {
        let mut p = policy();
        let t = Instant::now();
        p.recorded(true, t);
        let t = t + AGAIN;
        assert_eq!(p.step(&CLOSED, &wanted(), false, t, || false), Step::Stop);
        assert_eq!(p.take_notice(), Some(STOPPED));
        let t = t + AGAIN_SLOWLY;
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Wait(STOPPED));
        assert_eq!(p.take_notice(), None);
        p.login();
        assert_eq!(p.step(&Recording::Not, &wanted(), false, t, || false), Step::Record);
    }
}
