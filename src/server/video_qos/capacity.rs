//! How far the bitrate comes down, and how it climbs back, from what the path was measured to
//! carry.
//!
//! The controller in the parent module decides *when* the bitrate has to come down: two bad
//! probe replies in a row. How far, and how it comes back, it takes from tables written for
//! targets of a few Mbps: five to fifty percent down, 150 kbps up every three seconds. With a
//! target of 13 or 20 Mbps on a path that carries 8, that is many seconds of queue on the way
//! down and minutes on the way up.
//!
//! The probe travels in the stream the video travels in, in order. So the bytes written between
//! two probes arrive between the two probes, and the viewer has only to say *when* each probe
//! reached it: those bytes over that time is the rate the path delivered them at. While a queue
//! stands in the path, that rate is what the path carries.
//!
//! Two things more are needed to trust it. A pause in the path makes replies late too, and then
//! the path catches up: in the interval after it delivers more than was sent into it. A path
//! that is full delivers less than was sent in every interval, so that is what is asked for.
//! And a screen that hardly changes sends little, and little arrives; that says nothing of the
//! path. The samples count when the encoder was putting out something like its bitrate, or when
//! the video loop was waiting for its frames to be taken: a connection whose send is held up
//! takes no frame, so once the send buffers are full the encoder is slowed to the pace of the
//! path, sent and delivered are the same again, and only that wait still says the path is full.
//!
//! Then the stream goes below what the path carried in one step: well below at first, so the
//! queue drains, then just below. That level is remembered as a ceiling, in kbps of real output,
//! which is a property of the path and not of an encoder or a resolution. The way back is fast
//! up to it, and past it only by small steps after a wait that grows each time it is met again.
//!
//! Without such a sample nothing here acts and the parent's tables stand as they were: a viewer
//! from before the field, more than one viewer, more than one display. The one exception is a
//! target above the presets, which is climbed to and not jumped to, and climbed back to faster
//! than the table's 150 kbps, whoever watches.

use super::*;

/// A session whose target is above this starts here and climbs: it is what the best preset asks
/// for, so a weak path starts no worse than it did before targets went up.
pub(super) const START_RATIO: f32 = BR_BEST;
/// One step of that climb, a second apart.
const START_STEP: f32 = 1.5;
const START_STEP_EVERY: Duration = Duration::from_secs(1);
/// The climb goes on while replies come back this soon above the baseline.
const CLEAN_REPLY_MS: u32 = 50;
/// A screen that changed: this many frames encoded in the second.
const CHANGING_FRAMES: usize = 2;

/// The delivery rate is the higher of this many probe intervals: a full path shows what it
/// carries in each, and after a pause the interval of catching up shows that it carries more.
const SAMPLES: usize = 2;
/// A sample older than this says nothing of now.
const SAMPLE_AGE: Duration = Duration::from_secs(3);
/// The path kept up in an interval: it delivered this share of what was sent into it, or more.
/// The two are counted between the same probes; what is left is the probes' own jitter.
const KEPT_UP: f32 = 0.97;
/// The encoder was at work: it put out this share of its nominal bitrate or more.
const AT_WORK: f32 = 0.5;
/// The video loop waited this share of its time for its frames to be taken: the send is held up.
const HELD_UP_SHARE: f32 = 0.3;

/// After a cut the stream first keeps well below what the path carried, until the queue that
/// built up on the way is sent: low enough to send it in about this long, but between a quarter
/// and half of what the path carries. At half, a queue is sent in twice its length.
const DRAIN_IN: Duration = Duration::from_secs(6);
const DEEPEST_DRAIN: f32 = 0.25;
const DRAIN: f32 = 0.5;
const SHORTEST_DRAIN: Duration = Duration::from_secs(1);
const LONGEST_DRAIN: Duration = Duration::from_secs(15);
/// Then this far below it.
const BELOW_CAPACITY: f32 = 0.85;
/// One cut takes the stream no lower than this share of where it was: a sample can be wrong.
const DEEPEST_CUT: f32 = 0.1;
/// The fast way back ends this close to the ceiling.
const NEAR_CEILING: f32 = 0.9;
/// This far past the ceiling without congestion, the ceiling is no more.
const PAST_CEILING: f32 = 1.1;
/// One probing step past the fast way back, as a share of the ceiling.
const PROBE_STEP: f32 = 0.05;
/// The wait at the ceiling before probing past it: this at first, doubled each time the ceiling
/// is met again, up to the longest. Probing a path that has not changed costs a short queue each
/// time; not probing one that has, costs quality for as long as the wait. The longest wait, the
/// probing steps and the climb after them come to a minute and a half.
const FIRST_HOLD: Duration = Duration::from_secs(30);
const LONGEST_HOLD: Duration = Duration::from_secs(60);

/// A cut to this share or below is one the viewer is told about.
const NOTICEABLE_CUT: f32 = 0.8;
/// Back within this share of the most allowed, the stream is no longer "lowered".
const BACK: f32 = 0.9;

/// An encoder seen to put out this much more than its nominal bitrate, second after second, is
/// taken at its word, for as long as the last seconds like it are remembered.
const OVERSHOOT_SEEN: f32 = 1.15;
const MOST_OVERSHOOT: f32 = 3.0;
const OVERSHOOT_SECONDS: usize = 10;
/// The seconds an encoder's output is judged over.
const RATE_SECONDS: usize = 3;
/// Sent this much above a limit, the ratio follows what was sent and not the nominal bitrate.
const OVER_LIMIT: f32 = 1.1;
/// Half again its bitrate in a whole second is no change of scene: that is the encoder, and
/// remembered at once.
const LASTING_OVERSHOOT: f32 = 1.5;

/// An encoder that takes a new bitrate only by being created again is created again no sooner
/// than this after the last time: each time the picture stops for a moment.
const RESTARTS_APART: Duration = Duration::from_secs(30);
/// And the bitrate is tried higher again only this long after it came down: this at first,
/// doubled each time it comes down again, up to the longest. Then by half again at a time: a
/// path that has not changed is overrun by half and not by all the target asks for.
const FIRST_WAIT_BACK: Duration = Duration::from_secs(300);
const LONGEST_WAIT_BACK: Duration = Duration::from_secs(1200);
const STEP_BACK: f32 = 1.5;

/// What the path delivered between two probes, and what was sent into it in that time.
#[derive(Debug, Clone, Copy)]
pub(super) struct Delivery {
    at: Instant,
    delivered_kbps: u32,
    sent_kbps: u32,
}

/// A viewer's last deliveries.
#[derive(Debug, Default, Clone)]
pub(super) struct Deliveries(VecDeque<Delivery>);

impl Deliveries {
    fn push(&mut self, delivery: Delivery) {
        if self.0.len() >= SAMPLES {
            self.0.pop_front();
        }
        self.0.push_back(delivery);
    }

    /// The most the path delivered in the last intervals, what was sent in the latest, and
    /// whether it fell short of what was sent in every one of them; if the latest is fresh.
    fn latest(&self, now: Instant) -> Option<(u32, u32, bool)> {
        let last = self.0.back()?;
        if now.saturating_duration_since(last.at) > SAMPLE_AGE {
            return None;
        }
        let delivered = self.0.iter().map(|d| d.delivered_kbps).max()?;
        let fell_short = self
            .0
            .iter()
            .all(|d| (d.delivered_kbps as f32) < d.sent_kbps as f32 * KEPT_UP);
        Some((delivered, last.sent_kbps, fell_short))
    }
}

/// What the video loop measured, second by second.
#[derive(Debug, Default, Clone)]
pub(super) struct Rates {
    sent_kbps: VecDeque<u32>,
    held_up: VecDeque<f32>,
    encoded: VecDeque<usize>,
}

impl Rates {
    fn push(&mut self, sent_kbps: u32, held_up: f32, encoded: usize) {
        if self.sent_kbps.len() >= RATE_SECONDS {
            self.sent_kbps.pop_front();
            self.held_up.pop_front();
            self.encoded.pop_front();
        }
        self.sent_kbps.push_back(sent_kbps);
        self.held_up.push_back(held_up.clamp(0.0, 1.0));
        self.encoded.push_back(encoded);
    }

    /// The share of the last seconds the loop waited for its frames to be taken.
    fn held_up(&self) -> f32 {
        if self.held_up.is_empty() {
            return 0.0;
        }
        self.held_up.iter().sum::<f32>() / self.held_up.len() as f32
    }

    /// The last second, if the screen changed in it and nothing held the loop up: what the
    /// encoder put out when left to.
    fn last_free_kbps(&self) -> Option<u32> {
        let free = *self.held_up.back()? <= 0.1 && *self.encoded.back()? >= CHANGING_FRAMES;
        free.then(|| *self.sent_kbps.back().unwrap_or(&0))
    }

    /// The least encoded in any of the last seconds, once there are that many, if the screen
    /// changed in each and nothing held the loop up: what the encoder puts out second after
    /// second. One heavy second is a change of scene, which rate control takes back.
    fn sustained_kbps(&self) -> Option<u32> {
        if self.sent_kbps.len() < RATE_SECONDS
            || self.held_up.iter().any(|held| *held > 0.1)
            || self.encoded.iter().any(|frames| *frames < CHANGING_FRAMES)
        {
            return None;
        }
        self.sent_kbps.iter().copied().min()
    }
}

/// What is known when congestion is confirmed.
#[derive(Debug, Clone, Copy)]
pub(super) struct Evidence {
    /// What the path delivered, in kbps.
    pub delivered_kbps: u32,
    /// What was sent into it in the same time.
    pub sent_kbps: u32,
    /// The path delivered less than was sent into it in every one of the last intervals.
    pub fell_short: bool,
    /// The encoder's bitrate at the current ratio.
    pub nominal_kbps: u32,
    /// The share of its time the video loop waited for its frames to be taken.
    pub held_up: f32,
    /// How much queue there is to send, in milliseconds of the path's rate.
    pub queue_ms: u32,
}

impl Evidence {
    /// The delivery says what the path carries: the path was full, with something to carry.
    fn counts(&self) -> bool {
        let at_work = self.sent_kbps as f32 >= self.nominal_kbps as f32 * AT_WORK;
        self.delivered_kbps > 0
            && self.nominal_kbps > 0
            && ((self.fell_short && at_work) || self.held_up >= HELD_UP_SHARE)
    }
}

#[derive(Debug, Clone)]
pub(super) struct Capacity {
    /// The target this state belongs to; another target starts over.
    target: Option<f32>,
    /// Climbing from `START_RATIO`, and when it last took a step.
    starting: bool,
    start_step_at: Option<Instant>,
    /// The climb ended on a reply that showed a queue: nothing is raised before this.
    raise_not_before: Option<Instant>,
    /// What the path carried when it last fell short, in kbps of real output.
    ceiling_kbps: Option<f32>,
    /// What the stream may put out under that ceiling once the queue has drained, and how far
    /// below and until when it keeps to drain it.
    settled_kbps: f32,
    drain_kbps: f32,
    drain_until: Option<Instant>,
    /// How long the wait at the ceiling is now, and until when.
    hold: Duration,
    hold_until: Option<Instant>,
    /// How much more than its nominal bitrate the encoder put out in the last seconds it was
    /// seen at work.
    overshoots: VecDeque<f32>,
    /// Cut for the network by a fifth or more, and not back yet.
    lowered: bool,
}

impl Default for Capacity {
    fn default() -> Self {
        Capacity {
            target: None,
            starting: false,
            start_step_at: None,
            raise_not_before: None,
            ceiling_kbps: None,
            settled_kbps: 0.0,
            drain_kbps: 0.0,
            drain_until: None,
            hold: Duration::ZERO,
            hold_until: None,
            overshoots: Default::default(),
            lowered: false,
        }
    }
}

impl Capacity {
    /// What the path carried is no longer known: it carries more than it did, or the viewers
    /// changed, or what they ask for. Whether the stream is lowered is not touched: that is said
    /// of where the stream is, which this does not move.
    fn forget_ceiling(&mut self) {
        self.ceiling_kbps = None;
        self.settled_kbps = 0.0;
        self.drain_until = None;
        self.hold = Duration::ZERO;
        self.hold_until = None;
    }

    /// The ratio to go on with when the viewer's target is `target` and the stream is at
    /// `current`. `climbs` is false where the bitrate cannot follow from second to second.
    ///
    /// A target the presets reach is taken at once, every time it is set, as it always was. A
    /// higher one is climbed to: from `START_RATIO` at first, from where the stream is when it
    /// is raised later; set again unchanged, it changes nothing.
    fn on_target(&mut self, current: f32, target: f32, climbs: bool) -> f32 {
        if !climbs || target <= START_RATIO {
            self.target = Some(target);
            self.starting = false;
            self.forget_ceiling();
            self.lowered = false;
            return target;
        }
        if self.target == Some(target) {
            return current;
        }
        let first = self.target.is_none();
        self.target = Some(target);
        self.forget_ceiling();
        let from = if first {
            START_RATIO
        } else {
            current.min(target)
        };
        // Climbing to what is newly asked for is not "lowered for the network"; lowered for it
        // before and still short of what is asked now, is.
        self.lowered = self.lowered && from < target * BACK;
        self.starting = from < target;
        from
    }

    pub(super) fn starting(&self) -> bool {
        self.starting
    }

    /// The climb ends: the bitrate was cut, or the encoder turned out not to follow.
    pub(super) fn end_start(&mut self) {
        self.starting = false;
    }

    /// The climb ends on a reply that showed a queue: where it got to is held until `until`,
    /// and not raised further by the ordinary way up in the same breath.
    fn end_start_on_queue(&mut self, until: Instant) {
        self.starting = false;
        self.raise_not_before = Some(until);
    }

    fn raise_held(&self, now: Instant) -> bool {
        self.raise_not_before.is_some_and(|until| now < until)
    }

    /// One step of the climb, after a clean second with the screen changing; `ratio` itself when
    /// the last step was less than a second ago.
    fn start_step(&mut self, ratio: f32, most: f32, now: Instant) -> f32 {
        if self
            .start_step_at
            .is_some_and(|at| now.saturating_duration_since(at) < START_STEP_EVERY)
        {
            return ratio;
        }
        self.start_step_at = Some(now);
        let next = (ratio * START_STEP).min(most);
        if next >= most {
            self.starting = false;
        }
        next
    }

    /// Congestion is confirmed. When the delivery says what the path carries, that becomes the
    /// ceiling and the stream's allowance goes below it; `false` when it does not, and the
    /// parent's table decides.
    fn cut(&mut self, evidence: &Evidence, now: Instant) -> bool {
        if !evidence.counts() {
            return false;
        }
        // Should the sample be wrong: one cut takes the stream to a fifth of where it was at the
        // lowest, while it drains and after.
        let least = evidence.nominal_kbps as f32 * self.overshoot() * DEEPEST_CUT;
        let ceiling = (evidence.delivered_kbps as f32).max(least / BELOW_CAPACITY);
        self.hold = if self.ceiling_kbps.is_some() {
            (self.hold * 2).clamp(FIRST_HOLD, LONGEST_HOLD)
        } else {
            FIRST_HOLD
        };
        self.hold_until = Some(now + self.hold);
        self.ceiling_kbps = Some(ceiling);
        self.settled_kbps = ceiling * BELOW_CAPACITY;
        // Sending at the share `level` of what the path carries, a queue of q seconds is gone in
        // q / (1 - level).
        let queue = evidence.queue_ms as f32 / 1000.0;
        let level = (1.0 - queue / DRAIN_IN.as_secs_f32()).clamp(DEEPEST_DRAIN, DRAIN);
        self.drain_kbps = (ceiling * level).max(least).min(self.settled_kbps);
        let level = self.drain_kbps / ceiling;
        let drain = if level < 0.95 {
            Duration::from_secs_f32(queue / (1.0 - level)).clamp(SHORTEST_DRAIN, LONGEST_DRAIN)
        } else {
            LONGEST_DRAIN
        };
        self.drain_until = Some(now + drain);
        self.starting = false;
        true
    }

    /// Still sending the queue of the last cut: nothing is cut further on replies that are bad
    /// because of that queue.
    fn draining(&self, now: Instant) -> bool {
        self.drain_until.is_some_and(|until| now < until)
    }

    /// What the stream may put out under the ceiling now, in kbps; `None` without a ceiling.
    fn allowed_kbps(&self, now: Instant) -> Option<f32> {
        self.ceiling_kbps?;
        Some(if self.draining(now) {
            self.drain_kbps
        } else {
            self.settled_kbps
        })
    }

    pub(super) fn has_ceiling(&self) -> bool {
        self.ceiling_kbps.is_some()
    }

    /// One step up under the ceiling, with every viewer's delay good and the screen changing:
    /// fast to just below it, then, the wait over, by small steps past it until it is no more.
    fn raise_allowance(&mut self, now: Instant) {
        let Some(ceiling) = self.ceiling_kbps else {
            return;
        };
        if self.draining(now) {
            return;
        }
        let near = ceiling * NEAR_CEILING;
        if self.settled_kbps < near {
            self.settled_kbps = (self.settled_kbps * 1.25).min(near);
        } else if !self.hold_until.is_some_and(|until| now < until) {
            self.settled_kbps += ceiling * PROBE_STEP;
            if self.settled_kbps > ceiling * PAST_CEILING {
                // The path carries more than it did.
                self.forget_ceiling();
            }
        }
    }

    /// The next ratio on the way up without a ceiling. `excess_ms` is the worst viewer's delay
    /// above its baseline.
    fn raise_ratio(ratio: f32, excess_ms: u32) -> f32 {
        ratio
            * if excess_ms < 50 {
                1.25
            } else if excess_ms < 100 {
                1.15
            } else {
                1.05
            }
    }

    /// A second of the video loop at a nominal `nominal_kbps`. How much of its bitrate the
    /// encoder put out, second after second with nothing holding it back, is kept for the last
    /// such seconds: an encoder's excess can depend on the frame rate, which moves.
    fn learn_overshoot(&mut self, rates: &Rates, nominal_kbps: u32) {
        let Some(sustained) = rates.sustained_kbps() else {
            return;
        };
        if nominal_kbps == 0 {
            return;
        }
        self.note_output(sustained as f32 / nominal_kbps as f32);
    }

    /// The encoder was seen to put out `share` of its nominal bitrate.
    fn note_output(&mut self, share: f32) {
        if self.overshoots.len() >= OVERSHOOT_SECONDS {
            self.overshoots.pop_front();
        }
        self.overshoots.push_back(share.min(MOST_OVERSHOOT));
    }

    /// How much more than its nominal bitrate the encoder puts out, 1 when it does not.
    pub(super) fn overshoot(&self) -> f32 {
        let most = self.overshoots.iter().copied().fold(1.0, f32::max);
        if most > OVERSHOOT_SEEN {
            most
        } else {
            1.0
        }
    }

    /// The ratio went from `from` to `to`, where `most` is the most the viewer's target and the
    /// relay allow: is the stream lowered for the network, which the viewer is told about?
    fn note_change(&mut self, from: f32, to: f32, most: f32, cut: bool) {
        if cut && to <= from * NOTICEABLE_CUT && to < most * BACK {
            self.lowered = true;
        } else if to >= most * BACK {
            self.lowered = false;
        }
    }

    pub(super) fn lowered(&self) -> bool {
        self.lowered
    }
}

/// The ratio at which an encoder that puts out `nominal_kbps × overshoot` at `ratio` puts out
/// `limit_kbps`, taking its bitrate as proportional to the ratio near here. Hardware encoders are
/// not quite: asked again from the ratio this gives, a second later, the answer closes in.
fn ratio_for_kbps(ratio: f32, nominal_kbps: u32, overshoot: f32, limit_kbps: f32) -> f32 {
    ratio * limit_kbps / (nominal_kbps as f32 * overshoot.max(1.0))
}

/// For an encoder that takes a new bitrate only by being created again.
#[derive(Debug, Default, Clone)]
pub(super) struct Restart {
    /// The encoder runs below the viewer's target because this side brought it down.
    below: bool,
    /// When an encoder was last created, or asked to be, for whatever reason.
    last: Option<Instant>,
    /// How long after that the bitrate may be tried higher.
    wait_back: Duration,
}

impl VideoQoS {
    /// A probe came back. Between the probe before it and this one the connection wrote `bytes`
    /// of video, in `sent` by this side's clock; the viewer says they reached it in `received`,
    /// by its own.
    pub fn user_delivery(&mut self, id: i32, bytes: u64, sent: Duration, received: Duration) {
        let now = self.now();
        let kbps = |over: Duration| {
            let ms = over.as_millis().max(1) as u64;
            (bytes * 8 / ms).min(u32::MAX as u64) as u32
        };
        if let Some(user) = self.users.get_mut(&id) {
            user.deliveries.push(Delivery {
                at: now,
                delivered_kbps: kbps(received),
                sent_kbps: kbps(sent),
            });
        }
    }

    /// The viewer's connection goes through the relay.
    pub fn user_relayed(&mut self, id: i32, relayed: bool) {
        if let Some(user) = self.users.get_mut(&id) {
            user.relayed = relayed;
        }
    }

    /// The display's encoder cannot change its bitrate while it runs, but may be created again
    /// for a new one.
    pub fn set_restartable(&mut self, video_service_name: &str, restartable: bool) {
        if let Some(display) = self.displays.get_mut(video_service_name) {
            display.restartable = restartable;
        }
    }

    /// Whether the display's video loop is to create its encoder again for the ratio it now
    /// reads. Each display is asked for itself, and answers once.
    pub fn take_restart_request(&mut self, video_service_name: &str) -> bool {
        self.displays
            .get_mut(video_service_name)
            .is_some_and(|display| std::mem::take(&mut display.restart))
    }

    /// Asks every display's video loop to create its encoder again.
    fn request_restart(&mut self, now: Instant) {
        self.displays
            .values_mut()
            .for_each(|display| display.restart = true);
        // Counted from the asking: the loop that answers may take its time.
        self.restart.last = Some(now);
    }

    /// The ratio to create an encoder with. Where the stream is, but for the climb from the
    /// start: then the target. An encoder that follows the ratio while it runs is brought to
    /// where the climb is before its first frame; one that does not, runs at the target, as it
    /// always did. Which of the two it is, is known only once it is there.
    pub fn ratio_for_creation(&mut self) -> f32 {
        if self.capacity.starting() {
            self.max_ratio()
        } else {
            self.ratio()
        }
    }

    /// The bitrate of an encoder that was created with `ratio`, which is not always where the
    /// stream is (`ratio_for_creation`).
    pub fn store_bitrate_at(&mut self, ratio: f32, bitrate: u32) {
        self.bitrate_store = bitrate;
        self.bitrate_ratio = ratio;
    }

    /// Bitrate control is off, or the encoders cannot be controlled: the ratio is the viewer's
    /// target, as it always was.
    pub(super) fn follow_target(&mut self) {
        let known = self.displays.values().all(|display| display.known);
        if self.restart.below && !known {
            // An encoder this side holds down, while another display's is being created: what
            // that one can do is not known yet, and nothing is let go on its account.
            return;
        }
        self.ratio = self.latest_quality().ratio();
        if known {
            self.release_restart_hold();
            self.encoder_cannot_follow();
        }
    }

    /// An encoder's bitrate does not follow the ratio while it runs: there is no climb from the
    /// start for it, it has its target.
    pub(super) fn encoder_cannot_follow(&mut self) {
        if self.capacity.starting() {
            self.capacity.end_start();
            self.ratio = self.latest_quality().ratio();
        }
        if !self.restart.below && !self.restart_only() {
            self.capacity.lowered = false;
        }
    }

    /// `TestDelay.qos_state`: 1 while the stream is lowered for the network.
    pub fn qos_state(&self) -> u32 {
        self.capacity.lowered() as u32
    }

    /// A viewer came or went: the relay's limit is read again, and what was learned of the path
    /// described the viewers there were.
    pub(super) fn viewers_changed(&mut self) {
        self.relay_cap_kbps = Config::get_option(base::config::keys::OPTION_RELAY_MAX_BITRATE)
            .parse()
            .unwrap_or(0);
        self.capacity.forget_ceiling();
        self.ratio_cap = None;
    }

    /// A video loop started: an encoder is being created. What the last one put out says
    /// nothing of it.
    pub(super) fn encoder_created(&mut self) {
        self.restart.last = Some(self.now());
        self.capacity.overshoots.clear();
        self.steady_seconds = 0;
    }

    /// The most the viewer's target and the relay allow.
    pub(super) fn most_asked(&self) -> f32 {
        let target = self.latest_quality().ratio();
        match self.relay_ratio() {
            Some(relay) => target.min(relay),
            None => target,
        }
    }

    /// The most the ratio may be now: that, or less under a ceiling.
    pub(super) fn max_ratio(&self) -> f32 {
        let target = self.latest_quality().ratio();
        match self.ratio_cap {
            Some(cap) => target.min(cap),
            None => target,
        }
    }

    /// The relay's limit for each display's stream, in kbps, while a viewer is relayed.
    fn relay_kbps(&self) -> Option<f32> {
        let relayed = self.relay_cap_kbps > 0 && self.users.values().any(|user| user.relayed);
        // Every display has its own stream through the same relay.
        relayed.then(|| self.relay_cap_kbps as f32 / self.displays.len().max(1) as f32)
    }

    fn relay_ratio(&self) -> Option<f32> {
        let nominal = self.bitrate();
        let limit = self.relay_kbps()?;
        (nominal > 0).then(|| {
            ratio_for_kbps(
                self.bitrate_ratio,
                nominal,
                self.capacity.overshoot(),
                limit,
            )
        })
    }

    /// One viewer that says when the probes reach it, of one display: the deliveries describe
    /// one path and one encoder.
    fn evidence(&self) -> Option<Evidence> {
        if self.users.len() != 1 || self.displays.len() != 1 {
            return None;
        }
        let user = self.users.values().next()?;
        let display = self.displays.values().next()?;
        let (delivered_kbps, sent_kbps, fell_short) = user.deliveries.latest(self.now())?;
        // The probe that just came back saw the queue there was when it went out. It was under
        // way for its whole delay, and all that time more went into the path than came out.
        let saw = user.delay.avg_delay() as f32;
        let under_way = user.delay.delay_history.back().copied().unwrap_or_default() as f32;
        let growth = (sent_kbps as f32 / delivered_kbps.max(1) as f32 - 1.0).clamp(0.0, 4.0);
        Some(Evidence {
            delivered_kbps,
            sent_kbps,
            fell_short,
            nominal_kbps: self.bitrate(),
            held_up: display.rates.held_up(),
            queue_ms: (saw + under_way * growth) as u32,
        })
    }

    /// The way up takes the path's measure and not the table's 150 kbps: the viewer says when
    /// the probes reach it, or asks for more than the presets, which the table was not made for.
    pub(super) fn climbs_by_measure(&self) -> bool {
        self.evidence().is_some() || self.latest_quality().ratio() > START_RATIO
    }

    /// The ratio for confirmed congestion at `ratio`, where the deliveries say what the path
    /// carries; `None` where they do not and the parent's table decides. `Some(ratio)` while the
    /// queue of the last such cut is still being sent.
    pub(super) fn cut_by_delivery(&mut self, ratio: f32, least: f32) -> Option<f32> {
        let now = self.now();
        if self.capacity.draining(now) {
            return Some(ratio);
        }
        let evidence = self.evidence()?;
        if !self.capacity.cut(&evidence, now) {
            return None;
        }
        let allowed = self.capacity.allowed_kbps(now)?;
        // From the ratio the encoder's bitrate is the bitrate of, which is `ratio` unless the
        // video loop has yet to take the last change.
        let to = ratio_for_kbps(
            self.bitrate_ratio,
            evidence.nominal_kbps,
            self.capacity.overshoot(),
            allowed,
        );
        // Down, whatever the sample said, and not below a fifth of where the stream was.
        Some(to.min(ratio).max(ratio * DEEPEST_CUT).max(least))
    }

    /// The next ratio on the way up from `ratio`, every viewer's delay good and the screen
    /// changing; the caller keeps it under `max_ratio`, which this may have raised.
    pub(super) fn raise_by_measure(&mut self, ratio: f32, excess_ms: u32) -> f32 {
        let now = self.now();
        if self.capacity.raise_held(now) {
            return ratio;
        }
        if !self.capacity.has_ceiling() {
            return Capacity::raise_ratio(ratio, excess_ms);
        }
        self.capacity.raise_allowance(now);
        self.keep_under_limits();
        // Under a ceiling the ratio is the one the allowance comes to; the ceiling gone with
        // this step, the ratio goes on from where it is.
        match self.ratio_cap {
            Some(cap) if self.capacity.has_ceiling() => cap,
            _ => ratio,
        }
    }

    /// The ratio to go on with after a viewer set its image quality.
    pub(super) fn ratio_for_target(&mut self) -> f32 {
        let target = self.latest_quality().ratio();
        self.ratio_cap = None;
        // Whether the encoder follows the ratio while it runs is not asked here: the quality may
        // come before the encoder, or while it is being created. One that does not follow takes
        // its target when it says so (`encoder_cannot_follow`).
        self.capacity.on_target(self.ratio, target, self.abr_config)
    }

    /// One second of the climb from the start, for a display that encoded `encoded` frames in it.
    ///
    /// It begins once every viewer's frame rate has done its own start: the two would add up, and
    /// a queue from the bitrate would end the frame rate's start for good. It does not touch
    /// `adjust_ratio_instant`, so a cut that becomes due is not kept waiting by it. A screen that
    /// does not change keeps it waiting; a reply that shows a queue ends it.
    pub(super) fn climb_from_start(&mut self, encoded: usize) {
        if !self.capacity.starting() || self.users.is_empty() {
            return;
        }
        if self.users.values().any(|user| {
            user.delay.delay_history.is_empty() || user.delay.startup_good_samples != u8::MAX
        }) {
            return;
        }
        let excess = self
            .users
            .values()
            .map(|user| user.delay.avg_delay())
            .max()
            .unwrap_or_default();
        if excess >= CLEAN_REPLY_MS {
            let until = self.now() + Duration::from_secs(ADJUST_RATIO_INTERVAL as u64);
            self.capacity.end_start_on_queue(until);
            return;
        }
        if encoded < CHANGING_FRAMES {
            return;
        }
        let (most, now) = (self.max_ratio(), self.now());
        self.ratio = self.capacity.start_step(self.ratio, most, now);
    }

    /// What the video loop of a display measured since it last said: the bytes and the frames it
    /// encoded, and how long it waited for its frames to be taken, in `elapsed`.
    pub fn update_display_rates(
        &mut self,
        video_service_name: &str,
        sent_bytes: usize,
        encoded: usize,
        held_up: Duration,
        elapsed: Duration,
    ) {
        let secs = elapsed.as_secs_f32();
        if secs <= 0.0 {
            return;
        }
        let nominal = self.bitrate();
        let alone = self.displays.len() == 1;
        // What was sent in a second belongs to the ratio of that second: it says something of the
        // encoder at this ratio only if the ratio did not move meanwhile.
        self.steady_seconds = if self.ratio == self.ratio_at_rates {
            self.steady_seconds.saturating_add(1)
        } else {
            0
        };
        let steady = self.steady_seconds as usize >= RATE_SECONDS;
        let Some(display) = self.displays.get_mut(video_service_name) else {
            return;
        };
        display.rates.push(
            (sent_bytes as f32 * 8.0 / 1000.0 / secs) as u32,
            held_up.as_secs_f32() / secs,
            // By the second, as the frames are counted elsewhere.
            (encoded as f32 / secs).round() as usize,
        );
        // With one encoder its nominal bitrate is the one stored; with more, whose it is is not
        // known here.
        if alone && steady {
            self.capacity.learn_overshoot(&display.rates, nominal);
        }
        self.keep_under_limits();
        self.ratio_at_rates = self.ratio;
    }

    /// The stream's real output stays under the relay's limit and under the path's ceiling, where
    /// the bitrate follows the ratio while the encoder runs. The ratio that comes to is worked
    /// out from where the stream is, every second, so that it closes in.
    fn keep_under_limits(&mut self) {
        let (nominal, now) = (self.bitrate(), self.now());
        let limit = match (self.relay_kbps(), self.capacity.allowed_kbps(now)) {
            (Some(relay), Some(path)) => Some(relay.min(path)),
            (relay, path) => relay.or(path),
        };
        let Some(limit) = limit.filter(|_| nominal > 0 && self.in_vbr_state()) else {
            self.ratio_cap = None;
            return;
        };
        // From the ratio the encoder's bitrate is the bitrate of: asked twice before the video
        // loop takes a change, the answer is the same.
        let at = self.bitrate_ratio;
        let mut cap = ratio_for_kbps(at, nominal, self.capacity.overshoot(), limit);
        // What was really sent says more than the nominal bitrate when it is over the limit; but
        // only of the ratio it was sent at, so not in a second in which the ratio moved.
        let sent = self
            .displays
            .values()
            .next()
            .filter(|_| self.displays.len() == 1 && self.steady_seconds >= 1)
            .and_then(|display| display.rates.last_free_kbps());
        if let Some(sent) = sent.filter(|sent| *sent as f32 > limit * OVER_LIMIT) {
            cap = cap.min(at * limit / sent as f32);
            // An encoder whose output goes with the frame rate would be let back up as soon as
            // the frame rate fell, and be over the limit again as soon as it rose: that much
            // over its bitrate is remembered at once. A change of scene is not.
            let share = sent as f32 / nominal as f32;
            if share >= LASTING_OVERSHOOT {
                self.capacity.note_output(share);
            }
        }
        let cap = cap.max(self.min_ratio());
        self.ratio_cap = Some(cap);
        let most = self.max_ratio();
        if self.ratio > most {
            self.ratio = most;
        }
    }

    /// The ratio changed in `adjust_ratio`, from `from`; `most` is what the viewer's target and
    /// the relay allowed before it did.
    pub(super) fn note_ratio_change(&mut self, from: f32, most: f32, cut: bool) {
        let to = self.ratio;
        self.capacity.note_change(from, to, most, cut);
    }

    /// Bitrate control is on, and every display's encoder takes a new bitrate only by being
    /// created again, which it may be.
    pub(super) fn restart_only(&self) -> bool {
        self.abr_config
            && !self.displays.is_empty()
            && self
                .displays
                .values()
                .all(|display| !display.support_changing_quality && display.restartable)
    }

    /// One second of bitrate control by restart.
    ///
    /// Down: only when congestion is confirmed and the deliveries say what the path carries,
    /// only for a cut worth the pause, and no sooner than `RESTARTS_APART` after an encoder was
    /// last created. Up: after a long wait with every reply good and the screen changing, by
    /// half again, and after each further such wait by half again, until the target is reached.
    pub(super) fn restart_only_step(&mut self, encoded: usize) {
        let now = self.now();
        let most = self.latest_quality().ratio();
        self.capacity.end_start();
        if !self.restart.below {
            // Where it always was: the viewer's target, which a change of preset moves.
            self.ratio = most;
            self.capacity.lowered = false;
        }
        let since_restart = self
            .restart
            .last
            .map(|last| now.saturating_duration_since(last));
        let apart = since_restart.map_or(true, |since| since >= RESTARTS_APART);
        let confirmed = self
            .users
            .values()
            .any(|user| user.delay.needs_bitrate_reduction());
        if confirmed {
            if apart {
                self.restart_down(most, now);
            }
            return;
        }
        if !self.restart.below {
            // The viewer moved a custom quality, which the video loop does not create the
            // encoder again for by itself (a slider is dragged through many values): it is asked
            // to here, where the last time is known, so that the encoder's bitrate is the one
            // of the ratio again.
            let moved = self.bitrate() > 0 && (self.bitrate_ratio - most).abs() > most * 0.01;
            let asked = self.displays.values().any(|display| display.restart);
            if moved && apart && !asked {
                self.request_restart(now);
            }
            return;
        }
        if self.ratio >= most {
            // The viewer's target came down to where the stream is held: nothing is held down
            // any more, and the encoder is created at the target as for any change of it.
            self.restart_to(most, most, now);
            return;
        }
        let waited = since_restart.is_some_and(|since| since >= self.restart.wait_back);
        let good = self
            .users
            .values()
            .all(|user| user.delay.avg_delay() < DELAY_THRESHOLD_150MS);
        if !waited || !good || encoded < CHANGING_FRAMES {
            return;
        }
        // By half again; all the way when that comes within reach of the target, which would
        // else cost one more pause for the last tenth.
        let step = self.ratio * STEP_BACK;
        let to = if step >= most * BACK { most } else { step };
        self.restart_to(to, most, now);
    }

    /// Congestion is confirmed for an encoder that takes a new bitrate only by restart: if the
    /// deliveries say what the path carries and that is a fifth less than is sent or more, the
    /// encoder is created again for it, straight at where the stream settles.
    fn restart_down(&mut self, most: f32, now: Instant) {
        let Some(evidence) = self.evidence() else {
            return;
        };
        // Looked at, and left as it was if nothing comes of it.
        let before = self.capacity.clone();
        if !self.capacity.cut(&evidence, now) {
            return;
        }
        // Where the encoder is, which is the ratio its bitrate belongs to.
        let from = self.bitrate_ratio;
        let to = ratio_for_kbps(
            from,
            evidence.nominal_kbps,
            self.capacity.overshoot(),
            self.capacity.settled_kbps,
        )
        .max(from * DEEPEST_CUT)
        .max(self.min_ratio());
        if to > from * NOTICEABLE_CUT {
            self.capacity = before;
            return;
        }
        self.ratio = to;
        self.restart.below = true;
        self.capacity.note_change(from, to, most, true);
        self.restart.wait_back = if self.restart.wait_back.is_zero() {
            FIRST_WAIT_BACK
        } else {
            (self.restart.wait_back * 2).min(LONGEST_WAIT_BACK)
        };
        self.request_restart(now);
    }

    /// The encoder is created again for the ratio `to` on the way back up, of which `most` is
    /// the viewer's target.
    fn restart_to(&mut self, to: f32, most: f32, now: Instant) {
        let from = self.ratio;
        self.ratio = to;
        if to >= most {
            // All the way back: what the path carried then is not what it carries now.
            self.restart.below = false;
            self.capacity.forget_ceiling();
            self.capacity.lowered = false;
        } else {
            self.capacity.note_change(from, to, most, false);
        }
        self.request_restart(now);
    }

    /// The encoders are no longer held down by restart, as when one may no longer be created
    /// again: an encoder this side brought down is created once more, at the target.
    pub(super) fn release_restart_hold(&mut self) {
        if std::mem::take(&mut self.restart.below) {
            let now = self.now();
            self.capacity.forget_ceiling();
            self.request_restart(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(delivered_kbps: u32, sent_kbps: u32, held_up: f32) -> Evidence {
        Evidence {
            delivered_kbps,
            sent_kbps,
            fell_short: (delivered_kbps as f32) < sent_kbps as f32 * KEPT_UP,
            nominal_kbps: 20_000,
            held_up,
            queue_ms: 1_000,
        }
    }

    #[test]
    fn a_delivery_counts_only_when_the_path_was_full_with_something_to_carry() {
        // The viewer does not say when the probes reach it.
        assert!(!evidence(0, 20_000, 1.0).counts());
        // The encoder at work and less arrived than was sent, much less or a little: the path
        // was full, and what arrived is what it carries.
        assert!(evidence(8_000, 20_000, 0.0).counts());
        assert!(evidence(18_500, 20_000, 0.0).counts());
        // All that was sent arrived: the delay has another cause, and the path carries more.
        assert!(!evidence(19_900, 20_000, 0.0).counts());
        assert!(!evidence(26_000, 20_000, 0.0).counts());
        // A screen that hardly changes: little is sent, the path is not known.
        assert!(!evidence(250, 320, 0.0).counts());
        // The send buffers are full: the encoder is down to the path's pace, far below its
        // bitrate, sent and delivered are the same again, and the wait says why.
        assert!(evidence(4_000, 4_000, 0.5).counts());
    }

    #[test]
    fn a_pause_in_the_path_is_not_a_full_path() {
        let now = Instant::now();
        let delivery = |delivered_kbps, sent_kbps| Delivery {
            at: now,
            delivered_kbps,
            sent_kbps,
        };
        // Full: less arrives than is sent, interval after interval.
        let mut full = Deliveries::default();
        full.push(delivery(10_100, 21_000));
        full.push(delivery(9_900, 21_000));
        assert_eq!(full.latest(now), Some((10_100, 21_000, true)));
        // A pause: hardly anything arrives, then the path catches up with more than was sent.
        let mut pause = Deliveries::default();
        pause.push(delivery(2_000, 12_000));
        pause.push(delivery(22_000, 12_000));
        assert_eq!(pause.latest(now), Some((22_000, 12_000, false)));
        // Old samples say nothing of now.
        assert_eq!(
            full.latest(now + SAMPLE_AGE + Duration::from_millis(1)),
            None
        );
    }

    #[test]
    fn a_cut_goes_well_below_what_the_path_carried_then_just_below() {
        let now = Instant::now();
        let mut c = Capacity::default();
        assert!(c.cut(&evidence(8_000, 20_000, 0.0), now));
        // A second of queue is sent in two at half the path's rate.
        assert_eq!(c.allowed_kbps(now), Some(4_000.0));
        assert!(c.draining(now + Duration::from_millis(1_900)));
        assert_eq!(
            c.allowed_kbps(now + Duration::from_millis(2_100)),
            Some(6_800.0)
        );

        // Six seconds of queue: at a quarter of what the path carries, where it is sent in eight.
        let mut c = Capacity::default();
        let long = Evidence {
            queue_ms: 6_000,
            ..evidence(8_000, 20_000, 0.0)
        };
        assert!(c.cut(&long, now));
        assert_eq!(c.allowed_kbps(now), Some(2_000.0));
        assert!(c.draining(now + Duration::from_millis(7_900)));
        assert!(!c.draining(now + Duration::from_millis(8_100)));
    }

    #[test]
    fn a_delivery_that_does_not_count_leaves_the_table_to_decide() {
        let now = Instant::now();
        let mut c = Capacity::default();
        assert!(!c.cut(&evidence(300, 320, 0.0), now));
        assert!(!c.cut(&evidence(0, 20_000, 0.9), now));
        assert_eq!(c.allowed_kbps(now), None);
    }

    #[test]
    fn a_sample_far_too_low_does_not_take_the_stream_to_the_floor() {
        let now = Instant::now();
        let mut c = Capacity::default();
        // Held up, and 100 kbps arrived of 20 Mbps: believed down to a tenth of where it was,
        // even with a long queue to send.
        let wrong = Evidence {
            queue_ms: 9_000,
            ..evidence(100, 150, 0.9)
        };
        assert!(c.cut(&wrong, now));
        assert_eq!(c.allowed_kbps(now), Some(2_000.0));
        assert_eq!(c.allowed_kbps(now + LONGEST_DRAIN), Some(2_000.0));
        // A sample that is right is believed, however far down it says: 10 of 20 Mbps is 10.
        let mut c = Capacity::default();
        assert!(c.cut(&evidence(10_000, 20_000, 0.0), now));
        assert_eq!(c.ceiling_kbps, Some(10_000.0));
    }

    #[test]
    fn the_way_back_is_fast_to_the_ceiling_then_waits_then_probes() {
        let t0 = Instant::now();
        let mut c = Capacity::default();
        c.cut(&evidence(8_000, 20_000, 0.0), t0);
        assert_eq!(c.ceiling_kbps, Some(8_000.0));
        // While the queue drains nothing rises.
        c.raise_allowance(t0 + Duration::from_secs(1));
        assert_eq!(c.settled_kbps, 6_800.0);
        // Then one step to just under the ceiling, and no further for the half minute.
        let mut now = t0 + Duration::from_secs(3);
        c.raise_allowance(now);
        assert_eq!(c.settled_kbps, 7_200.0);
        while now < t0 + FIRST_HOLD {
            c.raise_allowance(now);
            assert_eq!(c.settled_kbps, 7_200.0);
            now += Duration::from_secs(3);
        }
        // Then a twentieth of the ceiling at a time, and a tenth past it the ceiling is gone.
        c.raise_allowance(now);
        assert_eq!(c.settled_kbps, 7_600.0);
        for _ in 0..3 {
            now += Duration::from_secs(3);
            c.raise_allowance(now);
        }
        assert_eq!(c.settled_kbps, 8_800.0);
        assert!(c.has_ceiling(), "at a tenth past it, not past that yet");
        c.raise_allowance(now + Duration::from_secs(3));
        assert!(!c.has_ceiling());
        assert_eq!(c.allowed_kbps(now), None);
    }

    #[test]
    fn a_ceiling_met_again_is_waited_at_longer() {
        let t0 = Instant::now();
        let mut c = Capacity::default();
        c.cut(&evidence(8_000, 20_000, 0.0), t0);
        assert_eq!(c.hold, FIRST_HOLD);
        c.cut(&evidence(7_500, 8_000, 0.4), t0);
        assert_eq!(c.hold, LONGEST_HOLD.min(FIRST_HOLD * 2));
        for _ in 0..5 {
            c.cut(&evidence(7_500, 8_000, 0.4), t0);
        }
        assert_eq!(c.hold, LONGEST_HOLD);
        // Gone and met anew, the wait starts over.
        c.forget_ceiling();
        c.cut(&evidence(8_000, 20_000, 0.0), t0);
        assert_eq!(c.hold, FIRST_HOLD);
    }

    #[test]
    fn a_high_target_is_climbed_to_and_a_preset_is_taken_at_once() {
        let t0 = Instant::now();
        let mut c = Capacity::default();
        // The session's first target, the balanced preset: as it always was.
        assert_eq!(c.on_target(0.67, 0.67, true), 0.67);
        assert!(!c.starting());
        // Raised to a custom 6: from where the stream is.
        assert_eq!(c.on_target(0.67, 6.0, true), 0.67);
        assert!(c.starting());
        // The same target again, say with another option: nothing starts over.
        c.end_start();
        assert_eq!(c.on_target(3.0, 6.0, true), 3.0);
        assert!(!c.starting());
        // Lowered to a preset: at once, and again each time it is set, as it always was.
        assert_eq!(c.on_target(3.0, 1.5, true), 1.5);
        assert_eq!(c.on_target(0.9, 1.5, true), 1.5);
        assert!(!c.starting());

        // A session that opens with a high target starts at the best preset's level, and takes
        // one step a second however often it is asked.
        let mut c = Capacity::default();
        assert_eq!(c.on_target(0.67, 6.0, true), START_RATIO);
        assert!(c.starting());
        let (mut ratio, mut steps) = (START_RATIO, 0);
        for half_second in 0.. {
            let now = t0 + Duration::from_millis(500 * half_second);
            let next = c.start_step(ratio, 6.0, now);
            steps += (next != ratio) as u32;
            ratio = next;
            if !c.starting() {
                break;
            }
            assert!(half_second < 20);
        }
        assert_eq!((ratio, steps), (6.0, 4), "1.5, 2.25, 3.4, 5.1, 6");

        // Where the bitrate cannot follow from second to second, the target is taken at once.
        let mut c = Capacity::default();
        assert_eq!(c.on_target(0.67, 6.0, false), 6.0);
        assert!(!c.starting());
    }

    #[test]
    fn lowered_is_a_cut_of_a_fifth_that_is_not_back() {
        let mut c = Capacity::default();
        // The table's five percent is not worth a word.
        c.note_change(6.0, 5.7, 6.0, true);
        assert!(!c.lowered());
        c.note_change(6.0, 2.0, 6.0, true);
        assert!(c.lowered());
        c.note_change(2.0, 4.0, 6.0, false);
        assert!(c.lowered(), "on the way back, not there yet");
        c.note_change(4.0, 5.5, 6.0, false);
        assert!(!c.lowered());
        // The climb from the start is not a cut.
        c.note_change(1.5, 2.25, 6.0, false);
        assert!(!c.lowered());
    }

    fn rates(seconds: &[(u32, f32, usize)]) -> Rates {
        let mut rates = Rates::default();
        for (sent, held, encoded) in seconds {
            rates.push(*sent, *held, *encoded);
        }
        rates
    }

    #[test]
    fn an_encoder_overshoots_when_it_does_so_second_after_second() {
        let mut c = Capacity::default();
        // One heavy second is a change of scene, not the encoder.
        c.learn_overshoot(
            &rates(&[(10_000, 0.0, 60), (25_000, 0.0, 60), (10_000, 0.0, 60)]),
            10_000,
        );
        assert_eq!(c.overshoot(), 1.0);
        // Asked for 10 Mbps, it puts out 20 second after second.
        c.learn_overshoot(
            &rates(&[(20_500, 0.0, 60), (20_000, 0.0, 60), (21_000, 0.0, 60)]),
            10_000,
        );
        assert_eq!(c.overshoot(), 2.0);
        // Held up, or with the screen still, its output says nothing about it.
        c.learn_overshoot(&rates(&[(40_000, 0.6, 60); 3]), 10_000);
        c.learn_overshoot(&rates(&[(40_000, 0.0, 1); 3]), 10_000);
        assert_eq!(c.overshoot(), 2.0);
        // At another frame rate it keeps to its bitrate: after ten such seconds that is believed.
        for _ in 0..OVERSHOOT_SECONDS {
            c.learn_overshoot(&rates(&[(10_000, 0.0, 30); 3]), 10_000);
        }
        assert_eq!(c.overshoot(), 1.0);
    }

    #[test]
    fn the_ratio_for_a_limit_closes_in_on_an_encoder_that_is_not_proportional() {
        // An encoder whose bitrate per ratio falls as the ratio rises, as the hardware ones':
        // 2073 kbps × ratio × (1 + 1 / (1 + (2073 × ratio − 2000) / 1000)).
        let nominal = |ratio: f32| {
            let base = 2073.0 * ratio;
            let factor = if base > 2000.0 {
                1.0 + 1.0 / (1.0 + (base - 2000.0) * 0.001)
            } else {
                2.0
            };
            (base * factor) as u32
        };
        for (from, limit) in [(1.5, 10_000.0), (6.0, 1_700.0)] {
            let mut ratio = from;
            for _ in 0..4 {
                ratio = ratio_for_kbps(ratio, nominal(ratio), 1.0, limit);
            }
            let kbps = nominal(ratio) as f32;
            assert!(
                (kbps - limit).abs() < limit * 0.02,
                "{kbps} for {limit} at ratio {ratio}"
            );
        }
    }
}
