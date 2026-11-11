//! The amyuni backend proper; the module's notes say why it works as it does.

use super::{
    display::{self, Layout, Output, Refused},
    plan::{self, Leftover, Look, Retake, Step},
};
use crate::{
    privacy_mode::win_input,
    vdisplay::manager::{Backend, Caps, Mode, Spec, TAKEN_BACK},
    virtual_display_manager::amyuni_idd,
};
use hbb_common::{anyhow::anyhow, bail, config::LocalConfig, log, ResultType};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// The interface path of the monitor we hold, kept across runs so that a run after a crash can
/// take back what was left. `PLUGGING` while a plug is under way.
const RECORD: &str = "vdisplay-win-monitor";
const PLUGGING: &str = "plugging";
/// The driver's table as it was before we wrote ours, while ours is in it: put back by a run
/// after a crash in between.
const SAVED_TABLE: &str = "vdisplay-win-table";
const APPEAR: Duration = Duration::from_secs(5);
const SETTLE: Duration = Duration::from_secs(6);
const GO: Duration = Duration::from_secs(2);
/// How long a hook let go of may take to end, on its own thread.
const HOOK_GONE: Duration = Duration::from_secs(1);
/// Set while the desktop is taken over, kept across runs: a run after one that ended so shows the
/// other displays again, should they still be out of the desktop.
const TAKEN: &str = "vdisplay-win-taken";
/// Why the desktop went back otherwise than by Ctrl+P, for the controlling side.
const INPUT_LOST: &str = "Local input at the remote device could no longer be held";
const NOT_KEPT: &str = "The desktop of the remote device could not be kept taken over";

/// Set while our display has the desktop to itself, or is taking it: upstream's privacy mode is
/// refused meanwhile, and turning it off leaves ours alone (`crate::privacy_mode`).
static TAKING_OVER: AtomicBool = AtomicBool::new(false);
/// Ctrl+P pressed at the device with the desktop taken over, not seen to yet.
static CTRL_P: AtomicBool = AtomicBool::new(false);

/// Whether our display has the desktop to itself, or is taking it.
pub fn taking_over() -> bool {
    TAKING_OVER.load(Ordering::SeqCst)
}

fn set_taking_over(on: bool) {
    TAKING_OVER.store(on, Ordering::SeqCst);
    LocalConfig::set_option(TAKEN.to_owned(), if on { "Y" } else { "" }.to_owned());
}

/// The person at the device pressed Ctrl+P, which takes a desktop taken over back.
pub fn ctrl_p() {
    if taking_over() {
        CTRL_P.store(true, Ordering::SeqCst);
    }
}

struct Ours {
    name: String,
    monitor: String,
    /// What it offers: the sizes written to the table when it was plugged in, and the driver's.
    modes: Vec<(u32, u32)>,
}

/// The desktop taken over by our display: the other displays out of it, as Win+P's "second
/// screen only" makes them though not saved, and local input held as upstream's privacy mode holds
/// it, Ctrl+P let through.
struct Taken {
    /// The layout before, to give the desktop back with.
    before: Layout,
    /// Whether other displays were part of it, to be shown again. Not so after Win+P's "second
    /// screen only", say. Known when taken over: the names in `before` may not resolve later.
    others: bool,
    /// Our display's size, which may change while it has the desktop to itself.
    size: (u32, u32),
    slips: plan::Slips,
}

pub struct Amyuni {
    ours: Option<Ours>,
    taken: Option<Taken>,
    /// Why the desktop last went back unasked, if not by Ctrl+P (`Backend::lost_why`).
    why: Option<String>,
}

/// The backend where it can work: installed, on Windows 10 2004 or later, with the driver's files
/// next to us.
pub fn new_backend() -> Option<Box<dyn Backend>> {
    let usable = crate::platform::is_installed()
        && crate::virtual_display_manager::is_virtual_display_supported()
        && driver_files_present();
    usable.then(|| {
        Box::new(Amyuni {
            ours: None,
            taken: None,
            why: None,
        }) as Box<dyn Backend>
    })
}

fn driver_files_present() -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.join(r"usbmmidd_v2\usbmmIdd.inf").exists()))
        .unwrap_or(false)
}

fn amyuni_monitors(outputs: &[Output]) -> Vec<String> {
    outputs
        .iter()
        .filter(|o| o.amyuni)
        .filter_map(|o| o.monitor.clone())
        .collect()
}

fn forget() {
    LocalConfig::set_option(RECORD.to_owned(), String::new());
    amyuni_idd::set_backend_holds(false);
}

/// Gives display `name` size `size` and sees that it stays so: the system may pick a mode of its
/// own for a monitor just plugged in, or fail a change asked while it does.
fn apply(name: &str, size: (u32, u32)) -> ResultType<()> {
    let start = Instant::now();
    loop {
        match display::set(name, size) {
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(300));
                if display::size(name) == Some(size) {
                    return Ok(());
                }
            }
            Err(e) if start.elapsed() > SETTLE => return Err(e),
            Err(e) => {
                log::debug!("{}; trying again", e);
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        if start.elapsed() > SETTLE {
            bail!("Virtual display {} would not stay at {}x{}", name, size.0, size.1);
        }
    }
}

/// Unplugs the newest amyuni monitor, ours as `leftover` found, and waits for `monitor` to go.
fn unplug(monitor: &str) -> ResultType<()> {
    amyuni_idd::plug_for_backend(false)?;
    let start = Instant::now();
    while start.elapsed() < GO {
        let present = amyuni_monitors(&display::outputs());
        if !present.iter().any(|p| p.eq_ignore_ascii_case(monitor)) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("Unplugged a virtual display, but ours ({}) is still there", monitor)
}

/// Takes back the display a previous run left, where it can be shown to be ours and unplugging
/// would take it; puts back the driver's table if a plug was cut short. Shows the other displays
/// again should that run have ended with the desktop taken over.
pub fn clean_leftover() {
    take_back_leftover();
    if LocalConfig::get_option(TAKEN).is_empty() || !display::on_console() {
        return;
    }
    // Unplugging ours brings them back by itself, the layout the system saved being the one from
    // before; not so with it left in place. Only then: a desktop of virtual displays alone may
    // well be what its user chose.
    let outputs = display::outputs();
    let others = outputs.iter().filter(|o| !o.amyuni);
    if !others.clone().any(|o| o.attached) && others.clone().any(|o| o.monitor.is_some()) {
        log::warn!("A previous run left the desktop taken over; putting the saved layout in force");
        if let Err(e) = display::apply_saved() {
            // Left for the next run, the lock screen up maybe.
            log::error!("{}", e);
            return;
        }
    }
    LocalConfig::set_option(TAKEN.to_owned(), String::new());
}

fn take_back_leftover() {
    let saved = LocalConfig::get_option(SAVED_TABLE);
    if !saved.is_empty() {
        match serde_json::from_str::<display::Table>(&saved) {
            Ok(table) => match display::put_table(&table) {
                Ok(()) => log::info!("Put the virtual display driver's mode table back"),
                Err(e) => log::error!("Failed to put the driver's mode table back: {}", e),
            },
            Err(e) => log::error!("Unreadable saved mode table: {}", e),
        }
        LocalConfig::set_option(SAVED_TABLE.to_owned(), String::new());
    }
    let record = LocalConfig::get_option(RECORD);
    // Off the console, ours would look gone: left for a run on the console.
    if record.is_empty() || !display::on_console() {
        return;
    }
    if record == PLUGGING {
        log::warn!("A virtual display was being plugged in when the last run ended; not taking it back, there being no telling it is ours");
        forget();
        return;
    }
    match plan::leftover(&record, &amyuni_monitors(&display::outputs())) {
        Leftover::Gone => forget(),
        Leftover::Unplug => {
            match unplug(&record) {
                Ok(()) => log::info!("Took back the virtual display a previous run left: {}", record),
                Err(e) => log::error!("Failed to take back a leftover virtual display: {}", e),
            }
            forget();
        }
        Leftover::Stuck => log::warn!(
            "A virtual display a previous run left ({}) is not the newest; left in place",
            record
        ),
    }
}

fn put_back(saved: &display::Table) {
    match display::put_table(saved) {
        Ok(()) => LocalConfig::set_option(SAVED_TABLE.to_owned(), String::new()),
        Err(e) => log::error!("Failed to put the driver's mode table back: {}", e),
    }
}

/// The interface path of the amyuni monitor plugged in since `before` was taken.
fn appeared(before: &[Output]) -> ResultType<String> {
    let start = Instant::now();
    loop {
        let found = display::outputs().into_iter().find_map(|o| {
            o.monitor
                .filter(|m| o.amyuni && !before.iter().any(|b| b.monitor.as_ref() == Some(m)))
        });
        if let Some(monitor) = found {
            return Ok(monitor);
        }
        if start.elapsed() > APPEAR {
            bail!("The virtual display did not show up");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Holds local input, as upstream's privacy mode does: all but Ctrl and P, and input of ours.
fn hold_input() -> ResultType<()> {
    // A hook let go of ends a moment later, and another is refused until then.
    let start = Instant::now();
    while win_input::hooked() && start.elapsed() < HOOK_GONE {
        std::thread::sleep(Duration::from_millis(50));
    }
    win_input::hook()
}

/// Display `name` alone in the desktop at `size`, as `before` has it; seen to have taken.
fn take_desktop(before: &Layout, name: &str, size: (u32, u32)) -> Result<(), Refused> {
    let failed = |e: hbb_common::anyhow::Error| Refused::Failed(e.to_string());
    before.alone(name, size).map_err(failed)?.apply()?;
    let views = Layout::now().map_err(failed)?.views();
    if !plan::taken_over(&views, name) {
        return Err(Refused::Failed(format!(
            "The desktop was not left to {}: {:?}",
            name, views
        )));
    }
    Ok(())
}

/// The desktop as `before` has it, display `name` at `size`, or else as the system saved it.
/// Answers whether the person at the device sees it again: at once if `before` showed nothing
/// but ours (`others` false).
fn put_layout_back(before: &Layout, others: bool, name: &str, size: (u32, u32)) -> bool {
    match before.resized(name, size) {
        Ok(layout) => {
            if let Err(e) = layout.apply() {
                log::warn!("Failed to put the display layout back: {}", e);
            }
        }
        Err(e) => log::warn!("{}", e),
    }
    if !others || shown_again(name) {
        return true;
    }
    log::warn!("The other displays did not come back; putting the saved layout in force");
    if let Err(e) = display::apply_saved() {
        log::error!("{}", e);
    }
    shown_again(name)
}

fn shown_again(name: &str) -> bool {
    Layout::now().map_or(false, |l| plan::others_shown(&l.views(), name))
}

/// Runs `change`, a change of the layout `maintain` makes by itself, outside the calls the manager
/// marks: the layout's generation moves on around it all the same, so capture errors that come of
/// it do not use up DXGI's recovery budget (`crate::vdisplay::layout_generation`).
fn changing_layout<T>(change: impl FnOnce() -> T) -> T {
    crate::vdisplay::layout_changed();
    let result = change();
    crate::vdisplay::layout_changed();
    result
}

/// Whether there is a display to show but ours: one with a monitor, not of the amyuni driver.
fn others_there() -> bool {
    display::outputs()
        .iter()
        .any(|o| !o.amyuni && o.monitor.is_some())
}

impl Amyuni {
    /// Display `name` takes the desktop over (`Taken`).
    fn take_over(&mut self, name: &str) -> ResultType<()> {
        // Raised before looking at upstream's privacy mode: one turned on from now on is refused.
        set_taking_over(true);
        self.why = None;
        match Self::try_take_over(name) {
            Ok(taken) => {
                log::info!("Virtual display {} took the desktop over", name);
                self.taken = Some(taken);
                Ok(())
            }
            Err((e, None)) => {
                set_taking_over(false);
                Err(e)
            }
            Err((e, Some(taken))) => {
                // Neither taken over as asked nor put back: kept so, local input held, for the
                // next looks to take it over again or give it up.
                log::error!("The desktop was left half taken over: {}", e);
                self.taken = Some(taken);
                Err(e)
            }
        }
    }

    /// On failure, with the error, the takeover as it is where the layout could not be put back.
    fn try_take_over(name: &str) -> Result<Taken, (hbb_common::anyhow::Error, Option<Taken>)> {
        // Refused by the connection before ever asking, short of a race.
        if crate::privacy_mode::is_in_privacy_mode() {
            return Err((anyhow!("The remote device is in privacy mode already"), None));
        }
        let before = Layout::now().map_err(|e| (e, None))?;
        let views = before.views();
        let Some(size) = views
            .iter()
            .find(|v| v.name.eq_ignore_ascii_case(name))
            .map(|v| v.size)
        else {
            return Err((anyhow!("Virtual display {} is not part of the desktop", name), None));
        };
        let others = plan::others_shown(&views, name);
        CTRL_P.store(false, Ordering::SeqCst);
        // Local input held first: the desktop is never taken over without it.
        hold_input().map_err(|e| (e, None))?;
        let taken = Taken {
            before,
            others,
            size,
            slips: plan::Slips::default(),
        };
        if let Err(e) = take_desktop(&taken.before, name, size) {
            if !put_layout_back(&taken.before, others, name, size) {
                return Err((e.into(), Some(taken)));
            }
            if let Err(e) = win_input::unhook() {
                log::error!("Failed to let go of local input: {}", e);
            }
            return Err((e.into(), None));
        }
        Ok(taken)
    }

    /// The desktop given back (`Taken.before`), local input let go once the person at the device
    /// sees it again. Still taken over on failure, to be given back later.
    fn give_back(&mut self, name: &str) -> ResultType<()> {
        let Some(taken) = &self.taken else {
            return Ok(());
        };
        // No other display left to show, its monitor unplugged meanwhile: nothing to give back.
        if !put_layout_back(&taken.before, taken.others, name, taken.size) && others_there() {
            bail!("Failed to give the desktop back");
        }
        self.leave();
        log::info!("Virtual display {} gave the desktop back", name);
        Ok(())
    }

    /// Ends the takeover, whatever ended it: local input let go, upstream's privacy mode free
    /// again. The layout is left as it is.
    fn leave(&mut self) {
        if self.taken.take().is_some() {
            if let Err(e) = win_input::unhook() {
                log::error!("Failed to let go of local input: {}", e);
            }
        }
        set_taking_over(false);
        CTRL_P.store(false, Ordering::SeqCst);
    }

    /// Resizes display `name` with the desktop taken over: only its own mode changes, the way of
    /// an extra screen bringing the other displays back.
    fn resize_taken_over(&mut self, name: &str, size: (u32, u32)) -> ResultType<()> {
        let Some(taken) = self.taken.as_mut() else {
            bail!("Virtual display {} does not have the desktop to itself", name);
        };
        Layout::now()?.resized(name, size)?.apply()?;
        let start = Instant::now();
        while display::size(name) != Some(size) {
            if start.elapsed() > GO {
                bail!("Virtual display {} would not take {}x{}", name, size.0, size.1);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        taken.size = size;
        Ok(())
    }

    fn retake(&self, name: &str) -> Retake {
        let Some(taken) = &self.taken else {
            return Retake::Done;
        };
        match take_desktop(&taken.before, name, taken.size) {
            Ok(()) => {
                log::info!("Took the desktop over again");
                Retake::Done
            }
            Err(Refused::Denied) => {
                log::warn!("Taking the desktop over again was refused for now");
                Retake::Denied
            }
            Err(e) => {
                log::warn!("Failed to take the desktop over again: {}", e);
                Retake::Failed
            }
        }
    }

    // The desktop taken over, kept so (`plan::look`). Answers our display if it went.
    fn keep_taken_over(&mut self, name: &str, present: bool) -> Vec<String> {
        let arranged = match Layout::now() {
            Ok(layout) => plan::taken_over(&layout.views(), name),
            // Not to be told now, as on the secure desktop maybe: looked at again next time.
            Err(e) => {
                log::debug!("{}", e);
                true
            }
        };
        let ctrl_p = CTRL_P.load(Ordering::SeqCst);
        match plan::look(present, ctrl_p, win_input::hooked(), arranged) {
            Look::Gone => {
                log::warn!("Virtual display {} went away with the desktop taken over", name);
                self.leave();
                self.ours = None;
                forget();
                return vec![name.to_owned()];
            }
            Look::TakenBack => {
                log::warn!("The person at this device took the desktop back");
                self.why = None;
                // Ctrl+P stays pressed should this fail: given back at the next look.
                if let Err(e) = changing_layout(|| self.give_back(name)) {
                    log::error!("{}", e);
                }
            }
            Look::Unhooked => {
                log::warn!("Local input is no longer held; holding it again");
                if let Err(e) = hold_input() {
                    log::error!("Failed to hold local input, giving the desktop back: {}", e);
                    self.why = Some(INPUT_LOST.to_owned());
                    if let Err(e) = changing_layout(|| self.give_back(name)) {
                        log::error!("{}", e);
                    }
                }
            }
            Look::Moved => {
                log::warn!("The desktop taken over was laid out anew; taking it over again");
                let retake = changing_layout(|| self.retake(name));
                let slipped = self.taken.as_mut().map_or(false, |t| t.slips.after(retake));
                if slipped {
                    log::error!("The desktop could not be kept taken over; its display goes");
                    // Word for its session should the display stay, given back.
                    self.why = Some(NOT_KEPT.to_owned());
                    match changing_layout(|| self.plug_out(name)) {
                        Ok(()) => return vec![name.to_owned()],
                        Err(e) => log::error!("{}", e),
                    }
                }
            }
            Look::Kept => {
                if let Some(taken) = &mut self.taken {
                    taken.slips.kept();
                }
            }
        }
        Vec::new()
    }

    /// On failure, with the error, `Some(its interface path, empty if it never showed)` when the
    /// monitor could not be taken out again.
    fn plug(
        &self,
        spec: &Spec,
        before: &[Output],
        announce: &mut dyn FnMut(&str),
    ) -> Result<Ours, (hbb_common::anyhow::Error, Option<String>)> {
        let ladder = plan::ladder(spec.width, spec.height);
        let saved = display::table().map_err(|e| (e, None))?;
        let json = serde_json::to_string(&saved).map_err(|e| (e.into(), None))?;
        LocalConfig::set_option(SAVED_TABLE.to_owned(), json);
        display::put_table(&display::entries(&ladder)).map_err(|e| {
            put_back(&saved);
            (e, None)
        })?;
        if let Err(e) = amyuni_idd::plug_for_backend(true) {
            put_back(&saved);
            return Err((e, None));
        }
        // The driver reads the table while the monitor arrives, some time after the call, as the
        // system asks it for modes: the table serves others again only once ours is set.
        let mut monitor = None;
        let settled = appeared(before).and_then(|new| {
            monitor = Some(new.clone());
            self.settle(new, &ladder, announce)
        });
        put_back(&saved);
        // The monitor is in: whatever failed takes it out again, unplugging taking the newest.
        settled.map_err(|e| match amyuni_idd::plug_for_backend(false) {
            Ok(()) => (e, None),
            Err(e2) => {
                log::error!("Failed to unplug the virtual display that failed: {}", e2);
                (e, Some(monitor.unwrap_or_default()))
            }
        })
    }

    /// Waits for monitor `monitor` to be part of the desktop with the modes of the table, then
    /// gives it the first. Until it is part of the desktop it may be listed under another display
    /// than the one it ends up on, so it is looked for by its interface path each time.
    fn settle(
        &self,
        monitor: String,
        ladder: &[(u32, u32)],
        announce: &mut dyn FnMut(&str),
    ) -> ResultType<Ours> {
        let start = Instant::now();
        let mut seen = String::new();
        let name = loop {
            let on = display::outputs()
                .into_iter()
                .find(|o| o.monitor.as_ref() == Some(&monitor));
            if let Some(o) = &on {
                if o.name != seen {
                    log::debug!(
                        "Virtual display {} on {} ({}) after {:?}",
                        monitor,
                        o.name,
                        if o.attached { "attached" } else { "not attached" },
                        start.elapsed()
                    );
                    seen = o.name.clone();
                }
                if o.attached && display::modes(&o.name).contains(&ladder[0]) {
                    break o.name.clone();
                }
            }
            if start.elapsed() > SETTLE {
                bail!(
                    "Virtual display {} was not made part of the desktop{}",
                    monitor,
                    if seen.is_empty() { String::new() } else { format!(" (on {})", seen) }
                );
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let modes = display::modes(&name);
        log::info!("Virtual display {} offers {:?} after {:?}", name, modes, start.elapsed());
        announce(&name);
        apply(&name, ladder[0])?;
        Ok(Ours {
            monitor,
            name,
            modes,
        })
    }
}

impl Backend for Amyuni {
    fn caps(&self) -> Caps {
        Caps {
            kind: "win-amyuni",
            max: 1,
            custom: true,
            hidpi: false,
            max_width: plan::MAX.0,
            max_height: plan::MAX.1,
            // An extra screen, or the desktop to itself, the other displays dark.
            modes: vec![Mode::Shared, Mode::Privacy],
        }
    }

    fn plug_in(&mut self, spec: &Spec, announce: &mut dyn FnMut(&str)) -> ResultType<String> {
        if self.ours.is_some() {
            bail!("There is a virtual display already");
        }
        if !display::on_console() {
            bail!("Virtual displays are only for the session on the console");
        }
        // Held before looking: no other path plugs a monitor in between.
        amyuni_idd::set_backend_holds(true);
        if let Err(e) = amyuni_idd::ensure_driver() {
            amyuni_idd::set_backend_holds(false);
            return Err(e);
        }
        let before = display::outputs();
        if let Some(other) = before.iter().find(|o| o.amyuni && o.monitor.is_some()) {
            amyuni_idd::set_backend_holds(false);
            // Unplugging takes the newest monitor: ours and another cannot be told apart then.
            bail!("Another virtual display is there already ({})", other.name);
        }
        LocalConfig::set_option(RECORD.to_owned(), PLUGGING.to_owned());
        crate::vdisplay::hide_new_displays(Some(before.iter().map(|o| o.name.clone()).collect()));
        let plugged = self.plug(spec, &before, announce);
        crate::vdisplay::hide_new_displays(None);
        match plugged {
            Ok(ours) => {
                LocalConfig::set_option(RECORD.to_owned(), ours.monitor.clone());
                log::info!("Plugged in amyuni display {} ({})", ours.name, ours.monitor);
                let name = ours.name.clone();
                self.ours = Some(ours);
                Ok(name)
            }
            Err((e, None)) => {
                forget();
                Err(e)
            }
            Err((e, Some(monitor))) => {
                // Still in: recorded for a later run to take back, the driver held meanwhile.
                let record = if monitor.is_empty() { PLUGGING.to_owned() } else { monitor };
                LocalConfig::set_option(RECORD.to_owned(), record);
                Err(e)
            }
        }
    }

    fn set_mode(&mut self, name: &str, spec: &Spec) -> ResultType<()> {
        let Some(ours) = self.ours.as_ref().filter(|o| o.name == name) else {
            bail!("{} is not our virtual display", name);
        };
        if !matches!(spec.mode, Mode::Shared | Mode::Privacy) {
            bail!(
                "Virtual display {} can only be an extra screen or have the desktop to itself",
                name
            );
        }
        let Some(size) = plan::best_fit(&ours.modes, spec.width, spec.height) else {
            bail!("Virtual display {} offers no mode", name);
        };
        let taking_over = self.taken.is_some();
        // Ctrl+P since the last look: the desktop is given back before anything else.
        let ctrl_p = taking_over && CTRL_P.swap(false, Ordering::SeqCst);
        let resize = display::size(name) != Some(size);
        for step in plan::steps(taking_over, ctrl_p, spec.mode == Mode::Privacy, resize) {
            match step {
                Step::GiveBack => {
                    if let Err(e) = self.give_back(name) {
                        if ctrl_p {
                            // Still to be seen to, at the next look.
                            CTRL_P.store(true, Ordering::SeqCst);
                        }
                        return Err(e);
                    }
                }
                Step::Resize => apply(name, size)?,
                Step::ResizeTakenOver => self.resize_taken_over(name, size)?,
                Step::TakeOver => self.take_over(name)?,
                Step::TakenBack => bail!("{}", TAKEN_BACK),
            }
        }
        Ok(())
    }

    fn plug_out(&mut self, name: &str) -> ResultType<()> {
        let Some(monitor) = self
            .ours
            .as_ref()
            .filter(|o| o.name == name)
            .map(|o| o.monitor.clone())
        else {
            return Ok(());
        };
        if !display::on_console() {
            // Ours would look gone, and the layout cannot be put back from here: left, recorded,
            // for this run back on the console or the next run to take back.
            bail!("Virtual display {} is left for later: not on the console", name);
        }
        // Given back first: should unplugging fail, the person at the device sees the desktop.
        let given_back = self.taken.is_none()
            || self.give_back(name).map_err(|e| log::error!("{}", e)).is_ok();
        let unplugged = match plan::leftover(&monitor, &amyuni_monitors(&display::outputs())) {
            Leftover::Gone => Ok(()),
            Leftover::Unplug => unplug(&monitor),
            Leftover::Stuck => Err(anyhow!(
                "Another virtual display was plugged in after {}: unplugging would take it",
                name
            )),
        };
        if unplugged.is_err() && !given_back {
            // Neither: still taken over, local input still held, so the person at the device has
            // Ctrl+P, and taking the display out is tried again.
            return unplugged;
        }
        // Gone, or the desktop given back: local input let go, unplugged or not. Gone with the
        // desktop not given back, the system shows the other displays again by itself.
        self.leave();
        unplugged?;
        log::info!("Plugged out amyuni display {}", name);
        self.ours = None;
        forget();
        Ok(())
    }

    fn reset_all(&mut self) -> ResultType<()> {
        if let Some(name) = self.ours.as_ref().map(|o| o.name.clone()) {
            self.plug_out(&name)?;
        }
        clean_leftover();
        Ok(())
    }

    fn maintain(&mut self) -> Vec<String> {
        let Some(ours) = &self.ours else {
            return Vec::new();
        };
        // Off the console ours looks gone, and the layout is not the machine's: looked at again
        // back on it.
        if !display::on_console() {
            return Vec::new();
        }
        let present = amyuni_monitors(&display::outputs())
            .iter()
            .any(|p| p.eq_ignore_ascii_case(&ours.monitor));
        if self.taken.is_some() {
            let name = ours.name.clone();
            return self.keep_taken_over(&name, present);
        }
        if present {
            return Vec::new();
        }
        log::warn!("Virtual display {} went away", ours.name);
        let name = ours.name.clone();
        self.ours = None;
        forget();
        vec![name]
    }

    fn usage(&self, name: &str) -> Option<Mode> {
        self.ours.as_ref().filter(|o| o.name == name)?;
        Some(if self.taken.is_some() {
            Mode::Privacy
        } else {
            Mode::Shared
        })
    }

    fn lost_why(&mut self, _name: &str) -> Option<String> {
        self.why.take()
    }

    // Ctrl+P, or the layout changed, is seen to at once.
    fn watchful(&self) -> bool {
        self.taken.is_some()
    }
}
