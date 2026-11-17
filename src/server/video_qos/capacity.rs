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
//! that is full delivers less than was sent in every interval, so that is what is asked for; or
//! that it did so in one interval and the replies were late at both ends of the next, in which
//! less may have been sent (the frame rate is braked on the first late reply, and a hardware
//! encoder sends less for it) while the queue stood all through. And a screen that hardly
//! changes sends little, and little arrives; that says nothing of the path. The samples count
//! when the encoder was putting out something like its bitrate, or when the video loop was
//! waiting for its frames to be taken: a connection whose send is held up takes no frame, so
//! once the send buffers are full the encoder is slowed to the pace of the path, sent and
//! delivered are the same again, and only that wait still says the path is full.
//!
//! Then the stream goes below what the path carried in one step: well below at first, until a
//! reply comes back clean and the queue is sent, then just below. That level is remembered as a
//! ceiling, in kbps of real output, which is a property of the path and not of an encoder or a
//! resolution. The way back is fast up to it, and past it only by small steps after a wait that
//! grows each time it is met again.
//!
//! What a ceiling comes to as a ratio depends on what the encoder really puts out. A software
//! encoder keeps to its bitrate over time, by leaving frames out when the picture cannot be
//! made small enough, so that single seconds of it are far over and far under: it is taken at
//! its bitrate. A hardware encoder's rate control is set for thirty frames a second, and its
//! output goes with the frame rate: twice its bitrate at sixty. That is measured, by the frame.
//!
//! An encoder that takes a new bitrate only by being created again (VAAPI) is brought down by
//! being created again, which takes a tenth of a second: straight to where the stream settles,
//! and tried higher again after a wait.
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

/// The delivery rate is taken over this many probe intervals together. A full path falls short
/// in each of them; after a pause the interval of catching up does not. And on a path that
/// loses packets the probes are held back with the stream until what was lost is sent again, so
/// that one interval reads too long and the next too short: together they read right.
const SAMPLES: usize = 2;
/// One interval more is kept, to know of a path that fell short and has had a queue since: the
/// cut may come a second or two after the frame rate was braked.
const SAMPLES_KEPT: usize = SAMPLES + 1;
/// A sample older than this says nothing of now.
const SAMPLE_AGE: Duration = Duration::from_secs(3);
/// The path kept up in an interval: it delivered this share of what was sent into it, or more.
/// The two are counted between the same probes; what is left is the probes' own jitter.
const KEPT_UP: f32 = 0.97;
/// The encoder was at work: it put out this share of its nominal bitrate or more.
const AT_WORK: f32 = 0.5;
/// The video loop waited this share of its time for its frames to be taken: the send is held up.
const HELD_UP_SHARE: f32 = 0.3;
/// An interval that took less than this to arrive is too short to time: its bytes over its time
/// is not taken for what the path carries.
const TIMED: Duration = Duration::from_millis(100);

/// After a cut the stream first keeps well below what the path carried, until the queue that
/// built up on the way is sent: low enough to send it in about this long, but between a quarter
/// and half of what the path carries. At half, a queue is sent in twice its length.
const DRAIN_IN: Duration = Duration::from_secs(6);
const DEEPEST_DRAIN: f32 = 0.25;
const DRAIN: f32 = 0.5;
/// How long that takes is reckoned from the queue the last reply showed, which is less than
/// there is by the time it is acted on: the stream stays low until a reply comes back clean, or
/// this long at the most.
const SHORTEST_DRAIN: Duration = Duration::from_secs(1);
const LONGEST_DRAIN: Duration = Duration::from_secs(30);
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

/// A hardware encoder's rate control is set for thirty frames a second: given sixty it puts out
/// twice its bitrate, given forty-five half again, as measured with VAAPI. What it puts out a
/// frame is taken from the last second that says it, and reckoned at the frame rate the stream
/// runs at; it counts from this much over its nominal bitrate on. A software encoder keeps to
/// its bitrate over time whatever the frame rate, by leaving frames out if it must, and is never
/// taken to put out more.
const OVERSHOOT_SEEN: f32 = 1.15;
const MOST_OVERSHOOT: f32 = 3.0;
/// A second says what the encoder puts out when this many frames went out in it: one frame more
/// or less is then a tenth of it at most.
const SMOOTH_FRAMES: usize = 10;
/// What it was seen to put out a frame is taken anew when two seconds in a row differ from it by
/// more than this, the same way: less is the scatter of one second to the next, and would move
/// the ratio every second; and one second alone is a change of scene.
const OUTPUT_MOVED: f32 = 0.1;
/// The frame rate its output is reckoned at is the highest of this many last seconds that said
/// what it puts out: the frame rate is braked for some seconds on a late reply and comes back,
/// and the encoder's output with it.
const FRAME_RATE_SECONDS: usize = 10;
/// The frame rate an encoder is taken to put out its bitrate at while it has not said
/// (`set_encoder_hardware`): the one hardware encoders were always created for.
const DEFAULT_ENCODER_FPS: u32 = 30;
/// An encoder that takes a new bitrate only by being created again, created for a frame rate the
/// stream's is now this much above or below, is created again for it.
const FPS_BAND: f32 = 1.25;
/// An encoder that takes a new bitrate only by being created again is created once more when it
/// puts out this much more than it was brought down to, and that no sooner than this after it
/// was created: its bitrate is known as soon as it is there, and is not proportional to the
/// ratio (for H.264 it hardly falls between two ratios that are a quarter apart), so that the
/// ratio is closed in on in two or three steps.
const OVER_LIMIT: f32 = 1.1;
const CLOSING_IN_APART: Duration = Duration::from_secs(2);
/// The seconds of the video loop that are kept.
const RATE_SECONDS: usize = 3;

/// An encoder that takes a new bitrate only by being created again is created again no sooner
/// than this after the last time. Each time the picture stops for a tenth of a second (measured
/// with VAAPI: 100 and 106 ms from the request to the new encoder) and a key frame goes out.
const RESTARTS_APART: Duration = Duration::from_secs(5);
/// And the bitrate is tried higher again only this long after it came down: this at first,
/// doubled each time a try ends in another cut, up to the longest. Then by half again at a time:
/// a path that has not changed is overrun by half and not by all the target asks for.
const FIRST_WAIT_BACK: Duration = Duration::from_secs(30);
const LONGEST_WAIT_BACK: Duration = Duration::from_secs(120);
const STEP_BACK: f32 = 1.5;
/// Where the relay's limit and not the viewer's target says where such an encoder runs, it is
/// created again only when it is this far from there: the ratio the limit comes to is worked out
/// from where the encoder is, and moves a little each time it is.
const RELAY_BAND: f32 = 0.1;
/// Such an encoder is created again for a cut to this share or below: a restart costs a tenth of
/// a second, and one a little over what the path carries stays over it for good otherwise.
const WORTH_A_RESTART: f32 = 0.95;

/// What was written between two probes, how long that took, and how long it took to arrive.
#[derive(Debug, Clone, Copy)]
pub(super) struct Delivery {
    at: Instant,
    bytes: u64,
    sent: Duration,
    received: Duration,
}

impl Delivery {
    fn kbps(bytes: u64, over: Duration) -> u32 {
        let ms = over.as_millis().max(1) as u64;
        (bytes * 8 / ms).min(u32::MAX as u64) as u32
    }

    fn delivered_kbps(&self) -> u32 {
        Self::kbps(self.bytes, self.received)
    }

    fn sent_kbps(&self) -> u32 {
        Self::kbps(self.bytes, self.sent)
    }

    /// The path delivered what was sent into it in this interval, as near as the probes tell.
    fn kept_up(&self) -> bool {
        self.delivered_kbps() as f32 >= self.sent_kbps() as f32 * KEPT_UP
    }

    /// It delivered more than was sent into it, by more than the probes' jitter: a queue is on
    /// its way out.
    fn caught_up(&self) -> bool {
        self.delivered_kbps() as f32 > self.sent_kbps() as f32 * (2.0 - KEPT_UP)
    }
}

/// What the last deliveries of a viewer say together.
#[derive(Debug, Clone, Copy)]
struct Delivered {
    /// The rate the path delivered at over all of them, in kbps, and in the latest alone.
    kbps: u32,
    latest_kbps: u32,
    /// What was sent into it in the latest.
    sent_kbps: u32,
    /// In every one of them the encoder was at work and the path fell short of what was sent.
    fell_short: bool,
    /// How many intervals there were since one of which that is true, of those kept; `None`
    /// when there was none, or it is the latest.
    since_fell_short: Option<usize>,
    /// How long the latest took to arrive.
    latest_received: Duration,
    /// It delivered more than was sent in the latest: the queue is getting shorter.
    shrinking: bool,
    /// What was sent in the interval the path last fell short in, with the encoder at work, and
    /// when that interval ended.
    sent_when_short_kbps: u32,
    fell_short_at: Option<Instant>,
}

/// A viewer's last deliveries.
#[derive(Debug, Default, Clone)]
pub(super) struct Deliveries(VecDeque<Delivery>);

impl Deliveries {
    fn push(&mut self, delivery: Delivery) {
        if self.0.len() >= SAMPLES_KEPT {
            self.0.pop_front();
        }
        self.0.push_back(delivery);
    }

    /// What the last `SAMPLES` say, if the latest is fresh. The rate is taken over the latest
    /// and the one before it if at least `at_work_kbps` was sent in that: an interval of a
    /// screen that hardly changed says nothing of what the path carries.
    fn latest(&self, now: Instant, at_work_kbps: f32) -> Option<Delivered> {
        let last = self.0.back()?;
        if now.saturating_duration_since(last.at) > SAMPLE_AGE {
            return None;
        }
        // Full: more went into the path than came out, and it was the stream that went in. An
        // interval of little sent that came up a little short says nothing of the path.
        let full = |d: &Delivery| d.sent_kbps() as f32 >= at_work_kbps && !d.kept_up();
        let recent = || self.0.iter().rev().take(SAMPLES);
        let counted = || {
            recent()
                .skip(1)
                .filter(|d| d.sent_kbps() as f32 >= at_work_kbps)
                .chain(std::iter::once(last))
        };
        let bytes = counted().map(|d| d.bytes).sum();
        let received = counted().map(|d| d.received).sum();
        Some(Delivered {
            kbps: Delivery::kbps(bytes, received),
            latest_kbps: last.delivered_kbps(),
            latest_received: last.received,
            sent_kbps: last.sent_kbps(),
            fell_short: recent().all(full),
            since_fell_short: self
                .0
                .iter()
                .rev()
                .position(full)
                .filter(|since| *since > 0),
            shrinking: last.caught_up(),
            sent_when_short_kbps: self
                .0
                .iter()
                .rev()
                .find(|d| full(d))
                .map_or(0, |d| d.sent_kbps()),
            fell_short_at: self.0.iter().rev().find(|d| full(d)).map(|d| d.at),
        })
    }
}

/// What the video loop measured, second by second.
#[derive(Debug, Default, Clone)]
pub(super) struct Rates {
    sent_kbps: VecDeque<u32>,
    held_up: VecDeque<f32>,
    frames: VecDeque<usize>,
}

impl Rates {
    /// A second in which `frames` frames went out, `sent_kbps` in all, the loop waiting the share
    /// `held_up` of it for its frames to be taken.
    fn push(&mut self, sent_kbps: u32, held_up: f32, frames: usize) {
        if self.sent_kbps.len() >= RATE_SECONDS {
            self.sent_kbps.pop_front();
            self.held_up.pop_front();
            self.frames.pop_front();
        }
        self.sent_kbps.push_back(sent_kbps);
        self.held_up.push_back(held_up.clamp(0.0, 1.0));
        self.frames.push_back(frames);
    }

    /// The share of the last seconds the loop waited for its frames to be taken.
    fn held_up(&self) -> f32 {
        if self.held_up.is_empty() {
            return 0.0;
        }
        self.held_up.iter().sum::<f32>() / self.held_up.len() as f32
    }

    /// The last second's output and the frames that went out in it, if it says what the
    /// encoder puts out: they were many, and the path did not hold the loop up.
    fn last_smooth(&self) -> Option<(u32, usize)> {
        let frames = *self.frames.back()?;
        let smooth = frames >= SMOOTH_FRAMES && *self.held_up.back()? < HELD_UP_SHARE;
        smooth.then(|| (*self.sent_kbps.back().unwrap_or(&0), frames))
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
    /// It delivered more than was sent into it in the latest: the reply was late for a queue
    /// that is on its way out.
    pub shrinking: bool,
    /// When the path last fell short with the encoder at work, of the intervals kept.
    pub fell_short_at: Option<Instant>,
    /// The path fell short an interval or two before the latest, with the encoder at work, and
    /// every reply since was late: the queue that built up stood in the path all through the
    /// latest, and the path was busy with it. The bytes of the latest then took
    /// as long to arrive as the path needs for them, however few they were: their rate is what
    /// it carries, though less was sent into it by then (the frame rate is braked on the first
    /// late reply, and a hardware encoder sends less for it). Late replies alone do not say so:
    /// on a path with jitter they come without a queue.
    pub queue_stood: bool,
    /// The encoder's bitrate at the current ratio.
    pub nominal_kbps: u32,
    /// How much more than that it puts out (`VideoQoS::overshoot`).
    pub overshoot: f32,
    /// The share of its time the video loop waited for its frames to be taken.
    pub held_up: f32,
    /// How much queue there is to send, in milliseconds of the path's rate.
    pub queue_ms: u32,
}

impl Evidence {
    /// The delivery says what the path carries: the path was full, with something to carry.
    fn counts(&self) -> bool {
        let at_work = self.sent_kbps as f32 >= self.nominal_kbps as f32 * AT_WORK;
        let full = (self.fell_short && at_work) || self.queue_stood;
        self.delivered_kbps > 0 && self.nominal_kbps > 0 && (full || self.held_up >= HELD_UP_SHARE)
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
    /// When the stream was last brought under what the path carried.
    cut_at: Option<Instant>,
    /// What a hardware encoder was last seen to put out a frame, as a share of what its nominal
    /// bitrate comes to a frame at the frame rate its rate control is set for; 1 until it is
    /// seen, and for a software encoder.
    output: f32,
    /// The frames a second of the last seconds that said what it puts out.
    frame_rates: VecDeque<usize>,
    /// What the last such second said, when it was not what was known: believed if the next
    /// says so too.
    output_moved: Option<f32>,
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
            cut_at: None,
            output: 1.0,
            frame_rates: Default::default(),
            output_moved: None,
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
        self.cut_at = None;
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
        // Should the sample be wrong: one cut takes the stream to a tenth of where it was at the
        // lowest, while it drains and after.
        let least = evidence.nominal_kbps as f32 * evidence.overshoot * DEEPEST_CUT;
        let ceiling = (evidence.delivered_kbps as f32).max(least / BELOW_CAPACITY);
        self.hold = if self.ceiling_kbps.is_some() {
            (self.hold * 2).clamp(FIRST_HOLD, LONGEST_HOLD)
        } else {
            FIRST_HOLD
        };
        self.hold_until = Some(now + self.hold);
        self.cut_at = Some(now);
        self.ceiling_kbps = Some(ceiling);
        self.settled_kbps = ceiling * BELOW_CAPACITY;
        // Sending at the share `level` of what the path carries, a queue of q seconds is gone in
        // q / (1 - level).
        let queue = evidence.queue_ms as f32 / 1000.0;
        let level = (1.0 - queue / DRAIN_IN.as_secs_f32()).clamp(DEEPEST_DRAIN, DRAIN);
        self.drain_kbps = (ceiling * level).max(least).min(self.settled_kbps);
        let level = self.drain_kbps / ceiling;
        // The queue is gone when a reply comes back clean (`queue_sent`); this is how long the
        // stream waits for one before it takes the queue for gone anyway. Reckoned from the
        // queue the reply showed, which had grown by the time it was acted on: twice that.
        let drain = if level < 0.95 {
            Duration::from_secs_f32(2.0 * queue / (1.0 - level))
                .clamp(SHORTEST_DRAIN, LONGEST_DRAIN)
        } else {
            LONGEST_DRAIN
        };
        self.drain_until = Some(now + drain);
        self.starting = false;
        log::info!(
            "qos: the path carried {} kbps of {} sent, queue {} ms: {:.0} kbps for up to {:.0} s, then {:.0}",
            evidence.delivered_kbps,
            evidence.sent_kbps,
            evidence.queue_ms,
            self.drain_kbps,
            drain.as_secs_f32(),
            self.settled_kbps,
        );
        true
    }

    /// Still sending the queue of the last cut: nothing is cut further on replies that are bad
    /// because of that queue.
    fn draining(&self, now: Instant) -> bool {
        self.drain_until.is_some_and(|until| now < until)
    }

    /// A reply came back clean: the probe met no queue on its way, so the queue of the last cut
    /// is sent, however long it was reckoned to take.
    fn queue_sent(&mut self) {
        self.drain_until = None;
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

    /// What the stream may put out under the ceiling with the queue sent; `None` without one.
    fn settled_under_ceiling(&self) -> Option<f32> {
        self.ceiling_kbps.map(|_| self.settled_kbps)
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
                log::info!("qos: {ceiling:.0} kbps is no longer what the path carries");
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

    /// A hardware encoder that puts out its nominal `nominal_kbps` at `encoder_fps` frames a
    /// second put out `sent_kbps` in `frames` frames, in a second that says what it puts out, at
    /// a ratio that did not move in it.
    fn learn_output(&mut self, sent_kbps: u32, frames: usize, nominal_kbps: u32, encoder_fps: u32) {
        if nominal_kbps == 0 || frames == 0 || encoder_fps == 0 {
            return;
        }
        if self.frame_rates.len() >= FRAME_RATE_SECONDS {
            self.frame_rates.pop_front();
        }
        self.frame_rates.push_back(frames);
        let a_frame = sent_kbps as f32 / nominal_kbps as f32 * encoder_fps as f32 / frames as f32;
        // Frames far smaller than its bitrate allows are a picture that hardly changes (a
        // pointer moving over a still screen): not what it puts out when it has work to do.
        let moved =
            a_frame >= AT_WORK && (a_frame - self.output).abs() > self.output * OUTPUT_MOVED;
        if !moved {
            self.output_moved = None;
            return;
        }
        // One second of many key frames puts out more, and one of an easy picture less: it is
        // the encoder when the second after it says the same, and then by the lesser move.
        let Some(before) = self.output_moved.replace(a_frame) else {
            return;
        };
        if (before > self.output) == (a_frame > self.output) {
            self.output = if a_frame > self.output {
                a_frame.min(before)
            } else {
                a_frame.max(before)
            };
            self.output_moved = None;
        }
    }

    /// A second that says nothing of what the encoder puts out: what the one before it said is
    /// not followed by another that says the same.
    fn no_word_of_output(&mut self) {
        self.output_moved = None;
    }

    /// The encoder is a software one: what a hardware one was seen to put out says nothing of
    /// it.
    fn forget_output(&mut self) {
        self.output = 1.0;
        self.frame_rates.clear();
        self.output_moved = None;
    }

    /// How much more than its nominal bitrate an encoder that puts it out at `encoder_fps` frames
    /// a second puts out, 1 when it does not: what a hardware encoder puts out a frame, at the
    /// frame rate the stream runs at when nothing brakes it, which is no more than the `fps_cap`
    /// the viewers allow now.
    pub(super) fn overshoot(&self, fps_cap: u32, encoder_fps: u32) -> f32 {
        self.overshoot_at(self.frames_seen().min(fps_cap as usize), encoder_fps)
    }

    /// The most frames a second of the last seconds that said what the encoder puts out.
    pub(super) fn frames_seen(&self) -> usize {
        self.frame_rates.iter().copied().max().unwrap_or(0)
    }

    /// The same, at `frames` frames a second.
    pub(super) fn overshoot_at(&self, frames: usize, encoder_fps: u32) -> f32 {
        let output = (self.output * frames as f32 / encoder_fps.max(1) as f32).min(MOST_OVERSHOOT);
        if output > OVERSHOOT_SEEN {
            output
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

/// The frame rate the display's encoder puts out its bitrate at, once it has said: of a hardware
/// encoder, which a software one keeps to at any frame rate.
fn encoder_fps(display: &DisplayData) -> Option<u32> {
    (display.hardware && display.fps > 0).then_some(display.fps)
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
    /// It was tried higher since it last came down: another cut now is the try's end.
    tried_back: bool,
}

impl VideoQoS {
    /// A probe came back. Between the probe before it and this one the connection wrote `bytes`
    /// of video, in `sent` by this side's clock; the viewer says they reached it in `received`,
    /// by its own.
    pub fn user_delivery(&mut self, id: i32, bytes: u64, sent: Duration, received: Duration) {
        let at = self.now();
        if let Some(user) = self.users.get_mut(&id) {
            user.deliveries.push(Delivery {
                at,
                bytes,
                sent,
                received,
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
        if self.restart.below && self.record() {
            // A recording starts, and while it runs the encoder is not created again for the
            // network: it is created now at what the viewer asks for, and not once more for
            // that when the recording has begun, which would cut the recording in two.
            self.restart.below = false;
            self.restart.tried_back = false;
            self.capacity.forget_ceiling();
            self.capacity.lowered = false;
            self.ratio = self.most_asked();
        }
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
        let target = self.latest_quality().ratio();
        if !known && (self.restart.below || self.ratio < target) {
            // An encoder this side holds down or holds to the relay's limit, while another
            // display's is being created: what that one can do is not known yet, and nothing is
            // let go on its account.
            return;
        }
        self.ratio = target;
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
        self.restart_wait_anew();
    }

    /// A video loop started: an encoder is being created. What the last one put out of its
    /// bitrate is kept until it is known what this one is (`set_encoder_hardware`): created again
    /// for another bitrate it is the same encoder.
    pub(super) fn encoder_created(&mut self) {
        self.restart.last = Some(self.now());
        self.steady_seconds = 0;
    }

    /// The display's encoder is a hardware one, whose output goes with the frame rate, or a
    /// software one, which keeps to its bitrate; and it puts out its bitrate at `fps` frames a
    /// second (`EncoderApi::set_fps`).
    pub fn set_encoder_hardware(&mut self, video_service_name: &str, hardware: bool, fps: u32) {
        let Some(display) = self.displays.get_mut(video_service_name) else {
            return;
        };
        display.hardware = hardware;
        if display.fps != fps {
            // A second that ran at the old frame rate and the new says nothing of either; the
            // first one an encoder says does not end such a second.
            display.fps_moved = display.fps > 0;
            display.fps = fps;
        }
        if self.displays.values().all(|display| !display.hardware) {
            self.capacity.forget_output();
        }
    }

    /// The frame rate the viewers allow the stream: what a hardware encoder is sized for, so
    /// that it puts out its bitrate when the stream runs at it.
    pub fn asked_fps(&self) -> u32 {
        self.highest_fps()
    }

    /// How much more than its nominal bitrate the encoder puts out, as it runs: of the display's
    /// encoder that puts out its bitrate at the lowest frame rate.
    pub(super) fn overshoot(&self) -> f32 {
        let fps = self.displays.values().filter_map(encoder_fps).min();
        self.capacity
            .overshoot(self.highest_fps(), fps.unwrap_or(DEFAULT_ENCODER_FPS))
    }

    /// How much more than its nominal bitrate the encoder a ratio is chosen for puts out: one
    /// that takes a new bitrate while it runs is that one; one that takes it only by being
    /// created again is a new one, created for the frame rate the stream may run at now.
    pub(super) fn overshoot_next(&self) -> f32 {
        if !self.restart_only() {
            return self.overshoot();
        }
        let fps = self.highest_fps();
        // The frames seen are of the frame rate allowed before, when it has risen since: the
        // stream is taken to come up to the new one.
        let frames = if self.fps_risen() {
            fps as usize
        } else {
            self.capacity.frames_seen().min(fps as usize)
        };
        self.capacity.overshoot_at(frames, fps)
    }

    /// The stream may run at more frames than an encoder was created for, by more than
    /// `FPS_BAND`: it puts out more than its bitrate.
    pub(super) fn fps_risen(&self) -> bool {
        let fps = self.highest_fps() as f32;
        self.displays
            .values()
            .filter_map(encoder_fps)
            .any(|encoder| fps > encoder as f32 * FPS_BAND)
    }

    /// Or at fewer: it puts out less.
    fn fps_fallen(&self) -> bool {
        let fps = self.highest_fps() as f32;
        self.displays
            .values()
            .filter_map(encoder_fps)
            .any(|encoder| fps * FPS_BAND < encoder as f32)
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
        (nominal > 0)
            .then(|| ratio_for_kbps(self.bitrate_ratio, nominal, self.overshoot_next(), limit))
    }

    /// One viewer that says when the probes reach it, of one display: the deliveries describe
    /// one path and one encoder.
    fn evidence(&self) -> Option<Evidence> {
        if self.users.len() != 1 || self.displays.len() != 1 {
            return None;
        }
        let user = self.users.values().next()?;
        let display = self.displays.values().next()?;
        let nominal_kbps = self.bitrate();
        let delivered = user
            .deliveries
            .latest(self.now(), nominal_kbps as f32 * AT_WORK)?;
        // The path fell short, and every reply since was late, that of the interval it fell
        // short in and of each one after: the queue stood through the latest.
        // And what the path delivered in the latest is less than was sent into it when it fell
        // short: a path that paused and now catches up delivers more, and carries the stream.
        let late = user.delay.consecutive_bad_samples as usize;
        let queue_stood = delivered.since_fell_short.is_some_and(|since| late > since)
            && delivered.latest_received >= TIMED
            && (delivered.latest_kbps as f32) < delivered.sent_when_short_kbps as f32 * KEPT_UP;
        // What the path carries is its rate over the intervals together. Where that rests on the
        // queue having stood through the latest alone (the frame rate was braked on the first
        // late reply, and a hardware encoder then sent less), it is the rate of the latest: the
        // one before began before there was a queue.
        let delivered_kbps = if queue_stood && !delivered.fell_short {
            delivered.latest_kbps
        } else {
            delivered.kbps
        };
        let sent_kbps = delivered.sent_kbps;
        // The probe that just came back saw the queue there was when it went out. It was under
        // way for its whole delay, and all that time more went into the path than came out.
        let saw = user.delay.avg_delay() as f32;
        let under_way = user.delay.delay_history.back().copied().unwrap_or_default() as f32;
        let growth = (sent_kbps as f32 / delivered_kbps.max(1) as f32 - 1.0).clamp(0.0, 4.0);
        Some(Evidence {
            delivered_kbps,
            sent_kbps,
            fell_short: delivered.fell_short,
            shrinking: delivered.shrinking,
            fell_short_at: delivered.fell_short_at,
            queue_stood,
            nominal_kbps,
            overshoot: self.overshoot(),
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
        // The path has not fallen short again since the stream was brought under what it
        // carried, and delivers more than is sent: the reply was late for the queue of that cut,
        // which is on its way out and took longer than was reckoned. Nothing is cut for it, by
        // the table either. A path that fell short again since (and delivers more than is sent
        // now only because the frame rate was braked), a queue that stands (as much delivered as
        // sent, the replies late all the same) or a send that is held up is another matter, and
        // goes on to be looked at.
        let since_cut = match (self.capacity.cut_at, evidence.fell_short_at) {
            (Some(cut), Some(short)) => short > cut,
            _ => false,
        };
        if self.capacity.has_ceiling()
            && evidence.shrinking
            && evidence.held_up < HELD_UP_SHARE
            && !since_cut
        {
            return Some(ratio);
        }
        if !self.capacity.cut(&evidence, now) {
            return None;
        }
        let allowed = self.capacity.allowed_kbps(now)?;
        // From the ratio the encoder's bitrate is the bitrate of, which is `ratio` unless the
        // video loop has yet to take the last change.
        let to = ratio_for_kbps(
            self.bitrate_ratio,
            evidence.nominal_kbps,
            evidence.overshoot,
            allowed,
        );
        // Down, whatever the sample said, and not below a tenth of where the stream was.
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
        self.restart_wait_anew();
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

    /// What the video loop of a display measured since it last said: the bytes and the frames
    /// that went out (a frame the encoder left out is none), and how long it waited for its
    /// frames to be taken, in `elapsed`.
    pub fn update_display_rates(
        &mut self,
        video_service_name: &str,
        sent_bytes: usize,
        sent_frames: usize,
        held_up: Duration,
        elapsed: Duration,
    ) {
        let secs = elapsed.as_secs_f32();
        if secs <= 0.0 {
            return;
        }
        let nominal = self.bitrate();
        let alone = self.displays.len() == 1;
        // What was sent in a second says something of the encoder at its ratio only if it ran at
        // one ratio all through it: the one asked for when the second began (`second_ends`),
        // which the video loop takes with its next frame; a ratio moved since, on a reply, is
        // not that one. An encoder that takes a new bitrate only by being created again runs at
        // the ratio it was created with whatever is asked for, from then on (`encoder_created`).
        let one_fps = self
            .displays
            .get_mut(video_service_name)
            .is_some_and(|display| !std::mem::take(&mut display.fps_moved));
        let one_ratio = self.restart_only() || self.bitrate_ratio == self.ratio_at_rates;
        self.steady_seconds = if one_ratio && one_fps {
            self.steady_seconds.saturating_add(1)
        } else {
            0
        };
        let steady = self.steady_seconds >= 1;
        let Some(display) = self.displays.get_mut(video_service_name) else {
            return;
        };
        display.rates.push(
            (sent_bytes as f32 * 8.0 / 1000.0 / secs) as u32,
            held_up.as_secs_f32() / secs,
            // By the second, as the frames are counted elsewhere.
            (sent_frames as f32 / secs).round() as usize,
        );
        // With one encoder its nominal bitrate is the one stored; with more, whose it is is not
        // known here.
        if alone && display.hardware {
            let fps = encoder_fps(display).unwrap_or(DEFAULT_ENCODER_FPS);
            match display.rates.last_smooth().filter(|_| steady) {
                Some((sent, frames)) => self.capacity.learn_output(sent, frames, nominal, fps),
                None => self.capacity.no_word_of_output(),
            }
        }
        self.keep_under_limits();
    }

    /// A video loop's second is over and the ratio is what the next one runs at.
    pub(super) fn second_ends(&mut self) {
        self.ratio_at_rates = self.ratio;
    }

    /// The stream's real output stays under the relay's limit and under the path's ceiling, where
    /// the bitrate follows the ratio while the encoder runs. The ratio that comes to is worked
    /// out from where the stream is, every second, so that it closes in on an encoder whose
    /// output is not proportional to the ratio.
    fn keep_under_limits(&mut self) {
        let (nominal, now) = (self.bitrate(), self.now());
        // Every viewer's replies come back clean: the probes meet no queue, so the queue of the
        // last cut is sent, whether the screen changes or not.
        let clean = self
            .users
            .values()
            .all(|user| user.delay.avg_delay() < CLEAN_REPLY_MS);
        if clean && self.capacity.draining(now) {
            self.capacity.queue_sent();
        }
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
        let cap = ratio_for_kbps(self.bitrate_ratio, nominal, self.overshoot(), limit);
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
    pub fn restart_only(&self) -> bool {
        self.abr_config
            && !self.displays.is_empty()
            && self
                .displays
                .values()
                .all(|display| !display.support_changing_quality && display.restartable)
    }

    /// One second of bitrate control by restart.
    ///
    /// Down: when congestion is confirmed and the deliveries say what the path carries, straight
    /// to where the stream settles if that is lower by a twentieth or more; and again, no sooner than
    /// `RESTARTS_APART` later, if the encoder then puts out more than that after all (its
    /// bitrate is not proportional to the ratio, and one step does not always get there). Up:
    /// after a wait with every reply good and the screen changing, by half again, and after each
    /// further such wait by half again, until the most the viewer and the relay allow is reached.
    pub(super) fn restart_only_step(&mut self, encoded: usize) {
        let now = self.now();
        let target = self.latest_quality().ratio();
        // Through the relay no more than its limit, which is worked out from where the encoder is.
        let most = self.most_asked();
        self.capacity.end_start();
        if !self.restart.below {
            // Where it always was: the viewer's target, which a change of preset moves; or the
            // relay's limit.
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
        if confirmed && apart && self.restart_down(most, now) {
            return;
        }
        // The stream may run at more frames than the encoder was created for: it puts out more
        // than it is taken to, and is created again for the frame rate, also on a congested path
        // that a restart for the path was not worth it for. At fewer it puts out less, which is
        // put right where nothing holds it down: held down or congested, that does no harm, and
        // the wait to step back up is not started anew for it.
        let asked = self.displays.values().any(|display| display.restart);
        let fallen = !confirmed && !self.restart.below && self.fps_fallen();
        if apart && !asked && (self.fps_risen() || fallen) {
            log::info!(
                "qos: encoder created again for {} fps, ratio {:.2}",
                self.highest_fps(),
                self.ratio
            );
            self.request_restart(now);
            return;
        }
        if confirmed {
            return;
        }
        if !self.restart.below {
            // The viewer moved a custom quality, which the video loop does not create the
            // encoder again for by itself (a slider is dragged through many values), or the
            // relay's limit comes to another ratio now that the encoder's output is known: the
            // loop is asked to here, where the last time is known, so that the encoder's bitrate
            // is the one of the ratio again.
            let band = if most < target { RELAY_BAND } else { 0.01 };
            let moved = self.bitrate() > 0 && (self.bitrate_ratio - most).abs() > most * band;
            let asked = self.displays.values().any(|display| display.restart);
            if moved && apart && !asked {
                if most < target {
                    log::info!(
                        "qos: the relay's limit comes to ratio {most:.2}, the encoder is at {:.2}",
                        self.bitrate_ratio
                    );
                }
                self.request_restart(now);
            }
            return;
        }
        if self.ratio >= most {
            // The viewer's target came down to where the stream is held: nothing is held down
            // any more, and the encoder is created at the target as for any change of it;
            // unless it is there already.
            if self.bitrate_ratio == most {
                self.ratio = most;
                self.restart.below = false;
                self.restart.tried_back = false;
                self.capacity.forget_ceiling();
                self.capacity.lowered = false;
            } else if apart {
                self.restart_to(most, most, now);
            }
            return;
        }
        let closing_in = since_restart.map_or(true, |since| since >= CLOSING_IN_APART);
        if closing_in && self.restart_over_allowance(now) {
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
        if std::mem::replace(&mut self.restart.tried_back, true) {
            // The try before this one held: the path carries more than it did, and the next
            // wait is the first again.
            self.restart.wait_back = FIRST_WAIT_BACK;
        }
        self.restart_to(to, most, now);
    }

    /// Held down under what the path carried, the encoder puts out more than it was brought
    /// down to: it is created again for what it really puts out. `true` when it is.
    fn restart_over_allowance(&mut self, now: Instant) -> bool {
        // Where the stream settles: such an encoder is not taken lower first to send the queue.
        let Some(allowed) = self.capacity.settled_under_ceiling() else {
            return false;
        };
        // What it puts out at the frame rate the stream runs at when nothing brakes it, as seen
        // of this encoder since it was created.
        let (from, nominal) = (self.bitrate_ratio, self.bitrate());
        let out = nominal as f32 * self.overshoot();
        // Of the encoder that runs at the ratio asked for, not of one about to be replaced.
        let asked = self.displays.values().any(|display| display.restart);
        if nominal == 0 || asked || from != self.ratio || out <= allowed * OVER_LIMIT {
            return false;
        }
        // The next one is created for the frame rate the stream may run at now.
        let to =
            ratio_for_kbps(from, nominal, self.overshoot_next(), allowed).max(self.min_ratio());
        if to > from * NEAR_CEILING {
            // At the floor, or as good as there: another encoder would put out the same.
            return false;
        }
        log::info!(
            "qos: the encoder puts out {out:.0} kbps where {allowed:.0} are allowed: created again, ratio {from:.2} -> {to:.2}"
        );
        self.ratio = to;
        self.request_restart(now);
        true
    }

    /// Congestion is confirmed for an encoder that takes a new bitrate only by restart: if the
    /// deliveries say what the path carries and where the stream settles under it is lower than
    /// where it is by enough to be worth it, the encoder is created again for it, straight there.
    /// `true` when it is.
    fn restart_down(&mut self, most: f32, now: Instant) -> bool {
        let Some(evidence) = self.evidence() else {
            return false;
        };
        // Looked at, and left as it was if nothing comes of it.
        let before = self.capacity.clone();
        if !self.capacity.cut(&evidence, now) {
            return false;
        }
        // Where the encoder is, which is the ratio its bitrate belongs to.
        let from = self.bitrate_ratio;
        let to = ratio_for_kbps(
            from,
            evidence.nominal_kbps,
            self.overshoot_next(),
            self.capacity.settled_kbps,
        )
        .max(from * DEEPEST_CUT)
        .max(self.min_ratio());
        if to > from * WORTH_A_RESTART {
            self.capacity = before;
            return false;
        }
        log::info!(
            "qos: the path carried {} kbps of {} sent: encoder created again, ratio {from:.2} -> {to:.2}",
            evidence.delivered_kbps,
            evidence.sent_kbps
        );
        self.ratio = to;
        self.capacity.note_change(from, to, most, true);
        // The wait before the next try grows when a try ended in this cut, not when the cut
        // before it did not go far enough.
        self.restart.wait_back = if !self.restart.below || self.restart.wait_back.is_zero() {
            FIRST_WAIT_BACK
        } else if std::mem::take(&mut self.restart.tried_back) {
            (self.restart.wait_back * 2).min(LONGEST_WAIT_BACK)
        } else {
            self.restart.wait_back
        };
        self.restart.below = true;
        self.request_restart(now);
        true
    }

    /// The encoder is created again for the ratio `to` on the way back up, of which `most` is
    /// the most the viewer's target and the relay allow.
    fn restart_to(&mut self, to: f32, most: f32, now: Instant) {
        let from = self.ratio;
        log::info!("qos: encoder created again on the way back, ratio {from:.2} -> {to:.2}");
        self.ratio = to;
        // A try for more than the path carried when the encoder was brought down: what it
        // carried then is no longer what the encoder is held to. If the path carries no more,
        // the next cut says what it does.
        self.capacity.forget_ceiling();
        if to >= most {
            // All the way back.
            self.restart.below = false;
            self.restart.tried_back = false;
            self.capacity.lowered = false;
        } else {
            self.capacity.note_change(from, to, most, false);
        }
        self.request_restart(now);
    }

    /// The viewers changed, or what they ask for: an encoder held down by restart does not wait
    /// out what the path of the viewers before did to it. The stream stays where it is, and the
    /// way back starts with the first wait.
    pub(super) fn restart_wait_anew(&mut self) {
        if self.restart.below {
            self.restart.wait_back = FIRST_WAIT_BACK;
            self.restart.tried_back = false;
        }
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
            shrinking: delivered_kbps as f32 > sent_kbps as f32 * (2.0 - KEPT_UP),
            fell_short_at: None,
            queue_stood: false,
            nominal_kbps: 20_000,
            overshoot: 1.0,
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
    fn a_cut_goes_well_below_what_the_path_carried_then_just_below() {
        let now = Instant::now();
        let mut c = Capacity::default();
        assert!(c.cut(&evidence(8_000, 20_000, 0.0), now));
        // A second of queue is sent in two at half the path's rate. The stream stays there for
        // twice as long, should no reply come back clean before: the queue had grown by the
        // time the reply that showed it was acted on.
        assert_eq!(c.allowed_kbps(now), Some(4_000.0));
        assert!(c.draining(now + Duration::from_millis(3_900)));
        assert_eq!(
            c.allowed_kbps(now + Duration::from_millis(4_100)),
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
        assert!(c.draining(now + Duration::from_millis(15_900)));
        assert!(!c.draining(now + Duration::from_millis(16_100)));
        // A reply that comes back clean met no queue: it is sent, whatever was reckoned.
        c.queue_sent();
        assert!(!c.draining(now + Duration::from_millis(8_500)));
        assert_eq!(
            c.allowed_kbps(now + Duration::from_millis(8_500)),
            Some(6_800.0)
        );
        // And however long the queue, the stream is not kept low for more than half a minute.
        let mut c = Capacity::default();
        let longest = Evidence {
            queue_ms: 60_000,
            ..evidence(8_000, 20_000, 0.0)
        };
        assert!(c.cut(&longest, now));
        assert!(!c.draining(now + LONGEST_DRAIN));
    }

    #[test]
    fn a_queue_that_stood_through_the_interval_says_what_the_path_carries() {
        // The frame rate was braked on the first late reply: a hardware encoder then sent a
        // fifth of its bitrate, less than the path carries, and all of it arrived. But the path
        // had fallen short before, and the replies at both ends of this interval were late.
        let stood = Evidence {
            queue_stood: true,
            ..evidence(8_000, 4_000, 0.0)
        };
        assert!(stood.counts());
        // Without that, such an interval says nothing: the path kept up.
        assert!(!evidence(8_000, 4_000, 0.0).counts());
        let mut c = Capacity::default();
        assert!(c.cut(&stood, Instant::now()));
        assert_eq!(c.ceiling_kbps, Some(8_000.0));
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
        // A reply comes back clean. Then one step to just under the ceiling, and no further for
        // the half minute.
        c.queue_sent();
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

    /// Seconds of (kbps sent, share held up, frames sent).
    fn rates(seconds: &[(u32, f32, usize)]) -> Rates {
        let mut rates = Rates::default();
        for (sent, held, frames) in seconds {
            rates.push(*sent, *held, *frames);
        }
        rates
    }

    #[test]
    fn what_a_hardware_encoder_puts_out_goes_with_the_frame_rate() {
        let mut c = Capacity::default();
        // Asked for 10 Mbps at sixty frames a second, it puts out 20: its rate control is set
        // for thirty, and every frame gets a thirtieth of the bitrate. That is what is taken of
        // a hardware encoder until it shows otherwise, so the first second says it.
        c.learn_output(20_000, 60, 10_000, 30);
        assert_eq!(c.overshoot(60, 30), 2.0);
        // The scatter of one second to the next is not taken for a change.
        c.learn_output(21_000, 60, 10_000, 30);
        c.learn_output(19_000, 60, 10_000, 30);
        assert_eq!(c.overshoot(60, 30), 2.0);
        // Nor is one second of many key frames, or one of an easy picture.
        c.learn_output(30_000, 60, 10_000, 30);
        c.learn_output(20_000, 60, 10_000, 30);
        c.learn_output(12_000, 60, 10_000, 30);
        c.learn_output(20_000, 60, 10_000, 30);
        assert_eq!(c.overshoot(60, 30), 2.0);
        // Nor two seconds that are not one after the other.
        c.learn_output(30_000, 60, 10_000, 30);
        c.no_word_of_output();
        c.learn_output(30_000, 60, 10_000, 30);
        assert_eq!(c.overshoot(60, 30), 2.0);
        c.no_word_of_output();
        // A late reply brakes the frame rate for some seconds, and it puts out less: it is
        // still reckoned at the sixty the stream comes back to.
        c.learn_output(10_000, 30, 10_000, 30);
        c.learn_output(5_000, 15, 10_000, 30);
        assert_eq!(c.overshoot(60, 30), 2.0);
        // A picture that changes thirty times a second, for good: it keeps to its bitrate.
        for _ in 0..FRAME_RATE_SECONDS {
            c.learn_output(10_000, 30, 10_000, 30);
        }
        assert_eq!(c.overshoot(60, 30), 1.0);
        // An encoder that puts out half again a frame, whatever the frame rate; by the lesser
        // of the two seconds that say so.
        c.learn_output(16_000, 30, 10_000, 30);
        c.learn_output(15_000, 30, 10_000, 30);
        assert_eq!(c.overshoot(60, 30), 1.5);
        // Nothing is made of more than three times its bitrate.
        for _ in 0..FRAME_RATE_SECONDS {
            c.learn_output(50_000, 60, 10_000, 30);
        }
        assert_eq!(c.overshoot(60, 30), MOST_OVERSHOOT);
        // A software encoder comes in its place.
        c.forget_output();
        assert_eq!(c.overshoot(60, 30), 1.0);
    }

    #[test]
    fn a_second_of_few_frames_or_held_up_says_nothing_of_the_encoder() {
        // Asked for 240 kbps of a picture it cannot make a frame of under 320 kbit, a software
        // encoder sends a frame in some seconds, two in others, none in others.
        for second in [(640, 0.0, 2), (0, 0.0, 0), (320, 0.0, 1), (2_880, 0.0, 9)] {
            assert_eq!(
                rates(&[second]).last_smooth().map(|(kbps, _)| kbps),
                None,
                "{second:?}"
            );
        }
        // Held up by the path, the loop sends what the path takes and not what the encoder would.
        assert_eq!(
            rates(&[(40_000, 0.6, 60)])
                .last_smooth()
                .map(|(kbps, _)| kbps),
            None
        );
        // Sixty frames with nothing in the way: that is the encoder.
        assert_eq!(
            rates(&[(20_000, 0.1, 60)])
                .last_smooth()
                .map(|(kbps, _)| kbps),
            Some(20_000)
        );
        // It is the last second that is asked about.
        let seconds = [(20_000, 0.0, 60), (640, 0.0, 2)];
        assert_eq!(rates(&seconds).last_smooth().map(|(kbps, _)| kbps), None);
    }

    #[test]
    fn the_rate_delivered_is_taken_over_both_intervals_and_a_pause_is_not_a_full_path() {
        let now = Instant::now();
        let delivery = |bytes, sent_ms, received_ms| Delivery {
            at: now,
            bytes,
            sent: Duration::from_millis(sent_ms),
            received: Duration::from_millis(received_ms),
        };
        // A megabyte a second (8 Mbps) into a path that carries half of it. What was lost on the
        // way is sent again and holds the first probe back: the first megabyte reads 2.6 s, the
        // second 1.4 s, four seconds for the two as it was.
        let mut d = Deliveries::default();
        d.push(delivery(1_000_000, 1_000, 2_600));
        d.push(delivery(1_000_000, 1_000, 1_400));
        let said = d.latest(now, 4_000.0).unwrap();
        assert_eq!(said.kbps, 4_000, "not the 5714 of the shorter interval");
        assert!(said.fell_short && !said.shrinking);
        assert_eq!(
            said.since_fell_short, None,
            "it is the latest that fell short"
        );
        // After a pause the path catches up: it did not fall short in every interval, and in
        // the latest it kept up.
        let mut d = Deliveries::default();
        d.push(delivery(1_000_000, 1_000, 3_000));
        d.push(delivery(1_000_000, 1_000, 500));
        let said = d.latest(now, 4_000.0).unwrap();
        assert!(!said.fell_short && said.shrinking);
        // A screen that hardly changed, then one that moves: the interval before says nothing
        // of the path. The rate is that of the latest alone, and the path is not taken to have
        // fallen short interval after interval for the little that came late in the first.
        let mut d = Deliveries::default();
        d.push(delivery(12_500, 1_000, 1_250));
        d.push(delivery(1_000_000, 1_000, 2_000));
        let said = d.latest(now, 4_000.0).unwrap();
        assert_eq!(said.kbps, 4_000, "not the 2492 of the two together");
        assert!(!said.fell_short && said.since_fell_short.is_none());
        // Full in the first interval; in the next two a quarter as much is sent (the frame rate
        // was braked) and arrives in a quarter of the time: the queue is on its way out, and
        // the path is as busy as it was.
        let mut d = Deliveries::default();
        d.push(delivery(1_000_000, 1_000, 2_000));
        d.push(delivery(250_000, 1_000, 500));
        let said = d.latest(now, 4_000.0).unwrap();
        assert!(!said.fell_short && said.shrinking);
        assert_eq!(said.since_fell_short, Some(1));
        assert_eq!(said.latest_kbps, 4_000);
        assert!(said.latest_received >= TIMED);
        d.push(delivery(250_000, 1_000, 500));
        assert_eq!(d.latest(now, 4_000.0).unwrap().since_fell_short, Some(2));
        // One more and it is no longer known when the path fell short.
        d.push(delivery(250_000, 1_000, 500));
        assert_eq!(d.latest(now, 4_000.0).unwrap().since_fell_short, None);
        // An old word says nothing of now.
        assert!(d
            .latest(now + SAMPLE_AGE + Duration::from_millis(1), 4_000.0)
            .is_none());
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
