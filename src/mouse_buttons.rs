//! Fork: a mouse button that went down on the controlled side comes up again, whatever happens
//! to the release the controlling side sent.
//!
//! The transport is reliable and in order, so a release the controlling side sent arrives. It
//! can still be lost on this side, in three places, each with its own part here:
//!
//! - It is not let through: the permission to control was taken away after the press, or the
//!   connection ended before it. [`Held`], one for a connection, knows the buttons its peer
//!   holds: their release goes in whatever the permission says, and they are let go when the
//!   permission or the connection goes.
//! - It does not go into the input stream. [`Guard`], in the process that injects, tries it
//!   again until it does.
//! - It goes in and takes no effect (another program's hook swallowing it, the input desktop
//!   changed between the press and the release). [`Guard`] looks at the state the system has
//!   the button in and injects the release again, twice at most.
//!
//! What is not here: the controlling side saying which buttons it holds, with a timeout on this
//! side. Nothing it sent is lost on the way, a controlling side that is gone ends the
//! connection, and a timeout would end a drag whenever the path stalls for longer than it.

use crate::input::{
    MOUSE_BUTTON_BACK, MOUSE_BUTTON_FORWARD, MOUSE_BUTTON_LEFT, MOUSE_BUTTON_RIGHT,
    MOUSE_BUTTON_WHEEL, MOUSE_TYPE_DOWN, MOUSE_TYPE_MASK, MOUSE_TYPE_UP,
};
use base::message_proto::MouseEvent;
use std::time::{Duration, Instant};

/// The buttons, as the bits a `MouseEvent`'s mask names them by above its type.
pub const BUTTONS: [i32; 5] = [
    MOUSE_BUTTON_LEFT,
    MOUSE_BUTTON_RIGHT,
    MOUSE_BUTTON_WHEEL,
    MOUSE_BUTTON_BACK,
    MOUSE_BUTTON_FORWARD,
];
const ALL: i32 =
    MOUSE_BUTTON_LEFT | MOUSE_BUTTON_RIGHT | MOUSE_BUTTON_WHEEL | MOUSE_BUTTON_BACK | MOUSE_BUTTON_FORWARD;

fn name(button: i32) -> &'static str {
    match button {
        MOUSE_BUTTON_LEFT => "left",
        MOUSE_BUTTON_RIGHT => "right",
        MOUSE_BUTTON_WHEEL => "middle",
        MOUSE_BUTTON_BACK => "back",
        MOUSE_BUTTON_FORWARD => "forward",
        _ => "?",
    }
}

/// The buttons of a set, by name, for the log.
pub fn names(buttons: i32) -> String {
    BUTTONS
        .iter()
        .filter(|b| buttons & **b != 0)
        .map(|b| name(*b))
        .collect::<Vec<_>>()
        .join("+")
}

fn kind(evt: &MouseEvent) -> i32 {
    evt.mask & MOUSE_TYPE_MASK
}

fn buttons(evt: &MouseEvent) -> i32 {
    (evt.mask >> 3) & ALL
}

/// The release of one button, as the controlling side would send it. A release moves nothing,
/// so it needs no position.
pub fn release_of(button: i32) -> MouseEvent {
    MouseEvent {
        mask: MOUSE_TYPE_UP | button << 3,
        ..Default::default()
    }
}

/// The controlled side: the buttons one controlling side holds down here.
#[derive(Default)]
pub struct Held(i32);

impl Held {
    /// Whether this is the release of a button held: it goes in whatever the permission to
    /// control says by now, the press having gone in.
    pub fn releases(&self, evt: &MouseEvent) -> bool {
        kind(evt) == MOUSE_TYPE_UP && self.0 & buttons(evt) != 0
    }

    /// A mouse event of the controlling side is about to be injected.
    pub fn injecting(&mut self, evt: &MouseEvent) {
        match kind(evt) {
            // Held from the attempt on: a press that failed to go in costs a release too many,
            // one that went in unnoticed a button stuck.
            MOUSE_TYPE_DOWN => self.0 |= buttons(evt),
            MOUSE_TYPE_UP => self.0 &= !buttons(evt),
            _ => {}
        }
    }

    /// Everything held, to let go of: the connection ends, or may no longer control.
    pub fn take(&mut self) -> i32 {
        std::mem::take(&mut self.0)
    }
}

/// How long after a release that went in the system is asked whether the button is up, and
/// after each release injected again for it; the release is injected again after every look but
/// the last. The system takes a release up at once: the first look leaves it room, the later
/// ones a program that holds input up for a moment.
const LOOKS: [Duration; 3] = [
    Duration::from_millis(50),
    Duration::from_millis(150),
    Duration::from_millis(400),
];
/// A release that did not go in is tried again after this, doubling up to `SLOWEST`.
const FIRST_RETRY: Duration = Duration::from_millis(100);
const SLOWEST: Duration = Duration::from_millis(1000);

/// What the injecting process still has to do about the release of one button.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Todo {
    /// The release did not go into the input stream: inject it again.
    Inject,
    /// It went in: see whether the button is up.
    Look,
}

#[derive(Clone, Copy)]
struct Pending {
    todo: Todo,
    /// Releases that did not go in (`Inject`), or injected again after a look (`Look`).
    tries: u32,
    due: Instant,
}

/// The injecting process: the buttons it pressed, and the releases not done with.
#[derive(Default)]
pub struct Guard {
    down: i32,
    pending: [Option<Pending>; BUTTONS.len()],
}

/// What became of a release, to log.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Nothing to say.
    Quiet,
    /// Did not go in, for the `tries`-th time.
    Failing { tries: u32 },
    /// Went in after not going in `tries` times.
    Recovered { tries: u32 },
    /// In, and the button still down: injected again, for the `tries`-th time.
    StillDown { tries: u32 },
    /// Still down after every release injected again: left alone, someone at the machine may be
    /// holding the button.
    GivenUp,
}

fn index(button: i32) -> Option<usize> {
    BUTTONS.iter().position(|b| *b == button)
}

impl Guard {
    /// A press was injected. Any release of the button not done with is: the next one counts.
    pub fn pressed(&mut self, button: i32) {
        if let Some(i) = index(button) {
            self.down |= button;
            self.pending[i] = None;
        }
    }

    /// A release was injected and went in or not. `look`: whether the system can be asked for
    /// the state of the button afterwards.
    pub fn released(&mut self, button: i32, went_in: bool, look: bool, now: Instant) -> Outcome {
        let Some(i) = index(button) else {
            return Outcome::Quiet;
        };
        if went_in {
            self.down &= !button;
            self.pending[i] = look.then(|| Pending {
                todo: Todo::Look,
                tries: 0,
                due: now + LOOKS[0],
            });
            Outcome::Quiet
        } else {
            self.pending[i] = Some(Pending {
                todo: Todo::Inject,
                tries: 1,
                due: now + FIRST_RETRY,
            });
            Outcome::Failing { tries: 1 }
        }
    }

    /// When the next of the pending work is due.
    pub fn next_due(&self) -> Option<Instant> {
        self.pending.iter().flatten().map(|p| p.due).min()
    }

    /// The work due now.
    pub fn due(&self, now: Instant) -> Vec<(i32, Todo)> {
        BUTTONS
            .iter()
            .zip(&self.pending)
            .filter_map(|(b, p)| p.filter(|p| p.due <= now).map(|p| (*b, p.todo)))
            .collect()
    }

    /// The release of `Todo::Inject` was injected again and went in or not.
    pub fn injected(&mut self, button: i32, went_in: bool, look: bool, now: Instant) -> Outcome {
        let Some(i) = index(button) else {
            return Outcome::Quiet;
        };
        let Some(p) = self.pending[i].filter(|p| p.todo == Todo::Inject) else {
            return Outcome::Quiet;
        };
        if went_in {
            self.released(button, true, look, now);
            Outcome::Recovered { tries: p.tries }
        } else {
            let wait = FIRST_RETRY
                .checked_mul(1 << p.tries.min(8))
                .map_or(SLOWEST, |w| w.min(SLOWEST));
            self.pending[i] = Some(Pending {
                todo: Todo::Inject,
                tries: p.tries + 1,
                due: now + wait,
            });
            Outcome::Failing { tries: p.tries + 1 }
        }
    }

    /// A look found the button `down` or up. `Outcome::StillDown` asks for the release to be
    /// injected again.
    pub fn looked(&mut self, button: i32, down: bool, now: Instant) -> Outcome {
        let Some(i) = index(button) else {
            return Outcome::Quiet;
        };
        let Some(p) = self.pending[i].filter(|p| p.todo == Todo::Look) else {
            return Outcome::Quiet;
        };
        if !down {
            self.pending[i] = None;
            return Outcome::Quiet;
        }
        let tries = p.tries + 1;
        match LOOKS.get(tries as usize) {
            Some(wait) => {
                self.pending[i] = Some(Pending {
                    todo: Todo::Look,
                    tries,
                    due: now + *wait,
                });
                Outcome::StillDown { tries }
            }
            None => {
                self.pending[i] = None;
                Outcome::GivenUp
            }
        }
    }

    /// The buttons pressed and not released, to release when the process exits.
    pub fn down(&self) -> i32 {
        self.down
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const L: i32 = MOUSE_BUTTON_LEFT;
    const R: i32 = MOUSE_BUTTON_RIGHT;

    fn evt(kind: i32, buttons: i32) -> MouseEvent {
        MouseEvent {
            mask: kind | buttons << 3,
            ..Default::default()
        }
    }

    fn mv(x: i32, y: i32) -> MouseEvent {
        MouseEvent {
            mask: crate::input::MOUSE_TYPE_MOVE,
            x,
            y,
            ..Default::default()
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_controlled_side_follows_presses_and_releases() {
        let mut h = Held::default();
        assert!(!h.releases(&evt(MOUSE_TYPE_UP, L)));
        h.injecting(&evt(MOUSE_TYPE_DOWN, L));
        assert!(h.releases(&evt(MOUSE_TYPE_UP, L)));
        assert!(!h.releases(&evt(MOUSE_TYPE_UP, R)));
        assert!(!h.releases(&evt(MOUSE_TYPE_DOWN, L)));
        assert!(!h.releases(&mv(0, 0)));
        h.injecting(&mv(0, 0));
        assert!(h.releases(&evt(MOUSE_TYPE_UP, L)));
        h.injecting(&evt(MOUSE_TYPE_UP, L));
        assert!(!h.releases(&evt(MOUSE_TYPE_UP, L)));
        assert_eq!(h.take(), 0);
    }

    // What goes when the connection or the permission does: every button still held, once.
    #[test]
    fn everything_held_is_taken_once() {
        let mut h = Held::default();
        h.injecting(&evt(MOUSE_TYPE_DOWN, L));
        h.injecting(&evt(MOUSE_TYPE_DOWN, R));
        h.injecting(&evt(MOUSE_TYPE_UP, L));
        assert_eq!(h.take(), R);
        assert_eq!(h.take(), 0);
        assert!(!h.releases(&evt(MOUSE_TYPE_UP, R)));
    }

    #[test]
    fn a_release_is_the_event_of_one_button_going_up() {
        let up = release_of(R);
        assert_eq!((kind(&up), buttons(&up), up.x, up.y), (MOUSE_TYPE_UP, R, 0, 0));
    }

    #[test]
    fn a_release_that_does_not_go_in_is_tried_again_ever_more_slowly() {
        let t = Instant::now();
        let mut g = Guard::default();
        g.pressed(L);
        assert_eq!(g.down(), L);
        assert_eq!(g.released(L, false, true, t), Outcome::Failing { tries: 1 });
        assert_eq!(g.down(), L);
        assert_eq!(g.due(t + ms(99)), vec![]);
        let mut now = t;
        for (n, wait) in [100, 200, 400, 800, 1000, 1000].into_iter().enumerate() {
            assert_eq!(g.next_due(), Some(now + ms(wait)));
            now += ms(wait);
            assert_eq!(g.due(now), vec![(L, Todo::Inject)]);
            let tries = n as u32 + 2;
            assert_eq!(g.injected(L, false, true, now), Outcome::Failing { tries });
        }
        // Once it goes in the button counts as up, and is looked at.
        now += ms(1000);
        assert_eq!(g.injected(L, true, true, now), Outcome::Recovered { tries: 7 });
        assert_eq!(g.down(), 0);
        assert_eq!(g.due(now + LOOKS[0]), vec![(L, Todo::Look)]);
    }

    #[test]
    fn a_button_still_down_after_its_release_is_released_again_twice_and_then_left() {
        let t = Instant::now();
        let mut g = Guard::default();
        g.pressed(L);
        assert_eq!(g.released(L, true, true, t), Outcome::Quiet);
        assert_eq!(g.down(), 0);
        assert_eq!(g.due(t + ms(49)), vec![]);
        let mut now = t + ms(50);
        assert_eq!(g.due(now), vec![(L, Todo::Look)]);
        assert_eq!(g.looked(L, true, now), Outcome::StillDown { tries: 1 });
        now += ms(150);
        assert_eq!(g.due(now - ms(1)), vec![]);
        assert_eq!(g.looked(L, true, now), Outcome::StillDown { tries: 2 });
        now += ms(400);
        assert_eq!(g.due(now), vec![(L, Todo::Look)]);
        assert_eq!(g.looked(L, true, now), Outcome::GivenUp);
        assert_eq!(g.next_due(), None);
    }

    #[test]
    fn a_button_found_up_is_done_with() {
        let t = Instant::now();
        let mut g = Guard::default();
        g.pressed(R);
        g.released(R, true, true, t);
        assert_eq!(g.looked(R, false, t + ms(50)), Outcome::Quiet);
        assert_eq!(g.next_due(), None);
        // Nothing is looked at where the system cannot be asked.
        g.pressed(R);
        g.released(R, true, false, t);
        assert_eq!(g.next_due(), None);
    }

    // The second press of a double click comes before the look at the first release.
    #[test]
    fn a_new_press_ends_the_work_on_the_last_release() {
        let t = Instant::now();
        let mut g = Guard::default();
        g.pressed(L);
        g.released(L, true, true, t);
        g.pressed(L);
        assert_eq!(g.next_due(), None);
        assert_eq!(g.looked(L, true, t + ms(50)), Outcome::Quiet);
        g.released(L, false, true, t);
        g.pressed(L);
        assert_eq!(g.next_due(), None);
        assert_eq!(g.injected(L, true, true, t + ms(100)), Outcome::Quiet);
        assert_eq!(g.down(), L);
    }

    #[test]
    fn each_button_has_its_own_work() {
        let t = Instant::now();
        let mut g = Guard::default();
        g.pressed(L);
        g.pressed(R);
        g.released(L, false, true, t);
        g.released(R, true, true, t);
        assert_eq!(g.down(), L);
        assert_eq!(g.next_due(), Some(t + ms(50)));
        assert_eq!(
            g.due(t + ms(100)),
            vec![(L, Todo::Inject), (R, Todo::Look)]
        );
    }

    #[test]
    fn buttons_are_named_for_the_log() {
        assert_eq!(names(L | MOUSE_BUTTON_WHEEL), "left+middle");
        assert_eq!(names(0), "");
    }
}
