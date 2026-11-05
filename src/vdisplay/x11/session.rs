//! The session's side: a `Backend` that has the root service light a connector, then lays the
//! display out with RandR, to the right of the others and never primary, so that whoever sits at
//! the machine sees nothing of it: their outputs stay where they were, and primary.
//!
//! vkms's connector, and a physical one that carries real modes, run a mode of ours, made before
//! the connector is lit, at the refresh rate of the slowest physical output: a compositor such as
//! KWin runs at the rate of the slowest output, and a slower display would slow the whole desktop
//! down. A connector that carries only a slow mode is scaled from it.

use super::{
    protocol::{Back, Kind, Request, Response, Restore},
    randr::{self, Look, Modeline, Output, Place, Provider},
};
use super::xephyr::{Nest, XephyrNest};
use crate::vdisplay::manager::{Backend, Caps, Mode, NestedTarget, Spec};
use hbb_common::{anyhow::anyhow, bail, log, ResultType};
use std::time::Duration;

/// The root service, as the session reaches it.
pub trait Root {
    /// Its answer; an error for one it refused, or when the service could not be reached.
    fn call(&mut self, request: Request) -> ResultType<Response>;
    /// Whether the last call failed for want of the service rather than by its refusal: the
    /// service went away (restarted, say), and with it whatever it had lit for us.
    fn lost(&self) -> bool {
        false
    }
}

/// The X server and the desktop running on it.
pub trait Desktop {
    /// The outputs, with their connector ids where the driver gives them (`xrandr --current
    /// --prop`: the configuration as it is, without probing the connectors).
    fn outputs(&mut self) -> ResultType<Vec<Output>>;
    /// Has the X server look at its connectors again (plain `xrandr`): for a driver that sends no
    /// hotplug event when a connector is forced on.
    fn probe(&mut self);
    fn xrandr(&mut self, args: &[String]) -> ResultType<()>;
    /// Makes `name` primary, the way this desktop wants to be told.
    fn set_primary(&mut self, name: &str) -> ResultType<()>;
    /// The scale the desktop shows everything at, where it turns that down for good once an output
    /// too small for it is enabled (`big_enough`); None where outputs are not held to it.
    fn global_scale(&mut self) -> Option<f64> {
        None
    }
    /// Sets the global scale back to one `global_scale` read.
    fn set_global_scale(&mut self, _scale: f64) -> ResultType<()> {
        Ok(())
    }
    /// `xrandr --listproviders`.
    fn providers(&mut self) -> ResultType<Vec<Provider>>;
    /// Where the pointer is, on the screen.
    fn pointer(&mut self) -> Option<(i32, i32)>;
    fn warp(&mut self, at: (i32, i32));
    /// Drops the transform of every CRTC without an output, which the next output to use the
    /// CRTC would show: one scaled leaves its transform there when it is turned off, and xrandr
    /// itself cannot reach a CRTC without an output.
    fn clear_transforms(&mut self);
    fn sleep(&mut self, duration: Duration);
}

// How long a lit connector may take to show up in RandR, and how often to look. The X server is
// asked to look itself halfway; for vkms, at once, should it not have yet: it probes vkms in no
// time, and may not take the change event the root service sends for one.
const COME_UP: Duration = Duration::from_secs(10);
const VKMS_PROBE: Duration = Duration::ZERO;
const LOOK: Duration = Duration::from_millis(250);
// The desktop rearranges outputs when one comes or goes; it is done once two looks this far
// apart agree, or after `SETTLE_MAX`.
const SETTLE_GAP: Duration = Duration::from_millis(500);
const SETTLE_MAX: Duration = Duration::from_secs(5);
// Attempts at a layout the desktop keeps.
const ARRANGE_TRIES: usize = 3;
// Layouts put back, should the desktop keep undoing them, sooner or later; then they are left as
// the desktop has them, until it has kept them right for `STEADY` looks in a row.
const PUT_BACKS: u32 = 3;
const STEADY: u32 = 10;
// Modes tried for a display a port may refuse, ever smaller, down to this width.
const MODE_TRIES: usize = 6;
const MIN_WIDTH: u32 = 1024;
// Looks in a row at a desktop taken over that could not be taken over again: then it is given
// back, and its displays go, the controlling side told.
const TAKEOVER_TRIES: u32 = 3;

struct Ours {
    /// The kernel's name, for the root service.
    connector: String,
    kind: Kind,
    /// The RandR output, which is the display's name.
    output: String,
    /// The mode it runs: ours, or the connector's own, scaled.
    mode: String,
    /// The size it shows.
    size: (i32, i32),
    /// The mode we made for it, to go with it.
    made: Option<String>,
    /// The fastest clock its port carries, where known.
    max_khz: Option<u32>,
    /// How the controlling side uses it. `Nested` runs a session of its own on it.
    usage: Mode,
    /// Where its nested session shows, as last read: the output's area, while the output shows it
    /// alone. Input there goes to the session, offset by it; none goes anywhere else, nor while
    /// it is hidden, when the area shows another output's monitor and is that output's input.
    shown: Option<randr::Area>,
    /// The X display of its nested session, once started (`Mode::Nested`).
    nested: Option<String>,
}

impl Ours {
    fn look(&self) -> Look {
        match self.kind {
            Kind::Scaled => Look::Scaled(self.mode.clone(), self.size),
            _ => Look::Mode(self.mode.clone(), self.size),
        }
    }
}

/// Someone else's output, as it was.
struct Was {
    name: String,
    at: (i32, i32),
    size: (i32, i32),
    /// The mode it showed at its rate, and how it was turned: how it comes back after a takeover.
    mode: Option<String>,
    rate: Option<f64>,
    rotation: Option<String>,
}

pub struct X11Backend<R: Root, D: Desktop> {
    root: R,
    desktop: D,
    /// Starts and stops the nested session of `Mode::Nested` displays.
    nest: Box<dyn Nest>,
    ours: Vec<Ours>,
    /// The other outputs as they were before our first display came, and since, as they were
    /// last seen with ours in place: where they go back to.
    others: Vec<Was>,
    /// The primary output from then, made primary again should one of ours become primary.
    primary: Option<String>,
    max: usize,
    /// The kind the displays are, as far as the root service could tell at the start.
    kind: Option<Kind>,
    /// This X server shows no output of vkms: physical connectors only.
    no_vkms: bool,
    /// Looks in a row at a layout to put right; layouts put back; looks in a row at a right one.
    drift: u32,
    put_back: u32,
    steady: u32,
    /// Looks in a row at a desktop taken over that would not be taken over again.
    slipping: u32,
    /// The desktop's global scale before the change being made (`keeping_scale`), put back the
    /// moment it is seen otherwise.
    kept_scale: Option<f64>,
}

impl<R: Root, D: Desktop> X11Backend<R, D> {
    pub fn new(root: R, desktop: D, max: usize, kind: Option<Kind>) -> Self {
        Self::new_with_nest(root, desktop, max, kind, Box::new(XephyrNest::default()))
    }

    pub fn new_with_nest(
        root: R,
        desktop: D,
        max: usize,
        kind: Option<Kind>,
        nest: Box<dyn Nest>,
    ) -> Self {
        Self {
            root,
            desktop,
            nest,
            ours: Vec::new(),
            others: Vec::new(),
            primary: None,
            max,
            kind,
            no_vkms: false,
            drift: 0,
            put_back: 0,
            steady: 0,
            slipping: 0,
            kept_scale: None,
        }
    }

    fn release(&mut self, connector: &str) -> ResultType<()> {
        self.root
            .call(Request::Release {
                connector: connector.to_owned(),
            })
            .map(|_| ())
    }

    // A connector, what it is, and its RandR output, off and disconnected: vkms's first, unless
    // this X server shows no output of it (it started before vkms was there, or binds no card
    // added to it and binding it here did not help); then a physical one, from then on.
    fn reserve(&mut self) -> ResultType<(Response, Kind, String)> {
        loop {
            let skip_vkms = self.no_vkms;
            let reserved = self.root.call(Request::Reserve { skip_vkms })?;
            let kind = reserved.kind.unwrap_or(Kind::Connector);
            let found = match self.find_output(&reserved, kind) {
                Ok(found) => found,
                Err(e) => {
                    self.release(&reserved.connector).ok();
                    return Err(e);
                }
            };
            match found {
                // Never one in use: matched wrong, it would be someone's monitor.
                Some(o) if o.connected || o.area.is_some() => {
                    self.release(&reserved.connector).ok();
                    bail!("Output {} of connector {} is in use", o.name, reserved.connector);
                }
                Some(o) => return Ok((reserved, kind, o.name)),
                None => {
                    self.release(&reserved.connector).ok();
                    if kind != Kind::Vkms || skip_vkms {
                        bail!("No RandR output for connector {}", reserved.connector);
                    }
                    log::warn!("The X server shows no output of vkms: using physical connectors");
                    self.no_vkms = true;
                }
            }
        }
    }

    fn find_output(&mut self, reserved: &Response, kind: Kind) -> ResultType<Option<Output>> {
        let outputs = self.desktop.outputs()?;
        let found =
            randr::output_for(&outputs, &reserved.connector, reserved.connector_id, kind).cloned();
        if found.is_some() || kind != Kind::Vkms {
            return Ok(found);
        }
        Ok(self.bind_vkms(reserved))
    }

    // X servers before 21.1 bind no card added to them, but where a distribution patched them
    // to: one unbound is bound here, and let go again unless that shows vkms's output.
    fn bind_vkms(&mut self, reserved: &Response) -> Option<Output> {
        let providers = self.desktop.providers().ok()?;
        let source = providers.iter().find(|p| p.source_output)?.id.clone();
        let unbound: Vec<String> = providers
            .iter()
            .filter(|p| p.sink_output && !p.source_output && p.associated == 0)
            .map(|p| p.id.clone())
            .collect();
        if unbound.is_empty() {
            return None;
        }
        for id in &unbound {
            let bind = args(&["--setprovideroutputsource", id, &source]);
            if let Err(e) = self.desktop.xrandr(&bind) {
                log::warn!("Failed to bind provider {}: {}", id, e);
            }
        }
        let found = self.desktop.outputs().ok().and_then(|outputs| {
            randr::output_for(
                &outputs,
                &reserved.connector,
                reserved.connector_id,
                Kind::Vkms,
            )
            .cloned()
        });
        if found.is_some() {
            log::info!("Bound the X server's added card, which shows vkms");
        } else {
            for id in &unbound {
                self.desktop
                    .xrandr(&args(&["--setprovideroutputsource", id, "0x0"]))
                    .ok();
            }
        }
        found
    }

    // The modes of `output` once RandR sees it connected; on the way, the X server is asked to
    // look once.
    fn come_up(&mut self, output: &str, kind: Kind) -> Option<Vec<String>> {
        let probe_at = if kind == Kind::Vkms {
            VKMS_PROBE
        } else {
            COME_UP / 2
        };
        let mut waited = Duration::ZERO;
        let mut probed = false;
        while waited < COME_UP {
            if let Ok(outputs) = self.desktop.outputs() {
                if let Some(o) = outputs.iter().find(|o| o.name == output && o.connected) {
                    if !o.modes.is_empty() {
                        return Some(o.modes.clone());
                    }
                }
            }
            if !probed && waited >= probe_at {
                self.desktop.probe();
                probed = true;
                continue;
            }
            self.desktop.sleep(LOOK);
            waited += LOOK;
        }
        None
    }

    fn settle(&mut self) {
        let mut last = self.desktop.outputs().ok();
        let mut waited = Duration::ZERO;
        while waited < SETTLE_MAX {
            self.hold_scale();
            self.desktop.sleep(SETTLE_GAP);
            waited += SETTLE_GAP;
            self.hold_scale();
            let now = self.desktop.outputs().ok();
            if now.is_some() && now == last {
                return;
            }
            last = now;
        }
    }

    // A mode of that name left by a server that went away is this very mode, as its name says:
    // made already, it is only added. Not added, it goes.
    fn make_mode(&mut self, output: &str, line: &Modeline) -> ResultType<()> {
        if let Err(e) = self.desktop.xrandr(&line.newmode_args()) {
            log::debug!("Mode {} not made, there already maybe: {}", line.name, e);
        }
        let added = self
            .desktop
            .xrandr(&args(&["--addmode", output, &line.name]));
        if added.is_err() {
            self.unmake_mode(output, &line.name);
        }
        added
    }

    fn unmake_mode(&mut self, output: &str, name: &str) {
        for call in [args(&["--delmode", output, name]), args(&["--rmmode", name])] {
            if let Err(e) = self.desktop.xrandr(&call) {
                log::debug!("xrandr {}: {}", call.join(" "), e);
            }
        }
    }

    // One xrandr call that puts everything where it goes; an error if the X server refused it.
    fn place(&mut self) -> ResultType<()> {
        let outputs = self.desktop.outputs()?;
        let wrong: Vec<Place> = self
            .places(&outputs)
            .into_iter()
            .filter(|p| !p.is_in(&outputs))
            .collect();
        let primary = self.primary_back(&outputs);
        if wrong.is_empty() && primary.is_none() {
            return Ok(());
        }
        self.desktop
            .xrandr(&randr::arrange_args(&wrong, primary.as_deref()))
    }

    // Display `i` placed at the first of `lines` the X server takes: a port can refuse a clock it
    // cannot carry (Intel's, one its PLLs cannot make, or above what its firmware allows), which
    // only setting the mode tells. The first is made already; one refused goes.
    fn place_trying(&mut self, i: usize, lines: &[Modeline]) -> ResultType<()> {
        let output = self.ours[i].output.clone();
        let mut last = None;
        for (n, line) in lines.iter().enumerate() {
            if n > 0 {
                if let Err(e) = self.make_mode(&output, line) {
                    last = Some(e);
                    continue;
                }
            }
            self.ours[i].mode = line.name.clone();
            self.ours[i].size = line.size();
            self.ours[i].made = Some(line.name.clone());
            match self.place() {
                Ok(()) => {
                    log::info!(
                        "Virtual display {} runs {} ({:.1} MHz, {:.2} Hz)",
                        output,
                        line.name,
                        line.clock_khz as f64 / 1000.0,
                        line.rate()
                    );
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("{} refused mode {}: {}", output, line.name, e);
                    self.unmake_mode(&output, &line.name);
                    self.ours[i].made = None;
                    last = Some(e);
                }
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("No mode for {}", output)))
    }

    // Everything where it goes, in one change, and none of ours primary; until the desktop
    // leaves it so.
    fn arrange(&mut self) -> ResultType<()> {
        for _ in 0..ARRANGE_TRIES {
            if let Err(e) = self.place() {
                log::warn!("Failed to lay out the virtual displays: {}", e);
            }
            self.settle();
            // Deepin keeps a primary output of its own and puts it back over RandR's: it has to
            // be told. Not while the desktop is taken over: it would save ours as its primary,
            // and RandR's is what the desktop follows meanwhile.
            let outputs = self.desktop.outputs()?;
            if let Some(primary) = self.primary_back(&outputs) {
                let made = if self.taking_over() {
                    self.desktop
                        .xrandr(&args(&["--output", &primary, "--primary"]))
                } else {
                    self.desktop.set_primary(&primary)
                };
                if let Err(e) = made {
                    log::warn!("Failed to make {} primary again: {}", primary, e);
                }
                self.settle();
            }
            let outputs = self.desktop.outputs()?;
            if self.arranged(&outputs) {
                self.follow(&outputs);
                return Ok(());
            }
        }
        // For the log, what the desktop kept otherwise; the controlling side gets the short of it.
        if let Ok(outputs) = self.desktop.outputs() {
            let wrong: Vec<Place> = self
                .places(&outputs)
                .into_iter()
                .filter(|p| !p.is_in(&outputs))
                .collect();
            let have: Vec<_> = outputs
                .iter()
                .filter(|o| o.connected)
                .map(|o| (&o.name, o.area, o.current.as_deref(), o.primary))
                .collect();
            log::warn!(
                "The desktop did not keep the virtual displays where they go: wanted {:?}, primary \
                 to {:?}; it has {:?}",
                wrong,
                self.primary_back(&outputs),
                have
            );
            // Wherever the desktop has them, nested sessions show only where that is safe.
            self.follow(&outputs);
        }
        bail!("The desktop did not keep the virtual displays where they go")
    }

    fn arranged(&self, outputs: &[Output]) -> bool {
        self.places(outputs).iter().all(|p| p.is_in(outputs)) && self.primary_back(outputs).is_none()
    }

    // Where everything goes: the others as `others_at` has them, ours in a row to their right,
    // oldest first. The desktop taken over, the others off, every one, and ours in a row from the
    // origin.
    fn places(&self, outputs: &[Output]) -> Vec<Place> {
        if self.taking_over() {
            let mut places: Vec<Place> = outputs
                .iter()
                .filter(|o| {
                    !self.is_ours(&o.name) && (o.area.is_some() || self.was(&o.name).is_some())
                })
                .map(|o| Place {
                    name: o.name.clone(),
                    at: (0, 0),
                    look: Look::Off,
                })
                .collect();
            let mut x = 0;
            for o in &self.ours {
                places.push(Place {
                    name: o.output.clone(),
                    at: (x, 0),
                    look: o.look(),
                });
                x += o.size.0;
            }
            return places;
        }
        let others = self.others_at(outputs);
        let sizes: Vec<(String, i32, i32)> = self
            .ours
            .iter()
            .map(|o| (o.output.clone(), o.size.0, o.size.1))
            .collect();
        let row = randr::layout(&others, &sizes);
        let mut places: Vec<Place> = others
            .iter()
            .filter(|o| self.was(&o.name).is_some())
            .filter_map(|o| {
                let (x, y, _, _) = o.area?;
                Some(Place {
                    name: o.name.clone(),
                    at: (x, y),
                    look: Look::Moved,
                })
            })
            .collect();
        places.extend(
            row.into_iter()
                .zip(&self.ours)
                .map(|((name, x, y), ours)| Place {
                    name,
                    at: (x, y),
                    look: ours.look(),
                }),
        );
        places
    }

    // The others that are on, where they go: where they were, at the size they have now; unless
    // one of them has been resized since, their user then having laid them out anew, which
    // stands. One that came since stays where it is.
    fn others_at(&self, outputs: &[Output]) -> Vec<Output> {
        let others: Vec<&Output> = outputs
            .iter()
            .filter(|o| !self.is_ours(&o.name) && o.area.is_some())
            .collect();
        let resized = others.iter().any(|o| match (o.area, self.was(&o.name)) {
            (Some((_, _, w, h)), Some(was)) => was.size != (w, h),
            _ => false,
        });
        others
            .into_iter()
            .map(|o| {
                let mut o = o.clone();
                if let (false, Some((_, _, w, h)), Some(was)) = (resized, o.area, self.was(&o.name))
                {
                    o.area = Some((was.at.0, was.at.1, w, h));
                }
                o
            })
            .collect()
    }

    // What to make primary should one of ours have become primary, or an output that is off: the
    // one from before, if it is still on, or else the first other that is. Any other primary is
    // someone's choice.
    fn primary_back(&self, outputs: &[Output]) -> Option<String> {
        if self.taking_over() {
            let first = &self.ours.first()?.output;
            let is = outputs.iter().any(|o| o.primary && &o.name == first);
            return (!is).then(|| first.clone());
        }
        let fine = outputs
            .iter()
            .find(|o| o.primary)
            .map_or(true, |o| !self.is_ours(&o.name) && o.area.is_some());
        if fine {
            return None;
        }
        let other_on = |name: &str| {
            !self.is_ours(name) && outputs.iter().any(|o| o.name == name && o.area.is_some())
        };
        self.primary
            .clone()
            .filter(|p| other_on(p))
            .or_else(|| {
                outputs
                    .iter()
                    .find(|o| other_on(&o.name))
                    .map(|o| o.name.clone())
            })
    }

    fn is_ours(&self, output: &str) -> bool {
        self.ours.iter().any(|o| o.output == output)
    }

    fn names(&self) -> Vec<&str> {
        self.ours.iter().map(|o| o.output.as_str()).collect()
    }

    fn was(&self, output: &str) -> Option<&Was> {
        self.others.iter().find(|w| w.name == output)
    }

    // The others as they are, to keep them so: before our first display comes, and whenever ours
    // are seen in place (their user may have moved them meanwhile). Not while the desktop is
    // taken over: then they are off, and kept as they were, to come back so.
    fn remember(&mut self, outputs: &[Output]) {
        if self.taking_over() {
            return;
        }
        let others: Vec<Was> = outputs
            .iter()
            .filter(|o| !self.is_ours(&o.name))
            .filter_map(|o| {
                let (x, y, w, h) = o.area?;
                Some(Was {
                    name: o.name.clone(),
                    at: (x, y),
                    size: (w, h),
                    mode: o.current.clone().or_else(|| o.modes.first().cloned()),
                    rate: o.rate,
                    rotation: o.rotation.clone(),
                })
            })
            .collect();
        self.others = others;
        if let Some(primary) = outputs
            .iter()
            .find(|o| o.primary && o.area.is_some() && !self.is_ours(&o.name))
        {
            self.primary = Some(primary.name.clone());
        }
    }

    fn forget(&mut self) {
        self.others.clear();
        self.primary = None;
        self.drift = 0;
        self.put_back = 0;
        self.steady = 0;
        self.slipping = 0;
    }

    // Whether the controlling side has the desktop to itself (`Mode::Privacy`): someone else's
    // outputs off, ours alone on, the first primary.
    fn taking_over(&self) -> bool {
        self.ours.iter().any(|o| o.usage == Mode::Privacy)
    }

    // How the desktop goes back: the others as they were before it was taken over, the primary
    // from then primary, and `off` off in the same call.
    fn restore(&self, off: Vec<String>) -> Restore {
        Restore {
            back: self
                .others
                .iter()
                .filter_map(|w| {
                    Some(Back {
                        name: w.name.clone(),
                        mode: w.mode.clone()?,
                        x: w.at.0,
                        y: w.at.1,
                        rotation: w.rotation.clone(),
                        rate: w.rate,
                        primary: self.primary.as_deref() == Some(w.name.as_str()),
                    })
                })
                .collect(),
            off,
        }
    }

    // The root service told how the desktop goes back, ours all off, should this process go
    // without giving it back itself: only then may it be taken over.
    fn arm(&mut self) -> ResultType<()> {
        let restore = self.restore(self.names().into_iter().map(str::to_owned).collect());
        if !restore.valid() {
            bail!("No monitor to give the desktop back to");
        }
        self.root.call(Request::Takeover { restore }).map(|_| ())
    }

    // The desktop handed to display `output` (`Mode::Privacy`), already laid out beside the
    // others with the mode it shows, which the port took: the root service armed, then in one
    // change the others off and ours alone, the first primary. Should that not take, the desktop
    // is given back at once.
    fn take_over(&mut self, output: &str) -> ResultType<()> {
        let Some(i) = self.ours.iter().position(|o| o.output == output) else {
            bail!("No virtual display {}", output);
        };
        if let Err(e) = self.arm() {
            // It may yet take it, answering too late: told the desktop is back, it forgets it.
            if !self.taking_over() {
                self.root.call(Request::Restored).ok();
            }
            return Err(e);
        }
        self.ours[i].usage = Mode::Privacy;
        self.put_back = 0;
        self.slipping = 0;
        if let Err(e) = self.arrange() {
            log::error!("Failed to take the desktop over: {}", e);
            self.ours[i].usage = Mode::Shared;
            if !self.taking_over() {
                self.give_back(&[]).ok();
            }
            return Err(e);
        }
        log::info!("The desktop is taken over by virtual display {}", output);
        Ok(())
    }

    // The desktop given back, nothing of ours taking it over any more: in one change the others
    // still there on as they were before, the primary from then primary, a monitor plugged in
    // meanwhile on at the mode it likes to their right, then the rest of ours, and `off` off;
    // short of that, each monitor on at the mode it likes. Only once one is seen on does `off` go
    // off for sure, the pointer go onto an output that is on, and the root service hear the
    // desktop is back. Without a monitor on, nothing goes off: the desktop stays taken over, to be
    // given back later, by this process or the root service.
    fn give_back(&mut self, off: &[String]) -> ResultType<()> {
        // A monitor may have gone or come meanwhile, which only a look at the connectors tells.
        self.desktop.probe();
        let outputs = self.desktop.outputs()?;
        let connected = |name: &str| outputs.iter().any(|o| o.name == name && o.connected);
        let mut restore = self.restore(off.to_vec());
        // One gone would fail the whole change.
        restore.back.retain(|b| connected(&b.name));
        let newcomers: Vec<(String, String, (i32, i32))> = outputs
            .iter()
            .filter(|o| o.connected && !self.is_ours(&o.name) && self.was(&o.name).is_none())
            .filter_map(|o| {
                let mode = o.modes.first()?.clone();
                let size = randr::mode_size(&mode)?;
                Some((o.name.clone(), mode, size))
            })
            .collect();
        if restore.back.is_empty() && newcomers.is_empty() {
            bail!("No monitor to give the desktop back to");
        }
        let mut places: Vec<Place> = restore
            .back
            .iter()
            .map(|b| Place {
                name: b.name.clone(),
                at: (b.x, b.y),
                look: Look::Back {
                    mode: b.mode.clone(),
                    rotation: b.rotation.clone(),
                    rate: b.rate,
                },
            })
            .collect();
        let others: Vec<Output> = self
            .others
            .iter()
            .filter(|w| restore.back.iter().any(|b| b.name == w.name))
            .map(|w| Output {
                name: w.name.clone(),
                area: Some((w.at.0, w.at.1, w.size.0, w.size.1)),
                ..Default::default()
            })
            .collect();
        let rest: Vec<&Ours> = self
            .ours
            .iter()
            .filter(|o| !off.contains(&o.output))
            .collect();
        let mut sizes: Vec<(String, i32, i32)> = newcomers
            .iter()
            .map(|(name, _, (w, h))| (name.clone(), *w, *h))
            .collect();
        sizes.extend(rest.iter().map(|o| (o.output.clone(), o.size.0, o.size.1)));
        for (name, x, y) in randr::layout(&others, &sizes) {
            let look = match newcomers.iter().find(|(n, _, _)| *n == name) {
                Some((_, mode, _)) => Look::Back {
                    mode: mode.clone(),
                    rotation: None,
                    rate: None,
                },
                None => match rest.iter().find(|o| o.output == name) {
                    Some(o) => o.look(),
                    None => continue,
                },
            };
            places.push(Place {
                name,
                at: (x, y),
                look,
            });
        }
        places.extend(off.iter().map(|name| Place {
            name: name.clone(),
            at: (0, 0),
            look: Look::Off,
        }));
        let monitors: Vec<String> = restore
            .back
            .iter()
            .map(|b| b.name.clone())
            .chain(newcomers.iter().map(|(name, _, _)| name.clone()))
            .collect();
        let primary = self
            .primary
            .clone()
            .filter(|p| monitors.contains(p))
            .or_else(|| monitors.first().cloned());
        if let Err(e) = self
            .desktop
            .xrandr(&randr::arrange_args(&places, primary.as_deref()))
        {
            log::error!("Failed to give the desktop back as it was: {}", e);
        }
        self.settle();
        self.slipping = 0;
        // Not known is not off: the next try finds out.
        let lit = |b: &mut Self| -> ResultType<(bool, bool)> {
            let outputs = b.desktop.outputs()?;
            let on = |name: &String| outputs.iter().any(|o| &o.name == name && o.area.is_some());
            Ok((monitors.iter().all(on), monitors.iter().any(on)))
        };
        if !lit(self)?.0 {
            let mut auto = Vec::new();
            for name in &monitors {
                auto.extend(args(&["--output", name, "--auto"]));
                if primary.as_ref() == Some(name) {
                    auto.push("--primary".to_owned());
                }
            }
            if let Err(e) = self.desktop.xrandr(&auto) {
                log::error!("Failed to turn the monitors on again: {}", e);
            }
            self.settle();
        }
        if !lit(self)?.1 {
            bail!("The monitors did not come back on");
        }
        for name in off {
            if let Some(kind) = self.ours.iter().find(|o| &o.output == name).map(|o| o.kind) {
                self.turn_off(name, kind);
            }
        }
        self.pointer_back();
        if let Err(e) = self.root.call(Request::Restored) {
            log::warn!("Failed to tell the root service the desktop is back: {}", e);
        }
        log::info!("The desktop is given back");
        Ok(())
    }

    // The pointer onto an output that is on, should it be on none, as when the output it was on
    // went off: the middle of the primary one, or else of the first.
    fn pointer_back(&mut self) {
        let Ok(outputs) = self.desktop.outputs() else {
            return;
        };
        let Some((px, py)) = self.desktop.pointer() else {
            return;
        };
        let on: Vec<&Output> = outputs.iter().filter(|o| o.area.is_some()).collect();
        let inside = |o: &&Output| {
            o.area
                .map_or(false, |(x, y, w, h)| px >= x && py >= y && px < x + w && py < y + h)
        };
        if on.iter().any(inside) {
            return;
        }
        if let Some((x, y, w, h)) = on
            .iter()
            .find(|o| o.primary)
            .or_else(|| on.first())
            .and_then(|o| o.area)
        {
            self.desktop.warp((x + w / 2, y + h / 2));
        }
    }

    // The pointer off display `output`, onto the middle of the primary output (or else another
    // one of someone's): left on a display nobody looks at, or on one that goes, which leaves it
    // at the edge of whichever output is nearest.
    fn pointer_home(&mut self, output: &str) {
        let Ok(outputs) = self.desktop.outputs() else {
            return;
        };
        let Some((x, y, w, h)) = outputs.iter().find(|o| o.name == output).and_then(|o| o.area)
        else {
            return;
        };
        let Some((px, py)) = self.desktop.pointer() else {
            return;
        };
        if px < x || py < y || px >= x + w || py >= y + h {
            return;
        }
        let others: Vec<&Output> = outputs
            .iter()
            .filter(|o| !self.is_ours(&o.name) && o.area.is_some())
            .collect();
        let home = others
            .iter()
            .find(|o| o.primary)
            .or_else(|| others.first())
            .and_then(|o| o.area);
        if let Some((x, y, w, h)) = home {
            self.desktop.warp((x + w / 2, y + h / 2));
        }
    }

    // Off; scaled, its transform dropped first while it can be: once the connector is let go,
    // RandR refuses the output a transform, having no mode for it, and it can only be turned off.
    fn turn_off(&mut self, output: &str, kind: Kind) {
        let Ok(outputs) = self.desktop.outputs() else {
            return;
        };
        let Some(o) = outputs.iter().find(|o| o.name == output) else {
            return;
        };
        if o.area.is_none() {
            return;
        }
        let calls = if kind == Kind::Scaled && o.connected && !o.modes.is_empty() {
            randr::off_args(output, randr::extent(&outputs))
        } else {
            vec![args(&["--output", output, "--off"])]
        };
        for args in calls {
            if let Err(e) = self.desktop.xrandr(&args) {
                log::warn!("Failed to turn virtual display {} off: {}", output, e);
            }
        }
    }

    // Off, its mode gone, given back, and the rest moved up. Answers whether the root service
    // let it go; gone from ours either way: it is off, and a service that would not answer has
    // dropped this client, and with it the connector.
    fn remove(&mut self, output: &str) -> ResultType<()> {
        let Some(i) = self.ours.iter().position(|o| o.output == output) else {
            return Ok(());
        };
        // The nested session hidden and stopped before the output goes off and the others move.
        if self.ours[i].nested.is_some() {
            self.nest.stop(output);
        }
        let kind = self.ours[i].kind;
        let gives_back = self.ours[i].usage == Mode::Privacy
            && self.ours.iter().filter(|o| o.usage == Mode::Privacy).count() == 1;
        if gives_back {
            // The last of ours to take the desktop over: given back in the same change as it goes
            // off, never a moment with no monitor on. Should no monitor come on, it stays, the
            // desktop taken over still, to be given back on the next try, or by the root service
            // should this process go first.
            self.ours[i].usage = Mode::Shared;
            if let Err(e) = self.give_back(&[output.to_owned()]) {
                self.ours[i].usage = Mode::Privacy;
                log::error!("Kept virtual display {}: {}", output, e);
                return Err(e);
            }
        } else {
            self.pointer_home(output);
            self.turn_off(output, kind);
        }
        let ours = self.ours.remove(i);
        if let Some(made) = &ours.made {
            self.unmake_mode(output, made);
        }
        // Still taken over by others of ours: the root service told how it goes back now.
        if self.taking_over() {
            if let Err(e) = self.arm() {
                log::warn!("Failed to tell the root service how the desktop goes back: {}", e);
            }
        }
        let released = self.release(&ours.connector);
        if kind == Kind::Scaled {
            self.desktop.clear_transforms();
        }
        self.after_removal();
        released
    }

    // The others put back, and with no display of ours left, nothing more to keep.
    fn after_removal(&mut self) {
        self.settle();
        if let Err(e) = self.arrange() {
            log::warn!("{}", e);
        }
        if self.ours.is_empty() {
            self.forget();
        }
    }

    // Starts the nested session of a `Mode::Nested` display, over the output it was given, at its
    // size, and shows it there once it is up.
    fn start_nested(&mut self, output: &str) -> ResultType<()> {
        let Some(i) = self.ours.iter().position(|o| o.output == output) else {
            bail!("No virtual display {}", output);
        };
        let outputs = self.desktop.outputs()?;
        let Some(area) = outputs
            .iter()
            .find(|o| o.name == output)
            .and_then(|o| o.area)
        else {
            bail!("Virtual display {} is not on", output);
        };
        let display = self.nest.start(output, area)?;
        self.ours[i].nested = Some(display);
        self.follow(&outputs);
        Ok(())
    }

    // Where each nested session shows, from a fresh reading of the outputs: over its output,
    // wherever the desktop has put that, but only while the output shows it alone. One the
    // desktop put over another output (a mirror, a layout of its own) would show on that
    // output's monitor too: hidden until it is beside again.
    fn follow(&mut self, outputs: &[Output]) {
        let lit = randr::lit(outputs);
        for o in &mut self.ours {
            o.shown = randr::alone(&o.output, &lit);
            if o.nested.is_some() {
                self.nest.place(&o.output, o.shown);
            }
        }
    }

    // `Backend::plug_in`, the desktop's scale aside.
    fn plug_in_sized(&mut self, spec: &Spec, announce: &mut dyn FnMut(&str)) -> ResultType<String> {
        let spec = &self.for_desktop(spec);
        let (reserved, kind, output) = self.reserve()?;
        let connector = reserved.connector.clone();
        let outputs = match self.desktop.outputs() {
            Ok(outputs) => outputs,
            Err(e) => {
                self.release(&connector).ok();
                return Err(e);
            }
        };
        if self.ours.is_empty() {
            self.remember(&outputs);
        }
        let rate = randr::lowest_rate(&outputs, &self.names());
        let lines = match kind {
            Kind::Scaled => Vec::new(),
            _ => ladder(spec, rate, reserved.max_khz),
        };
        if kind != Kind::Scaled && lines.is_empty() {
            self.release(&connector).ok();
            bail!(
                "Connector {} carries no mode of {}x{} at {:.2} Hz",
                connector,
                spec.width,
                spec.height,
                rate
            );
        }
        // Made while the connector is off: a desktop that lays its outputs out anew when their
        // modes change (GNOME's mutter) does so now, not with the display there.
        if let Some(first) = lines.first() {
            if let Err(e) = self.make_mode(&output, first) {
                self.release(&connector).ok();
                return Err(e);
            }
        }
        announce(&output);
        let activated = self.root.call(Request::Activate {
            connector: connector.clone(),
        });
        let modes = match activated.map(|_| self.come_up(&output, kind)) {
            Ok(Some(modes)) => modes,
            Ok(None) => {
                self.give_up(&connector, &output, lines.first());
                bail!("Virtual display {} did not come up", output);
            }
            Err(e) => {
                self.give_up(&connector, &output, lines.first());
                return Err(e);
            }
        };
        self.settle();
        self.ours.push(Ours {
            connector,
            kind,
            output: output.clone(),
            mode: modes[0].clone(),
            size: (spec.width as i32, spec.height as i32),
            made: None,
            max_khz: reserved.max_khz,
            // Beside the others, the desktop taken over only when asked afterwards (`take_over`).
            usage: match spec.mode {
                Mode::Privacy => Mode::Shared,
                mode => mode,
            },
            shown: None,
            nested: None,
        });
        let i = self.ours.len() - 1;
        let placed = match kind {
            Kind::Scaled => self.place(),
            _ => self.place_trying(i, &lines),
        };
        if let Err(e) = placed.and_then(|()| {
            self.settle();
            self.arrange()
        }) {
            self.remove(&output).ok();
            return Err(e);
        }
        // The display is up and laid out; a nested-session display now gets its own session on it.
        // If that fails the display is useless to the controlling side, so it is taken back.
        if spec.mode == Mode::Nested {
            if let Err(e) = self.start_nested(&output) {
                self.remove(&output).ok();
                return Err(e);
            }
        }
        // One that comes while the desktop is taken over joins in, laid out with the rest of ours
        // already, and goes off with them should this process go. One for the desktop is then
        // asked to take it over (`set_mode`), as the manager does, and stays beside the others
        // should that not take.
        if self.taking_over() {
            if let Err(e) = self.arm() {
                log::warn!("Failed to tell the root service how the desktop goes back: {}", e);
            }
        }
        self.put_back = 0;
        Ok(output)
    }

    // Runs `f`, the desktop's global scale put back should the desktop have turned it down
    // meanwhile: Deepin does for an output it lights by itself at its preferred size (vkms's
    // 1024x768) before ours is set, and for one a port carries only smaller than `big_enough`.
    // Only across our own changes, seconds each, which undo any change of the scale meanwhile,
    // the user's too. Looked at on every wait for the outputs to settle (`hold_scale`), so the
    // desktop shows the wrong scale a moment at most, not for the whole change.
    fn keeping_scale<T>(&mut self, f: impl FnOnce(&mut Self) -> ResultType<T>) -> ResultType<T> {
        self.kept_scale = self.desktop.global_scale();
        let done = f(self);
        self.hold_scale();
        self.kept_scale = None;
        done
    }

    // The desktop's scale put back to the one before the change being made, if it changed.
    fn hold_scale(&mut self) {
        let Some(before) = self.kept_scale else {
            return;
        };
        let Some(now) = self.desktop.global_scale() else {
            // Not answering: not asked again during this change, which it would only slow down.
            self.kept_scale = None;
            return;
        };
        if (now - before).abs() > 1e-6 {
            log::warn!(
                "The desktop turned its scale from {} to {} for a virtual display; putting it back",
                before,
                now
            );
            if let Err(e) = self.desktop.set_global_scale(before) {
                log::warn!("Failed to put the desktop's scale back: {}", e);
            }
        }
    }

    // `spec`, made big enough for the desktop to keep its scale (`big_enough`).
    fn for_desktop(&mut self, spec: &Spec) -> Spec {
        let Some(scale) = self.desktop.global_scale() else {
            return *spec;
        };
        let (width, height) = big_enough(spec.width, spec.height, scale);
        if (width, height) != (spec.width, spec.height) {
            log::info!(
                "Virtual display made {}x{} rather than {}x{}: the desktop would turn its scale of {} down for a smaller one",
                width,
                height,
                spec.width,
                spec.height,
                scale
            );
        }
        Spec {
            width,
            height,
            ..*spec
        }
    }

    // Display `name` made the size of `spec`, the others laid out anew around it.
    fn resize(&mut self, name: &str, spec: &Spec) -> ResultType<()> {
        let Some(i) = self.ours.iter().position(|o| o.output == name) else {
            bail!("No virtual display {}", name);
        };
        let spec = &self.for_desktop(spec);
        self.put_back = 0;
        if self.ours[i].kind == Kind::Scaled {
            self.ours[i].size = (spec.width as i32, spec.height as i32);
            return self.arrange();
        }
        let outputs = self.desktop.outputs()?;
        let rate = randr::lowest_rate(&outputs, &self.names());
        let lines = ladder(spec, rate, self.ours[i].max_khz);
        let (mode, size, made) = (
            self.ours[i].mode.clone(),
            self.ours[i].size,
            self.ours[i].made.clone(),
        );
        let mut placed = false;
        for line in &lines {
            if line.name == mode {
                placed = true;
                break;
            }
            if self.make_mode(name, line).is_err() {
                continue;
            }
            self.ours[i].mode = line.name.clone();
            self.ours[i].size = line.size();
            self.ours[i].made = Some(line.name.clone());
            match self.place() {
                Ok(()) => {
                    placed = true;
                    break;
                }
                Err(e) => {
                    log::warn!("{} refused mode {}: {}", name, line.name, e);
                    self.unmake_mode(name, &line.name);
                    self.ours[i].mode = mode.clone();
                    self.ours[i].size = size;
                    self.ours[i].made = made.clone();
                }
            }
        }
        if let (true, Some(old)) = (placed, &made) {
            if self.ours[i].made.as_ref() != Some(old) {
                self.unmake_mode(name, old);
            }
        }
        self.settle();
        let arranged = self.arrange();
        if !placed {
            bail!("Virtual display {} could not be made {}x{}", name, spec.width, spec.height);
        }
        arranged
    }

    // Display `name` used as `mode` from now on, as a controlling side asks when it takes it
    // back: the desktop taken over, or given back. A nested session is neither started nor
    // stopped here.
    fn switch(&mut self, name: &str, mode: Mode) -> ResultType<()> {
        let Some(i) = self.ours.iter().position(|o| o.output == name) else {
            bail!("No virtual display {}", name);
        };
        let usage = self.ours[i].usage;
        if usage == Mode::Nested || mode == Mode::Nested || (usage == Mode::Privacy) == (mode == Mode::Privacy) {
            return Ok(());
        }
        if mode == Mode::Privacy {
            return self.take_over(name);
        }
        self.ours[i].usage = Mode::Shared;
        let done = if self.taking_over() {
            self.arm()
        } else {
            self.give_back(&[])
        };
        if done.is_err() {
            // Still taken over, so used as before, to be asked again.
            self.ours[i].usage = usage;
        }
        done
    }

    // The desktop taken over, kept so: the moment someone turns an output on again, or moves
    // ours, it is taken over again, every time; should that not take for `TAKEOVER_TRIES` looks
    // in a row, it is given back and its displays go, the controlling side told. Answers those.
    fn keep_taken_over(&mut self, outputs: &[Output]) -> Vec<String> {
        if self.arranged(outputs) {
            self.slipping = 0;
            return Vec::new();
        }
        // On again by the root service's doing, maybe, the person here having taken the desktop
        // back since the last look (Ctrl+P): asked once more before it is taken over again.
        if self.root.call(Request::Status).map_or(false, |s| s.taken_back) {
            self.hand_back();
            return Vec::new();
        }
        log::warn!("The desktop taken over was taken back; taking it over again");
        let kept = self.arrange().is_ok();
        if kept {
            self.slipping = 0;
            return Vec::new();
        }
        self.slipping += 1;
        if self.slipping < TAKEOVER_TRIES {
            return Vec::new();
        }
        log::error!("The desktop could not be kept taken over; giving it back");
        let taking: Vec<String> = self
            .ours
            .iter()
            .filter(|o| o.usage == Mode::Privacy)
            .map(|o| o.output.clone())
            .collect();
        for output in &taking {
            self.remove(output).ok();
        }
        taking.into_iter().filter(|o| !self.is_ours(o)).collect()
    }

    // The person at the device took the desktop back (Ctrl+P, which the root service watches for
    // while it is taken over): given back for good, the displays kept as extra screens. The
    // controlling side learns it from their use (`usage`); the root service, from `Restored`.
    // Should that fail, the desktop is still taken over, and given back at the next look.
    fn hand_back(&mut self) {
        log::warn!("The person at this device took the desktop back");
        let taking: Vec<usize> = (0..self.ours.len())
            .filter(|i| self.ours[*i].usage == Mode::Privacy)
            .collect();
        for i in &taking {
            self.ours[*i].usage = Mode::Shared;
        }
        if let Err(e) = self.give_back(&[]) {
            log::error!("Failed to give the desktop back, to try again: {}", e);
            for i in taking {
                self.ours[i].usage = Mode::Privacy;
            }
        }
    }

    // A connector lit for a display that never came: given back, the mode made for it gone.
    fn give_up(&mut self, connector: &str, output: &str, made: Option<&Modeline>) {
        if let Some(line) = made {
            self.unmake_mode(output, &line.name);
        }
        self.release(connector).ok();
        self.after_removal();
    }
}

impl<R: Root + Send, D: Desktop + Send> Backend for X11Backend<R, D> {
    fn caps(&self) -> Caps {
        Caps {
            kind: match self.kind {
                Some(Kind::Vkms) => "x11-vkms",
                Some(Kind::Connector) => "x11-connector",
                Some(Kind::Scaled) => "x11-scaled",
                None => "x11",
            },
            max: self.max,
            custom: true,
            // X11 has no scale of its own per output.
            hidpi: false,
            max_width: 3840,
            max_height: 2160,
            // The shared extra screen; the desktop taken over, its monitors dark; a nested
            // session of the controlling side's own.
            modes: vec![Mode::Shared, Mode::Privacy, Mode::Nested],
        }
    }

    fn plug_in(&mut self, spec: &Spec, announce: &mut dyn FnMut(&str)) -> ResultType<String> {
        self.keeping_scale(|b| b.plug_in_sized(spec, announce))
    }

    fn set_mode(&mut self, name: &str, spec: &Spec) -> ResultType<()> {
        // The person here may have taken the desktop back since the last look (Ctrl+P), the root
        // service having turned the monitors on already: given back first, which a resize laid
        // out for a desktop taken over would undo, and not taken over again in the same breath.
        let taken_back = self.taking_over()
            && self.root.call(Request::Status).map_or(false, |s| s.taken_back);
        if taken_back {
            self.hand_back();
        }
        let resized = self.keeping_scale(|b| b.resize(name, spec));
        if taken_back && spec.mode == Mode::Privacy {
            resized?;
            bail!("{}", super::super::manager::TAKEN_BACK);
        }
        let switched = self.switch(name, spec.mode);
        resized.and(switched)
    }

    fn plug_out(&mut self, name: &str) -> ResultType<()> {
        self.remove(name)
    }

    // Nothing of ours yet; but a `--server` that went away may have left its output on, on a
    // connector the root service has put back since, with our modes on it, and a transform on a
    // CRTC. Any other output is someone else's, a monitor asleep that dropped its hotplug signal
    // say, and is left alone.
    fn reset_all(&mut self) -> ResultType<()> {
        // Any nested session of this run goes; one from a past run died with that `--server`.
        self.nest.stop_all();
        let released = self
            .root
            .call(Request::Status)
            .map(|status| status.released)
            .unwrap_or_default();
        let outputs = self.desktop.outputs()?;
        let leftovers: Vec<(Output, Kind)> = released
            .iter()
            .filter_map(|gone| {
                randr::output_for(&outputs, &gone.connector, gone.connector_id, gone.kind)
                    .map(|o| (o.clone(), gone.kind))
            })
            .filter(|(o, _)| !o.connected)
            .collect();
        for (o, kind) in leftovers {
            if o.area.is_some() {
                log::info!("Turning off virtual display {}, left on", o.name);
                self.turn_off(&o.name, kind);
            }
            for mode in o.modes.iter().filter(|m| Modeline::is_ours(m)) {
                self.unmake_mode(&o.name, mode);
            }
        }
        self.desktop.clear_transforms();
        Ok(())
    }

    // Asks the root service how it is, which finds out whether it is still there; puts our
    // displays back should the desktop have moved them, once it is done moving them.
    fn maintain(&mut self) -> Vec<String> {
        if self.ours.is_empty() {
            return Vec::new();
        }
        let status = self.root.call(Request::Status);
        if status.is_err() && self.root.lost() {
            log::warn!("The root service went away, and with it the virtual displays");
            self.nest.stop_all();
            let gone: Vec<(String, Kind, Option<String>)> = self
                .ours
                .iter()
                .map(|o| (o.output.clone(), o.kind, o.made.clone()))
                .collect();
            if self.taking_over() {
                // The desktop given back, ours all off in the same change.
                for o in &mut self.ours {
                    o.usage = Mode::Shared;
                }
                let off: Vec<String> = gone.iter().map(|(output, _, _)| output.clone()).collect();
                if let Err(e) = self.give_back(&off) {
                    log::error!("{}", e);
                }
            } else {
                for (output, kind, _) in &gone {
                    self.pointer_home(output);
                    self.turn_off(output, *kind);
                }
            }
            for (output, _, made) in &gone {
                if let Some(made) = made {
                    self.unmake_mode(output, made);
                }
            }
            self.ours.clear();
            self.desktop.clear_transforms();
            self.after_removal();
            return gone.into_iter().map(|(output, _, _)| output).collect();
        }
        if status.map_or(false, |s| s.taken_back) && self.taking_over() {
            self.hand_back();
            return Vec::new();
        }
        // A display whose nested session ended on its own is of no use to the controlling side:
        // it goes, as one whose session never came up does.
        let ended = self.nest.ended();
        if !ended.is_empty() {
            for output in &ended {
                log::warn!("Virtual display {} lost its nested session", output);
                self.remove(output).ok();
            }
            return ended;
        }
        let Ok(outputs) = self.desktop.outputs() else {
            return Vec::new();
        };
        // Gone without us: its connector was let go behind our back.
        let gone: Vec<String> = self
            .ours
            .iter()
            .filter(|o| !outputs.iter().any(|x| x.name == o.output && x.connected))
            .map(|o| o.output.clone())
            .collect();
        if !gone.is_empty() {
            for output in &gone {
                log::warn!("Virtual display {} went away", output);
                self.remove(output).ok();
            }
            // One that took the desktop over stays until the desktop is given back.
            return gone.into_iter().filter(|o| !self.is_ours(o)).collect();
        }
        // Laid out right or not, nested sessions follow where the desktop has their displays.
        self.follow(&outputs);
        if self.taking_over() {
            return self.keep_taken_over(&outputs);
        }
        if self.arranged(&outputs) {
            self.drift = 0;
            self.steady += 1;
            if self.steady >= STEADY {
                self.put_back = 0;
            }
            self.remember(&outputs);
            return gone;
        }
        self.steady = 0;
        self.drift += 1;
        // Wrong twice in a row: the desktop is done with whatever it did.
        if self.drift >= 2 && self.put_back < PUT_BACKS {
            self.drift = 0;
            self.put_back += 1;
            log::info!("The virtual displays were moved; putting them back");
            if let Err(e) = self.arrange() {
                log::warn!("{}", e);
            }
        }
        gone
    }

    fn left(&mut self, name: &str) {
        self.pointer_home(name);
    }

    fn usage(&self, name: &str) -> Option<Mode> {
        self.ours.iter().find(|o| o.output == name).map(|o| o.usage)
    }

    fn nested(&self) -> Vec<NestedTarget> {
        self.ours
            .iter()
            .filter_map(|o| {
                let (x, y, w, h) = o.shown?;
                Some(NestedTarget {
                    name: o.output.clone(),
                    x,
                    y,
                    w,
                    h,
                    display: o.nested.clone()?,
                })
            })
            .collect()
    }
}

// The smallest size of the shape of `width`x`height` an output may have without the desktop turning
// its global `scale` down for good. Deepin's dde-daemon sets its recommended scale (1.0 with a
// virtual display enabled) whenever an output is enabled whose size allows less than the scale in
// use: the steps of 0.25 from 1.0 up to the smaller of width/1024 and height/768, none at all under
// 1024x768 (display1 `calcMaxScaleFactor`, `tryToChangeScaleFactor`). Left alone past 3.0, which
// no size allows.
fn big_enough(width: u32, height: u32, scale: f64) -> (u32, u32) {
    if !scale.is_finite() || scale > 3.0 || width == 0 || height == 0 {
        return (width, height);
    }
    let step = 1.0 + ((scale.max(1.0) - 1.0) / 0.25 - 1e-9).ceil() * 0.25;
    let grow = (1024.0 * step / width as f64).max(768.0 * step / height as f64);
    if grow <= 1.0 {
        return (width, height);
    }
    // Rounded up, past what floating point adds: the width to the eights a mode's width comes in
    // (`Modeline::cvt_rb`), the height to even, as the video encoders want it.
    let up = |side: u32| (side as f64 * grow - 1e-6).ceil() as u32;
    ((up(width) + 7) & !7, (up(height) + 1) & !1)
}

// The modes to try for a display of `spec` at `rate`: its size, then smaller ones of its shape,
// should the port refuse it; none whose clock the port is known not to carry.
fn ladder(spec: &Spec, rate: f64, max_khz: Option<u32>) -> Vec<Modeline> {
    let (width, height) = (spec.width.max(1) as u64, spec.height as u64);
    let mut lines: Vec<Modeline> = Vec::new();
    let mut w = spec.width;
    loop {
        let line = Modeline::cvt_rb(w, (height * w as u64 / width) as u32, rate);
        if max_khz.map_or(true, |max| line.clock_khz <= max)
            && !lines.iter().any(|l| l.name == line.name)
        {
            lines.push(line);
        }
        if lines.len() >= MODE_TRIES || w <= MIN_WIDTH {
            return lines;
        }
        w = (w * 4 / 5).max(MIN_WIDTH);
    }
}

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vdisplay::x11::protocol::Gone;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    // A connector the root service could give out, and how it turns up in RandR.
    #[derive(Clone)]
    struct Port {
        connector: &'static str,
        id: u32,
        kind: Kind,
        output: &'static str,
        own_mode: &'static str,
        max_khz: Option<u32>,
    }

    const VKMS: Port = Port {
        connector: "Virtual-1",
        id: 36,
        kind: Kind::Vkms,
        output: "Virtual-1-1",
        own_mode: "1024x768",
        max_khz: None,
    };
    const DP: Port = Port {
        connector: "DP-1",
        id: 95,
        kind: Kind::Scaled,
        output: "DP-1",
        own_mode: "1280x720",
        max_khz: None,
    };
    const HDMI: Port = Port {
        connector: "HDMI-A-2",
        id: 110,
        kind: Kind::Connector,
        output: "HDMI-2",
        own_mode: "1280x720",
        max_khz: Some(300_000),
    };

    // The machine as the manual test found it: HDMI-1 on, DP-1 free.
    struct World {
        outputs: Vec<Output>,
        ports: Vec<Port>,
        reserved: Vec<String>,
        // The modes made, by name, and their clocks.
        modes: HashMap<String, u32>,
        // The X server refuses modes faster than this, as a port that cannot carry them.
        refuses_above_khz: Option<u32>,
        // An X server that binds no card added to it: vkms's output shows once bound.
        unbound: bool,
        // Outputs that still carry a transform from --scale-from.
        transformed: Vec<String>,
        // An output turned off while it still carried one: the next to use its CRTC would get it.
        stale_transforms: Vec<String>,
        transforms_cleared: usize,
        screen_resizes: usize,
        // The desktop makes a newly lit output primary, as Deepin did.
        steals_primary: bool,
        // It also puts the new output at the origin, mirroring, before anyone lays it out.
        mirrors_new: bool,
        // It moves the other outputs right of the new one, as Deepin's saved layout from the
        // manual test has it.
        shoves_others: bool,
        // It puts the new output right of the others, as Deepin did with vkms's.
        places_new_right: bool,
        // The kernel tells the X server about a forced connector at once; if not, only a probe
        // shows it.
        hotplug_events: bool,
        lit_unseen: Option<Port>,
        probes: usize,
        // An X server whose layout changes never take.
        ignores_xrandr: bool,
        released: Vec<String>,
        // What the root service says it put back.
        root_released: Vec<Gone>,
        lit: Vec<String>,
        refuse_reserve: bool,
        reservations: Vec<bool>,
        root_gone: bool,
        // The link to the root service breaks on release, which then puts the connector back.
        link_breaks_on_release: bool,
        pointer: (i32, i32),
        xrandr_calls: Vec<String>,
        // What the root service was told to give back, should the session go.
        armed: Option<Restore>,
        restored: usize,
        refuse_takeover: bool,
        // The person at the device took the desktop back (Ctrl+P), until it is given back; from
        // the Status asked so many times, should it be set.
        taken_back: bool,
        taken_back_at: Option<u32>,
        statuses: u32,
        // The desktop's global scale, held up by the size of every output (Deepin): turned down
        // to 1.0 when an output it lights is too small for it.
        global_scale: Option<f64>,
        scales_put_back: Vec<f64>,
    }

    impl World {
        fn office() -> Arc<Mutex<Self>> {
            Arc::new(Mutex::new(World {
                outputs: vec![
                    Output {
                        name: "HDMI-1".to_owned(),
                        connected: true,
                        primary: true,
                        area: Some((0, 0, 1920, 1080)),
                        modes: vec!["1920x1080".to_owned(), "2560x1440".to_owned()],
                        current: Some("1920x1080".to_owned()),
                        rate: Some(60.0),
                        connector_id: Some(103),
                        ..Default::default()
                    },
                    Output {
                        name: "DP-1".to_owned(),
                        connector_id: Some(95),
                        ..Default::default()
                    },
                ],
                ports: vec![DP],
                reserved: Vec::new(),
                modes: HashMap::new(),
                refuses_above_khz: None,
                unbound: false,
                transformed: Vec::new(),
                stale_transforms: Vec::new(),
                transforms_cleared: 0,
                screen_resizes: 0,
                steals_primary: true,
                mirrors_new: true,
                shoves_others: false,
                places_new_right: false,
                hotplug_events: true,
                lit_unseen: None,
                probes: 0,
                ignores_xrandr: false,
                released: Vec::new(),
                root_released: Vec::new(),
                lit: Vec::new(),
                refuse_reserve: false,
                reservations: Vec::new(),
                root_gone: false,
                link_breaks_on_release: false,
                pointer: (100, 100),
                xrandr_calls: Vec::new(),
                armed: None,
                restored: 0,
                refuse_takeover: false,
                taken_back: false,
                taken_back_at: None,
                statuses: 0,
                global_scale: None,
                scales_put_back: Vec::new(),
            }))
        }

        // vkms loaded at boot, its output in the X server, as on the office machine with it:
        // Deepin puts it right of HDMI-1 by itself.
        fn with_vkms() -> Arc<Mutex<Self>> {
            let world = Self::office();
            {
                let mut w = world.lock().unwrap();
                w.ports.insert(0, VKMS);
                w.outputs.push(Output {
                    name: "Virtual-1-1".to_owned(),
                    connector_id: Some(36),
                    ..Default::default()
                });
                w.steals_primary = false;
                w.mirrors_new = false;
                w.places_new_right = true;
            }
            world
        }

        // A free HDMI port of an Intel card, without vkms.
        fn with_hdmi() -> Arc<Mutex<Self>> {
            let world = Self::office();
            {
                let mut w = world.lock().unwrap();
                w.ports.insert(0, HDMI);
                w.outputs.push(Output {
                    name: "HDMI-2".to_owned(),
                    connector_id: Some(110),
                    ..Default::default()
                });
            }
            world
        }

        // With a second monitor on DP-2, right of HDMI-1.
        fn two_monitors() -> Arc<Mutex<Self>> {
            let world = Self::office();
            world.lock().unwrap().outputs.push(Output {
                name: "DP-2".to_owned(),
                connected: true,
                area: Some((1920, 0, 1920, 1080)),
                modes: vec!["1920x1080".to_owned()],
                current: Some("1920x1080".to_owned()),
                rate: Some(60.0),
                connector_id: Some(96),
                ..Default::default()
            });
            world
        }

        fn output(&mut self, name: &str) -> &mut Output {
            self.outputs.iter_mut().find(|o| o.name == name).unwrap()
        }

        fn output_ref(&self, name: &str) -> &Output {
            self.outputs.iter().find(|o| o.name == name).unwrap()
        }

        fn show(&mut self, port: &Port) {
            self.lit_unseen = None;
            let (steals, mirrors, shoves, right) = (
                self.steals_primary,
                self.mirrors_new,
                self.shoves_others,
                self.places_new_right,
            );
            let o = self.output(port.output);
            o.connected = true;
            o.modes.insert(0, port.own_mode.to_owned());
            let (w, h) = parse_size(port.own_mode).unwrap();
            if mirrors || shoves || right {
                o.current = Some(port.own_mode.to_owned());
            }
            if mirrors || shoves {
                o.area = Some((0, 0, w, h));
            }
            if right {
                o.area = Some((1920, 0, w, h));
            }
            // Lit by the desktop at its preferred mode, which Deepin checks its scale against.
            let too_small = self
                .global_scale
                .map_or(false, |s| deepin_max_scale(w as u32, h as u32) < s);
            if (mirrors || shoves || right) && too_small {
                self.global_scale = Some(1.0);
            }
            if shoves {
                self.output("HDMI-1").area = Some((w, 0, 1920, 1080));
            }
            if steals {
                for o in self.outputs.iter_mut() {
                    o.primary = o.name == port.output;
                }
            }
        }

        fn unplug(&mut self, output: &str) {
            if let Some(o) = self.outputs.iter_mut().find(|o| o.name == output) {
                o.connected = false;
                o.modes.retain(|m| Modeline::is_ours(m));
            }
        }

        // What xrandr does with its arguments, as far as the tests look.
        fn run_xrandr(&mut self, args: &[String]) -> Result<(), String> {
            let before = randr::extent(&self.outputs);
            let value = |i: usize| args.get(i).cloned().unwrap_or_default();
            match args[0].as_str() {
                "--newmode" => {
                    let name = value(1);
                    if self.modes.contains_key(&name) {
                        return Err("BadName".to_owned());
                    }
                    let clock: f64 = value(2).parse().unwrap();
                    self.modes.insert(name, (clock * 1000.0).round() as u32);
                    return Ok(());
                }
                "--addmode" => {
                    let (output, mode) = (value(1), value(2));
                    if !self.modes.contains_key(&mode) {
                        return Err(format!("cannot find mode {}", mode));
                    }
                    let o = self.output(&output);
                    if !o.modes.contains(&mode) {
                        o.modes.push(mode);
                    }
                    return Ok(());
                }
                "--delmode" => {
                    let (output, mode) = (value(1), value(2));
                    self.output(&output).modes.retain(|m| *m != mode);
                    return Ok(());
                }
                "--rmmode" => {
                    let mode = value(1);
                    if self.outputs.iter().any(|o| o.modes.contains(&mode)) {
                        return Err("BadAccess".to_owned());
                    }
                    self.modes.remove(&mode);
                    return Ok(());
                }
                "--setprovideroutputsource" => {
                    if value(2) == "0x0" {
                        self.unbound = true;
                        self.outputs.retain(|o| o.name != "Virtual-1-1");
                    } else if self.unbound {
                        self.unbound = false;
                        self.outputs.push(Output {
                            name: "Virtual-1-1".to_owned(),
                            connector_id: Some(36),
                            ..Default::default()
                        });
                    }
                    return Ok(());
                }
                _ => {}
            }
            let mut fb = None;
            let mut current = String::new();
            let mut i = 0;
            while i < args.len() {
                let value = args.get(i + 1).cloned().unwrap_or_default();
                let name = current.clone();
                let taken = match args[i].as_str() {
                    "--output" => {
                        current = value;
                        2
                    }
                    "--fb" => {
                        fb = parse_size(&value);
                        2
                    }
                    "--off" => {
                        if self.transformed.contains(&name) {
                            self.stale_transforms.push(name.clone());
                        }
                        let o = self.output(&name);
                        o.area = None;
                        o.current = None;
                        o.rotation = None;
                        1
                    }
                    "--primary" => {
                        for o in self.outputs.iter_mut() {
                            o.primary = o.name == name;
                        }
                        1
                    }
                    "--transform" => {
                        assert_eq!(value, "none");
                        let o = self.output(&name);
                        // RRSetCrtcConfig with a mode the output no longer has.
                        if !o.connected || o.modes.is_empty() {
                            return Err("BadMatch".to_owned());
                        }
                        if let (Some((x, y, _, _)), Some(mode)) = (o.area, o.current.clone()) {
                            let (w, h) = parse_size(&mode).unwrap();
                            o.area = Some((x, y, w, h));
                        }
                        self.transformed.retain(|o| o != &name);
                        2
                    }
                    "--mode" | "--scale-from" => {
                        if args[i] == "--mode" {
                            if !self.output(&name).modes.contains(&value) {
                                return Err(format!("cannot find mode {}", value));
                            }
                            let clock = self.modes.get(&value).copied().unwrap_or(0);
                            if self.refuses_above_khz.map_or(false, |max| clock > max) {
                                return Err("Configure crtc 1 failed".to_owned());
                            }
                        }
                        if args[i] == "--scale-from" && !self.transformed.contains(&name) {
                            self.transformed.push(name.clone());
                        }
                        let (w, h) = parse_size(&value).unwrap();
                        let o = self.output(&name);
                        let (x, y) = o.area.map_or((0, 0), |(x, y, _, _)| (x, y));
                        o.area = Some((x, y, w, h));
                        if args[i] == "--mode" {
                            o.current = Some(value.clone());
                        }
                        2
                    }
                    "--pos" => {
                        let (x, y) = parse_size(&value).unwrap();
                        let o = self.output(&name);
                        if let Some((_, _, w, h)) = o.area {
                            o.area = Some((x, y, w, h));
                        }
                        2
                    }
                    "--rate" => {
                        self.output(&name).rate = value.parse().ok();
                        2
                    }
                    // On at the mode it likes; off, without a monitor.
                    "--auto" => {
                        let o = self.output(&name);
                        match (o.connected, o.modes.first().cloned()) {
                            (true, Some(mode)) => {
                                let (w, h) = parse_size(&mode).unwrap();
                                let (x, y) = o.area.map_or((0, 0), |(x, y, _, _)| (x, y));
                                o.area = Some((x, y, w, h));
                                o.current = Some(mode);
                            }
                            _ => {
                                o.area = None;
                                o.current = None;
                            }
                        }
                        1
                    }
                    // After --mode: turned a quarter, the output shows its mode on its side.
                    "--rotate" => {
                        let o = self.output(&name);
                        o.rotation = (value != "normal").then(|| value.clone());
                        if let (Some((x, y, w, h)), "left" | "right") = (o.area, value.as_str()) {
                            o.area = Some((x, y, h, w));
                        }
                        2
                    }
                    other => panic!("xrandr {}", other),
                };
                i += taken;
            }
            if fb.or_else(|| randr::extent(&self.outputs)) != before {
                self.screen_resizes += 1;
            }
            Ok(())
        }
    }

    // "1920x1080", or a mode of ours named after its size, "2560x1440_60.00_rustdesk".
    fn parse_size(s: &str) -> Option<(i32, i32)> {
        let (w, h) = s.split_once('x')?;
        let h = h.split(|c: char| !c.is_ascii_digit()).next()?;
        Some((w.parse().ok()?, h.parse().ok()?))
    }

    struct FakeRoot(Arc<Mutex<World>>);

    impl Root for FakeRoot {
        fn call(&mut self, request: Request) -> ResultType<Response> {
            let mut w = self.0.lock().unwrap();
            if w.root_gone {
                bail!("Broken pipe");
            }
            match request {
                Request::Status => {
                    w.statuses += 1;
                    if w.taken_back_at == Some(w.statuses) {
                        w.taken_back = true;
                    }
                    Ok(Response {
                        free: 1,
                        released: w.root_released.clone(),
                        taken_back: w.taken_back,
                        ..Default::default()
                    })
                }
                Request::Reserve { .. } if w.refuse_reserve => bail!("No free virtual display"),
                Request::Reserve { skip_vkms } => {
                    w.reservations.push(skip_vkms);
                    let Some(port) = w
                        .ports
                        .iter()
                        .find(|p| {
                            !(skip_vkms && p.kind == Kind::Vkms)
                                && !w.reserved.iter().any(|r| r == p.connector)
                        })
                        .cloned()
                    else {
                        bail!("No free connector for a virtual display");
                    };
                    w.reserved.push(port.connector.to_owned());
                    Ok(Response {
                        connector: port.connector.to_owned(),
                        connector_id: Some(port.id),
                        kind: Some(port.kind),
                        max_khz: port.max_khz,
                        ..Default::default()
                    })
                }
                Request::Activate { connector } => {
                    let port = w
                        .ports
                        .iter()
                        .find(|p| p.connector == connector)
                        .cloned()
                        .unwrap();
                    w.lit.push(connector);
                    if w.hotplug_events {
                        w.show(&port);
                    } else {
                        w.lit_unseen = Some(port);
                    }
                    Ok(Response::default())
                }
                Request::Release { connector } => {
                    if let Some(port) = w.ports.iter().find(|p| p.connector == connector).cloned()
                    {
                        w.unplug(port.output);
                    }
                    w.reserved.retain(|r| *r != connector);
                    if w.link_breaks_on_release {
                        bail!("Broken pipe");
                    }
                    w.released.push(connector);
                    Ok(Response::default())
                }
                Request::Takeover { .. } if w.refuse_takeover => bail!("not supported"),
                Request::Takeover { .. } if w.taken_back => {
                    bail!("The person at this device took the desktop back")
                }
                Request::Takeover { restore } => {
                    assert!(restore.valid());
                    w.armed = Some(restore);
                    Ok(Response::default())
                }
                Request::Restored => {
                    w.armed = None;
                    w.taken_back = false;
                    w.restored += 1;
                    Ok(Response::default())
                }
                // GNOME's, not this backend's.
                Request::Hold { .. } | Request::LockInput | Request::TakenBack => unreachable!(),
            }
        }

        fn lost(&self) -> bool {
            self.0.lock().unwrap().root_gone
        }
    }

    struct FakeDesktop(Arc<Mutex<World>>);

    impl Desktop for FakeDesktop {
        fn outputs(&mut self) -> ResultType<Vec<Output>> {
            Ok(self.0.lock().unwrap().outputs.clone())
        }

        fn probe(&mut self) {
            let mut w = self.0.lock().unwrap();
            w.probes += 1;
            if let Some(port) = w.lit_unseen.clone() {
                w.show(&port);
            }
        }

        fn xrandr(&mut self, args: &[String]) -> ResultType<()> {
            let mut w = self.0.lock().unwrap();
            w.xrandr_calls.push(args.join(" "));
            if w.ignores_xrandr {
                return Ok(());
            }
            if let Err(e) = w.run_xrandr(args) {
                bail!(e);
            }
            Ok(())
        }

        fn set_primary(&mut self, name: &str) -> ResultType<()> {
            let mut w = self.0.lock().unwrap();
            if w.ignores_xrandr {
                return Ok(());
            }
            for o in w.outputs.iter_mut() {
                o.primary = o.name == name;
            }
            Ok(())
        }

        fn global_scale(&mut self) -> Option<f64> {
            self.0.lock().unwrap().global_scale
        }

        fn set_global_scale(&mut self, scale: f64) -> ResultType<()> {
            let mut w = self.0.lock().unwrap();
            w.global_scale = Some(scale);
            w.scales_put_back.push(scale);
            w.xrandr_calls.push(format!("scale {}", scale));
            Ok(())
        }

        fn providers(&mut self) -> ResultType<Vec<Provider>> {
            let w = self.0.lock().unwrap();
            let primary = Provider {
                id: "0x47".to_owned(),
                source_output: true,
                sink_output: true,
                associated: if w.unbound { 0 } else { 1 },
                name: "modesetting".to_owned(),
            };
            let vkms = Provider {
                id: "0x9b".to_owned(),
                sink_output: true,
                associated: if w.unbound { 0 } else { 1 },
                name: "modesetting".to_owned(),
                ..Default::default()
            };
            Ok(if w.ports.iter().any(|p| p.kind == Kind::Vkms) {
                vec![primary, vkms]
            } else {
                vec![primary]
            })
        }

        fn pointer(&mut self) -> Option<(i32, i32)> {
            Some(self.0.lock().unwrap().pointer)
        }

        fn warp(&mut self, at: (i32, i32)) {
            self.0.lock().unwrap().pointer = at;
        }

        fn clear_transforms(&mut self) {
            let mut w = self.0.lock().unwrap();
            w.transforms_cleared += 1;
            w.stale_transforms.clear();
        }

        fn sleep(&mut self, _: Duration) {}
    }

    fn backend(world: &Arc<Mutex<World>>) -> X11Backend<FakeRoot, FakeDesktop> {
        X11Backend::new(
            FakeRoot(world.clone()),
            FakeDesktop(world.clone()),
            1,
            Some(Kind::Scaled),
        )
    }

    fn spec(width: u32, height: u32) -> Spec {
        Spec {
            width,
            height,
            scale: 1.0,
            mode: Mode::Shared,
        }
    }

    fn nested_spec(width: u32, height: u32) -> Spec {
        Spec {
            mode: Mode::Nested,
            ..spec(width, height)
        }
    }

    #[derive(Default)]
    struct NestLog {
        // Each output a session was started on, and the area it was given: the size `Xephyr`
        // takes.
        started: Vec<(String, randr::Area)>,
        // Where each session was last placed: shown over an area, or hidden.
        placed: HashMap<String, Option<randr::Area>>,
        stopped: Vec<String>,
        running: usize,
        // Sessions to report ended on their own, at the next look.
        ending: Vec<String>,
    }

    // A nest that records what it was asked to do and hands out `:81`, `:82`, … instead of
    // spawning a real `Xephyr`.
    struct FakeNest(Arc<Mutex<NestLog>>);

    impl Nest for FakeNest {
        fn start(&mut self, output: &str, area: randr::Area) -> ResultType<String> {
            let mut l = self.0.lock().unwrap();
            l.started.push((output.to_owned(), area));
            l.running += 1;
            Ok(format!(":{}", 80 + l.started.len()))
        }
        fn place(&mut self, output: &str, area: Option<randr::Area>) {
            self.0.lock().unwrap().placed.insert(output.to_owned(), area);
        }
        fn ended(&mut self) -> Vec<String> {
            let mut l = self.0.lock().unwrap();
            let ended = std::mem::take(&mut l.ending);
            l.running = l.running.saturating_sub(ended.len());
            ended
        }
        fn stop(&mut self, output: &str) {
            let mut l = self.0.lock().unwrap();
            l.stopped.push(output.to_owned());
            l.running = l.running.saturating_sub(1);
        }
        fn stop_all(&mut self) {
            self.0.lock().unwrap().running = 0;
        }
    }

    fn nested_backend(
        world: &Arc<Mutex<World>>,
        log: &Arc<Mutex<NestLog>>,
    ) -> X11Backend<FakeRoot, FakeDesktop> {
        X11Backend::new_with_nest(
            FakeRoot(world.clone()),
            FakeDesktop(world.clone()),
            1,
            Some(Kind::Vkms),
            Box::new(FakeNest(log.clone())),
        )
    }

    fn placed(log: &Arc<Mutex<NestLog>>, name: &str) -> Option<Option<randr::Area>> {
        log.lock().unwrap().placed.get(name).copied()
    }

    #[test]
    fn a_nested_display_starts_a_session_on_its_output_and_reports_it() {
        let world = World::with_vkms();
        let log = Arc::new(Mutex::new(NestLog::default()));
        let mut b = nested_backend(&world, &log);
        let name = b.plug_in(&nested_spec(2560, 1440), &mut |_| {}).unwrap();
        let shown = area(&world, &name);
        assert_eq!(shown.map(|(_, _, w, h)| (w, h)), Some((2560, 1440)));
        // Started over the display's output once that ran at the display's size, and shown there.
        let started = log.lock().unwrap().started.clone();
        assert_eq!(started.len(), 1);
        assert_eq!((started[0].0.as_str(), Some(started[0].1)), (name.as_str(), shown));
        assert_eq!(placed(&log, &name), Some(shown));
        // It is reported for input routing: its X display, and where its output is.
        let targets = b.nested();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].name, name);
        assert_eq!(targets[0].display, ":81");
        let t = &targets[0];
        assert_eq!(Some((t.x, t.y, t.w, t.h)), shown);
        // Removing stops the nested session before the output goes, and it is no longer reported.
        b.plug_out(&name).unwrap();
        assert_eq!(log.lock().unwrap().stopped, vec![name]);
        assert!(b.nested().is_empty());
        assert_eq!(log.lock().unwrap().running, 0);
    }

    #[test]
    fn a_shared_display_starts_no_nested_session() {
        let world = World::with_vkms();
        let log = Arc::new(Mutex::new(NestLog::default()));
        let mut b = nested_backend(&world, &log);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        assert!(log.lock().unwrap().started.is_empty());
        assert!(log.lock().unwrap().placed.is_empty());
        assert!(b.nested().is_empty());
    }

    #[test]
    fn a_nested_session_hides_where_the_desktop_puts_its_display_over_a_monitor() {
        let world = World::with_vkms();
        let log = Arc::new(Mutex::new(NestLog::default()));
        let mut b = nested_backend(&world, &log);
        let name = b.plug_in(&nested_spec(2560, 1440), &mut |_| {}).unwrap();
        // The desktop mirrors it onto the monitor: hidden at the next look, before anything is
        // put back; input there stays the session's, where the output now is.
        let monitor = area(&world, "HDMI-1").unwrap();
        world.lock().unwrap().output(&name).area = Some((monitor.0, monitor.1, 2560, 1440));
        b.maintain();
        assert_eq!(placed(&log, &name), Some(None));
        // Nor does input there go to it: the area is the monitor's, and so is input to it.
        assert!(b.nested().is_empty());
        // Put back beside it (the second look at a moved layout): shown again, there.
        b.maintain();
        let back = area(&world, &name);
        assert_ne!(back, Some((monitor.0, monitor.1, 2560, 1440)));
        assert_eq!(placed(&log, &name), Some(back));
        let t = &b.nested()[0];
        assert_eq!(Some((t.x, t.y, t.w, t.h)), back);
        // Turned off behind our back is gone, not shown anywhere.
        world.lock().unwrap().output(&name).area = None;
        b.maintain();
        assert_eq!(placed(&log, &name), Some(None));
    }

    #[test]
    fn a_resized_nested_display_shows_its_session_at_the_new_size() {
        let world = World::with_vkms();
        let log = Arc::new(Mutex::new(NestLog::default()));
        let mut b = nested_backend(&world, &log);
        let name = b.plug_in(&nested_spec(2560, 1440), &mut |_| {}).unwrap();
        b.set_mode(&name, &nested_spec(1920, 1080)).unwrap();
        let resized = area(&world, &name);
        assert_eq!(resized.map(|(_, _, w, h)| (w, h)), Some((1920, 1080)));
        assert_eq!(placed(&log, &name), Some(resized));
        let t = &b.nested()[0];
        assert_eq!(Some((t.x, t.y, t.w, t.h)), resized);
    }

    #[test]
    fn a_display_whose_nested_session_ended_goes() {
        let world = World::with_vkms();
        let log = Arc::new(Mutex::new(NestLog::default()));
        let mut b = nested_backend(&world, &log);
        let name = b.plug_in(&nested_spec(2560, 1440), &mut |_| {}).unwrap();
        log.lock().unwrap().ending.push(name.clone());
        assert_eq!(b.maintain(), vec![name.clone()]);
        assert!(b.nested().is_empty());
        assert_eq!(area(&world, &name), None);
        // Nothing more to look after.
        assert!(b.maintain().is_empty());
    }

    fn privacy_spec(width: u32, height: u32) -> Spec {
        Spec {
            mode: Mode::Privacy,
            ..spec(width, height)
        }
    }

    // As the manager asks for one: an extra screen, then the desktop taken over.
    fn plug_in_privacy(
        b: &mut X11Backend<FakeRoot, FakeDesktop>,
        width: u32,
        height: u32,
    ) -> ResultType<String> {
        let name = b.plug_in(&spec(width, height), &mut |_| {})?;
        b.set_mode(&name, &privacy_spec(width, height))?;
        Ok(name)
    }

    // The mode of ours for 2560x1440 at the office's rate.
    const OURS_1440: &str = "2560x1440_60.00_rustdesk";
    const TAKE_OVER: &str =
        "--output HDMI-1 --off --output Virtual-1-1 --mode 2560x1440_60.00_rustdesk --pos 0x0 \
         --primary";
    const GIVE_BACK: &str = "--output HDMI-1 --mode 1920x1080 --rate 60.00 --rotate normal \
                             --pos 0x0 --primary --output Virtual-1-1 --off";

    fn armed(world: &Arc<Mutex<World>>) -> Option<Restore> {
        world.lock().unwrap().armed.clone()
    }

    #[test]
    fn a_display_for_the_desktop_takes_it_over_and_gives_it_back_in_one_change_each() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        let name = plug_in_privacy(&mut b, 2560, 1440).unwrap();
        assert_eq!(name, "Virtual-1-1");
        // The monitor off, ours alone at the origin and primary.
        assert_eq!(area(&world, "HDMI-1"), None);
        assert_eq!(area(&world, "Virtual-1-1"), Some((0, 0, 2560, 1440)));
        assert_eq!(primary(&world), "Virtual-1-1");
        // Laid out beside the monitor first, the mode taken; then over in one change, the root
        // service told beforehand how it goes back.
        let c = calls(&world);
        assert_eq!(
            c[c.len() - 2..],
            [
                format!("--output Virtual-1-1 --mode {} --pos 1920x0", OURS_1440),
                TAKE_OVER.to_owned(),
            ]
        );
        let restore = armed(&world).unwrap();
        assert_eq!(restore.args().join(" "), GIVE_BACK);
        // Kept so.
        assert!(b.maintain().is_empty());
        assert_eq!(calls(&world).len(), c.len());

        // The pointer on ours, which goes: given back in one change, the pointer home.
        world.lock().unwrap().pointer = (2400, 900);
        b.plug_out("Virtual-1-1").unwrap();
        let c = calls(&world);
        assert!(c.contains(&GIVE_BACK.to_owned()), "{:?}", c);
        assert!(!c.iter().any(|call| call == "--output Virtual-1-1 --off"));
        let w = world.lock().unwrap();
        assert_eq!(w.output_ref("HDMI-1").area, Some((0, 0, 1920, 1080)));
        assert!(w.output_ref("HDMI-1").primary);
        assert_eq!(w.output_ref("Virtual-1-1").area, None);
        assert_eq!(w.pointer, (960, 540));
        assert_eq!((w.armed.clone(), w.restored), (None, 1));
        assert_eq!(w.released, vec!["Virtual-1"]);
    }

    #[test]
    fn monitors_the_root_service_turned_on_for_ctrl_p_are_not_turned_off_again() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        let name = plug_in_privacy(&mut b, 2560, 1440).unwrap();
        // Ctrl+P just after the session asked how the root service is, which turns the monitor
        // on again.
        {
            let mut w = world.lock().unwrap();
            w.taken_back_at = Some(w.statuses + 2);
            w.run_xrandr(&args(&[
                "--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0", "--primary",
            ]))
            .unwrap();
        }
        assert!(b.maintain().is_empty());
        assert_eq!(b.usage(&name), Some(Mode::Shared));
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        let w = world.lock().unwrap();
        assert_eq!((w.restored, w.taken_back), (1, false));
    }

    #[test]
    fn a_resize_right_after_ctrl_p_does_not_turn_the_monitors_off_again() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        let name = plug_in_privacy(&mut b, 2560, 1440).unwrap();
        // Ctrl+P: the root service turns the monitor on; the window on the controlling side is
        // resized before the next look.
        {
            let mut w = world.lock().unwrap();
            w.taken_back = true;
            w.run_xrandr(&args(&[
                "--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0", "--primary",
            ]))
            .unwrap();
        }
        let e = b.set_mode(&name, &privacy_spec(2048, 1152)).unwrap_err();
        assert_eq!(e.to_string(), crate::vdisplay::manager::TAKEN_BACK);
        assert_eq!(b.usage(&name), Some(Mode::Shared));
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        assert_eq!(area(&world, &name), Some((1920, 0, 2048, 1152)));
        let w = world.lock().unwrap();
        assert_eq!((w.restored, w.taken_back), (1, false));
    }

    #[test]
    fn a_desktop_taken_back_is_taken_over_again_at_once() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        plug_in_privacy(&mut b, 2560, 1440).unwrap();
        // The desktop turns the monitor on again, primary.
        world
            .lock()
            .unwrap()
            .run_xrandr(&args(&[
                "--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0", "--primary",
            ]))
            .unwrap();
        assert!(b.maintain().is_empty());
        assert_eq!(area(&world, "HDMI-1"), None);
        assert_eq!(primary(&world), "Virtual-1-1");
        // Every time.
        for _ in 0..5 {
            world
                .lock()
                .unwrap()
                .run_xrandr(&args(&["--output", "HDMI-1", "--mode", "1920x1080"]))
                .unwrap();
            assert!(b.maintain().is_empty());
            assert_eq!(area(&world, "HDMI-1"), None);
        }
        assert!(armed(&world).is_some());
    }

    #[test]
    fn a_desktop_the_person_at_the_device_took_back_is_given_back_for_good() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        let name = plug_in_privacy(&mut b, 2560, 1440).unwrap();
        // Ctrl+P at the device: the root service says so at the next look, and refuses to have
        // the desktop taken over again meanwhile.
        world.lock().unwrap().taken_back = true;
        assert!(b.maintain().is_empty());
        assert_eq!(b.usage(&name), Some(Mode::Shared));
        {
            let w = world.lock().unwrap();
            assert_eq!(w.output_ref("HDMI-1").area, Some((0, 0, 1920, 1080)));
            assert!(w.output_ref("HDMI-1").primary);
            // The display stays, an extra screen beside the monitor.
            assert_eq!(w.output_ref(&name).area, Some((1920, 0, 2560, 1440)));
            assert_eq!((w.armed.clone(), w.restored, w.taken_back), (None, 1, false));
        }
        // Not taken over again by itself.
        for _ in 0..3 {
            assert!(b.maintain().is_empty());
        }
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        // Asked again, it is.
        b.set_mode(&name, &privacy_spec(2560, 1440)).unwrap();
        assert_eq!(b.usage(&name), Some(Mode::Privacy));
        assert_eq!(area(&world, "HDMI-1"), None);
    }

    #[test]
    fn a_desktop_that_could_not_be_given_back_is_still_taken_over_and_given_back_later() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        let name = plug_in_privacy(&mut b, 2560, 1440).unwrap();
        // Nowhere to give it back to, as asked by its session or taken back at the device.
        world.lock().unwrap().unplug("HDMI-1");
        assert!(b.set_mode(&name, &spec(2560, 1440)).is_err());
        assert_eq!(b.usage(&name), Some(Mode::Privacy));
        world.lock().unwrap().taken_back = true;
        assert!(b.maintain().is_empty());
        assert_eq!(b.usage(&name), Some(Mode::Privacy));
        assert_eq!(world.lock().unwrap().restored, 0);
        {
            let mut w = world.lock().unwrap();
            let hdmi = w.output("HDMI-1");
            hdmi.connected = true;
            hdmi.modes = vec!["1920x1080".to_owned()];
        }
        assert!(b.maintain().is_empty());
        assert_eq!(b.usage(&name), Some(Mode::Shared));
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        let w = world.lock().unwrap();
        assert_eq!((w.restored, w.taken_back), (1, false));
    }

    #[test]
    fn a_desktop_that_cannot_be_kept_taken_over_is_given_back_and_its_display_goes() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        plug_in_privacy(&mut b, 2560, 1440).unwrap();
        {
            let mut w = world.lock().unwrap();
            w.run_xrandr(&args(&["--output", "HDMI-1", "--mode", "1920x1080", "--pos", "0x0"]))
                .unwrap();
            w.ignores_xrandr = true;
        }
        for _ in 1..TAKEOVER_TRIES {
            assert!(b.maintain().is_empty());
        }
        assert_eq!(b.maintain(), vec!["Virtual-1-1"]);
        let w = world.lock().unwrap();
        assert_eq!(w.released, vec!["Virtual-1"]);
        assert!(w.output_ref("HDMI-1").area.is_some());
        assert_eq!((w.armed.clone(), w.restored), (None, 1));
    }

    #[test]
    fn without_a_root_service_to_give_it_back_the_desktop_is_not_taken_over() {
        let world = World::with_vkms();
        world.lock().unwrap().refuse_takeover = true;
        let mut b = backend(&world);
        assert!(plug_in_privacy(&mut b, 2560, 1440).is_err());
        let c = calls(&world);
        assert!(!c.iter().any(|call| call.contains("--output HDMI-1 --off")), "{:?}", c);
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        assert_eq!(primary(&world), "HDMI-1");
        // The display stays, an extra screen beside the monitor.
        assert_eq!(b.usage("Virtual-1-1"), Some(Mode::Shared));
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 2560, 1440)));
        let w = world.lock().unwrap();
        assert!(w.released.is_empty());
        // Should it have taken it after all, answering too late, it forgets it.
        assert_eq!(w.restored, 1);
    }

    #[test]
    fn a_display_held_switches_between_beside_the_monitor_and_taking_the_desktop_over() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 2560, 1440)));
        assert!(armed(&world).is_none());
        b.set_mode("Virtual-1-1", &privacy_spec(2560, 1440)).unwrap();
        assert_eq!(area(&world, "HDMI-1"), None);
        assert_eq!(area(&world, "Virtual-1-1"), Some((0, 0, 2560, 1440)));
        assert_eq!(primary(&world), "Virtual-1-1");
        assert!(armed(&world).is_some());
        b.set_mode("Virtual-1-1", &spec(2560, 1440)).unwrap();
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 2560, 1440)));
        assert_eq!(primary(&world), "HDMI-1");
        assert_eq!((armed(&world), world.lock().unwrap().restored), (None, 1));
        // Back as it was: kept so.
        assert!(b.maintain().is_empty());
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 2560, 1440)));
    }

    #[test]
    fn the_root_service_gone_the_desktop_taken_over_is_given_back() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        plug_in_privacy(&mut b, 2560, 1440).unwrap();
        world.lock().unwrap().root_gone = true;
        assert_eq!(b.maintain(), vec!["Virtual-1-1"]);
        let c = calls(&world);
        assert!(c.contains(&GIVE_BACK.to_owned()), "{:?}", c);
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        assert_eq!(primary(&world), "HDMI-1");
    }

    // The office with vkms, and a second monitor, DP-2, right of HDMI-1; or without one yet.
    fn with_two_monitors(second_on: bool) -> Arc<Mutex<World>> {
        let world = World::with_vkms();
        world.lock().unwrap().outputs.push(Output {
            name: "DP-2".to_owned(),
            connected: second_on,
            area: second_on.then_some((1920, 0, 1920, 1080)),
            modes: if second_on {
                vec!["1920x1080".to_owned()]
            } else {
                Vec::new()
            },
            current: second_on.then(|| "1920x1080".to_owned()),
            rate: Some(60.0),
            connector_id: Some(96),
            ..Default::default()
        });
        world
    }

    #[test]
    fn a_monitor_gone_meanwhile_is_left_out_and_the_others_come_back() {
        let world = with_two_monitors(true);
        let mut b = backend(&world);
        plug_in_privacy(&mut b, 2560, 1440).unwrap();
        assert_eq!(area(&world, "DP-2"), None);
        assert_eq!(area(&world, "Virtual-1-1"), Some((0, 0, 2560, 1440)));
        world.lock().unwrap().unplug("DP-2");
        b.plug_out("Virtual-1-1").unwrap();
        assert!(calls(&world).contains(&GIVE_BACK.to_owned()), "{:?}", calls(&world));
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        assert_eq!(area(&world, "Virtual-1-1"), None);
        assert_eq!(primary(&world), "HDMI-1");
        assert_eq!(world.lock().unwrap().restored, 1);
    }

    #[test]
    fn without_a_monitor_to_give_it_back_to_the_display_stays_until_there_is_one() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        plug_in_privacy(&mut b, 2560, 1440).unwrap();
        // The monitor went, through a KVM switch, say.
        world.lock().unwrap().unplug("HDMI-1");
        assert!(b.plug_out("Virtual-1-1").is_err());
        assert_eq!(area(&world, "Virtual-1-1"), Some((0, 0, 2560, 1440)));
        assert_eq!(primary(&world), "Virtual-1-1");
        assert!(armed(&world).is_some());
        assert!(world.lock().unwrap().released.is_empty());
        // Kept so, and not taken for gone.
        assert!(b.maintain().is_empty());
        // It is back: the desktop goes back to it.
        {
            let mut w = world.lock().unwrap();
            let hdmi = w.output("HDMI-1");
            hdmi.connected = true;
            hdmi.modes = vec!["1920x1080".to_owned()];
        }
        b.plug_out("Virtual-1-1").unwrap();
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        assert_eq!(area(&world, "Virtual-1-1"), None);
        assert_eq!(world.lock().unwrap().released, vec!["Virtual-1"]);
    }

    #[test]
    fn a_monitor_plugged_in_meanwhile_comes_on_beside_the_others() {
        let world = with_two_monitors(false);
        let mut b = backend(&world);
        plug_in_privacy(&mut b, 2560, 1440).unwrap();
        // Plugged in, and turned on by the desktop: off again, the desktop being taken over.
        {
            let mut w = world.lock().unwrap();
            let dp = w.output("DP-2");
            dp.connected = true;
            dp.modes = vec!["1920x1080".to_owned()];
            dp.area = Some((2560, 0, 1920, 1080));
            dp.current = Some("1920x1080".to_owned());
        }
        assert!(b.maintain().is_empty());
        assert_eq!(area(&world, "DP-2"), None);
        b.plug_out("Virtual-1-1").unwrap();
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        assert_eq!(area(&world, "DP-2"), Some((1920, 0, 1920, 1080)));
        assert_eq!(primary(&world), "HDMI-1");
    }

    #[test]
    fn a_monitor_turned_on_its_side_comes_back_so() {
        let world = World::with_vkms();
        {
            let mut w = world.lock().unwrap();
            let hdmi = w.output("HDMI-1");
            hdmi.area = Some((0, 0, 1080, 1920));
            hdmi.rotation = Some("left".to_owned());
        }
        let mut b = backend(&world);
        plug_in_privacy(&mut b, 2560, 1440).unwrap();
        assert_eq!(area(&world, "HDMI-1"), None);
        b.plug_out("Virtual-1-1").unwrap();
        let w = world.lock().unwrap();
        assert_eq!(w.output_ref("HDMI-1").area, Some((0, 0, 1080, 1920)));
        assert_eq!(w.output_ref("HDMI-1").rotation.as_deref(), Some("left"));
    }

    fn area(world: &Arc<Mutex<World>>, name: &str) -> Option<(i32, i32, i32, i32)> {
        world.lock().unwrap().output(name).area
    }

    fn primary(world: &Arc<Mutex<World>>) -> String {
        let w = world.lock().unwrap();
        w.outputs
            .iter()
            .find(|o| o.primary)
            .map(|o| o.name.clone())
            .unwrap_or_default()
    }

    fn calls(world: &Arc<Mutex<World>>) -> Vec<String> {
        world.lock().unwrap().xrandr_calls.clone()
    }

    #[test]
    fn a_vkms_display_runs_a_mode_of_ours_at_the_rate_of_the_others() {
        let world = World::with_vkms();
        world.lock().unwrap().output("HDMI-1").rate = Some(59.95);
        let mut b = backend(&world);
        let mut lit_with = Vec::new();
        let w = world.clone();
        let name = b
            .plug_in(&spec(2560, 1440), &mut |_| {
                lit_with = w.lock().unwrap().output("Virtual-1-1").modes.clone();
            })
            .unwrap();
        assert_eq!(name, "Virtual-1-1");
        // The mode was there before the connector was lit.
        assert_eq!(lit_with, vec!["2560x1440_59.95_rustdesk"]);
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 2560, 1440)));
        assert_eq!(primary(&world), "HDMI-1");
        let modeline = Modeline::cvt_rb(2560, 1440, 59.95);
        assert_eq!(
            calls(&world),
            vec![
                modeline.newmode_args().join(" "),
                "--addmode Virtual-1-1 2560x1440_59.95_rustdesk".to_owned(),
                "--output Virtual-1-1 --mode 2560x1440_59.95_rustdesk --pos 1920x0".to_owned(),
            ]
        );
        // Taken away: off, its mode gone, and the connector given back.
        b.plug_out("Virtual-1-1").unwrap();
        let w = world.lock().unwrap();
        assert_eq!(w.released, vec!["Virtual-1"]);
        assert!(w.modes.is_empty());
        assert!(w.outputs[2].area.is_none() && w.outputs[2].modes.is_empty());
        assert_eq!(w.transforms_cleared, 0, "nothing was scaled");
    }

    #[test]
    fn a_mode_left_by_a_server_that_went_away_is_used_again() {
        let world = World::with_vkms();
        let line = Modeline::cvt_rb(2560, 1440, 60.0);
        world
            .lock()
            .unwrap()
            .modes
            .insert(line.name.clone(), line.clock_khz);
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 2560, 1440)));
    }

    #[test]
    fn without_vkms_in_the_x_server_a_physical_connector_is_used_from_then_on() {
        let world = World::with_vkms();
        world.lock().unwrap().outputs.retain(|o| o.name != "Virtual-1-1");
        let mut b = backend(&world);
        assert_eq!(b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap(), "DP-1");
        {
            let w = world.lock().unwrap();
            assert_eq!(w.released, vec!["Virtual-1"]);
            assert_eq!(w.reservations, vec![false, true]);
            assert_eq!(w.lit, vec!["DP-1"]);
        }
        b.plug_out("DP-1").unwrap();
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        assert_eq!(world.lock().unwrap().reservations, vec![false, true, true]);
    }

    #[test]
    fn an_x_server_that_binds_no_card_added_to_it_has_vkms_bound() {
        let world = World::with_vkms();
        {
            let mut w = world.lock().unwrap();
            w.unbound = true;
            w.outputs.retain(|o| o.name != "Virtual-1-1");
        }
        let mut b = backend(&world);
        assert_eq!(b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap(), "Virtual-1-1");
        assert!(calls(&world).contains(&"--setprovideroutputsource 0x9b 0x47".to_owned()));
        assert!(world.lock().unwrap().released.is_empty());
    }

    #[test]
    fn a_mode_the_port_refuses_gives_way_to_a_smaller_one() {
        let world = World::with_hdmi();
        world.lock().unwrap().refuses_above_khz = Some(200_000);
        let mut b = backend(&world);
        assert_eq!(b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap(), "HDMI-2");
        // 2560x1440 at 241.7 MHz was refused; 2048x1152 at 158 MHz is what it runs.
        assert_eq!(area(&world, "HDMI-2"), Some((1920, 0, 2048, 1152)));
        let mut w = world.lock().unwrap();
        assert!(!w.modes.contains_key("2560x1440_60.00_rustdesk"));
        assert!(w.modes.contains_key("2048x1152_60.00_rustdesk"));
        assert_eq!(w.output("HDMI-2").modes.len(), 2, "its own and ours");
    }

    #[test]
    fn modes_a_port_is_known_not_to_carry_are_not_tried() {
        // 300 MHz: no 3840x2160 at 60 Hz (533 MHz); the first that fits is tried first.
        let lines = ladder(&spec(3840, 2160), 60.0, Some(300_000));
        assert!(lines.iter().all(|l| l.clock_khz <= 300_000));
        assert_eq!(lines[0].size(), (2456, 1382));
        assert!(lines.len() <= MODE_TRIES);
        assert_eq!(lines.last().unwrap().size().0, 1024);
        // Without a limit, the size asked for comes first.
        assert_eq!(ladder(&spec(3840, 2160), 60.0, None)[0].size(), (3840, 2160));
        // A small one is tried as it is.
        assert_eq!(ladder(&spec(800, 600), 60.0, None).len(), 1);
        // Nothing fits: nothing is tried.
        let world = World::with_hdmi();
        world.lock().unwrap().ports[0].max_khz = Some(10_000);
        let mut b = backend(&world);
        assert!(b.plug_in(&spec(2560, 1440), &mut |_| {}).is_err());
        let w = world.lock().unwrap();
        assert!(w.lit.is_empty() && w.xrandr_calls.is_empty());
        assert_eq!(w.released, vec!["HDMI-A-2"]);
    }

    #[test]
    fn a_display_is_too_small_for_the_desktop_scale_never() {
        // Already big enough, or no scale to keep.
        assert_eq!(big_enough(1504, 858, 1.0), (1504, 858));
        assert_eq!(big_enough(2048, 1152, 1.5), (2048, 1152));
        assert_eq!(big_enough(800, 600, f64::NAN), (800, 600));
        assert_eq!(big_enough(4000, 3000, 3.5), (4000, 3000));
        // Under 1024x768 the desktop allows no scale at all.
        assert_eq!(big_enough(800, 600, 1.0), (1024, 768));
        // At 125% it wants 1280x960: grown whole, the width to eights.
        assert_eq!(big_enough(1510, 858, 1.25), (1696, 960));
        // A scale between the steps needs the next step up.
        assert_eq!(big_enough(1280, 960, 1.1), (1280, 960));
        assert_eq!(big_enough(1280, 900, 1.3), (1640, 1152));
    }

    // dde-daemon display1 `calcMaxScaleFactor`: the largest scale Deepin allows an output of
    // `w`x`h`.
    fn deepin_max_scale(w: u32, h: u32) -> f64 {
        let most = (w as f64 / 1024.0).min(h as f64 / 768.0).min(3.0);
        let mut scale = 0.0;
        let mut i = 0;
        while i as f64 * 0.25 + 1.0 <= most {
            scale = 1.0 + i as f64 * 0.25;
            i += 1;
        }
        scale
    }

    #[test]
    fn a_display_made_big_enough_keeps_every_scale() {
        for scale in [1.0, 1.1, 1.25, 1.4, 1.5, 1.75, 2.0, 2.25, 2.5, 2.75, 3.0] {
            for (w, h) in [(480, 480), (800, 600), (1510, 858), (1366, 768), (3000, 500), (500, 3000)] {
                let (bw, bh) = big_enough(w, h, scale);
                // As the mode is made: the width in eights.
                let made = (bw - bw % 8, bh);
                assert!(
                    deepin_max_scale(made.0, made.1) >= scale,
                    "{}x{} at {} made {:?}",
                    w,
                    h,
                    scale,
                    made
                );
                // The shape kept, give or take the rounding.
                let shape = |a: u32, b: u32| a as f64 / b as f64;
                assert!((shape(bw, bh) / shape(w, h) - 1.0).abs() < 0.02, "{}x{} -> {}x{}", w, h, bw, bh);
            }
        }
    }

    #[test]
    fn a_display_is_made_big_enough_for_the_desktop_to_keep_its_scale() {
        let world = World::with_vkms();
        world.lock().unwrap().global_scale = Some(1.25);
        let mut b = backend(&world);
        b.plug_in(&spec(1510, 858), &mut |_| {}).unwrap();
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 1696, 960)));
        // Lit by the desktop at vkms's own 1024x768 first, it turned its scale down: put back as
        // soon as seen, before the display was even given its mode.
        {
            let w = world.lock().unwrap();
            assert_eq!(w.scales_put_back, vec![1.25]);
            assert_eq!(w.global_scale, Some(1.25));
            let at = |what: &str| w.xrandr_calls.iter().position(|c| c.contains(what)).unwrap();
            assert!(at("scale 1.25") < at("--output Virtual-1-1 --mode"));
        }
        b.set_mode("Virtual-1-1", &spec(1600, 1000)).unwrap();
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 1600, 1000)));
        // A desktop not held to a scale takes any size.
        world.lock().unwrap().global_scale = None;
        b.set_mode("Virtual-1-1", &spec(1000, 700)).unwrap();
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 1000, 700)));
    }

    #[test]
    fn resizing_a_display_of_our_mode_makes_a_new_one_and_drops_the_old() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        b.set_mode("Virtual-1-1", &spec(1920, 1080)).unwrap();
        assert_eq!(area(&world, "Virtual-1-1"), Some((1920, 0, 1920, 1080)));
        {
            let w = world.lock().unwrap();
            assert_eq!(
                w.modes.keys().collect::<Vec<_>>(),
                vec!["1920x1080_60.00_rustdesk"]
            );
        }
        // The same again changes nothing.
        let before = calls(&world).len();
        b.set_mode("Virtual-1-1", &spec(1920, 1080)).unwrap();
        assert_eq!(calls(&world).len(), before);
        // The same size at another rate, the monitor beside it being set to 75 Hz: another mode.
        world.lock().unwrap().output("HDMI-1").rate = Some(75.0);
        b.set_mode("Virtual-1-1", &spec(1920, 1080)).unwrap();
        let mut w = world.lock().unwrap();
        assert_eq!(
            w.output("Virtual-1-1").current.as_deref(),
            Some("1920x1080_75.00_rustdesk")
        );
        assert_eq!(w.modes.keys().collect::<Vec<_>>(), vec!["1920x1080_75.00_rustdesk"]);
    }

    #[test]
    fn a_mode_not_added_is_not_left_behind() {
        let world = World::with_vkms();
        // The X server makes the mode, but will not add it to the output.
        world.lock().unwrap().outputs.retain(|o| o.name != "Virtual-1-1");
        world.lock().unwrap().outputs.push(Output {
            name: "Virtual-1-1".to_owned(),
            connector_id: Some(36),
            ..Default::default()
        });
        struct NoAdd(FakeDesktop);
        impl Desktop for NoAdd {
            fn outputs(&mut self) -> ResultType<Vec<Output>> {
                self.0.outputs()
            }
            fn probe(&mut self) {}
            fn xrandr(&mut self, args: &[String]) -> ResultType<()> {
                if args[0] == "--addmode" {
                    bail!("BadMatch");
                }
                self.0.xrandr(args)
            }
            fn set_primary(&mut self, name: &str) -> ResultType<()> {
                self.0.set_primary(name)
            }
            fn providers(&mut self) -> ResultType<Vec<Provider>> {
                self.0.providers()
            }
            fn pointer(&mut self) -> Option<(i32, i32)> {
                None
            }
            fn warp(&mut self, _: (i32, i32)) {}
            fn clear_transforms(&mut self) {}
            fn sleep(&mut self, _: Duration) {}
        }
        let mut b = X11Backend::new(
            FakeRoot(world.clone()),
            NoAdd(FakeDesktop(world.clone())),
            1,
            Some(Kind::Vkms),
        );
        assert!(b.plug_in(&spec(2560, 1440), &mut |_| {}).is_err());
        let w = world.lock().unwrap();
        assert!(w.modes.is_empty());
        assert_eq!(w.released, vec!["Virtual-1"]);
    }

    #[test]
    fn the_pointer_leaves_a_display_that_goes_or_is_left() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        // On the virtual display when its session drops: to the middle of HDMI-1.
        world.lock().unwrap().pointer = (3000, 700);
        b.left("Virtual-1-1");
        assert_eq!(world.lock().unwrap().pointer, (960, 540));
        // Elsewhere, it stays.
        world.lock().unwrap().pointer = (100, 100);
        b.left("Virtual-1-1");
        assert_eq!(world.lock().unwrap().pointer, (100, 100));
        world.lock().unwrap().pointer = (2000, 10);
        b.plug_out("Virtual-1-1").unwrap();
        assert_eq!(world.lock().unwrap().pointer, (960, 540));
    }

    #[test]
    fn a_display_is_lit_laid_out_right_and_the_primary_kept() {
        let world = World::office();
        let mut b = backend(&world);
        let mut announced = Vec::new();
        let name = b
            .plug_in(&spec(2560, 1440), &mut |n| announced.push(n.to_owned()))
            .unwrap();
        assert_eq!(name, "DP-1");
        assert_eq!(announced, vec!["DP-1"]);
        assert_eq!(area(&world, "DP-1"), Some((1920, 0, 2560, 1440)));
        assert_eq!(primary(&world), "HDMI-1");
        // Scaled: no mode of ours. One call: our display placed and the primary output put back.
        assert_eq!(
            calls(&world),
            vec![
                "--output DP-1 --mode 1280x720 --scale-from 2560x1440 --pos 1920x0 \
                 --output HDMI-1 --primary"
            ]
        );
    }

    #[test]
    fn the_others_are_put_back_where_they_were_in_the_same_call() {
        let world = World::office();
        world.lock().unwrap().shoves_others = true;
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
        assert_eq!(area(&world, "DP-1"), Some((1920, 0, 2560, 1440)));
        assert_eq!(
            calls(&world),
            vec![
                "--output HDMI-1 --pos 0x0 --primary \
                 --output DP-1 --mode 1280x720 --scale-from 2560x1440 --pos 1920x0"
            ]
        );
    }

    #[test]
    fn the_name_is_announced_before_the_connector_is_lit() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        let w = world.clone();
        b.plug_in(&spec(2560, 1440), &mut |_| {
            assert!(w.lock().unwrap().lit.is_empty(), "announced after lighting");
        })
        .unwrap();
        assert_eq!(world.lock().unwrap().lit, vec!["Virtual-1"]);
    }

    #[test]
    fn resizing_and_removing_put_things_back() {
        let world = World::office();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        b.set_mode("DP-1", &spec(1920, 1080)).unwrap();
        assert_eq!(area(&world, "DP-1"), Some((1920, 0, 1920, 1080)));
        let resizes = world.lock().unwrap().screen_resizes;
        b.plug_out("DP-1").unwrap();
        let w = world.lock().unwrap();
        assert_eq!(w.outputs[1].area, None);
        assert_eq!(w.released, vec!["DP-1"]);
        assert!(w.outputs[0].primary);
        // The transform went while the output was still on, and the screen shrank once.
        assert!(w.stale_transforms.is_empty());
        assert_eq!(w.transforms_cleared, 1);
        assert_eq!(w.screen_resizes, resizes + 1);
        drop(w);
        // Gone already: nothing to do.
        b.plug_out("DP-1").unwrap();
        assert_eq!(world.lock().unwrap().released.len(), 1);
    }

    #[test]
    fn a_refused_reservation_touches_nothing() {
        let world = World::office();
        world.lock().unwrap().refuse_reserve = true;
        let mut b = backend(&world);
        let mut announced = false;
        assert!(b
            .plug_in(&spec(2560, 1440), &mut |_| announced = true)
            .is_err());
        assert!(!announced);
        assert!(calls(&world).is_empty());
    }

    #[test]
    fn a_connector_without_an_output_is_given_back() {
        let world = World::office();
        world.lock().unwrap().outputs.remove(1);
        let mut b = backend(&world);
        assert!(b.plug_in(&spec(2560, 1440), &mut |_| {}).is_err());
        assert_eq!(world.lock().unwrap().released, vec!["DP-1"]);
        assert!(world.lock().unwrap().lit.is_empty());
    }

    #[test]
    fn an_output_in_use_is_never_taken() {
        // The connector's output, as matched, has a monitor on: someone's, whatever the match.
        let world = World::office();
        {
            let mut w = world.lock().unwrap();
            let dp = w.output("DP-1");
            dp.connected = true;
            dp.area = Some((1920, 0, 1920, 1080));
            dp.modes = vec!["1920x1080".to_owned()];
        }
        let mut b = backend(&world);
        let mut announced = false;
        assert!(b
            .plug_in(&spec(2560, 1440), &mut |_| announced = true)
            .is_err());
        assert!(!announced);
        let w = world.lock().unwrap();
        assert!(w.lit.is_empty() && w.xrandr_calls.is_empty());
        assert_eq!(w.released, vec!["DP-1"]);
    }

    #[test]
    fn come_up_asks_the_x_server_to_look_once_without_a_hotplug_event() {
        for world in [World::office(), World::with_vkms()] {
            world.lock().unwrap().hotplug_events = false;
            let mut b = backend(&world);
            let name = b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
            assert_eq!(world.lock().unwrap().probes, 1);
            assert_eq!(area(&world, &name), Some((1920, 0, 2560, 1440)));
        }
    }

    // A desktop that never shows a lit connector, not even when probed.
    struct Blind(FakeDesktop);

    impl Desktop for Blind {
        fn outputs(&mut self) -> ResultType<Vec<Output>> {
            self.0.outputs()
        }
        fn probe(&mut self) {}
        fn xrandr(&mut self, args: &[String]) -> ResultType<()> {
            self.0.xrandr(args)
        }
        fn set_primary(&mut self, name: &str) -> ResultType<()> {
            self.0.set_primary(name)
        }
        fn providers(&mut self) -> ResultType<Vec<Provider>> {
            self.0.providers()
        }
        fn pointer(&mut self) -> Option<(i32, i32)> {
            None
        }
        fn warp(&mut self, _: (i32, i32)) {}
        fn clear_transforms(&mut self) {}
        fn sleep(&mut self, _: Duration) {}
    }

    #[test]
    fn a_display_that_did_not_come_up_leaves_nothing() {
        let world = World::with_vkms();
        world.lock().unwrap().hotplug_events = false;
        let mut b = X11Backend::new(
            FakeRoot(world.clone()),
            Blind(FakeDesktop(world.clone())),
            1,
            Some(Kind::Vkms),
        );
        assert!(b.plug_in(&spec(2560, 1440), &mut |_| {}).is_err());
        let w = world.lock().unwrap();
        assert_eq!(w.released, vec!["Virtual-1"]);
        assert!(w.modes.is_empty());
    }

    #[test]
    fn only_what_the_root_service_put_back_is_turned_off_at_start() {
        let world = World::two_monitors();
        {
            let mut w = world.lock().unwrap();
            // A dead server's output, on a connector put back: disconnected, still on.
            w.outputs[1].area = Some((3840, 0, 2560, 1440));
            w.transformed.push("DP-1".to_owned());
            // A monitor asleep that dropped its hotplug signal: someone's.
            w.outputs[2].connected = false;
            w.root_released = vec![Gone {
                connector: "DP-1".to_owned(),
                connector_id: Some(95),
                kind: Kind::Scaled,
            }];
        }
        let mut b = backend(&world);
        b.reset_all().unwrap();
        assert_eq!(area(&world, "DP-1"), None);
        assert_eq!(area(&world, "DP-2"), Some((1920, 0, 1920, 1080)));
        assert!(area(&world, "HDMI-1").is_some());
        // The transform it left on its CRTC goes too.
        let w = world.lock().unwrap();
        assert_eq!(w.transforms_cleared, 1);
        assert!(w.stale_transforms.is_empty());
    }

    #[test]
    fn a_vkms_display_left_on_goes_at_start_with_its_modes() {
        let world = World::with_vkms();
        let line = Modeline::cvt_rb(2560, 1440, 60.0);
        {
            let mut w = world.lock().unwrap();
            w.modes.insert(line.name.clone(), line.clock_khz);
            let o = w.output("Virtual-1-1");
            o.area = Some((1920, 0, 2560, 1440));
            o.modes = vec![line.name.clone()];
            w.root_released = vec![Gone {
                connector: "Virtual-1".to_owned(),
                connector_id: Some(36),
                kind: Kind::Vkms,
            }];
        }
        let mut b = backend(&world);
        b.reset_all().unwrap();
        let w = world.lock().unwrap();
        assert_eq!(w.outputs[2].area, None);
        assert!(w.outputs[2].modes.is_empty());
        assert!(w.modes.is_empty());
    }

    #[test]
    fn displays_moved_by_the_desktop_are_put_back_once_it_is_done() {
        let world = World::office();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        assert!(b.maintain().is_empty());
        // The desktop puts its own layout back.
        world.lock().unwrap().output("DP-1").area = Some((0, 0, 1280, 720));
        world.lock().unwrap().output("HDMI-1").area = Some((1280, 0, 1920, 1080));
        assert!(b.maintain().is_empty());
        assert_eq!(
            area(&world, "DP-1"),
            Some((0, 0, 1280, 720)),
            "not on first sight"
        );
        assert!(b.maintain().is_empty());
        assert_eq!(area(&world, "DP-1"), Some((1920, 0, 2560, 1440)));
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 1920, 1080)));
    }

    #[test]
    fn a_desktop_that_keeps_moving_them_is_given_up_on() {
        let world = World::office();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        let mut put_back = 0;
        for _ in 0..20 {
            world.lock().unwrap().output("DP-1").area = Some((0, 0, 1280, 720));
            b.maintain();
            if area(&world, "DP-1") == Some((1920, 0, 2560, 1440)) {
                put_back += 1;
            }
        }
        assert_eq!(put_back, PUT_BACKS);
    }

    #[test]
    fn a_desktop_that_undoes_them_later_is_given_up_on_too() {
        let world = World::office();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        let mut put_back = 0;
        for _ in 0..20 {
            // Right for a look, then undone, seen twice.
            b.maintain();
            world.lock().unwrap().output("DP-1").area = Some((0, 0, 1280, 720));
            b.maintain();
            b.maintain();
            if area(&world, "DP-1") == Some((1920, 0, 2560, 1440)) {
                put_back += 1;
            }
        }
        assert_eq!(put_back, PUT_BACKS);
        // Left right long enough, it is looked after again.
        world.lock().unwrap().output("DP-1").area = Some((1920, 0, 2560, 1440));
        for _ in 0..STEADY {
            b.maintain();
        }
        world.lock().unwrap().output("DP-1").area = Some((0, 0, 1280, 720));
        b.maintain();
        b.maintain();
        assert_eq!(area(&world, "DP-1"), Some((1920, 0, 2560, 1440)));
    }

    #[test]
    fn the_users_own_layout_stands() {
        let world = World::two_monitors();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        assert_eq!(area(&world, "DP-1"), Some((3840, 0, 2560, 1440)));
        // HDMI-1 made bigger; the desktop moves DP-2 out of its way, and ours along.
        {
            let mut w = world.lock().unwrap();
            w.output("HDMI-1").area = Some((0, 0, 2560, 1440));
            w.output("DP-2").area = Some((2560, 0, 1920, 1080));
            w.output("DP-1").area = Some((4480, 0, 2560, 1440));
        }
        b.maintain();
        b.maintain();
        assert_eq!(area(&world, "DP-2"), Some((2560, 0, 1920, 1080)));
        // Ours undone later: put back right of them as they are now, DP-2 left where it is.
        world.lock().unwrap().output("DP-1").area = Some((0, 0, 1280, 720));
        b.maintain();
        b.maintain();
        assert_eq!(area(&world, "DP-1"), Some((4480, 0, 2560, 1440)));
        assert_eq!(area(&world, "DP-2"), Some((2560, 0, 1920, 1080)));
        assert_eq!(area(&world, "HDMI-1"), Some((0, 0, 2560, 1440)));
    }

    #[test]
    fn a_primary_the_user_picks_stands() {
        let world = World::two_monitors();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        make_primary(&world, "DP-2");
        b.maintain();
        b.maintain();
        assert_eq!(primary(&world), "DP-2");
        // Ours made primary: the user's choice comes back, not the one from before.
        make_primary(&world, "DP-1");
        b.maintain();
        b.maintain();
        assert_eq!(primary(&world), "DP-2");
    }

    fn make_primary(world: &Arc<Mutex<World>>, name: &str) {
        for o in world.lock().unwrap().outputs.iter_mut() {
            o.primary = o.name == name;
        }
    }

    #[test]
    fn a_display_that_failed_is_not_kept() {
        let world = World::office();
        {
            let mut w = world.lock().unwrap();
            w.ignores_xrandr = true;
            w.link_breaks_on_release = true;
        }
        let mut b = backend(&world);
        assert!(b.plug_in(&spec(2560, 1440), &mut |_| {}).is_err());
        assert!(!b.is_ours("DP-1"));
        // The X server and the link are fine again: the next one works.
        {
            let mut w = world.lock().unwrap();
            w.ignores_xrandr = false;
            w.link_breaks_on_release = false;
            w.output("DP-1").area = None;
        }
        assert_eq!(b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap(), "DP-1");
        assert_eq!(area(&world, "DP-1"), Some((1920, 0, 2560, 1440)));
    }

    #[test]
    fn displays_are_given_up_when_the_root_service_goes() {
        let world = World::with_vkms();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        {
            let mut w = world.lock().unwrap();
            w.root_gone = true;
            // It put the connector back as it went.
            w.unplug("Virtual-1-1");
        }
        assert_eq!(b.maintain(), vec!["Virtual-1-1"]);
        let w = world.lock().unwrap();
        assert_eq!(w.outputs[2].area, None);
        assert!(w.modes.is_empty());
        drop(w);
        assert!(b.maintain().is_empty());
        // Scaled: only turned off, as RandR refuses a transform to an output without modes.
        let world = World::office();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        {
            let mut w = world.lock().unwrap();
            w.root_gone = true;
            w.unplug("DP-1");
        }
        assert_eq!(b.maintain(), vec!["DP-1"]);
        let w = world.lock().unwrap();
        assert!(!w.xrandr_calls.iter().any(|c| c.contains("--transform")));
        assert!(w.stale_transforms.is_empty());
    }

    #[test]
    fn a_display_whose_connector_went_is_reported_gone() {
        let world = World::office();
        let mut b = backend(&world);
        b.plug_in(&spec(2560, 1440), &mut |_| {}).unwrap();
        world.lock().unwrap().unplug("DP-1");
        assert_eq!(b.maintain(), vec!["DP-1"]);
        assert_eq!(area(&world, "DP-1"), None);
        assert!(b.maintain().is_empty());
        // And the next display starts from what there is then.
        b.plug_in(&spec(1920, 1080), &mut |_| {}).unwrap();
        assert_eq!(area(&world, "DP-1"), Some((1920, 0, 1920, 1080)));
    }

    #[test]
    fn caps_say_which_kind_the_displays_are() {
        let world = World::with_vkms();
        let b = X11Backend::new(
            FakeRoot(world.clone()),
            FakeDesktop(world.clone()),
            2,
            Some(Kind::Vkms),
        );
        assert_eq!((b.caps().kind, b.caps().max), ("x11-vkms", 2));
        assert_eq!(backend(&world).caps().kind, "x11-scaled");
    }
}
