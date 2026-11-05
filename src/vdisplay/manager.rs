//! The rules every platform shares: which session owns which virtual display, what a repeated
//! request means, and when a display goes away. A `Backend` only plugs displays in and out.

use hbb_common::{log, ResultType};
use std::time::{Duration, Instant};

/// How long the displays of a connection that ended without closing its session are kept, so a
/// controlling side that reconnects finds them where it left them.
pub(super) const GRACE: Duration = Duration::from_secs(30);

/// `ToggleVirtualDisplay.display` with `on = false`: every slot of the session.
pub(super) const ALL_SLOTS: i32 = -1;

/// Slots are monitors of the controlling side, so a handful is plenty.
const MAX_SLOT: i32 = 15;
const MIN_SIDE: u32 = 480;
const MAX_SCALE: f64 = 4.0;

/// Why a display that had the desktop to itself (`Mode::Privacy`) no longer has, its session not
/// having asked: the person at the device took it back (Ctrl+P).
pub(super) const TAKEN_BACK: &str = "The person at the remote device took the desktop back";
/// Why a display does not have the desktop to itself, or no longer: the session may not have it
/// so, the remote device not allowing privacy mode.
pub(super) const NO_PRIVACY: &str = "The remote device does not allow privacy mode";
/// Why a display is not used as asked, short of a more particular reason.
const UNUSABLE: &str = "The virtual display could not be used as asked";

/// How the controlling side wants to use a virtual display. The wire value is a plain int, so an
/// unknown one from a newer controlling side is simply taken as `Shared`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// An extra screen on the one shared desktop, driven by the shared cursor and focus (today's
    /// behaviour, and the fallback for any mode the backend does not offer).
    #[default]
    Shared,
    /// The physical screens go black while the controlling side has the desktop.
    Privacy,
    /// A session of its own on the virtual display, which the person at the device never sees.
    Nested,
    /// A second cursor and keyboard focus, so both sides work the one desktop at once.
    Concurrent,
}

impl Mode {
    /// The name used in `vdm_caps.modes` and when logging.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Shared => "shared",
            Mode::Privacy => "privacy",
            Mode::Nested => "nested",
            Mode::Concurrent => "concurrent",
        }
    }

    /// The wire value of `VirtualDisplaySpec.mode`.
    pub fn wire(self) -> i32 {
        match self {
            Mode::Shared => 0,
            Mode::Privacy => 1,
            Mode::Nested => 2,
            Mode::Concurrent => 3,
        }
    }

    /// From the wire value of `VirtualDisplaySpec.mode`. Anything unknown is `Shared`.
    pub fn from_wire(v: i32) -> Self {
        match v {
            1 => Mode::Privacy,
            2 => Mode::Nested,
            3 => Mode::Concurrent,
            _ => Mode::Shared,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spec {
    pub width: u32,
    pub height: u32,
    /// 1 for a plain display; above 1 asks for a HiDPI one where the backend has it.
    pub scale: f64,
    /// How the controlling side means to use it. Backends that do not offer a mode treat it as
    /// `Shared`; the controlling side is told which modes there are through `Caps::modes`.
    pub mode: Mode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Caps {
    /// Which backend serves the displays.
    pub kind: &'static str,
    /// Displays the backend can hold at once, across all sessions.
    pub max: usize,
    /// Any size within the bounds; otherwise the backend picks its nearest mode.
    pub custom: bool,
    /// Honours `Spec::scale` above 1.
    pub hidpi: bool,
    pub max_width: u32,
    pub max_height: u32,
    /// The modes this backend offers, `Shared` always among them. The controlling side greys out
    /// the rest.
    pub modes: Vec<Mode>,
}

/// One platform's way of making displays. Creating one must not block: it happens on the login
/// path. Slow work belongs in the methods, which only ever run on the manager's own thread.
pub trait Backend: Send {
    fn caps(&self) -> Caps;
    /// Adds a display of `spec` and returns its `DisplayInfo.name`, once the display is there.
    /// `announce` takes the name as soon as it is known and, where the platform allows, before
    /// the display shows up: listed as ours from then on, it never appears among the physical
    /// displays, not even for a moment. Never asked for `Mode::Privacy`: a display to have the
    /// desktop to itself is added as an extra screen, then asked for with `set_mode`, so a
    /// refusal keeps it as one.
    fn plug_in(&mut self, spec: &Spec, announce: &mut dyn FnMut(&str)) -> ResultType<String>;
    fn set_mode(&mut self, name: &str, spec: &Spec) -> ResultType<()>;
    /// Removes a display. One that is already gone counts as removed.
    fn plug_out(&mut self, name: &str) -> ResultType<()>;
    /// Removes every display of ours, including any a previous run left behind.
    fn reset_all(&mut self) -> ResultType<()>;
    /// Called every few seconds while there are displays: keeps them as they should be, and
    /// answers those that went away without being removed, to be forgotten.
    fn maintain(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// The session of display `name` went away for now, and may come back to it: nobody looks at
    /// it meanwhile.
    fn left(&mut self, _name: &str) {}
    /// The displays run as their own nested session (`Mode::Nested`), with where each sits in the
    /// desktop and the X display its session runs on, so input to it goes there and not to the
    /// shared desktop. Empty where the backend has no nested display.
    fn nested(&self) -> Vec<NestedTarget> {
        Vec::new()
    }
    /// How display `name` is used now, where the backend tells: a change of use asked for may not
    /// have taken (the desktop not taken over, or not given back).
    fn usage(&self, _name: &str) -> Option<Mode> {
        None
    }
    /// Why display `name` no longer has the desktop to itself, its session not having asked, when
    /// it is not the person at the device who took it back (Ctrl+P): word for its session in place
    /// of `TAKEN_BACK`. Asked once for each such change.
    fn lost_why(&mut self, _name: &str) -> Option<String> {
        None
    }
    /// Whether to be looked after more often than every few seconds for now: a desktop it took
    /// over by itself, which the person at the device may take back (Ctrl+P), or the compositor
    /// lay out anew, to be seen to at once.
    fn watchful(&self) -> bool {
        false
    }
}

/// A `Mode::Nested` display: its own X session shown on a virtual display of the shared desktop.
/// Input landing in its rectangle goes to its own X server, leaving the shared desktop's cursor
/// and focus alone.
#[derive(Clone, Debug, PartialEq)]
pub struct NestedTarget {
    /// The virtual display's `DisplayInfo.name`.
    pub name: String,
    /// Where the display sits in the shared desktop, in its pixels: input is offset by this.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// The X display its nested session runs on, e.g. `:9`.
    pub display: String,
}

/// Word for a session of a display whose use changed without its asking.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub conn_id: i32,
    pub slot: i32,
    pub name: String,
    pub why: String,
}

/// The controlling session a display belongs to.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Owner {
    pub peer_id: String,
    pub peer_name: String,
    pub session_id: u64,
}

impl Owner {
    pub(super) fn same_device(&self, other: &Owner) -> bool {
        self.peer_id == other.peer_id && self.peer_name == other.peer_name
    }
}

struct Record {
    name: String,
    owner: Owner,
    slot: i32,
    spec: Spec,
    conn_id: i32,
    /// When the display goes, once its connection has ended: at the end of the grace period
    /// (unless the session asks for the slot again first), or, after a removal failed, at the
    /// next try.
    due: Option<Instant>,
    /// Failed removals so far, which space the next tries out.
    failures: u32,
    /// Whether its session may have the desktop to itself (`Mode::Privacy`), as its connection
    /// last said.
    privacy: bool,
    /// The use its session last asked for, as it may have it. A desktop it had to itself that it
    /// may no longer have, or no longer wants, is given back, again at every look should that
    /// fail.
    wanted: Mode,
    /// The desktop it had to itself lost without its session asking, as when the person at the
    /// device took it back, and why: not given to the session again until it asks for any other
    /// use, which says it heard.
    lost: Option<String>,
    /// Word for its session of a change of use it did not ask for, kept until its connection is
    /// up to take it, and the connection last told.
    untold: Option<String>,
    told: Option<i32>,
}

// A display whose removal failed is tried again, ever less often. It is never forgotten while
// it may still be there: listed among the physical displays, it would renumber them.
fn retry_delay(failures: u32) -> Duration {
    GRACE * (1 << failures.saturating_sub(1).min(4))
}

pub struct Manager {
    backend: Box<dyn Backend>,
    caps: Caps,
    /// Oldest first, which is also the order the displays are listed in.
    records: Vec<Record>,
}

impl Manager {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let caps = backend.caps();
        Self {
            backend,
            caps,
            records: Vec::new(),
        }
    }

    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    /// Clears what a previous run left behind. Only before the first request.
    pub fn reset(&mut self) {
        if let Err(e) = self.backend.reset_all() {
            log::error!("Failed to remove leftover virtual displays: {}", e);
        }
    }

    /// How display `name` is used now, if it is ours.
    pub fn used(&self, name: &str) -> Option<Mode> {
        self.records
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.spec.mode)
    }

    /// The names of our displays, oldest first.
    pub fn names(&self) -> Vec<String> {
        self.records.iter().map(|r| r.name.clone()).collect()
    }

    /// The sessions whose displays wait out `GRACE`, one per display: a new session of the same
    /// device takes them back (`reclaim_stale`).
    pub fn waiting(&self) -> Vec<Owner> {
        self.records
            .iter()
            .filter(|r| r.due.is_some())
            .map(|r| r.owner.clone())
            .collect()
    }

    /// The nested-session displays there are, for input routing.
    pub fn nested(&self) -> Vec<NestedTarget> {
        self.backend.nested()
    }

    /// Gives `owner` a display for `slot`, announcing its name before it shows up, with a word on
    /// what of the request did not take, if something did not. Asking again for a slot the
    /// session already has is how a reconnecting session takes its display back: the same
    /// display, resized and used as asked if it can be. `privacy`: whether the connection may
    /// have the desktop to itself.
    pub fn plug_in(
        &mut self,
        conn_id: i32,
        owner: &Owner,
        slot: i32,
        spec: Spec,
        privacy: bool,
        announce: &mut dyn FnMut(&str),
    ) -> Result<(String, Option<String>), String> {
        if let Some(i) = self.find(owner, slot) {
            let name = self.take_back_at(i, conn_id, privacy);
            log::info!(
                "Gave virtual display {} back to slot {} of {} on connection {}",
                name,
                slot,
                owner.peer_id,
                conn_id
            );
            self.heard(i, spec.mode);
            // The display stays either way; the controlling side sees its real size, and is told
            // when it is not used as asked.
            let note = match fit(spec, &self.caps) {
                Ok(spec) => self.use_as_asked(i, owner, spec),
                Err(e) => {
                    log::warn!("Kept virtual display {} at its size: {}", name, e);
                    None
                }
            };
            return Ok((name, note));
        }
        if !(0..=MAX_SLOT).contains(&slot) {
            return Err(format!("invalid slot {}", slot));
        }
        let asked = fit(spec, &self.caps)?;
        // One for the desktop to itself comes as an extra screen, then is used as asked as one
        // held already is (`use_as_asked`): refused, by this device, another session or the
        // backend, it stays an extra screen, the answer saying why.
        let spec = Spec {
            mode: match asked.mode {
                Mode::Privacy => Mode::Shared,
                mode => mode,
            },
            ..asked
        };
        self.reclaim_stale(owner);
        if let Some(why) = self.conflict(owner, spec.mode) {
            return Err(why.to_owned());
        }
        if self.records.len() >= self.caps.max {
            return Err("No free virtual display".to_owned());
        }
        let name = self
            .backend
            .plug_in(&spec, announce)
            .map_err(|e| e.to_string())?;
        if self.records.iter().any(|r| r.name == name) {
            log::error!("The backend named two virtual displays {}", name);
            return Err("Failed to plug in a virtual display".to_owned());
        }
        // Kept as it is used: a change of use it was not asked for is word for its session.
        let wanted = spec.mode;
        let spec = Spec {
            mode: self.backend.usage(&name).unwrap_or(wanted),
            ..spec
        };
        let mut note = (spec.mode != wanted).then(|| UNUSABLE.to_owned());
        log::info!(
            "Plugged in virtual display {} ({}x{}@{}, {:?}) for slot {} of {} on connection {}",
            name,
            spec.width,
            spec.height,
            spec.scale,
            spec.mode,
            slot,
            owner.peer_id,
            conn_id
        );
        self.records.push(Record {
            name: name.clone(),
            owner: owner.clone(),
            slot,
            spec,
            conn_id,
            due: None,
            failures: 0,
            privacy,
            wanted,
            lost: None,
            untold: None,
            told: None,
        });
        if asked.mode == Mode::Privacy {
            let i = self.records.len() - 1;
            note = self.use_as_asked(i, owner, asked);
            // The answer tells its connection; no word beyond it, now or for a refusal it is
            // answered again.
            let r = &mut self.records[i];
            r.untold = None;
            r.told = Some(conn_id);
        }
        Ok((name, note))
    }

    /// Hands `owner`'s display for `slot` to connection `conn_id` as it is, short of a desktop it
    /// may not have to itself (`privacy`), which is given back; asked for as `asked`, which says
    /// what the session heard. None if the session has none there.
    pub fn take_back(
        &mut self,
        conn_id: i32,
        owner: &Owner,
        slot: i32,
        privacy: bool,
        asked: Mode,
    ) -> Option<String> {
        let i = self.find(owner, slot)?;
        let name = self.take_back_at(i, conn_id, privacy);
        self.heard(i, asked);
        self.demote_at(i);
        Some(name)
    }

    /// Removes the display of `slot`, or every display of the session for `ALL_SLOTS`. Answers
    /// per display, with its slot and name: on `Err` the display is still there. A slot without
    /// a display is answered as removed.
    pub fn plug_out(&mut self, owner: &Owner, slot: i32) -> Vec<(i32, String, Result<(), String>)> {
        let names: Vec<(i32, String)> = self
            .records
            .iter()
            .filter(|r| &r.owner == owner && (slot == ALL_SLOTS || r.slot == slot))
            .map(|r| (r.slot, r.name.clone()))
            .collect();
        if names.is_empty() && slot != ALL_SLOTS {
            return vec![(slot, String::new(), Ok(()))];
        }
        names
            .into_iter()
            .map(|(slot, name)| {
                let res = self.remove(&name);
                (slot, name, res)
            })
            .collect()
    }

    /// The connection `conn_id` ended. `closed`: its session ended for good, so its displays go
    /// now; otherwise they wait out `GRACE` for the session to come back.
    pub fn conn_ended(&mut self, conn_id: i32, closed: bool, now: Instant) {
        let names: Vec<String> = self
            .records
            .iter()
            .filter(|r| r.conn_id == conn_id && r.due.is_none())
            .map(|r| r.name.clone())
            .collect();
        for name in names {
            if closed {
                self.remove_or_retry(&name, now);
            } else if let Some(r) = self.records.iter_mut().find(|r| r.name == name) {
                r.due = Some(now + GRACE);
                self.backend.left(&name);
            }
        }
    }

    /// Removes the displays that are due.
    pub fn expire(&mut self, now: Instant) {
        let due: Vec<String> = self
            .records
            .iter()
            .filter(|r| r.due.map_or(false, |t| now >= t))
            .map(|r| r.name.clone())
            .collect();
        for name in due {
            self.remove_or_retry(&name, now);
        }
    }

    /// Looks after the displays there are, and forgets those the backend says went away. A
    /// display now used otherwise than its session asked is word for it (`word`): a desktop taken
    /// over was taken back at the device. A desktop its session may no longer have to itself is
    /// given back, should that have failed before.
    pub fn maintain(&mut self) {
        if self.records.is_empty() {
            return;
        }
        for name in self.backend.maintain() {
            log::warn!("Virtual display {} went away by itself", name);
            self.records.retain(|r| r.name != name);
        }
        for i in 0..self.records.len() {
            let r = &self.records[i];
            if let Some(usage) = self.backend.usage(&r.name).filter(|u| *u != r.spec.mode) {
                log::warn!(
                    "Virtual display {} is used as {} now, not as {}",
                    r.name,
                    usage.name(),
                    r.spec.mode.name()
                );
                let (name, lost) = (r.name.clone(), r.spec.mode == Mode::Privacy);
                let why = if lost {
                    self.backend
                        .lost_why(&name)
                        .unwrap_or_else(|| TAKEN_BACK.to_owned())
                } else {
                    UNUSABLE.to_owned()
                };
                let r = &mut self.records[i];
                if lost {
                    r.lost = Some(why.clone());
                }
                r.spec.mode = usage;
                r.untold = Some(why);
            }
            self.demote_at(i);
        }
    }

    /// The displays of connection `conn_id` may no longer have the desktop to themselves
    /// (`Mode::Privacy`, privacy mode no longer allowed): each that has becomes an extra screen,
    /// the desktop given back, word for its session (`word`).
    pub fn demote(&mut self, conn_id: i32) {
        for i in 0..self.records.len() {
            if self.records[i].conn_id == conn_id {
                self.records[i].privacy = false;
                self.demote_at(i);
            }
        }
    }

    /// Word for the sessions of displays used otherwise than they asked, for those whose
    /// connection is up to take it; the others' waits for them to come back.
    pub fn word(&mut self) -> Vec<Notice> {
        self.records
            .iter_mut()
            .filter(|r| r.due.is_none())
            .filter_map(|r| {
                let why = r.untold.take()?;
                r.told = Some(r.conn_id);
                Some(Notice {
                    conn_id: r.conn_id,
                    slot: r.slot,
                    name: r.name.clone(),
                    why,
                })
            })
            .collect()
    }

    /// Whether connection `conn_id` has displays, so word of them may come.
    pub fn serves(&self, conn_id: i32) -> bool {
        self.records
            .iter()
            .any(|r| r.conn_id == conn_id && r.due.is_none())
    }

    pub fn has_displays(&self) -> bool {
        !self.records.is_empty()
    }

    /// Whether the backend wants looking after more often for now (`Backend::watchful`).
    pub fn watchful(&self) -> bool {
        self.backend.watchful()
    }

    /// When `expire` next has work.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.records.iter().filter_map(|r| r.due).min()
    }

    /// Resizes `owner`'s display `name`, keeping its scale; another session's is left alone.
    /// Answers whether `name` is ours at all, which its caller must then leave alone either way.
    pub fn resize(&mut self, owner: &Owner, name: &str, width: u32, height: u32) -> bool {
        let Some(i) = self.records.iter().position(|r| r.name == name) else {
            return false;
        };
        if &self.records[i].owner != owner {
            log::warn!("Refused to resize virtual display {} of another session", name);
            return true;
        }
        let wanted = Spec {
            width,
            height,
            scale: self.records[i].spec.scale,
            mode: self.records[i].spec.mode,
        };
        match fit(wanted, &self.caps) {
            Ok(spec) if spec != self.records[i].spec => {
                let mut why = UNUSABLE.to_owned();
                match self.backend.set_mode(name, &spec) {
                    Ok(()) => self.records[i].spec = spec,
                    Err(e) => {
                        log::error!("Failed to resize virtual display {}: {}", name, e);
                        why = e.to_string();
                    }
                }
                if let Some(usage) = self.backend.usage(name) {
                    self.records[i].spec.mode = usage;
                }
                let r = &mut self.records[i];
                if wanted.mode == Mode::Privacy && r.spec.mode != Mode::Privacy {
                    r.lost = Some(why.clone());
                    r.untold = Some(why);
                }
            }
            Ok(_) => {}
            Err(e) => log::error!("Refused to resize virtual display {}: {}", name, e),
        }
        true
    }

    fn find(&self, owner: &Owner, slot: i32) -> Option<usize> {
        self.records
            .iter()
            .position(|r| &r.owner == owner && r.slot == slot)
    }

    // Why `owner` may not have a display used as `mode`, if it may not: a desktop taken over
    // (`Mode::Privacy`) is one session's alone, its monitors dark, and is taken over only while
    // no other session has displays. A session in its grace still counts: it may come back.
    fn conflict(&self, owner: &Owner, mode: Mode) -> Option<&'static str> {
        let mut others = false;
        for r in self.records.iter().filter(|r| &r.owner != owner) {
            if r.spec.mode == Mode::Privacy {
                return Some("Another session has the desktop to itself");
            }
            others = true;
        }
        (mode == Mode::Privacy && others).then_some("Another session uses virtual displays")
    }

    // Why a session may not have a desktop to itself now, if it may not: its connection may not
    // (`privacy`), or it has not heard yet of one it lost.
    fn privacy_refused(&self, privacy: bool, owner: &Owner) -> Option<String> {
        if !privacy {
            return Some(NO_PRIVACY.to_owned());
        }
        self.records
            .iter()
            .find(|r| &r.owner == owner && r.lost.is_some())
            .and_then(|r| r.lost.clone())
    }

    // The session of display `i` asked for it used as `mode`: one that asks for anything but the
    // desktop to itself heard of the one it lost.
    fn heard(&mut self, i: usize, mode: Mode) {
        if mode != Mode::Privacy {
            self.records[i].lost = None;
        }
    }

    // Display `i` at `spec`, as its session asks again, short of a use it may not have now: then
    // it keeps the one it has, but for a desktop it may not have to itself, which is given back;
    // it still takes the size, which follows a window on the controlling side. Answers the word
    // on what did not take. A desktop lost meanwhile, or refused for a reason the session may not
    // have heard of on this connection, is word for it beyond the answer (`word`).
    fn use_as_asked(&mut self, i: usize, owner: &Owner, mut spec: Spec) -> Option<String> {
        let name = self.records[i].name.clone();
        let (asked, kept) = (spec.mode, self.records[i].spec.mode);
        let refused = (asked == Mode::Privacy)
            .then(|| self.privacy_refused(self.records[i].privacy, owner))
            .flatten();
        let mut note = refused
            .clone()
            .or_else(|| self.conflict(owner, asked).map(str::to_owned));
        if let Some(why) = &note {
            log::warn!("Kept virtual display {} from its use asked: {}", name, why);
            spec.mode = match kept {
                Mode::Privacy if !self.records[i].privacy => Mode::Shared,
                kept => kept,
            };
        }
        self.records[i].wanted = spec.mode;
        if spec != self.records[i].spec {
            match self.backend.set_mode(&name, &spec) {
                Ok(()) => self.records[i].spec = spec,
                Err(e) => {
                    log::error!("Failed to change virtual display {}: {}", name, e);
                    note = Some(e.to_string());
                }
            }
            if let Some(usage) = self.backend.usage(&name) {
                self.records[i].spec.mode = usage;
            }
        }
        let r = &mut self.records[i];
        if r.spec.mode != spec.mode && note.is_none() {
            note = Some(UNUSABLE.to_owned());
        }
        if r.spec.mode == asked {
            // Used as asked: any word not passed on yet is out of date.
            r.untold = None;
        } else if asked == Mode::Privacy && kept == Mode::Privacy {
            // The desktop it had to itself lost unasked: not taken over again until it heard.
            r.lost = note.clone();
            r.untold = note.clone();
        } else if refused.is_some() && r.told != Some(r.conn_id) {
            r.untold = refused;
        }
        note
    }

    // Display `i` made an extra screen should it have the desktop to itself while its session may
    // not or no longer wants to, its session told in the first case; tried again at the next look
    // should that fail.
    fn demote_at(&mut self, i: usize) {
        let r = &self.records[i];
        if r.spec.mode != Mode::Privacy || (r.privacy && r.wanted == Mode::Privacy) {
            return;
        }
        let (name, unasked) = (r.name.clone(), !r.privacy);
        let spec = Spec {
            mode: Mode::Shared,
            ..r.spec
        };
        let done = self.backend.set_mode(&name, &spec);
        if let Err(e) = &done {
            log::error!("Failed to give the desktop of virtual display {} back: {}", name, e);
        }
        let usage = self
            .backend
            .usage(&name)
            .unwrap_or(if done.is_ok() { Mode::Shared } else { Mode::Privacy });
        let r = &mut self.records[i];
        r.spec.mode = usage;
        if usage != Mode::Privacy {
            log::info!("Virtual display {} no longer has the desktop to itself", name);
            if unasked {
                r.untold = Some(NO_PRIVACY.to_owned());
            }
        }
    }

    fn take_back_at(&mut self, i: usize, conn_id: i32, privacy: bool) -> String {
        let r = &mut self.records[i];
        r.conn_id = conn_id;
        r.due = None;
        r.failures = 0;
        r.privacy = privacy;
        r.name.clone()
    }

    // Displays of the same controlling device under another session whose connection has
    // ended: that session will not come back, as the device came back with a new one. They go
    // before the new display comes, so they cannot renumber it when they expire. A display whose
    // connection is still up is never taken: its session may be alive.
    fn reclaim_stale(&mut self, owner: &Owner) {
        let stale: Vec<String> = self
            .records
            .iter()
            .filter(|r| r.due.is_some() && r.owner.same_device(owner) && &r.owner != owner)
            .map(|r| r.name.clone())
            .collect();
        for name in stale {
            self.remove(&name).ok();
        }
    }

    fn remove_or_retry(&mut self, name: &str, now: Instant) {
        if self.remove(name).is_err() {
            if let Some(r) = self.records.iter_mut().find(|r| r.name == name) {
                r.due = Some(now + retry_delay(r.failures));
            }
        }
    }

    fn remove(&mut self, name: &str) -> Result<(), String> {
        match self.backend.plug_out(name) {
            Ok(()) => {
                log::info!("Plugged out virtual display {}", name);
                self.records.retain(|r| r.name != name);
                Ok(())
            }
            Err(e) => {
                log::error!("Failed to plug out virtual display {}: {}", name, e);
                if let Some(r) = self.records.iter_mut().find(|r| r.name == name) {
                    r.failures += 1;
                }
                Err(e.to_string())
            }
        }
    }
}

// The size the backend can give: scaled down to fit its bounds, keeping the shape; no side under
// `MIN_SIDE` (a small window on the controlling side then scrolls); and even on both sides, which
// the video encoders need.
fn fit(spec: Spec, caps: &Caps) -> Result<Spec, String> {
    if spec.width == 0 || spec.height == 0 {
        return Err(format!("invalid size {}x{}", spec.width, spec.height));
    }
    let (w, h) = (spec.width as f64, spec.height as f64);
    let ratio = 1f64
        .min(caps.max_width as f64 / w)
        .min(caps.max_height as f64 / h);
    let width = (((w * ratio) as u32) & !1).max(MIN_SIDE);
    let height = (((h * ratio) as u32) & !1).max(MIN_SIDE);
    let scale = if caps.hidpi && spec.scale.is_finite() && spec.scale > 1.0 {
        spec.scale.min(MAX_SCALE)
    } else {
        1.0
    };
    Ok(Spec {
        width,
        height,
        scale,
        mode: spec.mode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbb_common::bail;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct World {
        displays: Vec<(String, Spec)>,
        next: u32,
        fail_plug_out: bool,
        fail_set_mode: bool,
        // Hands out the last name again, as a broken backend might.
        repeat_name: bool,
        resets: u32,
        // Whether a name was announced while its display was not there yet.
        announced_early: Vec<bool>,
        // Why the next desktop lost was lost, if not taken back at the device.
        lost_why: Option<String>,
    }

    struct FakeBackend {
        world: Arc<Mutex<World>>,
        max: usize,
        hidpi: bool,
    }

    impl Backend for FakeBackend {
        fn caps(&self) -> Caps {
            Caps {
                kind: "fake",
                max: self.max,
                custom: true,
                hidpi: self.hidpi,
                max_width: 4096,
                max_height: 4096,
                modes: vec![Mode::Shared],
            }
        }

        fn plug_in(&mut self, spec: &Spec, announce: &mut dyn FnMut(&str)) -> ResultType<String> {
            let name = {
                let mut w = self.world.lock().unwrap();
                if !w.repeat_name {
                    w.next += 1;
                }
                format!("VIRTUAL-{}", w.next)
            };
            let early = !self.world.lock().unwrap().displays.iter().any(|(n, _)| *n == name);
            announce(&name);
            let mut w = self.world.lock().unwrap();
            w.announced_early.push(early);
            w.displays.push((name.clone(), *spec));
            Ok(name)
        }

        fn set_mode(&mut self, name: &str, spec: &Spec) -> ResultType<()> {
            let mut w = self.world.lock().unwrap();
            if w.fail_set_mode {
                bail!("stuck");
            }
            match w.displays.iter_mut().find(|(n, _)| n == name) {
                Some(d) => d.1 = *spec,
                None => bail!("no display {}", name),
            }
            Ok(())
        }

        fn plug_out(&mut self, name: &str) -> ResultType<()> {
            let mut w = self.world.lock().unwrap();
            if w.fail_plug_out {
                bail!("busy");
            }
            w.displays.retain(|(n, _)| n != name);
            Ok(())
        }

        fn reset_all(&mut self) -> ResultType<()> {
            let mut w = self.world.lock().unwrap();
            w.displays.clear();
            w.resets += 1;
            Ok(())
        }

        fn usage(&self, name: &str) -> Option<Mode> {
            let w = self.world.lock().unwrap();
            w.displays
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, spec)| spec.mode)
        }

        fn lost_why(&mut self, _name: &str) -> Option<String> {
            self.world.lock().unwrap().lost_why.take()
        }
    }

    fn manager(max: usize, hidpi: bool) -> (Manager, Arc<Mutex<World>>) {
        let world = Arc::new(Mutex::new(World::default()));
        let backend = FakeBackend {
            world: world.clone(),
            max,
            hidpi,
        };
        (Manager::new(Box::new(backend)), world)
    }

    fn owner(peer_id: &str, session_id: u64) -> Owner {
        Owner {
            peer_id: peer_id.to_owned(),
            peer_name: "mac".to_owned(),
            session_id,
        }
    }

    fn spec(width: u32, height: u32) -> Spec {
        Spec {
            width,
            height,
            scale: 1.0,
            mode: Mode::Shared,
        }
    }

    fn add(m: &mut Manager, conn_id: i32, owner: &Owner, slot: i32, spec: Spec) -> Result<String, String> {
        m.plug_in(conn_id, owner, slot, spec, true, &mut |_| {}).map(|(name, _)| name)
    }

    fn live(world: &Arc<Mutex<World>>) -> Vec<String> {
        world
            .lock()
            .unwrap()
            .displays
            .iter()
            .map(|(n, _)| n.clone())
            .collect()
    }

    fn privacy(width: u32, height: u32) -> Spec {
        Spec {
            mode: Mode::Privacy,
            ..spec(width, height)
        }
    }

    #[test]
    fn a_desktop_taken_over_is_one_session_alone() {
        let (mut m, world) = manager(4, false);
        let (a, b) = (owner("A", 1), owner("B", 2));
        add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        // Nobody else gets a display meanwhile, of any use.
        assert!(add(&mut m, 20, &b, 0, spec(1920, 1080)).is_err());
        assert!(add(&mut m, 20, &b, 0, privacy(1920, 1080)).is_err());
        // Its own session may have more.
        add(&mut m, 10, &a, 1, spec(1920, 1080)).unwrap();
        // Still so while its session may come back.
        m.conn_ended(10, false, Instant::now());
        assert!(add(&mut m, 20, &b, 0, spec(1920, 1080)).is_err());
        // Gone, the desktop is free.
        m.plug_out(&a, ALL_SLOTS);
        add(&mut m, 20, &b, 0, spec(1920, 1080)).unwrap();
        assert_eq!(live(&world).len(), 1);
    }

    #[test]
    fn a_desktop_is_taken_over_only_while_no_other_session_has_displays() {
        let (mut m, world) = manager(4, false);
        let (a, b) = (owner("A", 1), owner("B", 2));
        let shared = add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        // A new display asked for it: an extra screen, the controlling side told.
        let (other, note) = m
            .plug_in(20, &b, 0, privacy(1920, 1080), true, &mut |_| {})
            .unwrap();
        assert_eq!(note.as_deref(), Some("Another session uses virtual displays"));
        assert_eq!(mode_of(&world, &other), Mode::Shared);
        assert!(m.word().is_empty());
        // Nor by taking a display back for it: kept in its use, the controlling side told; it still
        // takes the size asked, which follows a window there.
        add(&mut m, 20, &b, 0, spec(1920, 1080)).unwrap();
        let (name, note) = m
            .plug_in(10, &a, 0, privacy(2048, 1030), true, &mut |_| {})
            .unwrap();
        assert_eq!(name, shared);
        assert_eq!(note.as_deref(), Some("Another session uses virtual displays"));
        let w = world.lock().unwrap();
        assert!(w.displays.iter().all(|(_, s)| s.mode == Mode::Shared));
        assert!(w.displays.contains(&(shared, spec(2048, 1030))));
    }

    // As the person at the device would by Ctrl+P: the backend uses `name` as an extra screen.
    fn taken_back(world: &Arc<Mutex<World>>, name: &str) {
        let mut w = world.lock().unwrap();
        w.displays.iter_mut().find(|(n, _)| n == name).unwrap().1.mode = Mode::Shared;
    }

    fn word(slot: i32, name: &str, why: &str, conn_id: i32) -> Notice {
        Notice {
            conn_id,
            slot,
            name: name.to_owned(),
            why: why.to_owned(),
        }
    }

    fn mode_of(world: &Arc<Mutex<World>>, name: &str) -> Mode {
        let w = world.lock().unwrap();
        w.displays.iter().find(|(n, _)| n == name).unwrap().1.mode
    }

    #[test]
    fn a_desktop_taken_back_at_the_device_is_word_for_its_session_once() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 2, privacy(2560, 1440)).unwrap();
        add(&mut m, 10, &a, 0, spec(1920, 1080)).unwrap();
        m.maintain();
        assert!(m.word().is_empty());
        taken_back(&world, &name);
        m.maintain();
        assert_eq!(m.word(), vec![word(2, &name, TAKEN_BACK, 10)]);
        m.maintain();
        assert!(m.word().is_empty());
        // The display stays; its size still follows the window.
        assert!(m.resize(&a, &name, 1920, 1080));
        let w = world.lock().unwrap();
        assert!(w.displays.contains(&(name, spec(1920, 1080))));
    }

    #[test]
    fn a_desktop_lost_otherwise_than_at_the_device_is_word_of_why() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        world.lock().unwrap().lost_why = Some("held no more".to_owned());
        taken_back(&world, &name);
        m.maintain();
        assert_eq!(m.word(), vec![word(0, &name, "held no more", 10)]);
        // Not taken over again before the session heard, as for one taken back.
        let (_, note) = m.plug_in(10, &a, 0, privacy(2560, 1440), true, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some("held no more"));
        assert_eq!(mode_of(&world, &name), Mode::Shared);
    }

    #[test]
    fn a_desktop_taken_back_is_not_taken_over_again_until_its_session_heard() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        taken_back(&world, &name);
        m.maintain();
        assert_eq!(m.word().len(), 1);
        // Asked before the session heard, as a window resized meanwhile asks: the size only.
        let (_, note) = m.plug_in(10, &a, 0, privacy(2048, 1152), true, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some(TAKEN_BACK));
        assert!(world.lock().unwrap().displays.contains(&(name.clone(), spec(2048, 1152))));
        assert!(m.word().is_empty());
        // The session heard, asking for an extra screen; asked again, it is taken over again.
        let (_, note) = m.plug_in(10, &a, 0, spec(2048, 1152), true, &mut |_| {}).unwrap();
        assert_eq!(note, None);
        let (_, note) = m.plug_in(10, &a, 0, privacy(2048, 1152), true, &mut |_| {}).unwrap();
        assert_eq!(note, None);
        assert_eq!(mode_of(&world, &name), Mode::Privacy);
    }

    #[test]
    fn word_for_a_session_away_waits_for_it_to_come_back() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        m.conn_ended(10, false, Instant::now());
        assert!(!m.serves(10));
        taken_back(&world, &name);
        m.maintain();
        assert!(m.word().is_empty());
        // Back, it asks for what it held: not taken over again, and told why once answered.
        let (_, note) = m.plug_in(20, &a, 0, privacy(2560, 1440), true, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some(TAKEN_BACK));
        assert_eq!(mode_of(&world, &name), Mode::Shared);
        assert!(m.serves(20));
        assert_eq!(m.word(), vec![word(0, &name, TAKEN_BACK, 20)]);
    }

    #[test]
    fn word_that_went_to_a_connection_already_dead_is_said_again_on_the_next() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        taken_back(&world, &name);
        m.maintain();
        // Sent before the end of the connection was known: lost on the way.
        assert_eq!(m.word().len(), 1);
        m.conn_ended(10, false, Instant::now());
        let (_, note) = m.plug_in(20, &a, 0, privacy(2560, 1440), true, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some(TAKEN_BACK));
        assert_eq!(m.word(), vec![word(0, &name, TAKEN_BACK, 20)]);
        // Once per connection.
        m.plug_in(20, &a, 0, privacy(2048, 1152), true, &mut |_| {}).unwrap();
        assert!(m.word().is_empty());
    }

    #[test]
    fn nothing_is_taken_over_for_a_session_that_has_not_heard_of_a_desktop_it_lost() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        taken_back(&world, &name);
        m.maintain();
        // A new display asked for meanwhile: an extra screen.
        let (other, note) = m.plug_in(10, &a, 1, privacy(1920, 1080), true, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some(TAKEN_BACK));
        assert_eq!(mode_of(&world, &other), Mode::Shared);
        // Heard, also when taken back by a connection refused virtual displays.
        assert!(m.take_back(10, &a, 0, true, Mode::Shared).is_some());
        let (_, note) = m.plug_in(10, &a, 1, privacy(1920, 1080), true, &mut |_| {}).unwrap();
        assert_eq!(note, None);
        assert_eq!(mode_of(&world, &other), Mode::Privacy);
    }

    #[test]
    fn a_new_display_the_desktop_is_not_taken_over_for_stays_an_extra_screen() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        world.lock().unwrap().fail_set_mode = true;
        let (name, note) = m.plug_in(10, &a, 0, privacy(2560, 1440), true, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some("stuck"));
        assert_eq!(live(&world), vec![name.clone()]);
        assert_eq!(mode_of(&world, &name), Mode::Shared);
        // The answer told it: no word beyond, now or at the next look.
        m.maintain();
        assert!(m.word().is_empty());
        // Asked again once it can be: taken over.
        world.lock().unwrap().fail_set_mode = false;
        let (again, note) = m.plug_in(10, &a, 0, privacy(2560, 1440), true, &mut |_| {}).unwrap();
        assert_eq!((again.as_str(), note), (name.as_str(), None));
        assert_eq!(mode_of(&world, &name), Mode::Privacy);
        // Another session having the desktop to itself still refuses a new display outright.
        let b = owner("B", 2);
        assert!(add(&mut m, 20, &b, 0, privacy(1920, 1080)).is_err());
        assert_eq!(live(&world), vec![name]);
    }

    #[test]
    fn a_desktop_its_session_gives_back_is_given_back_should_it_take_tries() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        world.lock().unwrap().fail_set_mode = true;
        let (_, note) = m.plug_in(10, &a, 0, spec(2560, 1440), true, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some("stuck"));
        m.maintain();
        assert_eq!(mode_of(&world, &name), Mode::Privacy);
        world.lock().unwrap().fail_set_mode = false;
        m.maintain();
        assert_eq!(mode_of(&world, &name), Mode::Shared);
        // Asked for: no word.
        assert!(m.word().is_empty());
    }

    #[test]
    fn word_out_of_date_by_the_time_its_session_is_back_is_not_said() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        m.conn_ended(10, false, Instant::now());
        // Privacy mode turned off for the connection that just ended, then on again.
        m.demote(10);
        assert_eq!(mode_of(&world, &name), Mode::Shared);
        let (_, note) = m.plug_in(20, &a, 0, privacy(2560, 1440), true, &mut |_| {}).unwrap();
        assert_eq!(note, None);
        assert_eq!(mode_of(&world, &name), Mode::Privacy);
        assert!(m.word().is_empty());
    }

    #[test]
    fn a_session_that_may_not_have_the_desktop_to_itself_gets_an_extra_screen() {
        let (mut m, world) = manager(4, false);
        let (a, b) = (owner("A", 1), owner("B", 2));
        // A new one.
        let (shared, note) = m.plug_in(10, &a, 0, privacy(2560, 1440), false, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some(NO_PRIVACY));
        assert_eq!(mode_of(&world, &shared), Mode::Shared);
        // The answer told it: no word beyond, nor when it is answered so again.
        assert!(m.word().is_empty());
        let (_, note) = m.plug_in(10, &a, 0, privacy(2560, 1440), false, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some(NO_PRIVACY));
        assert!(m.word().is_empty());
        m.plug_out(&a, ALL_SLOTS);
        // One taken over already, asked for again by a connection that may not: given back, the
        // session told beyond the answer, as its display now is not what it asked for.
        let taken = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        m.conn_ended(10, false, Instant::now());
        let (_, note) = m.plug_in(20, &a, 0, privacy(2560, 1440), false, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some(NO_PRIVACY));
        assert_eq!(mode_of(&world, &taken), Mode::Shared);
        assert_eq!(m.word(), vec![word(0, &taken, NO_PRIVACY, 20)]);
        m.plug_out(&a, ALL_SLOTS);
        // Taken back by a connection refused virtual displays (no control, say), as it is: short
        // of the desktop to itself.
        let taken = add(&mut m, 30, &b, 0, privacy(2560, 1440)).unwrap();
        m.conn_ended(30, false, Instant::now());
        assert_eq!(m.take_back(40, &b, 0, false, Mode::Privacy), Some(taken.clone()));
        assert_eq!(mode_of(&world, &taken), Mode::Shared);
        assert_eq!(m.word(), vec![word(0, &taken, NO_PRIVACY, 40)]);
    }

    #[test]
    fn a_connection_that_may_no_longer_have_the_desktop_to_itself_gives_it_back() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let taken = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        let shared = add(&mut m, 10, &a, 1, spec(1920, 1080)).unwrap();
        // Another connection's: nothing changes.
        m.demote(11);
        assert!(m.word().is_empty());
        m.demote(10);
        assert_eq!(m.word(), vec![word(0, &taken, NO_PRIVACY, 10)]);
        let w = world.lock().unwrap();
        assert!(w.displays.contains(&(taken.clone(), spec(2560, 1440))));
        assert!(w.displays.contains(&(shared, spec(1920, 1080))));
        drop(w);
        m.demote(10);
        m.maintain();
        assert!(m.word().is_empty());
        // Nor is it taken over again by asking.
        let (_, note) = m.plug_in(10, &a, 0, privacy(2560, 1440), false, &mut |_| {}).unwrap();
        assert_eq!(note.as_deref(), Some(NO_PRIVACY));
        assert_eq!(mode_of(&world, &taken), Mode::Shared);
    }

    #[test]
    fn a_desktop_that_would_not_be_given_back_is_tried_again() {
        let (mut m, world) = manager(4, false);
        let a = owner("A", 1);
        let taken = add(&mut m, 10, &a, 0, privacy(2560, 1440)).unwrap();
        world.lock().unwrap().fail_set_mode = true;
        m.demote(10);
        m.maintain();
        assert!(m.word().is_empty());
        assert_eq!(mode_of(&world, &taken), Mode::Privacy);
        world.lock().unwrap().fail_set_mode = false;
        m.maintain();
        assert_eq!(mode_of(&world, &taken), Mode::Shared);
        assert_eq!(m.word(), vec![word(0, &taken, NO_PRIVACY, 10)]);
    }

    #[test]
    fn a_name_is_announced_before_its_display_shows_up() {
        let (mut m, world) = manager(2, false);
        let mut announced = Vec::new();
        let (name, _) = m
            .plug_in(10, &owner("A", 1), 0, spec(2560, 1440), true, &mut |n| {
                announced.push(n.to_owned())
            })
            .unwrap();
        assert_eq!(announced, vec![name]);
        assert_eq!(world.lock().unwrap().announced_early, vec![true]);
    }

    #[test]
    fn a_repeated_request_is_the_same_display() {
        let (mut m, world) = manager(2, false);
        let a = owner("A", 1);
        let first = add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        assert_eq!(add(&mut m, 10, &a, 0, spec(2560, 1440)), Ok(first.clone()));
        // A new size is a resize, not a second display.
        assert_eq!(add(&mut m, 10, &a, 0, spec(1920, 1080)), Ok(first.clone()));
        assert_eq!(live(&world), vec![first.clone()]);
        assert_eq!(world.lock().unwrap().displays[0].1, spec(1920, 1080));
        // A size that does not fit keeps the display at its own.
        assert_eq!(add(&mut m, 10, &a, 0, spec(0, 0)), Ok(first));
        assert_eq!(world.lock().unwrap().displays[0].1, spec(1920, 1080));
    }

    #[test]
    fn taking_back_leaves_the_size_alone() {
        let (mut m, world) = manager(2, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        m.conn_ended(10, false, Instant::now());
        assert_eq!(m.take_back(20, &a, 0, true, Mode::Shared), Some(name.clone()));
        assert_eq!(m.next_deadline(), None);
        assert_eq!(world.lock().unwrap().displays[0].1, spec(2560, 1440));
        // Nothing to take back on a slot the session never had.
        assert_eq!(m.take_back(20, &a, 1, true, Mode::Shared), None);
        assert_eq!(m.take_back(20, &owner("B", 2), 0, true, Mode::Shared), None);
    }

    #[test]
    fn slots_are_per_session() {
        let (mut m, _world) = manager(3, false);
        let (a, b) = (owner("A", 1), owner("B", 2));
        let a0 = add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        let b0 = add(&mut m, 11, &b, 0, spec(2560, 1440)).unwrap();
        assert_ne!(a0, b0);
        // Removing a slot only ever touches the asking session's own display.
        assert_eq!(m.plug_out(&b, 0), vec![(0, b0, Ok(()))]);
        assert_eq!(m.names(), vec![a0]);
    }

    #[test]
    fn capacity_is_shared_and_frees_up() {
        let (mut m, _world) = manager(1, false);
        let (a, b) = (owner("A", 1), owner("B", 2));
        let a0 = add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        assert_eq!(
            add(&mut m, 11, &b, 0, spec(2560, 1440)),
            Err("No free virtual display".to_owned())
        );
        assert_eq!(m.plug_out(&a, ALL_SLOTS), vec![(0, a0, Ok(()))]);
        assert!(add(&mut m, 11, &b, 0, spec(2560, 1440)).is_ok());
    }

    #[test]
    fn a_closed_session_loses_its_displays_at_once() {
        let (mut m, world) = manager(2, false);
        let a = owner("A", 1);
        add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        add(&mut m, 10, &a, 1, spec(1920, 1080)).unwrap();
        m.conn_ended(10, true, Instant::now());
        assert!(live(&world).is_empty());
        assert!(m.names().is_empty());
        assert_eq!(m.next_deadline(), None);
    }

    #[test]
    fn a_dropped_connection_keeps_its_displays_for_the_grace_period() {
        let (mut m, world) = manager(2, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        let t0 = Instant::now();
        m.conn_ended(10, false, t0);
        assert_eq!(m.next_deadline(), Some(t0 + GRACE));
        m.expire(t0 + GRACE - Duration::from_millis(1));
        assert_eq!(live(&world), vec![name]);
        m.expire(t0 + GRACE);
        assert!(live(&world).is_empty());
    }

    #[test]
    fn a_reconnecting_session_takes_its_display_back() {
        let (mut m, world) = manager(2, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        let t0 = Instant::now();
        m.conn_ended(10, false, t0);
        assert_eq!(add(&mut m, 20, &a, 0, spec(2560, 1440)), Ok(name.clone()));
        assert_eq!(m.next_deadline(), None);
        m.expire(t0 + GRACE * 2);
        assert_eq!(live(&world), vec![name.clone()]);
        // The old connection ending late leaves the new one's display alone.
        m.conn_ended(10, true, t0 + GRACE * 2);
        assert_eq!(live(&world), vec![name.clone()]);
        // The display now belongs to the new connection.
        m.conn_ended(20, true, t0 + GRACE * 2);
        assert!(live(&world).is_empty());
    }

    #[test]
    fn a_new_session_of_the_same_device_reclaims_the_dead_ones() {
        let (mut m, world) = manager(2, false);
        let old = owner("A", 1);
        let other = owner("B", 3);
        add(&mut m, 10, &old, 0, spec(2560, 1440)).unwrap();
        let kept = add(&mut m, 11, &other, 0, spec(2560, 1440)).unwrap();
        m.conn_ended(10, false, Instant::now());
        m.conn_ended(11, false, Instant::now());
        // Device A came back with a new session: its orphan goes first, so even a full backend
        // has room, and the new display is not renumbered when the orphan would have expired.
        let new = add(&mut m, 12, &owner("A", 2), 0, spec(2560, 1440)).unwrap();
        assert_eq!(m.names(), vec![kept.clone(), new.clone()]);
        assert_eq!(live(&world), vec![kept, new]);
    }

    #[test]
    fn the_sessions_waiting_out_their_grace_are_listed_until_they_expire_or_come_back() {
        let (mut m, _world) = manager(2, false);
        let (a, b) = (owner("A", 1), owner("B", 3));
        add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        add(&mut m, 11, &b, 0, spec(2560, 1440)).unwrap();
        assert!(m.waiting().is_empty());
        let now = Instant::now();
        m.conn_ended(10, false, now);
        assert_eq!(m.waiting(), vec![a.clone()]);
        // Its session came back: not waiting any more.
        m.take_back(12, &a, 0, true, Mode::Shared);
        assert!(m.waiting().is_empty());
        m.conn_ended(11, false, now);
        assert_eq!(m.waiting(), vec![b]);
        m.expire(now + GRACE);
        assert!(m.waiting().is_empty());
    }

    #[test]
    fn a_live_session_of_the_same_device_is_never_taken_from() {
        let (mut m, _world) = manager(2, false);
        let first = add(&mut m, 10, &owner("A", 1), 0, spec(2560, 1440)).unwrap();
        let second = add(&mut m, 11, &owner("A", 2), 0, spec(2560, 1440)).unwrap();
        assert_eq!(m.names(), vec![first.clone(), second.clone()]);
        // Even full: a session whose connection is up may be alive.
        assert_eq!(
            add(&mut m, 12, &owner("A", 3), 0, spec(2560, 1440)),
            Err("No free virtual display".to_owned())
        );
        assert_eq!(m.names(), vec![first, second]);
    }

    #[test]
    fn a_display_that_will_not_go_is_tried_ever_less_often_but_kept() {
        let (mut m, world) = manager(2, false);
        let a = owner("A", 1);
        let name = add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        world.lock().unwrap().fail_plug_out = true;
        let (slot, still, res) = m.plug_out(&a, 0).remove(0);
        assert_eq!((slot, still.as_str()), (0, name.as_str()));
        assert!(res.is_err());
        // The session asked; it keeps the display, and nothing retries behind its back.
        assert_eq!(m.names(), vec![name.clone()]);
        assert_eq!(m.next_deadline(), None);
        let mut t = Instant::now();
        m.conn_ended(10, true, t);
        let mut delays = Vec::new();
        for _ in 0..7 {
            let next = m.next_deadline().unwrap();
            delays.push((next - t).as_secs());
            t = next;
            m.expire(t);
        }
        assert_eq!(delays, vec![60, 120, 240, 480, 480, 480, 480]);
        // Still listed as ours, so it renumbers nothing.
        assert_eq!(m.names(), vec![name]);
    }

    #[test]
    fn a_retried_display_goes_once_it_can() {
        let (mut m, world) = manager(2, false);
        let a = owner("A", 1);
        add(&mut m, 10, &a, 0, spec(2560, 1440)).unwrap();
        world.lock().unwrap().fail_plug_out = true;
        let t0 = Instant::now();
        m.conn_ended(10, true, t0);
        assert_eq!(m.next_deadline(), Some(t0 + GRACE));
        world.lock().unwrap().fail_plug_out = false;
        m.expire(t0 + GRACE);
        assert!(m.names().is_empty());
        assert!(live(&world).is_empty());
    }

    #[test]
    fn removing_what_is_not_there_succeeds() {
        let (mut m, _world) = manager(2, false);
        assert_eq!(m.plug_out(&owner("A", 1), 3), vec![(3, String::new(), Ok(()))]);
        // Nothing to answer per display; the caller answers for all.
        assert!(m.plug_out(&owner("A", 1), ALL_SLOTS).is_empty());
    }

    #[test]
    fn requests_are_checked() {
        let (mut m, _world) = manager(2, false);
        let a = owner("A", 1);
        assert!(add(&mut m, 10, &a, -1, spec(2560, 1440)).is_err());
        assert!(add(&mut m, 10, &a, MAX_SLOT + 1, spec(2560, 1440)).is_err());
        assert!(add(&mut m, 10, &a, 0, spec(0, 0)).is_err());
        assert!(m.names().is_empty());
        // Too small is no reason to refuse: the smallest size is given (`sizes_fit_the_backend`).
        assert!(add(&mut m, 10, &a, 0, spec(320, 200)).is_ok());
    }

    #[test]
    fn a_name_given_twice_is_refused() {
        let (mut m, world) = manager(3, false);
        let first = add(&mut m, 10, &owner("A", 1), 0, spec(2560, 1440)).unwrap();
        world.lock().unwrap().repeat_name = true;
        assert!(add(&mut m, 11, &owner("B", 2), 0, spec(2560, 1440)).is_err());
        assert_eq!(m.names(), vec![first]);
    }

    #[test]
    fn sizes_fit_the_backend() {
        let caps = Caps {
            kind: "fake",
            max: 1,
            custom: true,
            hidpi: false,
            max_width: 3840,
            max_height: 2160,
            modes: vec![Mode::Shared],
        };
        // Within bounds, odd sides are made even.
        assert_eq!(fit(spec(1366, 768), &caps), Ok(spec(1366, 768)));
        assert_eq!(fit(spec(1367, 769), &caps), Ok(spec(1366, 768)));
        // Too big is scaled down whole, keeping the shape.
        assert_eq!(fit(spec(5120, 2880), &caps), Ok(spec(3840, 2160)));
        assert_eq!(fit(spec(7680, 2160), &caps), Ok(spec(3840, 1080)));
        // Too small is raised to the smallest side; no size at all is refused.
        assert_eq!(fit(spec(400, 300), &caps), Ok(spec(480, 480)));
        assert_eq!(fit(spec(1200, 301), &caps), Ok(spec(1200, 480)));
        assert!(fit(spec(0, 0), &caps).is_err());
        // Without HiDPI the scale is dropped; with it, it is kept within reason.
        let hidpi = Spec {
            scale: 2.0,
            ..spec(2880, 1800)
        };
        assert_eq!(fit(hidpi, &caps).unwrap().scale, 1.0);
        let caps = Caps {
            hidpi: true,
            ..caps
        };
        assert_eq!(fit(hidpi, &caps).unwrap().scale, 2.0);
        let wild = Spec {
            scale: f64::NAN,
            ..spec(2880, 1800)
        };
        assert_eq!(fit(wild, &caps).unwrap().scale, 1.0);
        let huge = Spec {
            scale: 9.0,
            ..spec(2880, 1800)
        };
        assert_eq!(fit(huge, &caps).unwrap().scale, MAX_SCALE);
    }

    #[test]
    fn a_mode_rides_along_and_an_unknown_one_is_shared() {
        // The wire value maps to a mode; anything the controlled side does not know is Shared.
        assert_eq!(Mode::from_wire(0), Mode::Shared);
        assert_eq!(Mode::from_wire(1), Mode::Privacy);
        assert_eq!(Mode::from_wire(2), Mode::Nested);
        assert_eq!(Mode::from_wire(3), Mode::Concurrent);
        assert_eq!(Mode::from_wire(99), Mode::Shared);
        assert_eq!(Mode::from_wire(-1), Mode::Shared);
        assert_eq!(Mode::default(), Mode::Shared);
        assert_eq!(Mode::Nested.name(), "nested");
        // Fitting a size keeps the mode it was asked in.
        let caps = Caps {
            kind: "fake",
            max: 1,
            custom: true,
            hidpi: false,
            max_width: 3840,
            max_height: 2160,
            modes: vec![Mode::Shared],
        };
        let nested = Spec {
            mode: Mode::Nested,
            ..spec(5120, 2880)
        };
        let fitted = fit(nested, &caps).unwrap();
        assert_eq!((fitted.width, fitted.height), (3840, 2160));
        assert_eq!(fitted.mode, Mode::Nested);
    }

    #[test]
    fn resize_is_for_the_owning_session_only() {
        let (mut m, world) = manager(2, true);
        let (a, b) = (owner("A", 1), owner("B", 2));
        let hidpi = Spec {
            scale: 2.0,
            ..spec(2880, 1800)
        };
        let name = add(&mut m, 10, &a, 0, hidpi).unwrap();
        assert!(!m.resize(&a, "HDMI-1", 1920, 1080));
        // Ours, so the caller must not touch it, but not B's to resize.
        assert!(m.resize(&b, &name, 1280, 800));
        assert_eq!(world.lock().unwrap().displays[0].1, hidpi);
        assert!(m.resize(&a, &name, 1920, 1200));
        // The scale the session asked for survives a resize.
        assert_eq!(
            world.lock().unwrap().displays[0].1,
            Spec {
                scale: 2.0,
                ..spec(1920, 1200)
            }
        );
    }

    #[test]
    fn reset_clears_leftovers() {
        let (mut m, world) = manager(2, false);
        world
            .lock()
            .unwrap()
            .displays
            .push(("LEFTOVER".to_owned(), spec(1920, 1080)));
        m.reset();
        assert!(live(&world).is_empty());
        assert_eq!(world.lock().unwrap().resets, 1);
    }
}
