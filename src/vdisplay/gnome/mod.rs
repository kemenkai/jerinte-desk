//! Virtual displays on GNOME under Wayland: Mutter's own virtual monitors, each recorded in a
//! screen-cast session of its own (`scrap::wayland::mutter_capture::VirtualMonitor`) and captured
//! as the physical monitors are (`server::mutter`). Mutter takes a monitor back the moment its
//! session ends, which a restart of this `--server` would end: so while there are any, the root
//! service is asked not to restart it (`Request::Hold`), where there is one to ask.
//!
//! Mutter lays all the monitors out side by side as one of ours is added or resized; the others
//! are put back as they were right after, ours to their right (`layout`).
//!
//! A display may have the desktop to itself (`Mode::Privacy`): ours alone on, the others off, the
//! input here kept by the root service until given back or the person here takes it back
//! (Ctrl+P). Mutter 42.9 crashes turning off a monitor someone records with the cursor as
//! metadata, so a monitor is moved before it is turned off, which closes its recordings cleanly
//! (`apply_safely`); and the desktop is not taken over while anyone else records it.

mod layout;

use super::manager::{Backend, Caps, Mode, Spec, TAKEN_BACK};
use hbb_common::{bail, log, ResultType};
use layout::Remembered;
use scrap::mutter_model::{CastStream, DisplayState, LogicalConfig, VIRTUAL_PRODUCT};
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
pub use live::new_backend;

/// Each a screen-cast session of its own, with GNOME Shell's indicator: a handful is plenty.
const MAX: usize = 4;
/// How long Mutter may take to list a monitor just made, and to take one back.
const LISTED: Duration = Duration::from_secs(5);
const REMOVED: Duration = Duration::from_secs(5);
/// How long a resize may take: with a capturer about, the monitor is resized once the video
/// service builds the capturer again, which first waits up to 3 seconds for a frame.
const RESIZED: Duration = Duration::from_secs(10);
const LOOK: Duration = Duration::from_millis(100);
/// For how long after a change of ours the layout Mutter shows is taken for its doing: anyone
/// else's change is taken as it is, never undone.
const OURS_FOR: Duration = Duration::from_secs(10);
/// Tries at putting the monitors back, the state read again before each (its serial gone stale).
const APPLY_TRIES: usize = 3;
/// How long the recordings of the physical monitors take to be gone once stopped.
const STREAMS_GONE: Duration = Duration::from_secs(2);
/// Looks in a row at a desktop taken over that would not stay so, given back after; tries at
/// giving one back, its displays let go after, Mutter lighting its own monitors again.
const TAKEOVER_TRIES: u32 = 3;
const GIVE_BACK_TRIES: u32 = 3;
/// Tries at a size put off while the desktop was taken over.
const RESIZE_TRIES: u32 = 3;

// Translation keys, so the key itself is the English text.
const RECORDED_ELSEWHERE: &str =
    "Another program is recording the screen of the remote device, its desktop cannot be taken over";
const NO_INPUT_LOCK: &str =
    "The keyboard and mouse of the remote device cannot be locked, its desktop cannot be taken over";
const NOT_CONFIGURABLE: &str =
    "The displays of the remote device cannot be changed remotely, its desktop cannot be taken over";
const NOT_TAKEN_OVER: &str = "The desktop of the remote device could not be taken over";

/// Mutter, as virtual displays need it.
trait Mutter {
    type Monitor: Monitor;
    /// A virtual monitor of this size, made: Mutter lists it a moment later.
    fn create(&mut self, width: i32, height: i32) -> ResultType<Self::Monitor>;
    fn state(&mut self) -> ResultType<DisplayState>;
    /// A change of ours is about to make Mutter lay the monitors out again.
    fn expect_relayout(&mut self);
    /// Between two looks at the state.
    fn pause(&mut self, d: Duration);
    fn now(&self) -> Instant;
    /// Lays the monitors out as `config` says until they next change, against `state` (each
    /// monitor keeping its other settings), in the layout mode given if any.
    fn apply(
        &mut self,
        state: &DisplayState,
        config: &[LogicalConfig],
        layout_logical: Option<bool>,
    ) -> ResultType<()>;
    /// Whether it takes monitor configurations over D-Bus.
    fn apply_allowed(&mut self) -> bool;
    /// Every stream it records now, anyone's.
    fn streams(&mut self) -> ResultType<Vec<CastStream>>;
    /// The physical monitors recorded no more (`on`), the recording stopped, until let go.
    fn hold_physical(&mut self, on: bool);
}

/// A virtual monitor of ours, gone once dropped.
trait Monitor {
    /// Lists it among the displays to capture, as the connector Mutter gave it.
    fn show_as(&mut self, connector: &str);
    /// Answers whether a capturer of it is to settle the size.
    fn resize(&self, width: i32, height: i32) -> ResultType<bool>;
    /// Mutter closed its session or went, or its stream ended.
    fn is_gone(&self) -> bool;
}

/// The root service, which restarts `--server` now and then unless asked not to, and keeps the
/// input here from a desktop taken over.
trait Keeper {
    /// Asks it to keep this `--server` (`on`), or no more.
    fn keep(&mut self, on: bool) -> ResultType<()>;
    /// Which connection to it this is: it forgets what a client asked with its connection.
    fn generation(&self) -> u64;
    /// Asks it to keep the input here from the desktop this process took over.
    fn lock_input(&mut self) -> ResultType<()>;
    /// Whether the person here took the desktop back (Ctrl+P).
    fn taken_back(&mut self) -> ResultType<bool>;
    /// Tells it the desktop is given back.
    fn restored(&mut self) -> ResultType<()>;
}

struct Ours<V> {
    connector: String,
    monitor: V,
    /// Its size, as Mutter last listed it after a change of ours.
    size: (i32, i32),
    /// A size asked for while the desktop was taken over, its size fixed then: taken once given
    /// back, given up on after `RESIZE_TRIES` tries.
    asked: Option<(i32, i32)>,
    resize_tries: u32,
    usage: Mode,
}

struct GnomeBackend<M: Mutter, K> {
    mutter: M,
    keeper: K,
    /// Whether the root service keeps this `--server`, as it last answered.
    kept: bool,
    /// Told once that it does not answer.
    unkept: bool,
    /// Oldest first.
    ours: Vec<Ours<M::Monitor>>,
    /// The other monitors' layout, while there are ours.
    remembered: Option<Remembered>,
    /// When ours last changed.
    changed_at: Option<Instant>,
    /// Whether that change came to an end with the monitors as wanted: any change found since is
    /// someone else's.
    settled: bool,
    /// Told once that Mutter takes no configurations, the monitors staying as it lays them out.
    unapplied: bool,
    /// Whether the desktop may be taken over at all: Mutter took configurations when asked first.
    privacy: bool,
    /// The physical monitors recorded no more, for a desktop taken over; let go when dropped.
    physical_held: bool,
    /// The root service keeps the input here from the desktop, on its connection of this
    /// generation.
    locked: Option<u64>,
    /// The generation of the root service's connection the hold was asked on.
    kept_on: u64,
    /// Looks in a row at a desktop taken over that would not stay so; failed tries at giving it
    /// back, to be tried again on every look while any.
    slipping: u32,
    unreturned: u32,
    /// The root service not told yet that the desktop is given back: it ends this process should
    /// it not hear so soon after Ctrl+P, which it remembers by process, not by connection.
    unrestored: bool,
}

impl<M: Mutter, K: Keeper> GnomeBackend<M, K> {
    fn new(mut mutter: M, keeper: K) -> Self {
        let privacy = mutter.apply_allowed();
        Self {
            mutter,
            keeper,
            kept: false,
            unkept: false,
            ours: Vec::new(),
            remembered: None,
            changed_at: None,
            settled: false,
            unapplied: false,
            privacy,
            physical_held: false,
            locked: None,
            kept_on: 0,
            slipping: 0,
            unreturned: 0,
            unrestored: false,
        }
    }

    // Asked before a monitor is made, and again while there are some should it not have
    // answered (not up yet); let go once there are none. Without it the monitors work all the
    // same, only a restart of this server takes them away.
    fn keep(&mut self, on: bool) {
        if on == self.kept {
            return;
        }
        match self.keeper.keep(on) {
            Ok(()) => {
                self.unkept = false;
                self.kept_on = self.keeper.generation();
            }
            Err(e) if on && !self.unkept => {
                log::info!(
                    "Virtual displays are not kept through a restart of this server: {}",
                    e
                );
                self.unkept = true;
            }
            Err(_) => {}
        }
        // One let go is let go: should the call have failed, the connection went with it (see
        // `RootLink::hold`), and the service let go with it.
        self.kept = on && !self.unkept;
    }

    fn keep_while_any(&mut self) {
        self.keep(!self.ours.is_empty());
    }

    // The hold asked again should the connection it was asked on be gone (`lost`), or another be
    // open now: the root service forgot it with that connection.
    fn rekeep(&mut self, lost: bool) {
        if self.kept && (lost || self.keeper.generation() != self.kept_on) {
            self.kept = false;
            self.keep(true);
        }
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.ours.iter().position(|o| o.connector == name)
    }

    fn names(&self) -> Vec<String> {
        self.ours.iter().map(|o| o.connector.clone()).collect()
    }

    // Whether ours changed a moment ago without the monitors left as wanted: Mutter may then show
    // its own doing, or be about to.
    fn lately(&self) -> bool {
        !self.settled
            && self.changed_at.map_or(false, |at| {
                self.mutter.now().saturating_duration_since(at) < OURS_FOR
            })
    }

    // Whether the layout `state` shows is the others' as someone wants it: not lately, nor with
    // one of ours, `leaving` aside, not as made (gone, or at another size than Mutter confirmed),
    // Mutter laying the monitors out anew for that.
    fn theirs(&self, state: &DisplayState, leaving: &[String]) -> bool {
        !self.lately()
            && self
                .ours
                .iter()
                .filter(|o| !leaving.contains(&o.connector))
                .all(|o| !o.monitor.is_gone() && mode_of(state, &o.connector) == Some(o.size))
    }

    // The other monitors' layout as `state` shows it, should it be theirs (or none be remembered
    // yet): anyone else's change (in Settings, a monitor plugged in) so taken as it is.
    // Not while the desktop is taken over: the others are off then.
    fn remember(&mut self, state: &DisplayState, leaving: &[String]) {
        if self.taking_over() || (self.remembered.is_some() && !self.theirs(state, leaving)) {
            return;
        }
        if let Some(remembered) = Remembered::of(state, &self.names()) {
            self.remembered = Some(remembered);
        }
    }

    // After a change of ours, `done` should Mutter list what was asked: the other monitors put
    // back as remembered and ours to their right, should Mutter have laid them out anew. With the
    // last of ours gone the layout is forgotten: Mutter finds its stored one again by itself.
    fn put_back(&mut self, done: bool) {
        self.changed_at = Some(self.mutter.now());
        self.settled = false;
        if self.ours.is_empty() {
            self.remembered = None;
            return;
        }
        self.lay_out(done);
    }

    // The monitors laid out as wanted, should they not be; settled with it should the change of
    // ours it follows be `done`. Not while the desktop is taken over, kept so by itself.
    fn lay_out(&mut self, done: bool) {
        if self.taking_over() {
            return;
        }
        let Some(remembered) = self.remembered.clone() else {
            return;
        };
        let ours = self.names();
        for _ in 0..APPLY_TRIES {
            let Ok(state) = self.mutter.state() else {
                return;
            };
            let Some(wanted) = layout::wanted(&remembered, &state, &ours) else {
                log::info!("The monitors changed meanwhile: not put back");
                return;
            };
            if layout::current(&state).map_or(false, |c| layout::same(&c, &wanted)) {
                self.settled = done;
                return;
            }
            if !layout::valid(&wanted, &remembered, &state) {
                log::warn!(
                    "Not putting the monitors back, Mutter would refuse it: {:?}",
                    wanted
                );
                return;
            }
            if !self.mutter.apply_allowed() {
                if !self.unapplied {
                    log::info!(
                        "Mutter takes no monitor configurations: the monitors stay as laid out"
                    );
                    self.unapplied = true;
                }
                return;
            }
            match self.apply_safely(&state, &wanted) {
                Ok(()) => {
                    log::info!("Monitors put back as they were, the virtual ones to their right");
                    self.settled = done;
                    return;
                }
                // The state is read again: its serial may have gone stale meanwhile.
                Err(e) => log::warn!("Failed to put the monitors back: {}", e),
            }
        }
    }

    // Applies `config` against `state`, in its layout mode, each monitor it turns off moved first
    // (`layout::moving_off`): Mutter closes a moved monitor's recordings, which a monitor turned
    // off while recorded with the cursor as metadata would crash Mutter 42.9. Where they cannot be
    // moved, applied only should nobody record the desktop.
    fn apply_safely(&mut self, state: &DisplayState, config: &[LogicalConfig]) -> ResultType<()> {
        let layout_mode = state.layout_mode_changeable.then_some(state.layout_logical);
        if layout::turns_off(config, state) {
            match layout::moving_off(config, state) {
                Some(step) => {
                    // Ours of the physical monitors held meanwhile, else recorded anew where moved
                    // and taken for someone else's.
                    let pause = !self.physical_held;
                    if pause {
                        self.mutter.hold_physical(true);
                    }
                    let applied = self.move_off(state, &step, config, layout_mode);
                    if pause {
                        self.mutter.hold_physical(false);
                    }
                    return applied;
                }
                None if self.recorded_by_others() => {
                    bail!("Not turning monitors off that cannot be moved first, the desktop recorded")
                }
                None => {}
            }
        }
        self.mutter.expect_relayout();
        self.mutter.apply(state, config, layout_mode)
    }

    // `step` applied, then `config`: not should anyone record a monitor it turns off again where
    // `step` moved it (gnome-remote-desktop records anew on every change), to be moved again on the
    // next try, which closes that.
    fn move_off(
        &mut self,
        state: &DisplayState,
        step: &[LogicalConfig],
        config: &[LogicalConfig],
        layout_mode: Option<bool>,
    ) -> ResultType<()> {
        self.mutter.expect_relayout();
        self.mutter.apply(state, step, layout_mode)?;
        // Its serial is stale now.
        let moved = self.mutter.state()?;
        if self.recorded_where_moved(&moved, config) {
            bail!("Not turning monitors off recorded again where they were moved");
        }
        self.mutter.expect_relayout();
        self.mutter.apply(&moved, config, layout_mode)
    }

    // Whether anything records a monitor `config` turns off where it is now, moved off: begun
    // since, a stream's place being written as it starts, and the move having closed what
    // recorded it before. Mutter not telling, it is taken to.
    fn recorded_where_moved(&mut self, moved: &DisplayState, config: &[LogicalConfig]) -> bool {
        let going = layout::turned_off(config, moved);
        match self.mutter.streams() {
            Ok(streams) => streams
                .iter()
                .any(|s| going.iter().any(|l| s.position == Some((l.x, l.y)))),
            Err(e) => {
                log::warn!("Failed to list what Mutter records: {}", e);
                true
            }
        }
    }

    // Whether anything records what the desktop shows, a monitor, an area or a window: not ours
    // then, ours of the physical monitors stopped. Mutter not telling, it is taken to.
    fn recorded_by_others(&mut self) -> bool {
        match self.mutter.streams() {
            Ok(streams) => streams.iter().any(CastStream::of_the_desktop),
            Err(e) => {
                log::warn!("Failed to list what Mutter records: {}", e);
                true
            }
        }
    }

    fn make(&mut self, size: (i32, i32), announce: &mut dyn FnMut(&str)) -> ResultType<String> {
        // Ours among them: what is not there now is the one made.
        let before = self.mutter.state()?;
        self.remember(&before, &[]);
        self.mutter.expect_relayout();
        let mut monitor = self.mutter.create(size.0, size.1)?;
        let Some(connector) = self.wait_for(LISTED, |state| new_virtual(&before, state, size))
        else {
            // Dropped, its session stopped.
            bail!(
                "Mutter did not list the virtual monitor of {}x{} it made",
                size.0,
                size.1
            );
        };
        announce(&connector);
        monitor.show_as(&connector);
        log::info!(
            "Virtual display {} of {}x{} plugged in",
            connector,
            size.0,
            size.1
        );
        self.ours.push(Ours {
            connector: connector.clone(),
            monitor,
            size,
            asked: None,
            resize_tries: 0,
            usage: Mode::Shared,
        });
        Ok(connector)
    }

    // What `found` finds in the state, looking now and then until it does, for `within` at most
    // however slow Mutter is to answer. A state Mutter does not answer with finds nothing.
    fn wait_for<T>(
        &mut self,
        within: Duration,
        found: impl Fn(&DisplayState) -> Option<T>,
    ) -> Option<T> {
        let end = self.mutter.now() + within;
        loop {
            if let Some(t) = self.mutter.state().ok().as_ref().and_then(&found) {
                return Some(t);
            }
            if self.mutter.now() >= end {
                return None;
            }
            self.mutter.pause(LOOK);
        }
    }
}

impl<M, K> Backend for GnomeBackend<M, K>
where
    M: Mutter + Send,
    M::Monitor: Send,
    K: Keeper + Send,
{
    fn caps(&self) -> Caps {
        Caps {
            kind: "gnome-mutter",
            max: MAX,
            custom: true,
            // Mutter makes virtual monitors of scale 1.
            hidpi: false,
            max_width: 3840,
            max_height: 2160,
            // The desktop taken over only where Mutter takes configurations; the root service,
            // which may come up later, asked when it is.
            modes: if self.privacy {
                vec![Mode::Shared, Mode::Privacy]
            } else {
                vec![Mode::Shared]
            },
        }
    }

    // An extra screen, laid out beside the others; one for the desktop is then asked to take it
    // over (`set_mode`), as the manager does, and stays an extra screen should that not take. One
    // made while the desktop is taken over joins in.
    fn plug_in(&mut self, spec: &Spec, announce: &mut dyn FnMut(&str)) -> ResultType<String> {
        self.keep(true);
        let made = self.make((spec.width as i32, spec.height as i32), announce);
        self.put_back(made.is_ok());
        let name = match made {
            Ok(name) => name,
            Err(e) => {
                self.keep_while_any();
                return Err(e);
            }
        };
        // Mutter laid the monitors out anew for it, the others on again maybe; and the root
        // service not looked at while it was made.
        if self.taking_over() && self.root_keeps() {
            self.lay_out_taken_again(&name);
        }
        Ok(name)
    }

    // The person here may have taken the desktop back since the last look (Ctrl+P): given back
    // first, and not taken over again in the same breath.
    fn set_mode(&mut self, name: &str, spec: &Spec) -> ResultType<()> {
        let Some(i) = self.find(name) else {
            bail!("No virtual display {}", name);
        };
        let taken_back = self.taking_over() && self.keeper.taken_back().unwrap_or(false);
        if taken_back {
            log::warn!("The person at this device took the desktop back");
            self.hand_back().ok();
        }
        let resized = self.resize(i, (spec.width as i32, spec.height as i32));
        if taken_back && spec.mode == Mode::Privacy {
            resized?;
            bail!(TAKEN_BACK);
        }
        let switched = self.switch(i, spec.mode);
        resized.and(switched)
    }

    fn usage(&self, name: &str) -> Option<Mode> {
        self.find(name).map(|i| self.ours[i].usage)
    }

    // Dropped, its session stopped, and Mutter takes it back at once. One it still lists after
    // that is beyond reach, and gone for us all the same: a retry could do no more, and kept,
    // its name could clash with a new monitor's, Mutter giving the lowest free one. The last of
    // ours taking the desktop over gives it back first, never a moment with no monitor on;
    // should that fail, it stays, to be given back again.
    fn plug_out(&mut self, name: &str) -> ResultType<()> {
        let Some(i) = self.find(name) else {
            return Ok(());
        };
        let gives_back = self.ours[i].usage == Mode::Privacy
            && self.ours.iter().filter(|o| o.usage == Mode::Privacy).count() == 1;
        if gives_back {
            if let Err(e) = self.hand_back() {
                log::error!("Kept virtual display {}: {}", name, e);
                return Err(e);
            }
        }
        if let Ok(state) = self.mutter.state() {
            self.remember(&state, &[]);
        }
        self.mutter.expect_relayout();
        drop(self.ours.remove(i));
        log::info!("Virtual display {} plugged out", name);
        let removed = self.wait_for(REMOVED, |state| state.monitor(name).is_none().then_some(()));
        if removed.is_none() {
            log::warn!(
                "Mutter still lists virtual display {}, its session stopped",
                name
            );
        }
        if self.taking_over() {
            // The rest keep the desktop, laid out without it.
            self.lay_out_taken_again(name);
        } else {
            self.put_back(removed.is_some());
        }
        self.keep_while_any();
        Ok(())
    }

    // Nothing of a past run is left: Mutter took its monitors back as it left the bus.
    fn reset_all(&mut self) -> ResultType<()> {
        let names: Vec<String> = self.ours.iter().map(|o| o.connector.clone()).collect();
        for name in names {
            self.plug_out(&name)?;
        }
        Ok(())
    }

    // A monitor whose session Mutter closed (the screen locked, or someone stopped the sharing in
    // GNOME Shell's indicator), or that it no longer lists (it went), is gone: not made again, a
    // screen locked while used from afar having mostly been locked by someone at it. A desktop
    // taken over is kept so (`keep_taken_over`); sizes put off while it was are taken once it is
    // given back.
    fn maintain(&mut self) -> Vec<String> {
        if self.ours.is_empty() {
            return Vec::new();
        }
        self.rekeep(false);
        self.tell_restored();
        let state = self.mutter.state().ok();
        let mut gone: Vec<String> = self
            .ours
            .iter()
            .filter(|o| {
                o.monitor.is_gone()
                    || state
                        .as_ref()
                        .map_or(false, |s| s.monitor(&o.connector).is_none())
            })
            .map(|o| o.connector.clone())
            .collect();
        if !gone.is_empty() {
            log::warn!("Virtual displays {:?} went away", gone);
            let taking_over = self.taking_over();
            let ends = taking_over
                && self
                    .ours
                    .iter()
                    .all(|o| o.usage != Mode::Privacy || gone.contains(&o.connector));
            if ends {
                // The screen locked, most likely: the input here the person's at once.
                self.restore_input();
            }
            // The others as they were, should Mutter not have laid them out anew yet.
            if let Some(state) = state
                .as_ref()
                .filter(|s| gone.iter().all(|c| s.monitor(c).is_some()))
            {
                self.remember(state, &gone);
            }
            self.ours.retain(|o| !gone.contains(&o.connector));
            if ends {
                self.takeover_gone(&gone);
            } else {
                // Mutter lays the rest out anew once it took them back, which one closed it may
                // not have yet; until then they would be taken for monitors of someone else.
                let unlisted = self.wait_for(REMOVED, |state| {
                    gone.iter()
                        .all(|c| state.monitor(c).is_none())
                        .then_some(())
                });
                if self.taking_over() {
                    self.lay_out_taken_again(&gone.join(", "));
                } else {
                    self.put_back(unlisted.is_some());
                }
            }
        } else if self.taking_over() {
            gone = self.keep_taken_over(state.as_ref());
        } else if let Some(state) = &state {
            self.look_after(state);
        }
        if !self.taking_over() {
            self.resize_pending();
        }
        self.keep_while_any();
        gone
    }

    fn watchful(&self) -> bool {
        self.taking_over()
    }
}

impl<M: Mutter, K: Keeper> GnomeBackend<M, K> {
    // Display `i` at `size`: put off while the desktop is taken over, its size fixed then.
    fn resize(&mut self, i: usize, size: (i32, i32)) -> ResultType<()> {
        if self.taking_over() {
            let o = &mut self.ours[i];
            if o.size == size {
                o.asked = None;
            } else if o.asked != Some(size) {
                log::info!(
                    "Virtual display {} resized to {}x{} once the desktop is given back",
                    o.connector,
                    size.0,
                    size.1
                );
                o.asked = Some(size);
                o.resize_tries = 0;
            }
            return Ok(());
        }
        self.ours[i].asked = None;
        if self.ours[i].size == size {
            return Ok(());
        }
        let name = self.ours[i].connector.clone();
        if let Ok(state) = self.mutter.state() {
            self.remember(&state, &[]);
        }
        self.mutter.expect_relayout();
        let by_capturer = match self.ours[i].monitor.resize(size.0, size.1) {
            Ok(by_capturer) => by_capturer,
            Err(e) => {
                self.undo_resize(i);
                self.put_back(false);
                return Err(e);
            }
        };
        let resized = self.wait_for(RESIZED, |state| {
            (mode_of(state, &name) == Some(size)).then_some(())
        });
        if resized.is_none() {
            self.undo_resize(i);
        }
        self.put_back(resized.is_some());
        if resized.is_none() {
            bail!(
                "Mutter did not resize virtual display {} to {}x{}",
                name,
                size.0,
                size.1
            );
        }
        self.ours[i].size = size;
        log::info!(
            "Virtual display {} resized to {}x{}{}",
            name,
            size.0,
            size.1,
            if by_capturer { ", by its capturer" } else { "" }
        );
        Ok(())
    }

    // The sizes put off while the desktop was taken over, each tried `RESIZE_TRIES` times at most.
    fn resize_pending(&mut self) {
        for i in 0..self.ours.len() {
            let Some(size) = self.ours[i].asked else {
                continue;
            };
            if let Err(e) = self.resize(i, size) {
                let o = &mut self.ours[i];
                o.resize_tries += 1;
                if o.resize_tries < RESIZE_TRIES {
                    o.asked = Some(size);
                } else {
                    log::warn!(
                        "Gave up resizing virtual display {} to {}x{}: {}",
                        o.connector,
                        size.0,
                        size.1,
                        e
                    );
                }
            }
        }
    }

    // A resize given up on: the stream back at the size Mutter last confirmed, the one asked
    // having maybe reached it all the same, or about to.
    fn undo_resize(&mut self, i: usize) {
        let (width, height) = self.ours[i].size;
        if let Err(e) = self.ours[i].monitor.resize(width, height) {
            log::warn!(
                "Failed to take virtual display {} back to {}x{}: {}",
                self.ours[i].connector,
                width,
                height,
                e
            );
        }
    }

    // With all of ours there. One Mutter lists at another size than it confirmed had its stream
    // settle that late (a resize given up on, then undone): a change of ours, the monitors put
    // back after it. Otherwise the layout is taken for the others' where it is theirs, or put back
    // where Mutter may have laid it out anew for a change of ours a moment ago.
    fn look_after(&mut self, state: &DisplayState) {
        let mut resized = false;
        for o in &mut self.ours {
            if let Some(size) = mode_of(state, &o.connector).filter(|s| *s != o.size) {
                log::warn!(
                    "Virtual display {} is {}x{} now, its stream settled that late",
                    o.connector,
                    size.0,
                    size.1
                );
                o.size = size;
                resized = true;
            }
        }
        if resized {
            self.put_back(true);
        } else if self.theirs(state, &[]) {
            self.remember(state, &[]);
        } else if self.lately() {
            self.lay_out(false);
        }
    }

    // Whether a display of ours has the desktop to itself (`Mode::Privacy`).
    fn taking_over(&self) -> bool {
        self.ours.iter().any(|o| o.usage == Mode::Privacy)
    }

    // Whether `state` shows the desktop taken over: ours all on, nothing else.
    fn taken(&self, state: &DisplayState) -> bool {
        let shown: Vec<&String> = state.logical.iter().flat_map(|l| &l.connectors).collect();
        shown.iter().all(|c| self.find(c).is_some())
            && self.ours.iter().all(|o| shown.contains(&&o.connector))
    }

    // Display `i` used as `mode` from now on: the desktop taken over, or given back once the last
    // of ours taking it over no longer does.
    fn switch(&mut self, i: usize, mode: Mode) -> ResultType<()> {
        let privacy = mode == Mode::Privacy;
        if (self.ours[i].usage == Mode::Privacy) == privacy {
            return Ok(());
        }
        if privacy {
            return self.take_over(i);
        }
        if self.ours.iter().filter(|o| o.usage == Mode::Privacy).count() > 1 {
            self.ours[i].usage = Mode::Shared;
            return Ok(());
        }
        self.hand_back()
    }

    // The desktop handed to display `i`, one taking it over already joined: the root service and
    // the recording seen to (`engage`), then ours alone on, the others off. Should that not take,
    // the desktop is given back at once, and nothing of it left.
    fn take_over(&mut self, i: usize) -> ResultType<()> {
        if self.taking_over() {
            self.ours[i].usage = Mode::Privacy;
            return Ok(());
        }
        self.engage()?;
        if let Ok(state) = self.mutter.state() {
            self.remember(&state, &[]);
        }
        self.ours[i].usage = Mode::Privacy;
        if let Err(e) = self.lay_out_taken() {
            log::error!("Failed to take the desktop over: {}", e);
            match self.light_the_others() {
                Ok(()) => {
                    self.changed_at = Some(self.mutter.now());
                    self.settled = false;
                    self.disengage();
                }
                // Dark maybe, the input kept: given back on the looks to come, as a desktop that
                // would not be (`keep_taken_over`).
                Err(e) => {
                    log::error!("Failed to light the monitors again, to try again: {}", e);
                    self.unreturned = 1;
                }
            }
            bail!(NOT_TAKEN_OVER);
        }
        log::info!(
            "The desktop is taken over by virtual display {}",
            self.ours[i].connector
        );
        Ok(())
    }

    // Before the desktop is first taken over: Mutter taking configurations, the person here not
    // having taken it back a moment ago, the physical monitors recorded no more and those
    // recordings gone, nobody else recording the desktop (they would record it taken over, and
    // Mutter may crash turning a monitor off under them), and only then the input here locked.
    // Undone should any fail, answering why.
    fn engage(&mut self) -> ResultType<()> {
        if !self.mutter.apply_allowed() {
            bail!(NOT_CONFIGURABLE);
        }
        // A word of a desktop given back before, arriving after the input is locked anew, would
        // let it go.
        self.tell_restored();
        if self.unrestored {
            bail!(NO_INPUT_LOCK);
        }
        if self.keeper.taken_back().unwrap_or(false) {
            bail!(TAKEN_BACK);
        }
        self.hold_physical();
        let end = self.mutter.now() + STREAMS_GONE;
        while self.recorded_by_others() {
            if self.mutter.now() >= end {
                self.release_physical();
                bail!(RECORDED_ELSEWHERE);
            }
            self.mutter.pause(LOOK);
        }
        if let Err(e) = self.lock_input() {
            self.release_physical();
            return Err(e);
        }
        Ok(())
    }

    // What taking the desktop over asked undone, ours all extra screens: the root service told it
    // is given back, the physical monitors recorded again.
    fn disengage(&mut self) {
        for o in &mut self.ours {
            o.usage = Mode::Shared;
        }
        self.restore_input();
        self.release_physical();
        self.slipping = 0;
        self.unreturned = 0;
    }

    fn hold_physical(&mut self) {
        self.mutter.hold_physical(true);
        self.physical_held = true;
    }

    fn release_physical(&mut self) {
        if std::mem::take(&mut self.physical_held) {
            self.mutter.hold_physical(false);
        }
    }

    // The input here kept from the desktop by the root service, on the connection open now. The
    // connection goes should it fail, and the hold with it: asked again.
    fn lock_input(&mut self) -> ResultType<()> {
        match self.keeper.lock_input() {
            Ok(()) => {
                self.locked = Some(self.keeper.generation());
                Ok(())
            }
            Err(e) => {
                log::warn!("The root service does not keep the input here: {}", e);
                self.rekeep(true);
                if e.to_string() == TAKEN_BACK {
                    bail!(TAKEN_BACK);
                }
                bail!(NO_INPUT_LOCK);
            }
        }
    }

    // The root service told the desktop is given back, the input here let go, should it keep it.
    // Told again on the looks to come should it not hear (`tell_restored`).
    fn restore_input(&mut self) {
        if self.locked.take().is_some() {
            self.unrestored = true;
            self.tell_restored();
        }
    }

    // The root service told the desktop is given back, should it not have been yet: tried twice,
    // the second time on a connection made anew, the first having gone with its failure (and the
    // lock with it, but not what it remembers of Ctrl+P); then again on the looks to come.
    fn tell_restored(&mut self) {
        if !self.unrestored {
            return;
        }
        for _ in 0..2 {
            match self.keeper.restored() {
                Ok(()) => {
                    self.unrestored = false;
                    return;
                }
                Err(e) => {
                    log::warn!("Failed to tell the root service the desktop is given back: {}", e)
                }
            }
        }
        self.rekeep(true);
    }

    // The desktop as taken over: ours alone on, side by side, the first of them taking it over
    // primary.
    fn taken_layout(&self, state: &DisplayState) -> Option<Vec<LogicalConfig>> {
        let primary = self.ours.iter().position(|o| o.usage == Mode::Privacy)?;
        layout::alone(state, &self.names(), primary)
    }

    // The desktop laid out as taken over, should it not be; tried again on a state read anew
    // should Mutter refuse it (its serial stale), or not answer. Done once Mutter shows it so.
    fn lay_out_taken(&mut self) -> ResultType<()> {
        for tries in 0.. {
            let state = match self.mutter.state() {
                Ok(state) => state,
                // Busy right after a change, maybe.
                Err(e) if tries < APPLY_TRIES => {
                    log::warn!("Failed to read the monitors from Mutter: {}", e);
                    self.mutter.pause(LOOK);
                    continue;
                }
                Err(e) => return Err(e),
            };
            if self.taken(&state) {
                return Ok(());
            }
            if tries == APPLY_TRIES {
                break;
            }
            let Some(config) = self.taken_layout(&state) else {
                bail!("A virtual display of ours has no mode");
            };
            if let Err(e) = self.apply_safely(&state, &config) {
                log::warn!("Failed to lay the desktop out as taken over: {}", e);
            }
        }
        bail!("Mutter does not show the desktop taken over")
    }

    // The monitors not ours on again: as remembered with ours to their right, or short of that
    // (they changed meanwhile) those still there and any plugged in since (`layout::give_back`).
    // Done once Mutter shows them so, or on at least after a few tries; nothing to do without
    // any. Whatever lit them, the layout is seen to on the looks to come (`lately`).
    fn light_the_others(&mut self) -> ResultType<()> {
        let remembered = self.remembered.clone().unwrap_or_default();
        let ours = self.names();
        for tries in 0.. {
            let state = match self.mutter.state() {
                Ok(state) => state,
                Err(e) if tries < APPLY_TRIES => {
                    log::warn!("Failed to read the monitors from Mutter: {}", e);
                    self.mutter.pause(LOOK);
                    continue;
                }
                Err(e) => return Err(e),
            };
            let back = layout::wanted(&remembered, &state, &ours)
                .filter(|w| layout::valid(w, &remembered, &state))
                .or_else(|| layout::give_back(&remembered, &state, &ours));
            let Some(back) = back else {
                if state
                    .monitors
                    .iter()
                    .all(|m| ours.contains(&m.connector) || m.for_lease)
                {
                    return Ok(());
                }
                bail!("No layout to give the desktop back with");
            };
            if layout::current(&state).map_or(false, |c| layout::same(&c, &back)) {
                return Ok(());
            }
            if tries == APPLY_TRIES {
                // On all the same, if not where wanted: put back on the looks to come.
                if layout::shows(&state, &back) {
                    return Ok(());
                }
                break;
            }
            if let Err(e) = self.apply_safely(&state, &back) {
                log::warn!("Failed to light the monitors again: {}", e);
            }
        }
        bail!("Mutter does not show the monitors on again")
    }

    // The desktop given back, every display of ours an extra screen from now on: the others on
    // again, then at once the root service told (it ends this process should it not hear soon
    // after Ctrl+P), the sizes put off meanwhile taken, and only then the physical monitors
    // recorded again. Should the others not come on, the desktop stays taken over, to be given
    // back at the next try; after `GIVE_BACK_TRIES` its displays go (`keep_taken_over`).
    fn hand_back(&mut self) -> ResultType<()> {
        if let Err(e) = self.light_the_others() {
            self.unreturned += 1;
            log::error!("Failed to give the desktop back, to try again: {}", e);
            return Err(e);
        }
        for o in &mut self.ours {
            o.usage = Mode::Shared;
        }
        self.changed_at = Some(self.mutter.now());
        self.settled = false;
        self.restore_input();
        self.resize_pending();
        self.disengage();
        log::info!("The desktop is given back");
        Ok(())
    }

    // The last of ours taking the desktop over went, `gone`, by itself (the screen locked, most
    // likely) or let go, the input here given back first (`restore_input`, for the password):
    // once Mutter took them back, the others seen on, lit should Mutter not have, and the
    // physical monitors recorded again.
    fn takeover_gone(&mut self, gone: &[String]) {
        let unlisted = self.wait_for(REMOVED, |state| {
            gone.iter()
                .all(|c| state.monitor(c).is_none())
                .then_some(())
        });
        if unlisted.is_none() {
            log::warn!("Mutter still lists virtual displays {:?}", gone);
        }
        if let Err(e) = self.light_the_others() {
            log::error!("Failed to light the monitors again: {}", e);
        }
        self.changed_at = Some(self.mutter.now());
        self.settled = false;
        if self.ours.is_empty() {
            self.remembered = None;
        }
        for o in &mut self.ours {
            o.usage = Mode::Shared;
        }
        self.resize_pending();
        self.disengage();
    }

    // The desktop taken over, kept so on every look: given back should the person here have
    // taken it back (Ctrl+P), or the root service not keep the input here again on a connection
    // opened anew; laid out as taken over again should it not be (a monitor plugged in, Mutter
    // laying out anew), and given back after `TAKEOVER_TRIES` looks in a row it would not be.
    // One that would not be given back for `GIVE_BACK_TRIES` tries is let go with its displays,
    // answered.
    fn keep_taken_over(&mut self, state: Option<&DisplayState>) -> Vec<String> {
        if self.unreturned >= GIVE_BACK_TRIES {
            return self.let_go();
        }
        if self.unreturned > 0 {
            log::warn!("Giving the desktop back again");
            self.hand_back().ok();
            return Vec::new();
        }
        if !self.root_keeps() {
            return Vec::new();
        }
        let Some(state) = state else {
            return Vec::new();
        };
        if self.taken(state) {
            self.slipping = 0;
            return Vec::new();
        }
        log::warn!("The desktop taken over is not laid out so; laying it out again");
        match self.lay_out_taken() {
            Ok(()) => self.slipping = 0,
            Err(e) => {
                log::warn!("Failed to keep the desktop taken over: {}", e);
                self.slipping += 1;
                if self.slipping >= TAKEOVER_TRIES {
                    log::error!("The desktop could not be kept taken over; giving it back");
                    self.hand_back().ok();
                }
            }
        }
        Vec::new()
    }

    // Whether the root service still keeps the input here from the desktop taken over: given back
    // should the person here have taken it back (Ctrl+P), or the service not lock it again on a
    // connection opened anew.
    fn root_keeps(&mut self) -> bool {
        let asked = self.keeper.taken_back();
        if let Ok(true) = asked {
            log::warn!("The person at this device took the desktop back");
            self.hand_back().ok();
            return false;
        }
        if asked.is_err() || self.locked != Some(self.keeper.generation()) {
            self.rekeep(asked.is_err());
            if self.lock_input().is_err() {
                log::error!("The input here is no longer kept from the desktop: giving it back");
                self.hand_back().ok();
                return false;
            }
        }
        true
    }

    // The desktop laid out as taken over again after a change of ours (`what` made or gone),
    // which Mutter may have lit the others for; given back should that fail, never left so.
    fn lay_out_taken_again(&mut self, what: &str) {
        if let Err(e) = self.lay_out_taken() {
            log::error!(
                "Failed to keep the desktop taken over after {}: {}; giving it back",
                what,
                e
            );
            self.hand_back().ok();
        }
    }

    // A desktop that would not be given back: the displays of ours taking it over go, Mutter
    // lighting its own monitors again as it takes them back.
    fn let_go(&mut self) -> Vec<String> {
        let taking: Vec<String> = self
            .ours
            .iter()
            .filter(|o| o.usage == Mode::Privacy)
            .map(|o| o.connector.clone())
            .collect();
        log::error!(
            "The desktop could not be given back; virtual displays {:?} go",
            taking
        );
        self.restore_input();
        self.mutter.expect_relayout();
        self.ours.retain(|o| o.usage != Mode::Privacy);
        self.takeover_gone(&taking);
        taking
    }
}

// Let go of with the backend: the physical monitors recorded again.
impl<M: Mutter, K> Drop for GnomeBackend<M, K> {
    fn drop(&mut self) {
        if self.physical_held {
            self.mutter.hold_physical(false);
        }
    }
}

// The virtual monitor of `size` listed `now` and not `before`, should there be just one: another
// may be made meanwhile by someone else, GNOME's own remote desktop say.
fn new_virtual(before: &DisplayState, now: &DisplayState, size: (i32, i32)) -> Option<String> {
    let mut new = now.monitors.iter().filter(|m| {
        before.monitor(&m.connector).is_none()
            && m.product == VIRTUAL_PRODUCT
            && mode_of(now, &m.connector) == Some(size)
    });
    let one = new.next()?;
    new.next().is_none().then(|| one.connector.clone())
}

fn mode_of(state: &DisplayState, connector: &str) -> Option<(i32, i32)> {
    state
        .monitor(connector)?
        .current_mode()
        .map(|m| (m.width, m.height))
}

#[cfg(target_os = "linux")]
mod live {
    use super::{GnomeBackend, Keeper, Monitor, Mutter};
    use crate::vdisplay::{manager::Backend, x11::RootLink};
    use hbb_common::{log, ResultType};
    use scrap::{
        mutter_model::{CastStream, DisplayState, LogicalConfig},
        wayland::mutter_capture::{self, VirtualMonitor},
    };
    use std::time::{Duration, Instant};

    /// The backend of this session, on GNOME where capture goes through Mutter.
    pub fn new_backend() -> Option<Box<dyn Backend>> {
        if !crate::server::mutter::active() {
            return None;
        }
        log::info!("Virtual displays: Mutter's own virtual monitors");
        Some(Box::new(GnomeBackend::new(Live, RootLink::later())))
    }

    struct Live;

    impl Mutter for Live {
        type Monitor = VirtualMonitor;

        fn create(&mut self, width: i32, height: i32) -> ResultType<VirtualMonitor> {
            VirtualMonitor::create(width, height)
        }

        fn state(&mut self) -> ResultType<DisplayState> {
            mutter_capture::state()
        }

        fn expect_relayout(&mut self) {
            mutter_capture::expect_relayout()
        }

        fn pause(&mut self, d: Duration) {
            std::thread::sleep(d)
        }

        fn now(&self) -> Instant {
            Instant::now()
        }

        fn apply(
            &mut self,
            state: &DisplayState,
            config: &[LogicalConfig],
            layout_logical: Option<bool>,
        ) -> ResultType<()> {
            mutter_capture::apply_temporary(state, config, layout_logical)
        }

        fn apply_allowed(&mut self) -> bool {
            mutter_capture::apply_allowed()
        }

        fn streams(&mut self) -> ResultType<Vec<CastStream>> {
            mutter_capture::streams()
        }

        fn hold_physical(&mut self, on: bool) {
            crate::server::mutter::hold_physical(on)
        }
    }

    impl Monitor for VirtualMonitor {
        fn show_as(&mut self, connector: &str) {
            VirtualMonitor::show_as(self, connector)
        }

        fn resize(&self, width: i32, height: i32) -> ResultType<bool> {
            VirtualMonitor::resize(self, width, height)
        }

        fn is_gone(&self) -> bool {
            VirtualMonitor::is_gone(self)
        }
    }

    impl Keeper for RootLink {
        fn keep(&mut self, on: bool) -> ResultType<()> {
            self.hold(on)
        }

        fn generation(&self) -> u64 {
            RootLink::generation(self)
        }

        fn lock_input(&mut self) -> ResultType<()> {
            RootLink::lock_input(self)
        }

        fn taken_back(&mut self) -> ResultType<bool> {
            RootLink::taken_back(self)
        }

        fn restored(&mut self) -> ResultType<()> {
            RootLink::restored(self)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrap::mutter_model::{LogicalMonitor, Monitor as Listed, MonitorMode};
    use std::sync::{Arc, Mutex};

    // Mutter and the root service as the tests make them, and what was done to them, in order.
    #[derive(Default)]
    struct World {
        /// What Mutter lists: connector, product, size.
        listed: Vec<(String, &'static str, (i32, i32))>,
        /// Where each is: connector, x, y, primary. Side by side, as Mutter lays them out, after
        /// every change to what it lists.
        logical: Vec<(String, i32, i32, bool)>,
        /// Configurations refused (their serial stale, say) before one is taken.
        apply_fails: usize,
        /// Mutter takes no configurations.
        no_apply: bool,
        /// Changes to come after so many looks, and what they list.
        coming: Vec<(usize, String, &'static str, (i32, i32))>,
        /// How many looks a monitor made or resized takes to be listed so.
        delay: usize,
        /// Someone else makes a virtual monitor of the same size at the same time.
        twin: bool,
        /// Mutter keeps listing a monitor dropped, or does for so many looks.
        sticky: bool,
        leave_after: usize,
        leaving: Vec<(usize, String)>,
        /// Resizing fails, though Mutter resized the monitor.
        resize_fails: bool,
        /// How long Mutter takes to answer with its state, and how often it was asked.
        slow: Duration,
        looks: usize,
        /// Time passed.
        clock: Duration,
        gone: Vec<String>,
        no_root: bool,
        made: usize,
        done: Vec<String>,
        /// What others record, and how many looks ours of the physical monitors stay listed once
        /// held; listing them fails.
        streams: Vec<CastStream>,
        own_streams: usize,
        streams_fail: bool,
        held: bool,
        /// Someone records a monitor anew where it is moved to (only this one, if any), so many
        /// times, as gnome-remote-desktop does on every change; so do ours of the physical
        /// monitors unless held, the cursor hidden; listing fails once Mutter took so many
        /// configurations. A monitor turned off while recorded by someone else crashed Mutter
        /// (42.9, the cursor as metadata).
        rerecords: usize,
        rerecorded: Option<&'static str>,
        ours_rerecord: bool,
        streams_fail_after: Option<usize>,
        crashed: bool,
        /// The root service: the person here took the desktop back (Ctrl+P); it refuses to lock
        /// the input; which connection to it this is, and whether the last one went.
        taken_back: bool,
        lock_refused: bool,
        generation: u64,
        link_dropped: bool,
        /// Mutter answers no more with its state once it took so many configurations; the root
        /// service fails to hear the desktop is given back so many times.
        blind_after: Option<usize>,
        applies: usize,
        restore_fails: usize,
        /// So many of the next looks at Mutter's state fail.
        state_fails: usize,
    }

    type Shared = Arc<Mutex<World>>;

    fn world() -> Shared {
        let w: Shared = Default::default();
        let mut world = w.lock().unwrap();
        world.listed.push(("HDMI-1".to_owned(), "LG", (1920, 1080)));
        side_by_side(&mut world);
        drop(world);
        w
    }

    // HDMI-1 on the right and primary, 100 lower, DP-1 on the left: not side by side.
    fn stored_world() -> Shared {
        let w = world();
        let mut world = w.lock().unwrap();
        world.listed.push(("DP-1".to_owned(), "LG", (1280, 1024)));
        world.logical = vec![
            ("HDMI-1".to_owned(), 1280, 100, true),
            ("DP-1".to_owned(), 0, 0, false),
        ];
        drop(world);
        w
    }

    // As Mutter lays the monitors out when it finds no stored layout for them.
    fn side_by_side(w: &mut World) {
        let mut x = 0;
        w.logical = w
            .listed
            .iter()
            .enumerate()
            .map(|(i, (c, _, (width, _)))| {
                x += width;
                (c.clone(), x - width, 0, i == 0)
            })
            .collect();
    }

    // What was done since last asked; never a monitor turned off under someone recording it.
    fn done(w: &Shared) -> Vec<String> {
        let (crashed, done) = {
            let mut w = w.lock().unwrap();
            (w.crashed, std::mem::take(&mut w.done))
        };
        assert!(!crashed, "a monitor turned off while recorded: {:?}", done);
        done
    }

    fn set(w: &mut World, connector: &str, product: &'static str, size: (i32, i32)) {
        match w.listed.iter_mut().find(|(c, _, _)| c == connector) {
            Some(listed) => listed.2 = size,
            None => w.listed.push((connector.to_owned(), product, size)),
        }
        side_by_side(w);
    }

    struct FakeMutter(Shared, Instant);

    impl Mutter for FakeMutter {
        type Monitor = FakeMonitor;

        fn create(&mut self, width: i32, height: i32) -> ResultType<FakeMonitor> {
            let mut w = self.0.lock().unwrap();
            w.done.push(format!("create {}x{}", width, height));
            let connector = format!("Meta-{}", w.made);
            w.made += 1;
            let delay = w.delay;
            w.coming
                .push((delay, connector.clone(), VIRTUAL_PRODUCT, (width, height)));
            if w.twin {
                w.coming
                    .push((delay, "Meta-9".to_owned(), VIRTUAL_PRODUCT, (width, height)));
            }
            Ok(FakeMonitor {
                world: self.0.clone(),
                connector,
            })
        }

        fn state(&mut self) -> ResultType<DisplayState> {
            let mut w = self.0.lock().unwrap();
            w.looks += 1;
            if w.blind_after.map_or(false, |n| w.applies >= n) {
                bail!("Timeout was reached");
            }
            if w.state_fails > 0 {
                w.state_fails -= 1;
                bail!("Timeout was reached");
            }
            let slow = w.slow;
            w.clock += slow;
            // As Mutter lists them: the mode current only for a monitor that is on.
            let monitors = w
                .listed
                .iter()
                .map(|(connector, product, (width, height))| Listed {
                    connector: connector.clone(),
                    vendor: String::new(),
                    product: product.to_string(),
                    serial: String::new(),
                    modes: vec![MonitorMode {
                        id: format!("{}x{}", width, height),
                        width: *width,
                        height: *height,
                        refresh: 60.0,
                        current: w.logical.iter().any(|l| l.0 == *connector),
                        preferred: true,
                    }],
                    builtin: false,
                    underscanning: None,
                    color_mode: None,
                    for_lease: false,
                })
                .collect();
            let logical = w
                .logical
                .iter()
                .filter(|(c, ..)| w.listed.iter().any(|l| l.0 == *c))
                .map(|(c, x, y, primary)| LogicalMonitor {
                    x: *x,
                    y: *y,
                    scale: 1.0,
                    transform: 0,
                    primary: *primary,
                    connectors: vec![c.clone()],
                })
                .collect();
            Ok(DisplayState {
                serial: 1,
                monitors,
                logical,
                layout_logical: false,
                layout_mode_changeable: false,
            })
        }

        fn expect_relayout(&mut self) {
            self.0.lock().unwrap().done.push("relayout".to_owned());
        }

        fn pause(&mut self, d: Duration) {
            let mut w = self.0.lock().unwrap();
            w.clock += d;
            if w.held {
                w.own_streams = w.own_streams.saturating_sub(1);
            }
            for c in &mut w.coming {
                c.0 = c.0.saturating_sub(1);
            }
            let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut w.coming)
                .into_iter()
                .partition(|c| c.0 == 0);
            w.coming = later;
            for (_, connector, product, size) in now {
                set(&mut w, &connector, product, size);
            }
            for l in &mut w.leaving {
                l.0 = l.0.saturating_sub(1);
            }
            let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut w.leaving)
                .into_iter()
                .partition(|l| l.0 == 0);
            w.leaving = later;
            for (_, connector) in now {
                w.listed.retain(|c| c.0 != connector);
                side_by_side(&mut w);
            }
        }

        fn now(&self) -> Instant {
            self.1 + self.0.lock().unwrap().clock
        }

        fn apply(
            &mut self,
            _state: &DisplayState,
            config: &[LogicalConfig],
            _layout_logical: Option<bool>,
        ) -> ResultType<()> {
            let mut w = self.0.lock().unwrap();
            if w.apply_fails > 0 {
                w.apply_fails -= 1;
                w.done.push("apply failed".to_owned());
                bail!("The requested configuration is based on stale information");
            }
            let mut config = config.to_vec();
            config.sort_by_key(|l| (l.x, l.y));
            let told: Vec<String> = config
                .iter()
                .map(|l| {
                    let star = if l.primary { "*" } else { "" };
                    format!("{}@{},{}{}", l.monitors[0].0, l.x, l.y, star)
                })
                .collect();
            w.done.push(format!("apply {}", told.join(" ")));
            w.applies += 1;
            let before = std::mem::replace(
                &mut w.logical,
                config
                    .iter()
                    .map(|l| (l.monitors[0].0.clone(), l.x, l.y, l.primary))
                    .collect(),
            );
            // Mutter closes the recordings of a monitor moved or turned off: cleanly once moved.
            for (connector, x, y, _) in before {
                let now = w
                    .logical
                    .iter()
                    .find(|l| l.0 == connector)
                    .map(|l| (l.1, l.2));
                if now == Some((x, y)) {
                    continue;
                }
                let recorded = w
                    .streams
                    .iter()
                    .any(|s| s.position == Some((x, y)) && !s.path.ends_with("/ours"));
                w.streams.retain(|s| s.position != Some((x, y)));
                let Some(at) = now else {
                    w.crashed |= recorded;
                    continue;
                };
                if w.rerecords > 0 && w.rerecorded.map_or(true, |c| c == connector) {
                    w.rerecords -= 1;
                    let id = format!("r{}", w.rerecords);
                    w.streams.push(recording(&id, Some(at), Some((1, 1))));
                }
                if w.ours_rerecord && !w.held {
                    w.streams.push(recording("ours", Some(at), Some((1, 1))));
                }
            }
            Ok(())
        }

        fn apply_allowed(&mut self) -> bool {
            !self.0.lock().unwrap().no_apply
        }

        fn streams(&mut self) -> ResultType<Vec<CastStream>> {
            let w = self.0.lock().unwrap();
            if w.streams_fail || w.streams_fail_after.map_or(false, |n| w.applies >= n) {
                bail!("No answer from Mutter");
            }
            let mut streams = w.streams.clone();
            if w.own_streams > 0 {
                streams.push(recording("u1", Some((0, 0)), Some((1920, 1080))));
            }
            Ok(streams)
        }

        fn hold_physical(&mut self, on: bool) {
            let mut w = self.0.lock().unwrap();
            w.done.push(format!("hold physical {}", if on { "on" } else { "off" }));
            w.held = on;
        }
    }

    fn recording(id: &str, position: Option<(i32, i32)>, size: Option<(i32, i32)>) -> CastStream {
        CastStream {
            path: format!("/org/gnome/Mutter/ScreenCast/Stream/{}", id),
            position,
            size,
        }
    }

    struct FakeMonitor {
        world: Shared,
        connector: String,
    }

    impl Monitor for FakeMonitor {
        fn show_as(&mut self, connector: &str) {
            assert_eq!(connector, self.connector);
            self.world
                .lock()
                .unwrap()
                .done
                .push(format!("show {}", connector));
        }

        fn resize(&self, width: i32, height: i32) -> ResultType<bool> {
            let mut w = self.world.lock().unwrap();
            w.done.push(format!("resize {}x{}", width, height));
            if w.resize_fails {
                set(&mut w, &self.connector, VIRTUAL_PRODUCT, (width, height));
                bail!("No first frame of {}x{}", width, height);
            }
            // The size the stream settles last is the one Mutter takes.
            let connector = self.connector.clone();
            w.coming.retain(|c| c.1 != connector);
            let delay = w.delay;
            w.coming
                .push((delay, connector, VIRTUAL_PRODUCT, (width, height)));
            Ok(true)
        }

        fn is_gone(&self) -> bool {
            self.world.lock().unwrap().gone.contains(&self.connector)
        }
    }

    impl Drop for FakeMonitor {
        fn drop(&mut self) {
            let mut w = self.world.lock().unwrap();
            w.done.push(format!("drop {}", self.connector));
            let connector = self.connector.clone();
            w.coming.retain(|c| c.1 != connector);
            if w.leave_after > 0 {
                let after = w.leave_after;
                w.leaving.push((after, connector));
            } else if !w.sticky {
                w.listed.retain(|c| c.0 != connector);
                side_by_side(&mut w);
            }
        }
    }

    struct FakeKeeper(Shared);

    impl FakeKeeper {
        // A call, on a connection made anew should the last have gone, as `RootLink` makes it.
        fn call(&self) -> ResultType<std::sync::MutexGuard<'_, World>> {
            let mut w = self.0.lock().unwrap();
            if w.no_root {
                w.link_dropped = true;
                bail!("No root service");
            }
            if w.link_dropped || w.generation == 0 {
                w.link_dropped = false;
                w.generation += 1;
            }
            Ok(w)
        }
    }

    impl Keeper for FakeKeeper {
        fn keep(&mut self, on: bool) -> ResultType<()> {
            self.call()?.done.push(format!("keep {}", on));
            Ok(())
        }

        fn generation(&self) -> u64 {
            self.0.lock().unwrap().generation
        }

        fn lock_input(&mut self) -> ResultType<()> {
            let mut w = self.call()?;
            if w.taken_back || w.lock_refused {
                w.link_dropped = true;
                bail!(if w.taken_back { TAKEN_BACK } else { "Refused" });
            }
            w.done.push("lock input".to_owned());
            Ok(())
        }

        fn taken_back(&mut self) -> ResultType<bool> {
            Ok(self.call()?.taken_back)
        }

        fn restored(&mut self) -> ResultType<()> {
            let mut w = self.call()?;
            if w.restore_fails > 0 {
                w.restore_fails -= 1;
                w.link_dropped = true;
                bail!("Timed out");
            }
            w.taken_back = false;
            w.done.push("restored".to_owned());
            Ok(())
        }
    }

    fn backend(w: &Shared) -> GnomeBackend<FakeMutter, FakeKeeper> {
        GnomeBackend::new(FakeMutter(w.clone(), Instant::now()), FakeKeeper(w.clone()))
    }

    fn spec(width: u32, height: u32) -> Spec {
        Spec {
            width,
            height,
            scale: 1.0,
            mode: Mode::Shared,
        }
    }

    fn plug_in(
        b: &mut GnomeBackend<FakeMutter, FakeKeeper>,
        w: &Shared,
        size: (u32, u32),
    ) -> ResultType<String> {
        let w = w.clone();
        b.plug_in(&spec(size.0, size.1), &mut |name| {
            w.lock().unwrap().done.push(format!("announce {}", name))
        })
    }

    #[test]
    fn a_monitor_is_kept_first_and_named_as_mutter_lists_it() {
        let w = world();
        w.lock().unwrap().delay = 3;
        let mut b = backend(&w);
        assert_eq!(plug_in(&mut b, &w, (2560, 1440)).unwrap(), "Meta-0");
        assert_eq!(
            done(&w),
            [
                "keep true",
                "relayout",
                "create 2560x1440",
                "announce Meta-0",
                "show Meta-0"
            ]
        );
        // Kept already.
        assert_eq!(plug_in(&mut b, &w, (1920, 1080)).unwrap(), "Meta-1");
        assert_eq!(
            done(&w),
            [
                "relayout",
                "create 1920x1080",
                "announce Meta-1",
                "show Meta-1"
            ]
        );
    }

    #[test]
    fn a_monitor_not_listed_as_one_is_given_up() {
        let w = world();
        w.lock().unwrap().delay = 1000;
        let mut b = backend(&w);
        assert!(plug_in(&mut b, &w, (2560, 1440)).is_err());
        assert_eq!(
            done(&w),
            [
                "keep true",
                "relayout",
                "create 2560x1440",
                "drop Meta-0",
                "keep false"
            ]
        );
        // Another made at the same moment, of the same size: not told apart.
        {
            let mut w = w.lock().unwrap();
            w.delay = 1;
            w.twin = true;
        }
        assert!(plug_in(&mut b, &w, (2560, 1440)).is_err());
        assert_eq!(
            done(&w),
            [
                "keep true",
                "relayout",
                "create 2560x1440",
                "drop Meta-1",
                "keep false"
            ]
        );
        assert!(b.ours.is_empty());
        // Mutter slow to answer: given up on in time all the same, not after so many looks.
        {
            let mut w = w.lock().unwrap();
            w.twin = false;
            w.delay = 1000;
            w.slow = Duration::from_secs(2);
            w.looks = 0;
        }
        assert!(plug_in(&mut b, &w, (2560, 1440)).is_err());
        // The one before it made the monitor, then three within the 5 seconds.
        assert_eq!(w.lock().unwrap().looks, 4);
    }

    #[test]
    fn a_resize_waits_for_mutter_and_the_same_size_is_left_alone() {
        let w = world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        done(&w);
        b.set_mode("Meta-0", &spec(1600, 900)).unwrap();
        assert!(done(&w).is_empty());
        // Resized by its capturer, built again a few looks later.
        w.lock().unwrap().delay = 30;
        b.set_mode("Meta-0", &spec(1280, 720)).unwrap();
        assert_eq!(done(&w), ["relayout", "resize 1280x720"]);
        w.lock().unwrap().delay = 1000;
        assert!(b.set_mode("Meta-0", &spec(1920, 1080)).is_err());
        assert!(b.set_mode("Meta-7", &spec(1920, 1080)).is_err());
        // Not resized as asked: asked again, it is tried again.
        done(&w);
        w.lock().unwrap().delay = 1;
        b.set_mode("Meta-0", &spec(1920, 1080)).unwrap();
        assert_eq!(done(&w), ["relayout", "resize 1920x1080"]);
        // Never anything but an extra screen of the shared desktop.
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        assert_eq!(b.usage("Meta-7"), None);
    }

    #[test]
    fn the_last_plugged_out_lets_the_server_go() {
        let w = world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        plug_in(&mut b, &w, (1280, 720)).unwrap();
        done(&w);
        b.plug_out("Meta-0").unwrap();
        assert_eq!(done(&w), ["relayout", "drop Meta-0"]);
        // Gone counts as removed.
        b.plug_out("Meta-0").unwrap();
        assert!(done(&w).is_empty());
        // One Mutter still lists, its session stopped, is gone for us all the same.
        w.lock().unwrap().sticky = true;
        b.plug_out("Meta-1").unwrap();
        assert_eq!(done(&w), ["relayout", "drop Meta-1", "keep false"]);
        b.plug_out("Meta-1").unwrap();
        assert!(done(&w).is_empty());
    }

    #[test]
    fn monitors_mutter_took_back_are_told_gone() {
        let w = world();
        let mut b = backend(&w);
        assert!(b.maintain().is_empty());
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        plug_in(&mut b, &w, (1280, 720)).unwrap();
        done(&w);
        assert!(b.maintain().is_empty());
        // Its session closed.
        w.lock().unwrap().gone.push("Meta-0".to_owned());
        assert_eq!(b.maintain(), ["Meta-0"]);
        assert_eq!(done(&w), ["drop Meta-0"]);
        // No longer listed: Mutter restarted, say.
        w.lock().unwrap().listed.retain(|c| c.0 != "Meta-1");
        assert_eq!(b.maintain(), ["Meta-1"]);
        assert_eq!(done(&w), ["drop Meta-1", "keep false"]);
        assert!(b.maintain().is_empty());
    }

    #[test]
    fn without_the_root_service_monitors_work_all_the_same() {
        let w = world();
        w.lock().unwrap().no_root = true;
        let mut b = backend(&w);
        assert_eq!(plug_in(&mut b, &w, (1600, 900)).unwrap(), "Meta-0");
        assert_eq!(
            done(&w),
            [
                "relayout",
                "create 1600x900",
                "announce Meta-0",
                "show Meta-0"
            ]
        );
        assert!(b.maintain().is_empty());
        // It came up meanwhile: asked again.
        w.lock().unwrap().no_root = false;
        assert!(b.maintain().is_empty());
        assert_eq!(done(&w), ["keep true"]);
        b.plug_out("Meta-0").unwrap();
        assert_eq!(done(&w), ["relayout", "drop Meta-0", "keep false"]);
    }

    #[test]
    fn the_others_are_put_back_after_every_change_of_ours() {
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (2560, 1440)).unwrap();
        assert_eq!(
            done(&w),
            [
                "keep true",
                "relayout",
                "create 2560x1440",
                "announce Meta-0",
                "show Meta-0",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100"
            ]
        );
        b.set_mode("Meta-0", &spec(1280, 720)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "resize 1280x720",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100"
            ]
        );
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "create 1600x900",
                "announce Meta-1",
                "show Meta-1",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100 Meta-1@4480,100"
            ]
        );
        b.plug_out("Meta-0").unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "drop Meta-0",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-1@3200,100"
            ]
        );
        // The last: Mutter finds its stored layout again by itself.
        b.plug_out("Meta-1").unwrap();
        assert_eq!(done(&w), ["relayout", "drop Meta-1", "keep false"]);
        assert!(b.remembered.is_none());
    }

    #[test]
    fn a_layout_someone_else_made_is_taken_not_undone() {
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        done(&w);
        // Right after, someone puts DP-1 above HDMI-1 in Settings: ours having come to an end
        // with the monitors as wanted, that is not Mutter's doing.
        w.lock().unwrap().logical = vec![
            ("HDMI-1".to_owned(), 0, 1024, true),
            ("DP-1".to_owned(), 0, 0, false),
            ("Meta-0".to_owned(), 1920, 1024, false),
        ];
        // Mutter lays them out side by side again for a resize: put back as that someone left
        // them.
        b.set_mode("Meta-0", &spec(1280, 720)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "resize 1280x720",
                "relayout",
                "apply DP-1@0,0 HDMI-1@0,1024* Meta-0@1920,1024"
            ]
        );
        // A resize Mutter is slow to: given up on, then done by Mutter after all, side by side.
        w.lock().unwrap().delay = 1000;
        assert!(b.set_mode("Meta-0", &spec(1920, 1080)).is_err());
        {
            let mut w = w.lock().unwrap();
            w.coming.clear();
            w.delay = 1;
            set(&mut w, "Meta-0", VIRTUAL_PRODUCT, (1920, 1080));
        }
        done(&w);
        // Ours not having come to an end, that is taken for Mutter's doing for a while.
        b.set_mode("Meta-0", &spec(1920, 1080)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "resize 1920x1080",
                "relayout",
                "apply DP-1@0,0 HDMI-1@0,1024* Meta-0@1920,1024"
            ]
        );
    }

    #[test]
    fn a_layout_someone_else_made_right_after_one_left_as_wanted_is_taken() {
        // Laid out side by side already, ours to the right of them: nothing to put back.
        let w = world();
        set(&mut w.lock().unwrap(), "DP-1", "LG", (1280, 1024));
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        assert!(!done(&w).iter().any(|e| e.starts_with("apply")));
        // Right after, someone puts DP-1 above HDMI-1.
        w.lock().unwrap().logical = vec![
            ("HDMI-1".to_owned(), 0, 1024, true),
            ("DP-1".to_owned(), 0, 0, false),
            ("Meta-0".to_owned(), 1920, 1024, false),
        ];
        b.set_mode("Meta-0", &spec(1280, 720)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "resize 1280x720",
                "relayout",
                "apply DP-1@0,0 HDMI-1@0,1024* Meta-0@1920,1024"
            ]
        );
    }

    #[test]
    fn a_resize_that_failed_is_put_back_all_the_same() {
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        done(&w);
        // Mutter resized it, its capturer failing to after; taken back to the size Mutter
        // confirmed.
        w.lock().unwrap().resize_fails = true;
        assert!(b.set_mode("Meta-0", &spec(1280, 720)).is_err());
        assert_eq!(
            done(&w),
            [
                "relayout",
                "resize 1280x720",
                "resize 1600x900",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100"
            ]
        );
    }

    #[test]
    fn the_rest_are_put_back_once_mutter_took_one_gone_back() {
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        plug_in(&mut b, &w, (1280, 720)).unwrap();
        done(&w);
        // Its session closed, Mutter listing it a moment longer.
        {
            let mut w = w.lock().unwrap();
            w.gone.push("Meta-0".to_owned());
            w.leave_after = 3;
        }
        assert_eq!(b.maintain(), ["Meta-0"]);
        assert_eq!(
            done(&w),
            [
                "drop Meta-0",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-1@3200,100"
            ]
        );
    }

    // Two of ours, then someone puts DP-1 above HDMI-1 in Settings.
    fn moved_by_someone(w: &Shared) -> GnomeBackend<FakeMutter, FakeKeeper> {
        let mut b = backend(w);
        plug_in(&mut b, w, (1600, 900)).unwrap();
        plug_in(&mut b, w, (1280, 720)).unwrap();
        done(w);
        w.lock().unwrap().logical = vec![
            ("HDMI-1".to_owned(), 0, 1024, true),
            ("DP-1".to_owned(), 0, 0, false),
            ("Meta-0".to_owned(), 1920, 1024, false),
            ("Meta-1".to_owned(), 3520, 1024, false),
        ];
        b
    }

    #[test]
    fn a_layout_someone_else_made_is_kept_when_one_of_ours_goes() {
        // Mutter still listing the one gone when it is found so.
        let w = stored_world();
        let mut b = moved_by_someone(&w);
        {
            let mut w = w.lock().unwrap();
            w.gone.push("Meta-0".to_owned());
            w.leave_after = 3;
        }
        assert_eq!(b.maintain(), ["Meta-0"]);
        assert_eq!(
            done(&w),
            [
                "drop Meta-0",
                "relayout",
                "apply DP-1@0,0 HDMI-1@0,1024* Meta-1@1920,1024"
            ]
        );
        // Mutter having taken it back already, laying the rest out anew: as found at a look
        // before.
        let w = stored_world();
        let mut b = moved_by_someone(&w);
        assert!(b.maintain().is_empty());
        {
            let mut w = w.lock().unwrap();
            w.listed.retain(|c| c.0 != "Meta-0");
            side_by_side(&mut w);
        }
        assert_eq!(b.maintain(), ["Meta-0"]);
        assert_eq!(
            done(&w),
            [
                "drop Meta-0",
                "relayout",
                "apply DP-1@0,0 HDMI-1@0,1024* Meta-1@1920,1024"
            ]
        );
    }

    #[test]
    fn mutter_laying_out_anew_for_one_of_ours_gone_is_not_taken_for_anyone_else() {
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        plug_in(&mut b, &w, (1280, 720)).unwrap();
        // Mutter took Meta-0 back and laid the rest out side by side, a resize of Meta-1 asked
        // before that is found.
        {
            let mut w = w.lock().unwrap();
            w.listed.retain(|c| c.0 != "Meta-0");
            side_by_side(&mut w);
        }
        done(&w);
        b.set_mode("Meta-1", &spec(1024, 768)).unwrap();
        assert!(!done(&w).iter().any(|e| e.starts_with("apply")));
        assert_eq!(b.maintain(), ["Meta-0"]);
        assert_eq!(
            done(&w),
            [
                "drop Meta-0",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-1@3200,100"
            ]
        );
    }

    #[test]
    fn a_resize_given_up_on_is_undone_and_its_late_doing_put_back() {
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        done(&w);
        w.lock().unwrap().delay = 1000;
        assert!(b.set_mode("Meta-0", &spec(1280, 720)).is_err());
        // The stream back at the size Mutter confirmed.
        assert_eq!(done(&w), ["relayout", "resize 1280x720", "resize 1600x900"]);
        // Mutter took the size given up on after all, well after: a doing of ours all the same,
        // put back.
        {
            let mut w = w.lock().unwrap();
            w.coming.clear();
            w.clock += OURS_FOR;
            set(&mut w, "Meta-0", VIRTUAL_PRODUCT, (1280, 720));
        }
        assert!(b.maintain().is_empty());
        assert_eq!(
            done(&w),
            [
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100"
            ]
        );
        // Then the one it was taken back to.
        set(
            &mut w.lock().unwrap(),
            "Meta-0",
            VIRTUAL_PRODUCT,
            (1600, 900),
        );
        assert!(b.maintain().is_empty());
        assert_eq!(
            done(&w),
            [
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100"
            ]
        );
        // As asked first: nothing to do.
        b.set_mode("Meta-0", &spec(1600, 900)).unwrap();
        assert!(done(&w).is_empty());
    }

    #[test]
    fn mutter_laying_out_anew_a_moment_after_ours_is_put_back_on_a_look() {
        let w = stored_world();
        let mut b = backend(&w);
        w.lock().unwrap().apply_fails = 3;
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        done(&w);
        // Not put back that time; a look within the moment puts them back.
        assert!(b.maintain().is_empty());
        assert_eq!(
            done(&w),
            [
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100"
            ]
        );
        // After the moment, a layout found is taken.
        {
            let mut w = w.lock().unwrap();
            w.clock += OURS_FOR;
            w.logical = vec![
                ("HDMI-1".to_owned(), 0, 1024, true),
                ("DP-1".to_owned(), 0, 0, false),
                ("Meta-0".to_owned(), 1920, 1024, false),
            ];
        }
        assert!(b.maintain().is_empty());
        assert!(done(&w).is_empty());
        b.set_mode("Meta-0", &spec(1280, 720)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "resize 1280x720",
                "relayout",
                "apply DP-1@0,0 HDMI-1@0,1024* Meta-0@1920,1024"
            ]
        );
    }

    #[test]
    fn put_back_only_where_mutter_takes_it_again_on_a_stale_serial() {
        let w = stored_world();
        w.lock().unwrap().apply_fails = 2;
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        assert_eq!(
            done(&w)[5..],
            [
                "relayout",
                "apply failed",
                "relayout",
                "apply failed",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100"
            ]
        );
        // Three tries at most.
        w.lock().unwrap().apply_fails = 5;
        b.set_mode("Meta-0", &spec(1280, 720)).unwrap();
        let tries = done(&w).iter().filter(|e| e.starts_with("apply")).count();
        assert_eq!(tries, APPLY_TRIES);
        // Left side by side by Mutter that time: not taken for anyone's layout, put back next.
        w.lock().unwrap().apply_fails = 0;
        b.set_mode("Meta-0", &spec(1920, 1080)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "resize 1920x1080",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100"
            ]
        );
        // Mutter taking no configurations: nothing tried.
        {
            let mut w = w.lock().unwrap();
            w.apply_fails = 0;
            w.no_apply = true;
        }
        b.set_mode("Meta-0", &spec(1600, 900)).unwrap();
        assert_eq!(done(&w), ["relayout", "resize 1600x900"]);
    }

    fn privacy(width: u32, height: u32) -> Spec {
        Spec {
            mode: Mode::Privacy,
            ..spec(width, height)
        }
    }

    // As the manager asks for one: an extra screen, then the desktop taken over.
    fn plug_in_privacy(
        b: &mut GnomeBackend<FakeMutter, FakeKeeper>,
        w: &Shared,
        size: (u32, u32),
    ) -> ResultType<String> {
        let name = plug_in(b, w, size)?;
        b.set_mode(&name, &privacy(size.0, size.1))?;
        Ok(name)
    }

    // Meta-0, 2560x1440, has the desktop to itself.
    fn taking_it_over(w: &Shared) -> GnomeBackend<FakeMutter, FakeKeeper> {
        let mut b = backend(w);
        plug_in_privacy(&mut b, w, (2560, 1440)).unwrap();
        assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        done(w);
        b
    }

    const GIVEN_BACK: [&str; 4] = [
        "relayout",
        "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100",
        "restored",
        "hold physical off",
    ];

    #[test]
    fn the_desktop_is_taken_over_by_ours_alone_and_given_back_as_it_was() {
        let w = stored_world();
        let mut b = backend(&w);
        assert_eq!(b.caps().modes, [Mode::Shared, Mode::Privacy]);
        assert!(!b.watchful());
        assert_eq!(plug_in_privacy(&mut b, &w, (2560, 1440)).unwrap(), "Meta-0");
        assert_eq!(
            done(&w),
            [
                "keep true",
                "relayout",
                "create 2560x1440",
                "announce Meta-0",
                "show Meta-0",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100",
                // The recording of the physical monitors stopped before the input is locked.
                "hold physical on",
                "lock input",
                // Each moved first, a pixel lower than ours, then off.
                "relayout",
                "apply Meta-0@0,0* DP-1@2560,1 HDMI-1@3840,1",
                "relayout",
                "apply Meta-0@0,0*",
            ]
        );
        assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        assert!(b.watchful());
        // Kept so.
        assert!(b.maintain().is_empty());
        assert!(done(&w).is_empty());
        // Given back as it was, ours to the right; the input let go once the others are on,
        // the physical monitors recorded again last.
        b.set_mode("Meta-0", &spec(2560, 1440)).unwrap();
        assert_eq!(done(&w), GIVEN_BACK);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        assert!(!b.watchful());
    }

    #[test]
    fn the_last_display_taking_the_desktop_over_gives_it_back_before_it_goes() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        b.plug_out("Meta-0").unwrap();
        let mut expected = GIVEN_BACK.to_vec();
        expected.extend(["relayout", "drop Meta-0", "keep false"]);
        assert_eq!(done(&w), expected);
        // Not given back: kept, the desktop taken over still.
        let w = stored_world();
        let mut b = taking_it_over(&w);
        w.lock().unwrap().apply_fails = 1000;
        assert!(b.plug_out("Meta-0").is_err());
        assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        assert!(!done(&w)
            .iter()
            .any(|e| e == "restored" || e.starts_with("drop") || e.starts_with("hold")));
        // Given back on the next look Mutter takes it.
        w.lock().unwrap().apply_fails = 0;
        assert!(b.maintain().is_empty());
        assert_eq!(done(&w), GIVEN_BACK);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
    }

    #[test]
    fn the_desktop_is_not_taken_over_while_another_program_records_it() {
        let elsewhere = [
            // A monitor; an area or a window.
            recording("u7", Some((0, 0)), Some((1920, 1080))),
            recording("u7", None, Some((800, 600))),
        ];
        for stream in elsewhere {
            let w = world();
            w.lock().unwrap().streams.push(stream);
            let mut b = backend(&w);
            let e = plug_in_privacy(&mut b, &w, (1600, 900)).unwrap_err();
            assert_eq!(e.to_string(), RECORDED_ELSEWHERE);
            // An extra screen still.
            assert_eq!(done(&w)[5..], ["hold physical on", "hold physical off"]);
            assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        }
        // Mutter not telling: taken to.
        let w = world();
        w.lock().unwrap().streams_fail = true;
        let mut b = backend(&w);
        let e = plug_in_privacy(&mut b, &w, (1600, 900)).unwrap_err();
        assert_eq!(e.to_string(), RECORDED_ELSEWHERE);
    }

    #[test]
    fn a_monitor_recorded_again_where_it_was_moved_off_is_not_turned_off_under_it() {
        // Recorded anew once moved off, once: moved again, which closes that, then off.
        let w = world();
        w.lock().unwrap().rerecords = 1;
        let mut b = backend(&w);
        plug_in_privacy(&mut b, &w, (1600, 900)).unwrap();
        assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        let log = done(&w);
        let locked = log.iter().position(|e| e == "lock input").unwrap();
        assert_eq!(
            log[locked + 1..],
            [
                "relayout",
                "apply Meta-0@0,0* HDMI-1@1600,1",
                "relayout",
                "apply Meta-0@0,0* HDMI-1@1600,2",
                "relayout",
                "apply Meta-0@0,0*",
            ]
        );
        // Of two turned off, the second recorded anew: neither turned off till it is not.
        let w = world();
        {
            let mut w = w.lock().unwrap();
            w.listed.push(("DP-1".to_owned(), "LG", (1280, 1024)));
            side_by_side(&mut w);
            w.rerecords = 1;
            w.rerecorded = Some("DP-1");
        }
        let mut b = backend(&w);
        plug_in_privacy(&mut b, &w, (2560, 1440)).unwrap();
        let log = done(&w);
        let locked = log.iter().position(|e| e == "lock input").unwrap();
        assert_eq!(
            log[locked + 1..],
            [
                "relayout",
                "apply Meta-0@0,0* HDMI-1@2560,1 DP-1@4480,1",
                "relayout",
                "apply Meta-0@0,0* HDMI-1@2560,2 DP-1@4480,2",
                "relayout",
                "apply Meta-0@0,0*",
            ]
        );
        // Recorded anew every time, or Mutter not telling who records once it is moved off:
        // never turned off, the desktop not taken over, the monitor lit where it was.
        for recorded_anew in [true, false] {
            let w = world();
            {
                let mut w = w.lock().unwrap();
                if recorded_anew {
                    w.rerecords = 1000;
                } else {
                    w.streams_fail_after = Some(1);
                }
            }
            let mut b = backend(&w);
            let e = plug_in_privacy(&mut b, &w, (1600, 900)).unwrap_err();
            assert_eq!(e.to_string(), NOT_TAKEN_OVER);
            let log = done(&w);
            assert!(!log.iter().any(|e| e == "apply Meta-0@0,0*"), "{:?}", log);
            assert!(log.iter().any(|e| e.starts_with("apply HDMI-1@0,0*")), "{:?}", log);
            assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        }
    }

    #[test]
    fn ours_of_the_physical_monitors_are_held_while_a_monitor_is_turned_off_again() {
        // DP-1 off, lit by Mutter with ours: turned off again, ours of the physical monitors not
        // recorded meanwhile, else taken for someone recording it again where it was moved off.
        let dp_1_off = || {
            let w = world();
            {
                let mut w = w.lock().unwrap();
                w.listed.push(("DP-1".to_owned(), "LG", (1280, 1024)));
                w.ours_rerecord = true;
            }
            w
        };
        let w = dp_1_off();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        let log = done(&w);
        let held = log.iter().position(|e| e == "hold physical on").unwrap();
        assert_eq!(
            log[held..],
            [
                "hold physical on",
                "relayout",
                "apply HDMI-1@0,0* Meta-0@1920,0 DP-1@3520,1",
                "relayout",
                "apply HDMI-1@0,0* Meta-0@1920,0",
                "hold physical off",
            ]
        );
        // Let go however it ends: the move refused, Mutter not answering after it, or not telling
        // who records.
        for fail in 0..3 {
            let w = dp_1_off();
            {
                let mut w = w.lock().unwrap();
                match fail {
                    0 => w.apply_fails = 1,
                    1 => w.blind_after = Some(1),
                    _ => w.streams_fail_after = Some(1),
                }
            }
            let mut b = backend(&w);
            plug_in(&mut b, &w, (1600, 900)).unwrap();
            let holds: Vec<String> = done(&w)
                .into_iter()
                .filter(|e| e.starts_with("hold"))
                .collect();
            assert!(!holds.is_empty());
            assert!(
                holds.chunks(2).all(|h| h == ["hold physical on", "hold physical off"]),
                "{}: {:?}",
                fail,
                holds
            );
            assert!(!w.lock().unwrap().held);
        }
        // Someone else recording it anew where moved off, once: let go, moved again, then off.
        let w = dp_1_off();
        {
            let mut w = w.lock().unwrap();
            w.rerecords = 1;
            w.rerecorded = Some("DP-1");
        }
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        let log = done(&w);
        let held = log.iter().position(|e| e == "hold physical on").unwrap();
        assert_eq!(
            log[held..],
            [
                "hold physical on",
                "relayout",
                "apply HDMI-1@0,0* Meta-0@1920,0 DP-1@3520,1",
                "hold physical off",
                "hold physical on",
                "relayout",
                "apply HDMI-1@0,0* Meta-0@1920,0 DP-1@3520,2",
                "relayout",
                "apply HDMI-1@0,0* Meta-0@1920,0",
                "hold physical off",
            ]
        );
        // Someone else recording a monitor kept on, or an area: no reason to wait.
        let w = dp_1_off();
        w.lock().unwrap().streams = vec![
            recording("u7", Some((0, 0)), Some((1920, 1080))),
            recording("u8", None, Some((800, 600))),
        ];
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        let log = done(&w);
        assert_eq!(log.iter().filter(|e| *e == "hold physical on").count(), 1);
        assert!(
            log.iter().any(|e| e == "apply HDMI-1@0,0* Meta-0@1920,0"),
            "{:?}",
            log
        );
        // Not while the desktop is taken over: held already, and kept so.
        let w = world();
        let mut b = taking_it_over(&w);
        w.lock()
            .unwrap()
            .listed
            .push(("DP-1".to_owned(), "LG", (1280, 1024)));
        side_by_side(&mut w.lock().unwrap());
        assert!(b.maintain().is_empty());
        let log = done(&w);
        assert!(!log.iter().any(|e| e.starts_with("hold")), "{:?}", log);
        assert!(log.iter().any(|e| e == "apply Meta-0@0,0*"), "{:?}", log);
    }

    #[test]
    fn the_desktop_is_taken_over_once_our_own_recordings_of_it_are_gone() {
        // Ours of the physical monitors gone a few looks after they are stopped; someone else's
        // virtual monitor shows nothing of the desktop.
        let w = world();
        {
            let mut w = w.lock().unwrap();
            w.own_streams = 3;
            w.streams.push(recording("u9", None, None));
        }
        let mut b = backend(&w);
        plug_in_privacy(&mut b, &w, (1600, 900)).unwrap();
        assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        // Never gone: given up on in time.
        let w = world();
        w.lock().unwrap().own_streams = 1000;
        let mut b = backend(&w);
        let started = w.lock().unwrap().clock;
        let e = plug_in_privacy(&mut b, &w, (1600, 900)).unwrap_err();
        assert_eq!(e.to_string(), RECORDED_ELSEWHERE);
        let took = w.lock().unwrap().clock - started;
        assert!(took >= STREAMS_GONE && took < STREAMS_GONE + Duration::from_secs(1));
    }

    #[test]
    fn the_desktop_is_not_taken_over_without_the_input_locked() {
        // No root service: the monitor works all the same, not the desktop to itself.
        let w = world();
        w.lock().unwrap().no_root = true;
        let mut b = backend(&w);
        let e = plug_in_privacy(&mut b, &w, (1600, 900)).unwrap_err();
        assert_eq!(e.to_string(), NO_INPUT_LOCK);
        assert_eq!(done(&w)[4..], ["hold physical on", "hold physical off"]);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        // Refused: the hold, gone with the connection, asked again.
        let w = world();
        w.lock().unwrap().lock_refused = true;
        let mut b = backend(&w);
        let e = plug_in_privacy(&mut b, &w, (1600, 900)).unwrap_err();
        assert_eq!(e.to_string(), NO_INPUT_LOCK);
        assert_eq!(
            done(&w)[5..],
            ["hold physical on", "keep true", "hold physical off"]
        );
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        // Taken back a moment ago: nothing stopped.
        let w = world();
        w.lock().unwrap().taken_back = true;
        let mut b = backend(&w);
        let e = plug_in_privacy(&mut b, &w, (1600, 900)).unwrap_err();
        assert_eq!(e.to_string(), TAKEN_BACK);
        assert!(done(&w)[5..].is_empty());
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        // Mutter taking no configurations: not offered.
        let w = world();
        w.lock().unwrap().no_apply = true;
        let mut b = backend(&w);
        assert_eq!(b.caps().modes, [Mode::Shared]);
        let e = plug_in_privacy(&mut b, &w, (1600, 900)).unwrap_err();
        assert_eq!(e.to_string(), NOT_CONFIGURABLE);
    }

    #[test]
    fn a_desktop_mutter_would_not_take_over_is_left_as_it_was() {
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        done(&w);
        w.lock().unwrap().apply_fails = 1000;
        let e = b.set_mode("Meta-0", &privacy(1600, 900)).unwrap_err();
        assert_eq!(e.to_string(), NOT_TAKEN_OVER);
        let mut expected = vec!["hold physical on", "lock input"];
        for _ in 0..APPLY_TRIES {
            expected.extend(["relayout", "apply failed"]);
        }
        // Still on as they were: nothing to light.
        expected.extend(["restored", "hold physical off"]);
        assert_eq!(done(&w), expected);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        assert!(!b.watchful());
    }

    #[test]
    fn ctrl_p_gives_the_desktop_back_with_its_displays_as_extra_screens() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        w.lock().unwrap().taken_back = true;
        assert!(b.maintain().is_empty());
        assert_eq!(done(&w), GIVEN_BACK);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        // The root service missing the word the first time: told again, on a new connection.
        let w = stored_world();
        let mut b = taking_it_over(&w);
        {
            let mut w = w.lock().unwrap();
            w.taken_back = true;
            w.restore_fails = 1;
        }
        assert!(b.maintain().is_empty());
        assert_eq!(done(&w), GIVEN_BACK);
        assert!(!b.unrestored);
        // Taken back right before the session asks for it again: given back, not taken again.
        let w = stored_world();
        let mut b = taking_it_over(&w);
        w.lock().unwrap().taken_back = true;
        let e = b.set_mode("Meta-0", &privacy(2560, 1440)).unwrap_err();
        assert_eq!(e.to_string(), TAKEN_BACK);
        assert_eq!(done(&w), GIVEN_BACK);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
    }

    #[test]
    fn a_desktop_laid_out_anew_is_taken_over_again_or_given_back() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        // A monitor plugged in, Mutter lighting them all side by side.
        set(&mut w.lock().unwrap(), "DP-2", "AOC", (1280, 720));
        assert!(b.maintain().is_empty());
        assert_eq!(
            done(&w),
            [
                "relayout",
                "apply Meta-0@0,0* HDMI-1@2560,1 DP-1@4480,1 DP-2@5760,1",
                "relayout",
                "apply Meta-0@0,0*",
            ]
        );
        // Mutter refusing it for so many looks in a row: given back, the newcomer on too.
        {
            let mut w = w.lock().unwrap();
            side_by_side(&mut w);
            w.apply_fails = 1000;
        }
        for _ in 0..TAKEOVER_TRIES {
            assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
            assert!(b.maintain().is_empty());
        }
        let log = done(&w);
        assert_eq!(log[log.len() - 2..], GIVEN_BACK[2..]);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
    }

    #[test]
    fn a_size_asked_for_while_the_desktop_is_taken_over_is_taken_once_given_back() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        b.set_mode("Meta-0", &privacy(1920, 1080)).unwrap();
        assert!(b.maintain().is_empty());
        assert!(done(&w).is_empty());
        b.set_mode("Meta-0", &spec(1920, 1080)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100",
                "restored",
                "relayout",
                "resize 1920x1080",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100",
                "hold physical off",
            ]
        );
        assert!(b.ours[0].asked.is_none());
        // One Mutter will not take is tried so many times, then given up on.
        let w = stored_world();
        let mut b = taking_it_over(&w);
        b.set_mode("Meta-0", &privacy(1920, 1080)).unwrap();
        w.lock().unwrap().delay = 1000;
        b.set_mode("Meta-0", &spec(1920, 1080)).unwrap();
        for _ in 0..RESIZE_TRIES {
            b.maintain();
        }
        let tries = done(&w).iter().filter(|e| *e == "resize 1920x1080").count();
        assert_eq!(tries, RESIZE_TRIES as usize);
        assert!(b.ours[0].asked.is_none());
    }

    #[test]
    fn a_second_display_joins_the_desktop_taken_over() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        assert_eq!(plug_in(&mut b, &w, (1280, 720)).unwrap(), "Meta-1");
        assert_eq!(
            done(&w),
            [
                "relayout",
                "create 1280x720",
                "announce Meta-1",
                "show Meta-1",
                // Mutter lit them all for it: off again, ours side by side.
                "relayout",
                "apply Meta-0@0,0* Meta-1@2560,0 HDMI-1@3840,1 DP-1@5760,1",
                "relayout",
                "apply Meta-0@0,0* Meta-1@2560,0",
            ]
        );
        assert_eq!(b.usage("Meta-1"), Some(Mode::Shared));
        // Meta-1 taking it over too, then Meta-0 no longer: still taken over.
        b.set_mode("Meta-1", &privacy(1280, 720)).unwrap();
        b.set_mode("Meta-0", &spec(2560, 1440)).unwrap();
        assert!(done(&w).is_empty());
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        assert!(b.watchful());
        // Meta-0 goes, Mutter lighting the others for it: the rest keep the desktop.
        b.plug_out("Meta-0").unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "drop Meta-0",
                "relayout",
                "apply Meta-1@0,0* HDMI-1@1280,1 DP-1@3200,1",
                "relayout",
                "apply Meta-1@0,0*",
            ]
        );
        b.plug_out("Meta-1").unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-1@3200,100",
                "restored",
                "hold physical off",
                "relayout",
                "drop Meta-1",
                "keep false",
            ]
        );
    }

    #[test]
    fn the_screen_locked_while_the_desktop_is_taken_over_gives_the_input_back_at_once() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        // Mutter closed its session.
        w.lock().unwrap().gone.push("Meta-0".to_owned());
        assert_eq!(b.maintain(), ["Meta-0"]);
        assert_eq!(
            done(&w),
            [
                "restored",
                "drop Meta-0",
                // Mutter lit the others side by side as it took it back: as they were.
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100*",
                "hold physical off",
                "keep false",
            ]
        );
        assert!(!b.watchful());
        // Mutter finds its stored layout again by itself.
        assert!(b.remembered.is_none());
    }

    #[test]
    fn the_input_is_locked_again_on_a_connection_to_the_root_service_opened_anew() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        // The service restarted: what it was asked went with the old connection.
        w.lock().unwrap().generation += 1;
        assert!(b.maintain().is_empty());
        assert_eq!(done(&w), ["keep true", "lock input"]);
        assert!(b.maintain().is_empty());
        assert!(done(&w).is_empty());
        // Refusing then: given back.
        {
            let mut w = w.lock().unwrap();
            w.generation += 1;
            w.lock_refused = true;
        }
        assert!(b.maintain().is_empty());
        let mut expected = vec!["keep true", "keep true"];
        expected.extend(GIVEN_BACK);
        assert_eq!(done(&w), expected);
        // Gone: given back, its lock gone with it.
        let w = stored_world();
        let mut b = taking_it_over(&w);
        w.lock().unwrap().no_root = true;
        assert!(b.maintain().is_empty());
        assert_eq!(
            done(&w),
            [GIVEN_BACK[0], GIVEN_BACK[1], GIVEN_BACK[3]]
        );
        // Back: told the desktop is given back, which it missed, and kept again.
        w.lock().unwrap().no_root = false;
        assert!(b.maintain().is_empty());
        assert_eq!(done(&w), ["restored", "keep true"]);
        assert!(b.maintain().is_empty());
        assert!(done(&w).is_empty());
    }

    #[test]
    fn a_desktop_that_would_not_be_given_back_is_let_go_with_its_displays() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        {
            let mut w = w.lock().unwrap();
            w.taken_back = true;
            w.apply_fails = 1000;
        }
        for _ in 0..GIVE_BACK_TRIES {
            assert!(b.maintain().is_empty());
            assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        }
        assert!(!done(&w).iter().any(|e| e == "restored"));
        w.lock().unwrap().apply_fails = 0;
        assert_eq!(b.maintain(), ["Meta-0"]);
        assert_eq!(
            done(&w),
            [
                "restored",
                "relayout",
                "drop Meta-0",
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100*",
                "hold physical off",
                "keep false",
            ]
        );
        assert!(!b.watchful());
    }

    #[test]
    fn the_physical_monitors_are_recorded_again_should_the_backend_go() {
        let w = stored_world();
        drop(taking_it_over(&w));
        assert_eq!(done(&w), ["hold physical off", "drop Meta-0"]);
    }

    #[test]
    fn a_display_of_ours_turned_off_while_the_desktop_is_taken_over_is_turned_on_again() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        plug_in(&mut b, &w, (1280, 720)).unwrap();
        done(&w);
        w.lock().unwrap().logical.retain(|l| l.0 != "Meta-1");
        assert!(b.maintain().is_empty());
        assert_eq!(done(&w), ["relayout", "apply Meta-0@0,0* Meta-1@2560,0"]);
    }

    #[test]
    fn a_remembered_layout_mutter_would_refuse_is_not_given_back() {
        // DP-1 apart from HDMI-1, as no layout Mutter takes has it: the two side by side instead.
        let w = stored_world();
        w.lock().unwrap().logical = vec![
            ("HDMI-1".to_owned(), 5000, 0, true),
            ("DP-1".to_owned(), 0, 0, false),
        ];
        let mut b = taking_it_over(&w);
        b.set_mode("Meta-0", &spec(2560, 1440)).unwrap();
        assert_eq!(
            done(&w),
            [
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,0* Meta-0@3200,0",
                "restored",
                "hold physical off",
            ]
        );
    }

    #[test]
    fn a_desktop_whose_monitors_were_all_unplugged_is_given_back_all_the_same() {
        let w = world();
        let mut b = taking_it_over(&w);
        {
            let mut w = w.lock().unwrap();
            w.listed.retain(|c| c.0 != "HDMI-1");
            w.taken_back = true;
        }
        assert!(b.maintain().is_empty());
        // Nothing to light.
        assert_eq!(done(&w), ["restored", "hold physical off"]);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
    }

    #[test]
    fn a_desktop_not_lit_again_after_a_failed_takeover_stays_taken_over_to_be_given_back() {
        // Mutter took ours alone, then answered no more: the others may be dark.
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        done(&w);
        w.lock().unwrap().blind_after = Some(3);
        let e = b.set_mode("Meta-0", &privacy(1600, 900)).unwrap_err();
        assert_eq!(e.to_string(), NOT_TAKEN_OVER);
        assert!(!done(&w).iter().any(|e| e == "restored" || e.starts_with("hold physical off")));
        assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        assert!(b.watchful());
        // Given back on the next look.
        w.lock().unwrap().blind_after = None;
        assert!(b.maintain().is_empty());
        assert_eq!(
            done(&w),
            [
                "relayout",
                "apply DP-1@0,0 HDMI-1@1280,100* Meta-0@3200,100",
                "restored",
                "hold physical off"
            ]
        );
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
    }

    #[test]
    fn a_desktop_that_cannot_be_kept_taken_over_with_a_display_added_is_given_back() {
        let w = stored_world();
        let mut b = taking_it_over(&w);
        w.lock().unwrap().apply_fails = 1000;
        plug_in(&mut b, &w, (1280, 720)).unwrap();
        // Mutter lit them all for it, and takes nothing: on as it laid them out.
        let log = done(&w);
        assert_eq!(log[log.len() - 2..], GIVEN_BACK[2..]);
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        // Ctrl+P while one was made: given back once it is.
        let w = stored_world();
        let mut b = taking_it_over(&w);
        w.lock().unwrap().taken_back = true;
        plug_in(&mut b, &w, (1280, 720)).unwrap();
        assert_eq!(b.usage("Meta-0"), Some(Mode::Shared));
        assert!(done(&w).iter().any(|e| e == "restored"));
    }

    #[test]
    fn mutter_not_answering_once_is_asked_again() {
        // While taking the desktop over.
        let w = stored_world();
        let mut b = backend(&w);
        plug_in(&mut b, &w, (1600, 900)).unwrap();
        w.lock().unwrap().state_fails = 2;
        b.set_mode("Meta-0", &privacy(1600, 900)).unwrap();
        assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        // While giving it back.
        let w = stored_world();
        let mut b = taking_it_over(&w);
        {
            let mut w = w.lock().unwrap();
            w.taken_back = true;
            w.state_fails = 2;
        }
        assert!(b.maintain().is_empty());
        assert_eq!(done(&w), GIVEN_BACK);
    }

    #[test]
    fn the_desktop_is_not_taken_over_again_before_the_root_service_heard_it_was_given_back() {
        // Its word arriving after the input is locked anew would let it go.
        let w = stored_world();
        let mut b = taking_it_over(&w);
        w.lock().unwrap().restore_fails = 4;
        b.set_mode("Meta-0", &spec(2560, 1440)).unwrap();
        assert!(b.unrestored);
        done(&w);
        let e = b.set_mode("Meta-0", &privacy(2560, 1440)).unwrap_err();
        assert_eq!(e.to_string(), NO_INPUT_LOCK);
        assert!(!done(&w).iter().any(|e| e == "lock input"));
        // Heard: taken over.
        b.set_mode("Meta-0", &privacy(2560, 1440)).unwrap();
        assert_eq!(b.usage("Meta-0"), Some(Mode::Privacy));
        let log = done(&w);
        let at = |what: &str| log.iter().position(|e| e == what).unwrap();
        assert!(at("restored") < at("lock input"));
    }
}
