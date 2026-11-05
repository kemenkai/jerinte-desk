//! The root service's side: which connector each `--server` holds, lit or not, and putting the
//! kernel back as it was when one is let go, the `--server` goes away, or the service starts again
//! after a crash. Only root writes these settings, and root alone picks the connector: a client
//! names only connectors it was given.
//!
//! Best is the vkms driver's connector, loaded at boot and kept off until it is needed (see
//! `vkms`): turning it on and off is all there is to it. Without it, a physical connector without
//! a monitor is forced on with an EDID of ours.

use super::{
    connectors::{self, Connector},
    edid::Sink,
    protocol::{Gone, Kind, Restore},
};
use hbb_common::{bail, log, ResultType};
use serde_derive::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// The EDID a connector is lit with as a last resort, where the kernel has it built in: 1024x768,
/// 65 MHz, no HDMI block.
pub const BUILTIN_EDID: &str = "edid/1024x768.bin";

/// Connectors held at once, by all clients together.
const MAX_LEASES: usize = 2;

// Lightings a user may ask for in `BURST_WINDOW`: plenty for a session, too few to keep the
// kernel probing connectors. By user, not by connection: a client can always connect again.
const BURST: usize = 6;
const BURST_WINDOW: Duration = Duration::from_secs(60);

// Connectors put back that a new `--server` may find its predecessor's output still on.
const RELEASED_KEPT: usize = 8;

// How long a `--server` that let its last connector go is still taken to hold one: it puts the
// other outputs back after letting go.
const AFTER_RELEASE: Duration = Duration::from_secs(15);

/// Connectors held by all clients, and when the last went, for the service deciding whether to
/// restart a `--server`.
pub struct Held {
    count: AtomicUsize,
    emptied: Mutex<Option<Instant>>,
}

impl Held {
    const fn new() -> Self {
        Self {
            count: AtomicUsize::new(0),
            emptied: Mutex::new(None),
        }
    }

    fn set(&self, count: usize, now: Instant) {
        if self.count.swap(count, Ordering::SeqCst) > 0 && count == 0 {
            *self.emptied.lock().unwrap() = Some(now);
        }
    }

    fn held(&self, now: Instant) -> bool {
        self.count.load(Ordering::SeqCst) > 0
            || self
                .emptied
                .lock()
                .unwrap()
                .map_or(false, |at| now.saturating_duration_since(at) < AFTER_RELEASE)
    }
}

static HELD: Held = Held::new();

/// Whether a session holds a connector, or let its last go a moment ago. Its `--server` must not
/// be restarted meanwhile: that would take its displays away, the one waiting out its grace for a
/// reconnecting session too, or cut short the putting back of the other outputs.
pub fn held() -> bool {
    HELD.held(Instant::now())
}

/// What root changes in the kernel. sysfs in the service; a record in the tests.
pub trait Kernel {
    fn connectors(&self) -> Vec<Connector>;
    /// The running kernel's release, "6.6.127-amd64-desktop".
    fn release(&self) -> String;
    fn cmdline(&self) -> String;
    /// /sys/module/drm/parameters/edid_firmware.
    fn edid_param(&self) -> ResultType<String>;
    fn set_edid_param(&mut self, value: &str) -> ResultType<()>;
    /// /sys/module/firmware_class/parameters/path: where the firmware loader looks first.
    fn firmware_path(&self) -> ResultType<String>;
    fn set_firmware_path(&mut self, value: &str) -> ResultType<()>;
    /// The directory of ours that path may name.
    fn our_firmware_dir(&self) -> String;
    /// Puts a file there, or takes it away with None.
    fn put_firmware(&mut self, name: &str, bytes: Option<&[u8]>) -> ResultType<()>;
    /// Writes "on", "off" or "detect" to the connector's status; "on" makes the kernel read its
    /// modes.
    fn set_status(&mut self, card: &str, name: &str, status: &str) -> ResultType<()>;
    /// Tells whoever follows the card's events, the X server, that its connectors changed: the
    /// vkms driver says nothing when one is turned on or off.
    fn hotplug(&mut self, card: &str) -> ResultType<()>;
    fn modes(&self, card: &str, name: &str) -> Vec<String>;
    /// Whether the connector has an EDID: a forced connector without one still gets modes, made
    /// up by the kernel.
    fn has_edid(&self, card: &str, name: &str) -> bool;
    /// Gives the connector `edid`, or takes the EDID given back with None, through debugfs.
    fn override_edid(&mut self, card: &str, name: &str, edid: Option<&[u8]>) -> ResultType<()>;
    /// The CRTCs of `card`, where the card says.
    fn crtcs(&self, card: &str) -> Option<usize>;
    /// The encoders connector `id` of `card` can be driven by, where the card says.
    fn encoders(&self, card: &str, id: u32) -> Option<Vec<u32>>;
    /// What an Intel card's display is, which bounds the clocks of its HDMI ports.
    fn intel_display(&self, card: &str) -> Option<IntelDisplay>;
    /// Remembers the connectors lit, for `recover` after the service died with some lit.
    fn save_lit(&mut self, lit: &[Lit]) -> ResultType<()>;
    fn load_lit(&self) -> Vec<Lit>;
    /// What the loader of vkms left word of at this boot (`vkms::LOADED`, or why not), if it ran.
    fn vkms_word(&self) -> Option<String>;

    // The X server of a session, for giving back a desktop taken over by a client that went.

    /// The X server of process `pid`'s session, as its environment says, with a copy of its X
    /// authority kept by root for as long as it is needed: the session's own file may change.
    fn x_of(&mut self, pid: u32) -> ResultType<XServer>;
    /// The copy made for `x` gone.
    fn drop_x(&mut self, x: &XServer);
    /// Runs `xrandr` on `x` with `args`; with none, a probe of every connector.
    fn xrandr(&mut self, x: &XServer, args: &[String]) -> ResultType<()>;
    /// Remembers the desktops taken over, for `recover` after the service died meanwhile.
    fn save_taken(&mut self, taken: &[Taken]) -> ResultType<()>;
    fn load_taken(&self) -> Vec<Taken>;

    // The keyboards and pointers here, kept from a desktop taken over (`input`).

    /// Keeps them from the desktop, or lets them go. Said again while on, at every look at the
    /// seat (`heartbeat`): a service stuck stops saying it, and the lock lets go by itself.
    fn lock_input(&mut self, on: bool);
    /// Whether Ctrl+P was pressed on a keyboard kept, since last asked.
    fn input_escaped(&mut self) -> bool;

    // A session's `--server` that took its desktop over by itself, for ending it should it not
    // give the desktop back once taken back.

    /// When process `pid` started (`/proc/<pid>/stat`, in clock ticks since boot), telling it from
    /// a later one given the same pid; None once it is gone.
    fn process_start(&self, pid: u32) -> Option<u64>;
    /// Kills process `pid` (SIGKILL).
    fn end_process(&mut self, pid: u32);
}

/// A session's X server, as root reaches it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct XServer {
    /// `:0`.
    pub display: String,
    /// Root's copy of its X authority, if it has one.
    #[serde(default)]
    pub xauthority: Option<String>,
}

/// A desktop a client has taken over (`Mode::Privacy`), and how to give it back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Taken {
    pub restore: Restore,
    pub x: XServer,
}

/// A process, told apart from a later one given the same pid by when it started.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Process {
    pid: u32,
    start: u64,
}

/// A client that took its desktop over by itself, laying it out as root cannot (GNOME's,
/// `Request::LockInput`): the input here is kept from the desktop meanwhile.
struct InputLock {
    client: u64,
    uid: u32,
    process: Process,
}

/// The desktop the person here took back (Ctrl+P) from a process that took it over by itself,
/// which it is to give back. By process, not by connection: its reconnecting changes nothing.
struct Escape {
    uid: u32,
    process: Process,
    at: Instant,
    /// Ended for not giving it back in time.
    ended: bool,
}

// How long a process taken back may take to give the desktop back before it is ended: longer than
// the longest a session's manager is busy with a display (plugging one in waits up to 13 seconds).
const GIVE_BACK_LIMIT: Duration = Duration::from_secs(15);
// How long after a process was ended its user may not take a desktop over again: its controlling
// side reconnects to a new process, which never heard it was taken back.
const ENDED_BARS: Duration = Duration::from_secs(60);

/// How a connector was lit.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// vkms's: only turned on.
    Vkms,
    /// Ours, named in `drm.edid_firmware`, and found by the firmware loader in a directory of
    /// ours `firmware_class.path` names.
    Firmware,
    /// Ours, given through debugfs.
    Override,
    /// The kernel's own, named in `drm.edid_firmware`: kernels before 6.9 have some built in.
    Builtin,
}

/// A connector lit by us, and how.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lit {
    pub card: String,
    pub name: String,
    pub source: Source,
    #[serde(default)]
    pub id: Option<u32>,
    #[serde(default = "kind_connector")]
    pub kind: Kind,
}

fn kind_connector() -> Kind {
    Kind::Connector
}

/// An Intel card's display, as debugfs tells: "display version: 13", "platform: DG2".
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IntelDisplay {
    pub version: Option<u32>,
    pub platform: String,
}

/// A connector given out.
#[derive(Clone, Debug, PartialEq)]
pub struct Reserved {
    pub connector: Connector,
    pub kind: Kind,
    pub max_khz: Option<u32>,
}

struct Lease {
    client: u64,
    /// Whose session it is for.
    uid: u32,
    connector: Connector,
    kind: Kind,
    max_khz: Option<u32>,
    lit: Option<Lit>,
    /// Turned off while another session is on the screen.
    dark: bool,
}

pub struct Leases<K: Kernel> {
    kernel: K,
    leases: Vec<Lease>,
    /// When which users asked for connectors to be lit, oldest first.
    lightings: Vec<(u32, Instant)>,
    /// Connectors put back, most recent last.
    released: Vec<Gone>,
    /// The user whose session is on the screen, as last followed.
    active: Option<u32>,
    /// Desktops taken over, by client.
    taken: Vec<(u64, Taken)>,
    /// Clients whose desktop the person here took back (Ctrl+P): not taken over again until they
    /// gave it back.
    taken_back: Vec<u64>,
    /// Whether the keyboards and pointers here are kept from the desktop.
    locked: bool,
    /// Clients whose `--server` keeps virtual displays of its own (GNOME's, which Mutter takes
    /// back with the process): not to be restarted, as with a connector held.
    holds: Vec<u64>,
    held: &'static Held,
    /// Desktops taken over by their sessions themselves.
    locks: Vec<InputLock>,
    /// Of those, the ones the person here took back, not given back yet.
    escapes: Vec<Escape>,
    /// Users whose process was ended for not giving a desktop back, and until when they may not
    /// take one over again.
    barred: Vec<(u32, Instant)>,
}

impl<K: Kernel> Leases<K> {
    pub fn new(kernel: K) -> Self {
        Self::counted_in(kernel, &HELD)
    }

    fn counted_in(kernel: K, held: &'static Held) -> Self {
        Self {
            kernel,
            leases: Vec::new(),
            lightings: Vec::new(),
            released: Vec::new(),
            active: None,
            taken: Vec::new(),
            taken_back: Vec::new(),
            locked: false,
            holds: Vec::new(),
            held,
            locks: Vec::new(),
            escapes: Vec::new(),
            barred: Vec::new(),
        }
    }

    /// Client `client`'s `--server` keeps virtual displays of its own, or no more (`on`): it is
    /// not restarted meanwhile. One hold a client; it goes with the client's connection.
    pub fn hold(&mut self, client: u64, on: bool) {
        self.holds.retain(|c| *c != client);
        if on {
            self.holds.push(client);
        }
        self.count();
    }

    /// Puts back what a previous run of the service left lit, and first any desktop taken over
    /// meanwhile: its outputs on again before ours go, never a moment with nothing on. Before
    /// anything else.
    pub fn recover(&mut self) {
        // What root wrote; still, nothing but a plain restore is run.
        let taken: Vec<Taken> = self
            .kernel
            .load_taken()
            .into_iter()
            .filter(|t| t.restore.valid())
            .collect();
        for t in &taken {
            self.give_back(t);
        }
        for lit in self.kernel.load_lit() {
            log::info!("Releasing virtual display connector {} left lit", lit.name);
            self.unlight(&lit);
        }
        self.save().ok();
        for t in &taken {
            self.probe(t);
        }
        self.save_taken();
    }

    /// Client `client`, process `pid`, is about to take the desktop over: should it go without
    /// giving it back, `restore` does, on its X server. It must have a connector lit; said again,
    /// the new `restore` stands.
    pub fn take_over(&mut self, client: u64, pid: u32, restore: Restore) -> ResultType<()> {
        if self.taken_back.contains(&client) {
            bail!("{}", super::super::manager::TAKEN_BACK);
        }
        if !restore.valid() {
            bail!("Not a way to give a desktop back");
        }
        if !self
            .leases
            .iter()
            .any(|l| l.client == client && l.lit.is_some())
        {
            bail!("No virtual display lit for this session");
        }
        let (x, before) = match self.taken.iter().position(|(c, _)| *c == client) {
            Some(i) => {
                let (_, t) = self.taken.remove(i);
                (t.x.clone(), Some(t))
            }
            None => (self.kernel.x_of(pid)?, None),
        };
        self.taken.push((client, Taken { restore, x: x.clone() }));
        // Only once remembered may the session turn monitors off: a service that died before
        // would not know how to give them back.
        if let Err(e) = self.try_save_taken() {
            self.taken.pop();
            match before {
                Some(t) => self.taken.push((client, t)),
                None => self.kernel.drop_x(&x),
            }
            return Err(e);
        }
        log::info!("A session takes the desktop of X display {} over", x.display);
        self.update_lock();
        Ok(())
    }

    /// Client `client`, process `pid` of user `uid`, took its desktop over by itself, laying it out
    /// as root cannot (GNOME's): the input here is kept from the desktop until it gives the desktop
    /// back (`restored`), the person here takes it back (Ctrl+P), or its connection goes. Refused
    /// to a process taken back that did not give the desktop back yet, and for a while to a user
    /// whose process was ended for not giving one back.
    pub fn lock_input(&mut self, client: u64, uid: u32, pid: u32, now: Instant) -> ResultType<()> {
        let Some(start) = self.kernel.process_start(pid) else {
            bail!("No process {}", pid);
        };
        let process = Process { pid, start };
        self.barred.retain(|(_, until)| now < *until);
        if self.escapes.iter().any(|e| e.process == process)
            || self.barred.iter().any(|(u, _)| *u == uid)
        {
            bail!("{}", super::super::manager::TAKEN_BACK);
        }
        self.locks.retain(|l| l.client != client);
        self.locks.push(InputLock {
            client,
            uid,
            process,
        });
        log::info!("A session takes its desktop over by itself");
        self.update_lock();
        Ok(())
    }

    /// Process `pid` gave back the desktop the person here took back from it.
    pub fn escape_over(&mut self, pid: u32) {
        let start = self.kernel.process_start(pid);
        self.escapes
            .retain(|e| !(e.process.pid == pid && Some(e.process.start) == start));
    }

    /// Client `client` gave the desktop back itself.
    pub fn restored(&mut self, client: u64) {
        self.taken_back.retain(|c| *c != client);
        self.locks.retain(|l| l.client != client);
        if let Some(i) = self.taken.iter().position(|(c, _)| *c == client) {
            let (_, t) = self.taken.remove(i);
            log::info!("The desktop of X display {} is given back", t.x.display);
            self.kernel.drop_x(&t.x);
            self.save_taken();
        }
        self.update_lock();
    }

    /// Whether the person here took back the desktop `client` took over, which it is to give back.
    pub fn taken_back(&self, client: u64) -> bool {
        self.taken_back.contains(&client)
    }

    /// The same for client `client` of process `pid`, which may have taken it over by itself on a
    /// connection gone since.
    pub fn taken_back_by(&self, client: u64, pid: u32) -> bool {
        if self.taken_back(client) {
            return true;
        }
        let start = self.kernel.process_start(pid);
        self.escapes
            .iter()
            .any(|e| e.process.pid == pid && Some(e.process.start) == start)
    }

    /// At every look at the seat: the lock told it is still wanted, and Ctrl+P on a keyboard kept
    /// ends the takeover of the sessions it keeps the input for. The input is the desktop's again
    /// at once, and the monitors on again as they were, for whoever pressed it, and should the
    /// session be slow to give the desktop back, or hung: ours stay where they are, the session's
    /// to lay out beside them when it gives the desktop back, which it learns from `taken_back`.
    pub fn heartbeat(&mut self) {
        self.heartbeat_at(Instant::now())
    }

    /// `heartbeat` at `now`. A session that took its desktop over by itself gives it back itself;
    /// should it not have in `GIVE_BACK_LIMIT`, it is ended (`end_overdue`).
    pub fn heartbeat_at(&mut self, now: Instant) {
        if self.kernel.input_escaped() && self.locked {
            let locking: Vec<u64> = self
                .taken
                .iter()
                .map(|(c, _)| *c)
                .filter(|c| self.locking_for(*c))
                .collect();
            log::warn!("The person at this device took the desktop back");
            self.taken_back.extend(locking.iter().copied());
            let (escaping, kept): (Vec<InputLock>, Vec<InputLock>) = std::mem::take(&mut self.locks)
                .into_iter()
                .partition(|l| !self.away(l.uid));
            self.locks = kept;
            self.escapes.extend(escaping.into_iter().map(|l| Escape {
                uid: l.uid,
                process: l.process,
                at: now,
                ended: false,
            }));
            self.update_lock();
            let lit: Vec<Taken> = self
                .taken
                .iter()
                .filter(|(c, _)| locking.contains(c))
                .map(|(_, t)| Taken {
                    restore: Restore {
                        back: t.restore.back.clone(),
                        off: Vec::new(),
                    },
                    x: t.x.clone(),
                })
                .collect();
            for t in &lit {
                self.give_back(t);
            }
        }
        if self.locked {
            self.kernel.lock_input(true);
        }
        self.end_overdue(now);
    }

    // A process taken back that did not give its desktop back in time is ended: Mutter takes its
    // virtual monitors back with it and lights the desktop's own again, and the service starts a
    // new `--server`, as for one that died. Only the very process: one given its pid since is not.
    // One gone by itself is forgotten.
    fn end_overdue(&mut self, now: Instant) {
        let kernel = &mut self.kernel;
        let barred = &mut self.barred;
        self.escapes.retain_mut(|e| {
            if kernel.process_start(e.process.pid) != Some(e.process.start) {
                return false;
            }
            if !e.ended && now.saturating_duration_since(e.at) >= GIVE_BACK_LIMIT {
                log::error!(
                    "The session taken back did not give its desktop back: ending process {}",
                    e.process.pid
                );
                kernel.end_process(e.process.pid);
                barred.push((e.uid, now + ENDED_BARS));
                e.ended = true;
            }
            true
        });
    }

    // Whether the input here is kept from the desktop for `client`: it took the desktop over, its
    // display is lit, its session is the one on the screen, and the person here did not take the
    // desktop back; or it took its desktop over by itself, and its session is the one on the
    // screen. Its session away, the desktop is given back and the input is another user's.
    fn locking_for(&self, client: u64) -> bool {
        let taken = self.taken.iter().any(|(c, _)| *c == client)
            && !self.taken_back.contains(&client)
            && self
                .leases
                .iter()
                .any(|l| l.client == client && l.lit.is_some() && !self.away(l.uid));
        taken
            || self
                .locks
                .iter()
                .any(|l| l.client == client && !self.away(l.uid))
    }

    fn update_lock(&mut self) {
        let on = self.taken.iter().any(|(c, _)| self.locking_for(*c))
            || self.locks.iter().any(|l| self.locking_for(l.client));
        if on != self.locked {
            self.locked = on;
            self.kernel.lock_input(on);
        }
    }

    // The desktop of `t` given back as it says, or failing that each output on at the mode it
    // likes: a monitor left dark would be worse than one at another mode. Should even that fail
    // (its X server gone, say), ours are let go all the same: a desktop that sees them go, such
    // as Deepin's, turns its own monitors on again.
    fn give_back(&mut self, t: &Taken) {
        match self.kernel.xrandr(&t.x, &t.restore.args()) {
            Ok(()) => log::info!("Gave back the desktop of X display {}", t.x.display),
            Err(e) => {
                log::error!("Failed to give back the desktop of {}: {}", t.x.display, e);
                if let Err(e) = self.kernel.xrandr(&t.x, &t.restore.fallback_args()) {
                    log::error!("Failed to turn the monitors of {} on: {}", t.x.display, e);
                }
            }
        }
    }

    // The X server of `t` made to look at its connectors, ours being gone: one that missed the
    // change would keep showing the desktop on them. Its X authority copy is no longer needed.
    fn probe(&mut self, t: &Taken) {
        if let Err(e) = self.kernel.xrandr(&t.x, &[]) {
            log::warn!("Failed to have X display {} look at its outputs: {}", t.x.display, e);
        }
        self.kernel.drop_x(&t.x);
    }

    fn save_taken(&mut self) {
        if let Err(e) = self.try_save_taken() {
            log::error!("Failed to remember the desktops taken over: {}", e);
        }
    }

    fn try_save_taken(&mut self) -> ResultType<()> {
        let taken: Vec<Taken> = self.taken.iter().map(|(_, t)| t.clone()).collect();
        self.kernel.save_taken(&taken)
    }

    /// The connectors last put back, most recent last.
    pub fn released(&self) -> Vec<Gone> {
        self.released.clone()
    }

    /// Displays that could still be given out; the kind the next would be; why there are no vkms
    /// displays, if there are none.
    pub fn status(&self) -> (usize, Option<Kind>, String) {
        let candidates = self.candidates(false);
        let free = candidates
            .len()
            .min(MAX_LEASES.saturating_sub(self.leases.len()));
        let best = candidates.first().filter(|_| free > 0).map(|(_, kind)| *kind);
        let vkms = self.kernel.connectors().iter().any(|c| c.driver == "vkms");
        let note = match (vkms, self.vkms_at_boot()) {
            (true, true) => String::new(),
            (true, false) => "vkms was loaded with the desktop running, which is not safe to use: \
                              restart the computer"
                .to_owned(),
            (false, _) => self.kernel.vkms_word().unwrap_or_else(|| {
                "vkms is loaded at boot, before the desktop starts: restart the computer to use it"
                    .to_owned()
            }),
        };
        (free, best, note)
    }

    // Whether vkms was there before the desktop started, which its loader at boot says: loaded
    // with an X server running, it would take that X server down when it lets it go (before
    // 21.1.25), and the cursor would be drawn in software meanwhile.
    fn vkms_at_boot(&self) -> bool {
        self.kernel.vkms_word().as_deref() == Some(super::vkms::LOADED)
    }

    /// Gives `client`, a session of user `uid`, a connector, not lit yet: its name is known before
    /// the display is there. `skip_vkms`: none of vkms's, which that session's X server lacks.
    pub fn reserve(&mut self, client: u64, uid: u32, skip_vkms: bool) -> ResultType<Reserved> {
        // One not lit yet is one the client gave up on, its answer not having come in time, say:
        // a client reserves one at a time, and lights or lets go of it before the next.
        self.leases.retain(|l| l.client != client || l.lit.is_some());
        self.count();
        if self.leases.len() >= MAX_LEASES {
            bail!("No free virtual display");
        }
        let Some((connector, kind)) = self.candidates(skip_vkms).into_iter().next() else {
            bail!("No free connector for a virtual display");
        };
        let max_khz = self.max_khz(&connector, kind);
        self.leases.push(Lease {
            client,
            uid,
            connector: connector.clone(),
            kind,
            max_khz,
            lit: None,
            dark: false,
        });
        self.count();
        Ok(Reserved {
            connector,
            kind,
            max_khz,
        })
    }

    /// Lights `client`'s connector `name`. Answers its modes.
    pub fn activate(&mut self, client: u64, name: &str, now: Instant) -> ResultType<Vec<String>> {
        let Some(i) = self.find(client, name) else {
            bail!("Connector {} is not reserved", name);
        };
        let lease = &self.leases[i];
        let (card, uid, kind, id) = (
            lease.connector.card.clone(),
            lease.uid,
            lease.kind,
            lease.connector.id,
        );
        if lease.lit.is_some() {
            return Ok(self.kernel.modes(&card, name));
        }
        // Lit, it would show on the X server of the session on the screen, someone else's.
        if self.away(uid) {
            bail!("The session of this virtual display is not the one on the screen");
        }
        self.lightings
            .retain(|(_, at)| now.saturating_duration_since(*at) < BURST_WINDOW);
        if self.lightings.iter().filter(|(who, _)| *who == uid).count() >= BURST {
            bail!("Too many virtual displays asked for; try again in a minute");
        }
        self.lightings.push((uid, now));
        for source in self.sources(kind) {
            let lit = Lit {
                card: card.clone(),
                name: name.to_owned(),
                source,
                id,
                kind,
            };
            // Remembered before it is lit, so a crash halfway leaves nothing behind; nothing is
            // lit that could not be remembered.
            self.leases[i].lit = Some(lit.clone());
            if let Err(e) = self.save() {
                self.leases[i].lit = None;
                bail!("Cannot keep a record of the connectors lit: {}", e);
            }
            match self.light(&lit, self.sink(i)) {
                Ok(()) if source == Source::Vkms => return Ok(self.kernel.modes(&card, name)),
                Ok(()) if self.kernel.has_edid(&card, name) => {
                    let modes = self.kernel.modes(&card, name);
                    if !modes.is_empty() {
                        return Ok(modes);
                    }
                    log::warn!("Connector {} has an EDID, but no modes", name);
                }
                Ok(()) => log::warn!("Connector {} got no EDID ({:?})", name, source),
                Err(e) => log::warn!("Failed to light connector {} ({:?}): {}", name, source, e),
            }
            self.unlight_lease(i);
        }
        bail!("Failed to light connector {}", name)
    }

    /// Lets `client`'s connector `name` go.
    pub fn release(&mut self, client: u64, name: &str) -> ResultType<()> {
        let Some(i) = self.find(client, name) else {
            // Gone already counts as let go.
            return Ok(());
        };
        self.unlight_lease(i);
        self.leases.remove(i);
        self.save().ok();
        // A session lets its displays go once it gave the desktop back: should word of that have
        // been lost, the way to give it back is forgotten now, not used on a layout since changed.
        if !self
            .leases
            .iter()
            .any(|l| l.client == client && l.lit.is_some())
        {
            self.restored(client);
        }
        Ok(())
    }

    /// Client `client` went. A desktop it took over and did not give back is given back first: the
    /// other outputs on again before ours go dark, then its X server made to look.
    pub fn release_client(&mut self, client: u64) {
        let taken = self
            .taken
            .iter()
            .position(|(c, _)| *c == client)
            .map(|i| self.taken.remove(i).1);
        if let Some(t) = &taken {
            log::warn!("A session went with the desktop of {} taken over", t.x.display);
            self.give_back(t);
        }
        self.taken_back.retain(|c| *c != client);
        self.locks.retain(|l| l.client != client);
        self.update_lock();
        self.hold(client, false);
        let names: Vec<String> = self
            .leases
            .iter()
            .filter(|l| l.client == client)
            .map(|l| l.connector.name.clone())
            .collect();
        for name in names {
            self.release(client, &name).ok();
        }
        if let Some(t) = &taken {
            self.probe(t);
            self.save_taken();
        }
    }

    /// Follows the session on the screen, user `active`'s: a connector lit for another user's
    /// session is turned off meanwhile, as every X server on the machine sees it, the one of a
    /// login screen too, which could put its login box on it. Turned on again when its session is
    /// back. The login screen's server, running as root, follows nothing: it runs only while that
    /// screen is on.
    pub fn follow(&mut self, active: Option<u32>) {
        if active.is_none() {
            return;
        }
        self.active = active;
        for i in 0..self.leases.len() {
            let lease = &self.leases[i];
            let Some(lit) = lease.lit.clone() else {
                continue;
            };
            let away = self.away(lease.uid);
            if away == lease.dark {
                continue;
            }
            // A desktop taken over is given back before its display goes dark: its session finds
            // the display gone and lets it go, and is back to its own monitors when it returns.
            if away {
                let client = lease.client;
                if let Some(t) = self
                    .taken
                    .iter()
                    .find(|(c, _)| *c == client)
                    .map(|(_, t)| t.clone())
                {
                    self.give_back(&t);
                }
                self.update_lock();
            }
            let status = if away { "off" } else { "on" };
            if let Err(e) = self.kernel.set_status(&lit.card, &lit.name, status) {
                log::error!("Failed to turn connector {} {}: {}", lit.name, status, e);
                continue;
            }
            if lit.source == Source::Vkms {
                if let Err(e) = self.kernel.hotplug(&lit.card) {
                    log::warn!("Failed to tell the X server about {}: {}", lit.name, e);
                }
            }
            log::info!(
                "Virtual display connector {} turned {}: its session is {}",
                lit.name,
                status,
                if away { "away" } else { "back" }
            );
            self.leases[i].dark = away;
        }
        self.update_lock();
    }

    // Whether user `uid`'s session is not the one on the screen, as far as is known.
    fn away(&self, uid: u32) -> bool {
        uid != 0 && self.active.map_or(false, |active| active != uid)
    }

    fn find(&self, client: u64, name: &str) -> Option<usize> {
        self.leases
            .iter()
            .position(|l| l.client == client && l.connector.name == name)
    }

    // The connectors that could be given out together, best first, each with the kind of
    // display it makes. A physical one only where it is safe and our setting can be undone:
    //
    // - Not named by the edid_firmware parameter already: that entry is someone's setting, which
    //   letting the connector go would remove.
    // - Not one an entry for it would not name alone: the kernel takes an entry for every
    //   connector whose name starts with the entry's, on every card, so DP-1 also stands for
    //   DP-10 and for another card's DP-1, where a monitor would get our EDID instead of its own.
    // - Not one the kernel command line turned off.
    // - Not the other end of a port in use: DisplayPort++ makes a DP and an HDMI connector of each
    //   port, sharing an encoder, and only one of them can be on.
    // - Not the last free CRTC of its card, which a monitor plugged in meanwhile would need.
    fn candidates(&self, skip_vkms: bool) -> Vec<(Connector, Kind)> {
        let all = self.kernel.connectors();
        let param = self.kernel.edid_param().unwrap_or_default();
        let off = connectors::turned_off(&self.kernel.cmdline());
        let taken: Vec<(String, String)> = self
            .leases
            .iter()
            .map(|l| (l.connector.card.clone(), l.connector.name.clone()))
            .collect();
        let held = |c: &Connector| taken.iter().any(|(card, name)| *card == c.card && *name == c.name);
        let encoders = |c: &Connector| c.id.and_then(|id| self.kernel.encoders(&c.card, id));
        // Encoders in use, and CRTCs still free, by card.
        let mut busy: HashMap<String, Vec<u32>> = HashMap::new();
        let mut spare: HashMap<String, Option<usize>> = HashMap::new();
        for c in all.iter().filter(|c| c.connected || c.enabled || held(c)) {
            busy.entry(c.card.clone())
                .or_default()
                .extend(encoders(c).unwrap_or_default());
        }
        let vkms_at_boot = self.vkms_at_boot();
        let mut picked = Vec::new();
        for c in connectors::usable(&all, &taken) {
            let Some(kind) = c.kind() else {
                continue;
            };
            if kind == Kind::Vkms {
                if vkms_at_boot && !skip_vkms {
                    picked.push((c.clone(), kind));
                }
                continue;
            }
            let alone = !all
                .iter()
                .any(|o| (o.card != c.card || o.name != c.name) && o.name.starts_with(&c.name));
            if connectors::names(&param, &c.name)
                || off.contains(&c.name)
                || !alone
                || self.fixed_clocks(c)
            {
                continue;
            }
            let own = encoders(c).unwrap_or_default();
            let used = busy.entry(c.card.clone()).or_default();
            if own.iter().any(|e| used.contains(e)) {
                continue;
            }
            let free = spare.entry(c.card.clone()).or_insert_with(|| {
                self.kernel.crtcs(&c.card).map(|crtcs| {
                    let in_use = all
                        .iter()
                        .filter(|o| o.card == c.card && (o.enabled || held(o)))
                        .count();
                    crtcs.saturating_sub(in_use)
                })
            });
            if let Some(free) = free {
                if *free <= 1 {
                    continue;
                }
                *free -= 1;
            }
            used.extend(own);
            picked.push((c.clone(), kind));
        }
        picked
    }

    // Whether the port takes only the clocks of a fixed table, which those of our modes never
    // are: Intel's DG2 and display version 14 on, before 6.15 (their PHYs' HDMI rates), and so
    // the xe driver's cards.
    fn fixed_clocks(&self, c: &Connector) -> bool {
        if kernel_at_least(&self.kernel.release(), (6, 15)) {
            return false;
        }
        match c.driver.as_str() {
            "xe" => true,
            "i915" => self.kernel.intel_display(&c.card).map_or(false, |d| {
                d.version.map_or(false, |v| v >= 14) || d.platform == "DG2"
            }),
            _ => false,
        }
    }

    // The fastest pixel clock a connector of `kind` carries, where it is bounded and known: the
    // TMDS clock of Intel's HDMI ports by display version (gen 8-9 300 MHz, gen 10 on 594, gen 13
    // on 600 from 6.11), nouveau's by kernel (6.11 raised it from 165 MHz). Others carry
    // anything, or only the slow mode they are scaled from.
    fn max_khz(&self, c: &Connector, kind: Kind) -> Option<u32> {
        if kind != Kind::Connector {
            return None;
        }
        let release = self.kernel.release();
        match c.driver.as_str() {
            "i915" => Some(
                match self.kernel.intel_display(&c.card).and_then(|d| d.version) {
                    Some(v) if v >= 13 && kernel_at_least(&release, (6, 11)) => 600_000,
                    Some(v) if v >= 10 => 594_000,
                    Some(v) if v >= 8 => 300_000,
                    Some(v) if v >= 5 => 225_000,
                    Some(_) => 165_000,
                    None => 300_000,
                },
            ),
            // Display version 12 and on.
            "xe" => Some(594_000),
            "nouveau" => Some(if kernel_at_least(&release, (6, 11)) {
                297_000
            } else {
                165_000
            }),
            _ => None,
        }
    }

    // The EDID lease `i` is lit with: an HDMI sink where the driver would take one without the
    // HDMI block for DVI (165 MHz), HDMI 2.0 above 340 MHz; otherwise a plain one.
    fn sink(&self, i: usize) -> Sink {
        let lease = &self.leases[i];
        match (lease.kind, lease.connector.driver.as_str()) {
            (Kind::Connector, "i915" | "xe" | "nouveau") => {
                if lease.max_khz.map_or(false, |khz| khz > 340_000) {
                    Sink::Hdmi20
                } else {
                    Sink::Hdmi
                }
            }
            _ => Sink::Plain,
        }
    }

    // Where an EDID can come from, best first: ours through the firmware loader, unless someone
    // else set where it looks; ours through debugfs; the kernel's own, while it has them, which
    // have no HDMI block. vkms needs none.
    fn sources(&self, kind: Kind) -> Vec<Source> {
        if kind == Kind::Vkms {
            return vec![Source::Vkms];
        }
        let mut sources = Vec::new();
        match self.kernel.firmware_path() {
            Ok(path) if path.is_empty() || path == self.kernel.our_firmware_dir() => {
                sources.push(Source::Firmware)
            }
            Ok(path) => log::info!("The firmware loader looks in {} first; not ours", path),
            Err(_) => {}
        }
        sources.push(Source::Override);
        if has_builtin_edids(&self.kernel.release()) {
            sources.push(Source::Builtin);
        }
        sources
    }

    fn light(&mut self, lit: &Lit, sink: Sink) -> ResultType<()> {
        let (card, name) = (lit.card.as_str(), lit.name.as_str());
        match lit.source {
            Source::Vkms => {
                self.kernel.set_status(card, name, "on")?;
                return self.kernel.hotplug(card);
            }
            Source::Firmware => {
                self.kernel
                    .put_firmware(sink.file_name(), Some(&super::edid::generate(sink)))?;
                let dir = self.kernel.our_firmware_dir();
                if self.kernel.firmware_path()? != dir {
                    self.kernel.set_firmware_path(&dir)?;
                }
                let param = self.kernel.edid_param()?;
                self.kernel
                    .set_edid_param(&connectors::with_edid(&param, name, sink.file_name()))?;
            }
            Source::Override => {
                self.kernel
                    .override_edid(card, name, Some(&super::edid::generate(sink)))?;
            }
            Source::Builtin => {
                let param = self.kernel.edid_param()?;
                self.kernel
                    .set_edid_param(&connectors::with_edid(&param, name, BUILTIN_EDID))?;
            }
        }
        self.kernel.set_status(card, name, "on")
    }

    fn unlight_lease(&mut self, i: usize) {
        if let Some(lit) = self.leases[i].lit.take() {
            self.unlight(&lit);
            self.save().ok();
        }
    }

    // Every step, whatever the others did, so as much as can be is put back. Off first and back
    // to detecting last, our EDID gone in between: a monitor plugged into the connector meanwhile
    // must come up with its own. vkms's stays off: it has nothing to detect, and would turn up
    // connected.
    fn unlight(&mut self, lit: &Lit) {
        if let Err(e) = self.kernel.set_status(&lit.card, &lit.name, "off") {
            log::error!("Failed to turn connector {} off: {}", lit.name, e);
        }
        if lit.source == Source::Vkms {
            if let Err(e) = self.kernel.hotplug(&lit.card) {
                log::warn!("Failed to tell the X server about {}: {}", lit.name, e);
            }
        } else {
            self.unlight_edid(lit);
            if let Err(e) = self.kernel.set_status(&lit.card, &lit.name, "detect") {
                log::error!("Failed to let connector {} detect again: {}", lit.name, e);
            }
        }
        self.released.retain(|gone| gone.connector != lit.name);
        self.released.push(Gone {
            connector: lit.name.clone(),
            connector_id: lit.id,
            kind: lit.kind,
        });
        if self.released.len() > RELEASED_KEPT {
            self.released.remove(0);
        }
    }

    fn unlight_edid(&mut self, lit: &Lit) {
        match self.kernel.edid_param() {
            Ok(param) if connectors::names(&param, &lit.name) => {
                if let Err(e) = self
                    .kernel
                    .set_edid_param(&connectors::without_edid(&param, &lit.name))
                {
                    log::error!("Failed to drop the EDID entry of {}: {}", lit.name, e);
                }
            }
            Ok(_) => {}
            Err(e) if lit.source != Source::Override => {
                log::error!("Failed to read edid_firmware: {}", e)
            }
            Err(_) => {}
        }
        match lit.source {
            Source::Override => {
                if let Err(e) = self.kernel.override_edid(&lit.card, &lit.name, None) {
                    log::error!("Failed to take the EDID of {} back: {}", lit.name, e);
                }
            }
            // The last one through the loader: it looks where it looked before, and our files go.
            Source::Firmware if !self.through_loader() => {
                let ours = self.kernel.our_firmware_dir();
                if matches!(self.kernel.firmware_path(), Ok(path) if path == ours) {
                    if let Err(e) = self.kernel.set_firmware_path("") {
                        log::error!("Failed to put firmware_class.path back: {}", e);
                    }
                }
                for sink in [Sink::Plain, Sink::Hdmi, Sink::Hdmi20] {
                    if let Err(e) = self.kernel.put_firmware(sink.file_name(), None) {
                        log::error!("Failed to remove our EDID file: {}", e);
                    }
                }
            }
            _ => {}
        }
    }

    // Whether a connector still lit takes its EDID through the firmware loader.
    fn through_loader(&self) -> bool {
        self.leases
            .iter()
            .filter_map(|l| l.lit.as_ref())
            .any(|l| l.source == Source::Firmware)
    }

    fn save(&mut self) -> ResultType<()> {
        self.count();
        let lit: Vec<Lit> = self.leases.iter().filter_map(|l| l.lit.clone()).collect();
        let saved = self.kernel.save_lit(&lit);
        if let Err(e) = &saved {
            log::error!(
                "Failed to remember the lit virtual display connectors: {}",
                e
            );
        }
        saved
    }

    fn count(&self) {
        self.held.set(self.leases.len() + self.holds.len(), Instant::now());
    }
}

// The kernel's built-in EDIDs went in 6.9.
fn has_builtin_edids(release: &str) -> bool {
    !kernel_at_least(release, (6, 9)) && kernel_at_least(release, (1, 0))
}

/// Whether kernel `release` ("6.6.127-amd64-desktop") is `at_least` (major, minor).
pub fn kernel_at_least(release: &str, at_least: (u32, u32)) -> bool {
    let mut numbers = release
        .split(|c: char| !c.is_ascii_digit())
        .map(|n| n.parse::<u32>().unwrap_or(0));
    let major = numbers.next().unwrap_or(0);
    let minor = numbers.next().unwrap_or(0);
    (major, minor) >= at_least
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIRMWARE_DIR: &str = "/run/rustdesk/firmware";
    // The desktop's user, whom the sessions' servers run as unless a test says otherwise.
    const ME: u32 = 1000;

    struct FakeKernel {
        connectors: Vec<Connector>,
        release: String,
        cmdline: String,
        param: String,
        firmware_path: String,
        firmware: HashMap<String, Vec<u8>>,
        status: HashMap<String, String>,
        overridden: HashMap<String, bool>,
        // Whether the kernel has its EDIDs built in, whatever its release says.
        builtin: bool,
        crtcs: Option<usize>,
        encoders: HashMap<String, Vec<u32>>,
        intel: Option<IntelDisplay>,
        saved: Vec<Lit>,
        fail_status: bool,
        fail_save: bool,
        word: Option<String>,
        // Every status written, parameter set and event sent, in order.
        steps: Vec<String>,
        // The desktops taken over, as saved.
        taken: Vec<Taken>,
        // The X server refuses an exact layout, taking only --auto.
        refuse_xrandr: bool,
        // Ctrl+P pressed on a keyboard kept.
        escape: bool,
        // The processes there are, by pid, with when each started.
        started: HashMap<u32, u64>,
    }

    fn connector(card: &str, name: &str, connected: bool, id: u32, driver: &str) -> Connector {
        Connector {
            card: card.to_owned(),
            name: name.to_owned(),
            id: Some(id),
            connected,
            enabled: connected,
            driver: driver.to_owned(),
        }
    }

    impl FakeKernel {
        // The office machine, without vkms: HDMI-A-1 in use, DP-1 free, and a free HDMI port.
        fn office() -> Self {
            Self {
                connectors: vec![
                    connector("card0", "DP-1", false, 95, "i915"),
                    connector("card0", "HDMI-A-1", true, 103, "i915"),
                    connector("card0", "HDMI-A-2", false, 110, "i915"),
                ],
                release: "6.6.127-amd64-desktop-hwe".to_owned(),
                cmdline: "BOOT_IMAGE=/vmlinuz quiet video=HDMI-A-1:e".to_owned(),
                param: "HDMI-A-1:edid/1920x1080.bin".to_owned(),
                firmware_path: String::new(),
                firmware: HashMap::new(),
                status: HashMap::new(),
                overridden: HashMap::new(),
                builtin: true,
                crtcs: Some(4),
                encoders: HashMap::from([
                    ("DP-1".to_owned(), vec![94]),
                    ("HDMI-A-1".to_owned(), vec![102]),
                    ("HDMI-A-2".to_owned(), vec![109]),
                ]),
                intel: Some(IntelDisplay {
                    version: Some(9),
                    platform: "COFFEELAKE".to_owned(),
                }),
                saved: Vec::new(),
                fail_status: false,
                fail_save: false,
                word: Some("blacklisted".to_owned()),
                steps: Vec::new(),
                taken: Vec::new(),
                refuse_xrandr: false,
                escape: false,
                started: HashMap::from([(4242, 1), (4343, 1)]),
            }
        }

        // With vkms loaded at boot, its connector off.
        fn with_vkms() -> Self {
            let mut kernel = Self::office();
            kernel
                .connectors
                .push(connector("card1", "Virtual-1", false, 36, "vkms"));
            kernel.word = Some(super::super::vkms::LOADED.to_owned());
            kernel
        }

        // A newer kernel: nothing built in.
        fn recent() -> Self {
            Self {
                release: "6.11.0-29-generic".to_owned(),
                builtin: false,
                ..Self::office()
            }
        }

        // The EDID the kernel would read for connector `name`, by name.
        fn edid(&self, name: &str) -> Option<&str> {
            if self.overridden.get(name).copied().unwrap_or(false) {
                return Some("override");
            }
            let entry = self.param.split(',').find_map(|e| {
                let (connector, file) = e.split_once(':')?;
                (connector == name).then_some(file)
            })?;
            let found = if entry == BUILTIN_EDID {
                self.builtin
            } else {
                self.firmware_path == FIRMWARE_DIR && self.firmware.contains_key(entry)
            };
            found.then_some(entry)
        }

        fn vkms(&self) -> bool {
            self.status.get("Virtual-1").map_or(false, |s| s == "on")
        }
    }

    impl Kernel for FakeKernel {
        fn connectors(&self) -> Vec<Connector> {
            self.connectors.clone()
        }
        fn release(&self) -> String {
            self.release.clone()
        }
        fn cmdline(&self) -> String {
            self.cmdline.clone()
        }
        fn edid_param(&self) -> ResultType<String> {
            Ok(self.param.clone())
        }
        fn set_edid_param(&mut self, value: &str) -> ResultType<()> {
            self.steps.push(format!("param {}", value));
            self.param = value.to_owned();
            Ok(())
        }
        fn firmware_path(&self) -> ResultType<String> {
            Ok(self.firmware_path.clone())
        }
        fn set_firmware_path(&mut self, value: &str) -> ResultType<()> {
            self.firmware_path = value.to_owned();
            Ok(())
        }
        fn our_firmware_dir(&self) -> String {
            FIRMWARE_DIR.to_owned()
        }
        fn put_firmware(&mut self, name: &str, bytes: Option<&[u8]>) -> ResultType<()> {
            match bytes {
                Some(bytes) => self.firmware.insert(name.to_owned(), bytes.to_vec()),
                None => self.firmware.remove(name),
            };
            Ok(())
        }
        fn set_status(&mut self, _card: &str, name: &str, status: &str) -> ResultType<()> {
            if self.fail_status {
                bail!("denied");
            }
            self.steps.push(format!("{} {}", name, status));
            self.status.insert(name.to_owned(), status.to_owned());
            Ok(())
        }
        fn hotplug(&mut self, card: &str) -> ResultType<()> {
            self.steps.push(format!("{} change", card));
            Ok(())
        }
        fn modes(&self, _card: &str, name: &str) -> Vec<String> {
            match self.status.get(name).map(String::as_str) {
                Some("on") if name.starts_with("Virtual") => vec!["1024x768".to_owned()],
                // Without an EDID, the kernel makes some up.
                Some("on") if self.edid(name).is_none() => {
                    vec!["1024x768".to_owned(), "800x600".to_owned()]
                }
                Some("on") => vec!["1280x720".to_owned()],
                _ => Vec::new(),
            }
        }
        fn has_edid(&self, _card: &str, name: &str) -> bool {
            self.status.get(name).map_or(false, |s| s == "on") && self.edid(name).is_some()
        }
        fn override_edid(
            &mut self,
            _card: &str,
            name: &str,
            edid: Option<&[u8]>,
        ) -> ResultType<()> {
            self.overridden.insert(name.to_owned(), edid.is_some());
            Ok(())
        }
        fn crtcs(&self, _card: &str) -> Option<usize> {
            self.crtcs
        }
        fn encoders(&self, _card: &str, id: u32) -> Option<Vec<u32>> {
            let name = &self.connectors.iter().find(|c| c.id == Some(id))?.name;
            self.encoders.get(name).cloned()
        }
        fn intel_display(&self, _card: &str) -> Option<IntelDisplay> {
            self.intel.clone()
        }
        fn save_lit(&mut self, lit: &[Lit]) -> ResultType<()> {
            if self.fail_save {
                bail!("read-only");
            }
            self.saved = lit.to_vec();
            Ok(())
        }
        fn load_lit(&self) -> Vec<Lit> {
            self.saved.clone()
        }
        fn vkms_word(&self) -> Option<String> {
            self.word.clone()
        }
        fn x_of(&mut self, pid: u32) -> ResultType<XServer> {
            if pid == 0 {
                bail!("no such process");
            }
            Ok(XServer {
                display: ":0".to_owned(),
                xauthority: Some(format!("/run/rustdesk/xauth-{}", pid)),
            })
        }
        fn drop_x(&mut self, x: &XServer) {
            self.steps.push(format!("drop {}", x.xauthority.as_deref().unwrap_or("-")));
        }
        fn xrandr(&mut self, x: &XServer, args: &[String]) -> ResultType<()> {
            self.steps
                .push(format!("xrandr {} {}", x.display, args.join(" ")).trim_end().to_owned());
            if self.refuse_xrandr && !args.is_empty() && !args.contains(&"--auto".to_owned()) {
                bail!("Configure crtc 0 failed");
            }
            Ok(())
        }
        fn save_taken(&mut self, taken: &[Taken]) -> ResultType<()> {
            if self.fail_save {
                bail!("read-only");
            }
            self.taken = taken.to_vec();
            Ok(())
        }
        fn load_taken(&self) -> Vec<Taken> {
            self.taken.clone()
        }
        fn lock_input(&mut self, on: bool) {
            self.steps
                .push(if on { "input kept" } else { "input free" }.to_owned());
        }
        fn input_escaped(&mut self) -> bool {
            std::mem::take(&mut self.escape)
        }
        fn process_start(&self, pid: u32) -> Option<u64> {
            self.started.get(&pid).copied()
        }
        fn end_process(&mut self, pid: u32) {
            self.steps.push(format!("end {}", pid));
            self.started.remove(&pid);
        }
    }

    fn leases(kernel: FakeKernel) -> Leases<FakeKernel> {
        Leases::counted_in(kernel, Box::leak(Box::new(Held::new())))
    }

    fn held(leases: &Leases<FakeKernel>) -> usize {
        leases.held.count.load(Ordering::SeqCst)
    }

    fn reserve(leases: &mut Leases<FakeKernel>, client: u64) -> Reserved {
        leases.reserve(client, ME, false).unwrap()
    }

    fn office_restore() -> Restore {
        use super::super::protocol::Back;
        Restore {
            back: vec![Back {
                name: "HDMI-1".to_owned(),
                mode: "1920x1080".to_owned(),
                x: 0,
                y: 0,
                rotation: None,
                rate: None,
                primary: true,
            }],
            off: vec!["Virtual-1-1".to_owned()],
        }
    }

    // Client 1 with vkms lit, having taken the desktop over.
    fn taken_over() -> Leases<FakeKernel> {
        let mut leases = leases(FakeKernel::with_vkms());
        reserve(&mut leases, 1);
        leases.activate(1, "Virtual-1", Instant::now()).unwrap();
        leases.take_over(1, 4242, office_restore()).unwrap();
        leases.kernel.steps.clear();
        leases
    }

    const GIVE_BACK: &str = "xrandr :0 --output HDMI-1 --mode 1920x1080 --rotate normal --pos 0x0 \
                             --primary --output Virtual-1-1 --off";

    #[test]
    fn a_hold_keeps_the_server_as_a_connector_does_until_its_client_goes() {
        let mut leases = leases(FakeKernel::with_vkms());
        leases.hold(7, true);
        // Once a client.
        leases.hold(7, true);
        assert_eq!(held(&leases), 1);
        // A connector held and let go meanwhile leaves the hold counted.
        reserve(&mut leases, 1);
        assert_eq!(held(&leases), 2);
        leases.release_client(1);
        assert_eq!(held(&leases), 1);
        leases.hold(7, false);
        assert_eq!(held(&leases), 0);
        leases.hold(7, true);
        leases.release_client(7);
        assert_eq!(held(&leases), 0);
        // Not restarted for a while after, as after the last connector.
        assert!(leases.held.held(Instant::now()));
    }

    #[test]
    fn a_session_that_goes_with_the_desktop_taken_over_has_it_given_back_first() {
        let mut leases = taken_over();
        assert_eq!(leases.kernel.taken.len(), 1);
        assert_eq!(leases.kernel.taken[0].x.xauthority.as_deref(), Some("/run/rustdesk/xauth-4242"));
        leases.release_client(1);
        // The monitors on again before ours goes dark, then the X server made to look.
        assert_eq!(
            leases.kernel.steps,
            vec![
                GIVE_BACK,
                "input free",
                "Virtual-1 off",
                "card1 change",
                "xrandr :0",
                "drop /run/rustdesk/xauth-4242",
            ]
        );
        assert!(leases.kernel.taken.is_empty());
        assert!(leases.kernel.saved.is_empty());
    }

    #[test]
    fn a_session_that_lets_its_display_go_has_given_the_desktop_back() {
        let mut leases = taken_over();
        leases.release(1, "Virtual-1").unwrap();
        assert!(leases.taken.is_empty() && leases.kernel.taken.is_empty());
        leases.kernel.steps.clear();
        leases.release_client(1);
        assert!(leases.kernel.steps.is_empty());
    }

    #[test]
    fn a_desktop_given_back_by_its_session_is_left_alone() {
        let mut leases = taken_over();
        leases.restored(1);
        assert!(leases.kernel.taken.is_empty());
        leases.kernel.steps.clear();
        leases.release_client(1);
        assert_eq!(leases.kernel.steps, vec!["Virtual-1 off", "card1 change"]);
    }

    #[test]
    fn a_desktop_is_taken_over_only_as_a_plain_restore_says_and_with_a_display_lit() {
        let mut leases = leases(FakeKernel::with_vkms());
        reserve(&mut leases, 1);
        // Nothing lit yet.
        assert!(leases.take_over(1, 4242, office_restore()).is_err());
        leases.activate(1, "Virtual-1", Instant::now()).unwrap();
        let mut bad = office_restore();
        bad.back[0].mode = "1920x1080 --fb 1x1".to_owned();
        assert!(leases.take_over(1, 4242, bad).is_err());
        // Nor without the process's X server.
        assert!(leases.take_over(1, 0, office_restore()).is_err());
        assert!(leases.kernel.taken.is_empty());
        // Said again, the new way stands, on the X server found the first time.
        leases.take_over(1, 4242, office_restore()).unwrap();
        let mut two = office_restore();
        two.off.push("Virtual-1-2".to_owned());
        leases.take_over(1, 9999, two.clone()).unwrap();
        assert_eq!(leases.kernel.taken.len(), 1);
        assert_eq!(leases.kernel.taken[0].restore, two);
        assert_eq!(leases.kernel.taken[0].x.xauthority.as_deref(), Some("/run/rustdesk/xauth-4242"));
    }

    #[test]
    fn a_desktop_is_not_taken_over_unless_root_remembers_how_to_give_it_back() {
        let mut leases = leases(FakeKernel::with_vkms());
        reserve(&mut leases, 1);
        leases.activate(1, "Virtual-1", Instant::now()).unwrap();
        leases.kernel.fail_save = true;
        assert!(leases.take_over(1, 4242, office_restore()).is_err());
        assert!(leases.taken.is_empty());
        // Its copy of the X authority gone with it.
        assert_eq!(leases.kernel.steps.last().unwrap(), "drop /run/rustdesk/xauth-4242");
    }

    #[test]
    fn a_desktop_that_will_not_be_laid_out_as_it_was_gets_its_monitors_on_all_the_same() {
        let mut leases = taken_over();
        leases.kernel.refuse_xrandr = true;
        leases.release_client(1);
        assert_eq!(
            &leases.kernel.steps[..2],
            &[
                GIVE_BACK,
                "xrandr :0 --output HDMI-1 --auto --primary --output Virtual-1-1 --off"
            ]
        );
    }

    #[test]
    fn a_service_that_died_with_a_desktop_taken_over_gives_it_back_when_it_comes_again() {
        let leases = taken_over();
        let (saved, taken) = (leases.kernel.saved.clone(), leases.kernel.taken.clone());
        let mut kernel = FakeKernel::with_vkms();
        kernel.saved = saved;
        kernel.taken = taken;
        let mut again = super::tests::leases(kernel);
        again.recover();
        assert_eq!(
            again.kernel.steps,
            vec![
                GIVE_BACK,
                "Virtual-1 off",
                "card1 change",
                "xrandr :0",
                "drop /run/rustdesk/xauth-4242",
            ]
        );
        assert!(again.kernel.taken.is_empty());
        assert!(again.kernel.saved.is_empty());
    }

    #[test]
    fn a_desktop_taken_over_is_given_back_before_its_display_goes_dark_for_another_user() {
        let mut leases = taken_over();
        leases.follow(Some(ME));
        assert!(leases.kernel.steps.is_empty());
        leases.follow(Some(ME + 1));
        assert_eq!(
            leases.kernel.steps,
            vec![GIVE_BACK, "input free", "Virtual-1 off", "card1 change"]
        );
    }

    #[test]
    fn the_input_here_is_kept_from_the_desktop_while_it_is_taken_over() {
        let mut leases = leases(FakeKernel::with_vkms());
        reserve(&mut leases, 1);
        leases.activate(1, "Virtual-1", Instant::now()).unwrap();
        leases.kernel.steps.clear();
        // Remembered how to give it back first, then the input kept, before the session turns the
        // monitors off.
        leases.take_over(1, 4242, office_restore()).unwrap();
        assert_eq!(leases.kernel.steps, vec!["input kept"]);
        // Said again, it changes nothing; the look at the seat says it is still wanted.
        leases.take_over(1, 4242, office_restore()).unwrap();
        leases.heartbeat();
        assert_eq!(leases.kernel.steps, vec!["input kept", "input kept"]);
        leases.kernel.steps.clear();
        leases.restored(1);
        leases.heartbeat();
        assert_eq!(leases.kernel.steps, vec!["drop /run/rustdesk/xauth-4242", "input free"]);
    }

    #[test]
    fn another_user_on_the_screen_has_the_input_even_with_the_display_not_turned_off() {
        let mut leases = taken_over();
        leases.kernel.fail_status = true;
        leases.follow(Some(ME + 1));
        assert_eq!(leases.kernel.steps, vec![GIVE_BACK, "input free"]);
        leases.kernel.steps.clear();
        // Tried again, the input stays free.
        leases.follow(Some(ME + 1));
        assert_eq!(leases.kernel.steps, vec![GIVE_BACK]);
        // Back before its session let the display go: kept again, its session takes the desktop
        // over again as it finds it taken back.
        leases.kernel.fail_status = false;
        leases.kernel.steps.clear();
        leases.follow(Some(ME));
        assert_eq!(leases.kernel.steps, vec!["input kept"]);
    }

    #[test]
    fn ctrl_p_here_takes_the_desktop_back_until_its_session_gave_it_back() {
        let mut leases = taken_over();
        // Not kept, a Ctrl+P is nothing; the word of it is gone all the same.
        leases.restored(1);
        leases.kernel.escape = true;
        leases.heartbeat();
        assert!(!leases.taken_back(1) && !leases.kernel.escape);
        leases.take_over(1, 4242, office_restore()).unwrap();
        leases.kernel.steps.clear();
        leases.kernel.escape = true;
        leases.heartbeat();
        // The input free at once, the monitor on again, ours left as they are; the desktop the
        // session's to give back.
        let lit = Restore {
            off: Vec::new(),
            ..office_restore()
        };
        let display = leases.taken[0].1.x.display.clone();
        let steps = vec![
            "input free".to_owned(),
            format!("xrandr {} {}", display, lit.args().join(" ")),
        ];
        assert_eq!(leases.kernel.steps, steps);
        assert!(!steps[1].contains("Virtual-1-1"));
        assert!(leases.taken_back(1));
        assert_eq!(leases.kernel.taken.len(), 1);
        // Its session taking it over again before it heard: refused.
        assert!(leases.take_over(1, 4242, office_restore()).is_err());
        leases.heartbeat();
        assert_eq!(leases.kernel.steps, steps);
        // Given back, the session may take it over again when asked.
        leases.restored(1);
        assert!(!leases.taken_back(1));
        leases.take_over(1, 4242, office_restore()).unwrap();
        assert_eq!(leases.kernel.steps.last().unwrap(), "input kept");
    }

    // GNOME's: no connector lit, no X server; the session lays its desktop out itself.
    #[test]
    fn a_session_taking_its_desktop_over_by_itself_keeps_the_input_until_it_gives_it_back() {
        let mut leases = leases(FakeKernel::office());
        let t = Instant::now();
        leases.lock_input(1, ME, 4242, t).unwrap();
        leases.heartbeat_at(t);
        assert_eq!(leases.kernel.steps, vec!["input kept", "input kept"]);
        assert!(!leases.taken_back_by(1, 4242));
        leases.kernel.steps.clear();
        leases.restored(1);
        leases.heartbeat_at(t);
        assert_eq!(leases.kernel.steps, vec!["input free"]);
        // Its connection gone, the same.
        leases.lock_input(1, ME, 4242, t).unwrap();
        leases.release_client(1);
        assert_eq!(leases.kernel.steps, vec!["input free", "input kept", "input free"]);
        // A process there is no more cannot.
        assert!(leases.lock_input(1, ME, 9999, t).is_err());
    }

    #[test]
    fn ctrl_p_takes_back_a_desktop_taken_over_by_its_session_for_as_long_as_its_process_lasts() {
        let mut leases = leases(FakeKernel::office());
        let t = Instant::now();
        leases.lock_input(1, ME, 4242, t).unwrap();
        leases.kernel.steps.clear();
        leases.kernel.escape = true;
        leases.heartbeat_at(t);
        // The input free at once; nothing laid out by root.
        assert_eq!(leases.kernel.steps, vec!["input free"]);
        assert!(leases.taken_back_by(1, 4242));
        assert!(leases.lock_input(1, ME, 4242, t).is_err());
        // Connected again, the same process is still to give it back.
        leases.release_client(1);
        assert!(leases.taken_back_by(2, 4242));
        assert!(leases.lock_input(2, ME, 4242, t).is_err());
        // Another process is not.
        assert!(!leases.taken_back_by(3, 4343));
        // Given back, it may take it over again.
        leases.restored(2);
        leases.escape_over(4242);
        assert!(!leases.taken_back_by(2, 4242));
        leases.lock_input(2, ME, 4242, t).unwrap();
        assert_eq!(leases.kernel.steps.last().unwrap(), "input kept");
    }

    #[test]
    fn a_session_that_does_not_give_its_desktop_back_is_ended_and_its_user_held_off() {
        let mut leases = leases(FakeKernel::office());
        let t = Instant::now();
        leases.lock_input(1, ME, 4242, t).unwrap();
        leases.kernel.escape = true;
        leases.heartbeat_at(t);
        leases.kernel.steps.clear();
        leases.heartbeat_at(t + GIVE_BACK_LIMIT - Duration::from_millis(1));
        assert!(leases.kernel.steps.is_empty());
        leases.heartbeat_at(t + GIVE_BACK_LIMIT);
        assert_eq!(leases.kernel.steps, vec!["end 4242"]);
        // Once.
        leases.kernel.started.insert(4242, 1);
        leases.heartbeat_at(t + GIVE_BACK_LIMIT * 2);
        assert_eq!(leases.kernel.steps, vec!["end 4242"]);
        leases.kernel.started.remove(&4242);
        leases.heartbeat_at(t + GIVE_BACK_LIMIT * 2);
        assert!(!leases.taken_back_by(1, 4242));
        // Its user's next `--server` may not take a desktop over at once.
        let then = t + GIVE_BACK_LIMIT + ENDED_BARS - Duration::from_millis(1);
        assert!(leases.lock_input(2, ME, 4343, then).is_err());
        leases.lock_input(2, ME, 4343, then + Duration::from_millis(1)).unwrap();
    }

    #[test]
    fn a_process_given_the_pid_of_one_taken_back_is_not_ended() {
        let mut leases = leases(FakeKernel::office());
        let t = Instant::now();
        leases.lock_input(1, ME, 4242, t).unwrap();
        leases.kernel.escape = true;
        leases.heartbeat_at(t);
        leases.kernel.started.insert(4242, 2);
        leases.kernel.steps.clear();
        leases.heartbeat_at(t + GIVE_BACK_LIMIT);
        assert!(leases.kernel.steps.is_empty());
        assert!(!leases.taken_back_by(1, 4242));
        leases.lock_input(1, ME, 4242, t + GIVE_BACK_LIMIT).unwrap();
    }

    #[test]
    fn another_user_on_the_screen_has_the_input_from_a_session_taking_its_desktop_over() {
        let mut leases = leases(FakeKernel::office());
        let t = Instant::now();
        leases.follow(Some(ME));
        leases.lock_input(1, ME, 4242, t).unwrap();
        leases.follow(Some(ME + 1));
        // Ctrl+P there is not this session's.
        leases.kernel.escape = true;
        leases.heartbeat_at(t);
        assert!(!leases.taken_back_by(1, 4242));
        leases.follow(Some(ME));
        assert_eq!(leases.kernel.steps, vec!["input kept", "input free", "input kept"]);
        // Ctrl+P while the session on the screen keeps the input too: only that one is taken
        // back, and ended should it not give its desktop back; the one away keeps its lock.
        leases.lock_input(2, ME + 1, 4343, t).unwrap();
        leases.follow(Some(ME + 1));
        leases.kernel.escape = true;
        leases.heartbeat_at(t);
        assert!(leases.taken_back_by(2, 4343));
        assert!(!leases.taken_back_by(1, 4242));
        leases.kernel.steps.clear();
        leases.heartbeat_at(t + GIVE_BACK_LIMIT);
        assert_eq!(leases.kernel.steps, vec!["end 4343"]);
        leases.follow(Some(ME));
        assert_eq!(leases.kernel.steps, vec!["end 4343", "input kept"]);
    }

    #[test]
    fn a_session_that_goes_after_ctrl_p_is_forgotten() {
        let mut leases = taken_over();
        leases.kernel.escape = true;
        leases.heartbeat();
        leases.release_client(1);
        assert!(!leases.taken_back(1));
        assert!(!leases.kernel.steps.contains(&"input kept".to_owned()));
    }

    #[test]
    fn vkms_comes_first_and_is_only_turned_on_and_off() {
        let mut leases = leases(FakeKernel::with_vkms());
        let now = Instant::now();
        assert_eq!(leases.status(), (2, Some(Kind::Vkms), String::new()));
        let r = reserve(&mut leases, 1);
        assert_eq!((r.connector.name.as_str(), r.kind, r.max_khz), ("Virtual-1", Kind::Vkms, None));
        assert_eq!(held(&leases), 1);
        leases.activate(1, "Virtual-1", now).unwrap();
        assert!(leases.kernel.vkms());
        assert_eq!(leases.kernel.steps, vec!["Virtual-1 on", "card1 change"]);
        assert_eq!(leases.kernel.saved[0].source, Source::Vkms);
        // The kernel's parameters are not touched.
        assert_eq!(leases.kernel.param, "HDMI-A-1:edid/1920x1080.bin");
        leases.kernel.steps.clear();
        leases.release(1, "Virtual-1").unwrap();
        // Off, never back to detecting: it would come up connected.
        assert_eq!(leases.kernel.steps, vec!["Virtual-1 off", "card1 change"]);
        assert_eq!(
            leases.released(),
            vec![Gone {
                connector: "Virtual-1".to_owned(),
                connector_id: Some(36),
                kind: Kind::Vkms,
            }]
        );
        assert!(leases.kernel.saved.is_empty());
        assert_eq!(held(&leases), 0);
        // Its server still puts the other outputs back: not restarted for a while.
        let now = Instant::now();
        assert!(leases.held.held(now));
        assert!(!leases.held.held(now + AFTER_RELEASE));
    }

    #[test]
    fn a_session_without_vkms_is_given_a_connector() {
        let mut leases = leases(FakeKernel::with_vkms());
        let r = leases.reserve(1, ME, true).unwrap();
        assert_eq!((r.connector.name.as_str(), r.kind), ("HDMI-A-2", Kind::Connector));
        // vkms's connector someone turned on is theirs.
        let mut kernel = FakeKernel::with_vkms();
        kernel.connectors[3].connected = true;
        let mut leases = super::tests::leases(kernel);
        assert_eq!(leases.status().1, Some(Kind::Connector));
        assert_eq!(reserve(&mut leases, 1).connector.name, "HDMI-A-2");
    }

    #[test]
    fn without_vkms_the_reason_is_told() {
        let mut kernel = FakeKernel::office();
        assert_eq!(
            super::tests::leases(FakeKernel::office()).status(),
            (2, Some(Kind::Connector), "blacklisted".to_owned())
        );
        // No word at all: the package was put in since the last boot.
        kernel.word = None;
        assert!(leases(kernel).status().2.contains("restart"));
    }

    #[test]
    fn vkms_loaded_with_the_desktop_running_is_not_used() {
        let mut kernel = FakeKernel::with_vkms();
        kernel.word = None;
        let mut leases = leases(kernel);
        let (_, kind, note) = leases.status();
        assert_eq!(kind, Some(Kind::Connector));
        assert!(note.contains("restart"));
        assert_eq!(reserve(&mut leases, 1).connector.name, "HDMI-A-2");
    }

    #[test]
    fn a_connector_is_reserved_then_lit_and_put_back() {
        let mut leases = leases(FakeKernel::office());
        let now = Instant::now();
        let r = reserve(&mut leases, 1);
        // An HDMI port of an Intel gen 9 card: a real mode, up to 300 MHz.
        assert_eq!((r.connector.name.as_str(), r.kind, r.max_khz), ("HDMI-A-2", Kind::Connector, Some(300_000)));
        assert_eq!(held(&leases), 1);
        // Reserved, not lit: the kernel is untouched.
        assert_eq!(leases.kernel.param, "HDMI-A-1:edid/1920x1080.bin");
        assert_eq!(leases.activate(1, "HDMI-A-2", now).unwrap(), vec!["1280x720"]);
        // Our EDID, with the HDMI block, through the firmware loader.
        assert_eq!(leases.kernel.saved[0].source, Source::Firmware);
        assert_eq!(leases.kernel.firmware_path, FIRMWARE_DIR);
        assert_eq!(
            leases.kernel.firmware[Sink::Hdmi.file_name()],
            super::super::edid::generate(Sink::Hdmi)
        );
        assert_eq!(
            leases.kernel.param,
            "HDMI-A-2:rustdesk-vd-hdmi.bin,HDMI-A-1:edid/1920x1080.bin"
        );
        assert_eq!(leases.kernel.status["HDMI-A-2"], "on");
        leases.kernel.steps.clear();
        leases.release(1, "HDMI-A-2").unwrap();
        // Our EDID is gone before the connector detects again.
        assert_eq!(
            leases.kernel.steps,
            vec![
                "HDMI-A-2 off",
                "param HDMI-A-1:edid/1920x1080.bin",
                "HDMI-A-2 detect"
            ]
        );
        // The loader looks where it looked before, and nothing of ours is left.
        assert_eq!(leases.kernel.firmware_path, "");
        assert!(leases.kernel.firmware.is_empty());
        assert!(leases.kernel.saved.is_empty());
        assert_eq!(leases.status().0, 2);
        assert_eq!(held(&leases), 0);
    }

    #[test]
    fn the_sink_and_the_clock_go_by_driver() {
        let kernel_with = |driver: &str, name: &str, version: Option<u32>, release: &str| {
            let mut kernel = FakeKernel::office();
            kernel.connectors = vec![
                connector("card0", "HDMI-A-1", true, 103, driver),
                connector("card0", name, false, 95, driver),
            ];
            kernel.encoders.clear();
            kernel.intel = Some(IntelDisplay {
                version,
                platform: "ALDERLAKE_S".to_owned(),
            });
            kernel.release = release.to_owned();
            kernel
        };
        let lit = |kernel: FakeKernel| {
            let mut leases = leases(kernel);
            let r = reserve(&mut leases, 1);
            leases.activate(1, &r.connector.name, Instant::now()).unwrap();
            let file = leases.kernel.firmware.keys().next().cloned().unwrap_or_default();
            (r.kind, r.max_khz, file)
        };
        assert_eq!(
            lit(kernel_with("i915", "HDMI-A-2", Some(12), "6.6.0")),
            (Kind::Connector, Some(594_000), "rustdesk-vd-hdmi20.bin".to_owned())
        );
        // 600 MHz from display version 13, on 6.11 and on.
        assert_eq!(lit(kernel_with("i915", "HDMI-A-2", Some(13), "6.6.0")).1, Some(594_000));
        assert_eq!(lit(kernel_with("i915", "HDMI-A-2", Some(13), "6.11.0")).1, Some(600_000));
        assert_eq!(
            lit(kernel_with("i915", "HDMI-A-2", Some(7), "6.6.0")),
            (Kind::Connector, Some(225_000), "rustdesk-vd-hdmi.bin".to_owned())
        );
        assert_eq!(
            lit(kernel_with("i915", "HDMI-A-2", None, "6.6.0")).1,
            Some(300_000)
        );
        assert_eq!(
            lit(kernel_with("i915", "DP-2", Some(9), "6.6.0")),
            (Kind::Scaled, None, "rustdesk-vd-plain.bin".to_owned())
        );
        assert_eq!(lit(kernel_with("nouveau", "HDMI-A-2", None, "6.10.3")).1, Some(165_000));
        assert_eq!(lit(kernel_with("nouveau", "HDMI-A-2", None, "6.11.0")).1, Some(297_000));
    }

    #[test]
    fn ports_that_take_only_fixed_clocks_are_left_alone_before_6_15() {
        let kernel_with = |driver: &str, version: u32, platform: &str, release: &str| {
            let mut kernel = FakeKernel::office();
            kernel.connectors.iter_mut().for_each(|c| c.driver = driver.to_owned());
            kernel.intel = Some(IntelDisplay {
                version: Some(version),
                platform: platform.to_owned(),
            });
            kernel.release = release.to_owned();
            kernel
        };
        let free = |kernel: FakeKernel| leases(kernel).status().0;
        assert_eq!(free(kernel_with("i915", 14, "METEORLAKE", "6.12.0")), 0);
        assert_eq!(free(kernel_with("i915", 13, "DG2", "6.6.0")), 0);
        assert_eq!(free(kernel_with("xe", 20, "LUNARLAKE", "6.14.9")), 0);
        assert_eq!(free(kernel_with("i915", 13, "ALDERLAKE_P", "6.6.0")), 2);
        assert_eq!(free(kernel_with("i915", 14, "METEORLAKE", "6.15.0")), 2);
    }

    #[test]
    fn a_session_not_on_the_screen_gets_nothing_lit() {
        let mut leases = leases(FakeKernel::with_vkms());
        leases.follow(Some(113));
        reserve(&mut leases, 1);
        assert!(leases.activate(1, "Virtual-1", Instant::now()).is_err());
        assert!(!leases.kernel.vkms());
        leases.follow(Some(ME));
        assert!(leases.activate(1, "Virtual-1", Instant::now()).is_ok());
    }

    #[test]
    fn a_reservation_given_up_on_goes_with_the_next() {
        let mut leases = leases(FakeKernel::with_vkms());
        reserve(&mut leases, 1);
        // Its answer came too late; the client asks again.
        assert_eq!(reserve(&mut leases, 1).connector.name, "Virtual-1");
        assert_eq!(held(&leases), 1);
        // One lit stays.
        leases.activate(1, "Virtual-1", Instant::now()).unwrap();
        assert_eq!(reserve(&mut leases, 1).connector.name, "HDMI-A-2");
        assert_eq!(held(&leases), 2);
        // Another client's is not touched.
        let mut leases = super::tests::leases(FakeKernel::with_vkms());
        reserve(&mut leases, 1);
        reserve(&mut leases, 2);
        assert_eq!(held(&leases), 2);
    }

    #[test]
    fn only_the_holder_may_light_or_let_go() {
        let mut leases = leases(FakeKernel::office());
        let now = Instant::now();
        reserve(&mut leases, 1);
        assert!(leases.activate(2, "HDMI-A-2", now).is_err());
        assert!(
            leases.activate(1, "HDMI-A-1", now).is_err(),
            "never one it was not given"
        );
        leases.activate(1, "HDMI-A-2", now).unwrap();
        // Another client's release is a no-op, not a release.
        leases.release(2, "HDMI-A-2").unwrap();
        assert_eq!(leases.kernel.status["HDMI-A-2"], "on");
    }

    #[test]
    fn a_client_going_away_lets_its_connectors_go() {
        let mut leases = leases(FakeKernel::office());
        let now = Instant::now();
        let a = reserve(&mut leases, 1);
        leases.activate(1, &a.connector.name, now).unwrap();
        let b = reserve(&mut leases, 2);
        assert_eq!((b.connector.name.as_str(), b.kind), ("DP-1", Kind::Scaled));
        assert_eq!(leases.status().0, 0);
        assert!(leases.reserve(3, ME, false).is_err());
        leases.release_client(1);
        assert_eq!(leases.kernel.status["HDMI-A-2"], "detect");
        assert_eq!(leases.status().0, 1);
        assert_eq!(held(&leases), 1);
        // Client 2's reservation stands.
        assert!(leases.activate(2, "DP-1", now).is_ok());
    }

    #[test]
    fn the_other_end_of_a_port_in_use_is_left_alone() {
        // DisplayPort++: DP-1 and HDMI-A-2 are one port, which a monitor on DP-1 uses.
        let mut kernel = FakeKernel::office();
        kernel.connectors[0].connected = true;
        kernel.connectors[0].enabled = true;
        kernel.encoders.insert("HDMI-A-2".to_owned(), vec![94]);
        let leases = leases(kernel);
        assert_eq!(leases.status().0, 0);
        // Free, both ends of the port count once, its HDMI end first.
        let mut kernel = FakeKernel::office();
        kernel.encoders.insert("HDMI-A-2".to_owned(), vec![94]);
        let mut leases = super::tests::leases(kernel);
        assert_eq!(leases.status(), (1, Some(Kind::Connector), "blacklisted".to_owned()));
        assert_eq!(reserve(&mut leases, 1).connector.name, "HDMI-A-2");
        assert!(leases.reserve(2, ME, false).is_err());
    }

    #[test]
    fn a_free_crtc_is_always_left_for_a_monitor() {
        // Three CRTCs, one for HDMI-A-1: one for a virtual display, one spare.
        let mut kernel = FakeKernel::with_vkms();
        kernel.crtcs = Some(3);
        let mut leases = leases(kernel);
        assert_eq!(leases.status().0, 2);
        reserve(&mut leases, 1);
        assert_eq!(reserve(&mut leases, 2).connector.name, "HDMI-A-2");
        leases.release_client(1);
        // vkms's CRTC is its own; the card's last one stays free.
        assert!(leases.reserve(3, ME, true).is_err());
        assert_eq!(reserve(&mut leases, 3).connector.name, "Virtual-1");
        // Unknown: not held back.
        let mut kernel = FakeKernel::office();
        kernel.crtcs = None;
        assert_eq!(super::tests::leases(kernel).status().0, 2);
    }

    #[test]
    fn a_connector_of_another_session_goes_dark_while_it_is_away() {
        let mut leases = leases(FakeKernel::with_vkms());
        let now = Instant::now();
        reserve(&mut leases, 1);
        leases.activate(1, "Virtual-1", now).unwrap();
        leases.kernel.steps.clear();
        leases.follow(Some(ME));
        leases.follow(None);
        assert!(leases.kernel.steps.is_empty());
        // Switched to the login screen's user, then back.
        leases.follow(Some(113));
        assert!(!leases.kernel.vkms());
        leases.follow(Some(113));
        assert_eq!(leases.kernel.steps, vec!["Virtual-1 off", "card1 change"]);
        leases.follow(Some(ME));
        assert!(leases.kernel.vkms());
        // Released while dark: off it stays.
        leases.follow(Some(113));
        leases.release_client(1);
        assert_eq!(leases.kernel.status["Virtual-1"], "off");
        // The login screen's own server runs as root, and is never put out.
        reserve_as(&mut leases, 2, 0);
        leases.activate(2, "Virtual-1", now).unwrap();
        leases.follow(Some(113));
        assert!(leases.kernel.vkms());
    }

    fn reserve_as(leases: &mut Leases<FakeKernel>, client: u64, uid: u32) -> Reserved {
        leases.reserve(client, uid, false).unwrap()
    }

    #[test]
    fn someone_elses_firmware_path_is_left_alone() {
        let mut kernel = FakeKernel::recent();
        kernel.firmware_path = "/opt/vendor/firmware".to_owned();
        let mut leases = leases(kernel);
        reserve(&mut leases, 1);
        leases.activate(1, "HDMI-A-2", Instant::now()).unwrap();
        assert_eq!(leases.kernel.saved[0].source, Source::Override);
        assert_eq!(leases.kernel.overridden["HDMI-A-2"], true);
        assert_eq!(leases.kernel.firmware_path, "/opt/vendor/firmware");
        // Not named in the parameter while overridden: nothing to fail loading.
        assert_eq!(leases.kernel.param, "HDMI-A-1:edid/1920x1080.bin");
        leases.release(1, "HDMI-A-2").unwrap();
        assert_eq!(leases.kernel.overridden["HDMI-A-2"], false);
        assert_eq!(leases.kernel.firmware_path, "/opt/vendor/firmware");
    }

    #[test]
    fn the_loader_stays_ours_while_any_connector_needs_it() {
        let mut kernel = FakeKernel::recent();
        kernel.crtcs = None;
        let mut leases = leases(kernel);
        let now = Instant::now();
        reserve(&mut leases, 1);
        leases.activate(1, "HDMI-A-2", now).unwrap();
        reserve(&mut leases, 2);
        leases.activate(2, "DP-1", now).unwrap();
        assert_eq!(leases.kernel.firmware.len(), 2);
        leases.release(1, "HDMI-A-2").unwrap();
        assert_eq!(leases.kernel.firmware_path, FIRMWARE_DIR);
        assert!(leases.kernel.has_edid("card0", "DP-1"));
        leases.release(2, "DP-1").unwrap();
        assert_eq!(leases.kernel.firmware_path, "");
        assert!(leases.kernel.firmware.is_empty());
    }

    #[test]
    fn the_kernels_own_edid_is_the_last_resort() {
        // Our loader directory taken: debugfs, then the kernel's built-in EDID.
        let mut kernel = FakeKernel::office();
        kernel.firmware_path = "/opt/vendor/firmware".to_owned();
        let leases = leases(kernel);
        assert_eq!(
            leases.sources(Kind::Connector),
            vec![Source::Override, Source::Builtin]
        );
        let recent = super::tests::leases(FakeKernel::recent());
        assert_eq!(
            recent.sources(Kind::Connector),
            vec![Source::Firmware, Source::Override]
        );
        assert_eq!(recent.sources(Kind::Vkms), vec![Source::Vkms]);
    }

    #[test]
    fn a_connector_that_will_not_light_is_put_back() {
        let mut kernel = FakeKernel::office();
        kernel.fail_status = true;
        let mut leases = leases(kernel);
        reserve(&mut leases, 1);
        assert!(leases.activate(1, "HDMI-A-2", Instant::now()).is_err());
        assert_eq!(leases.kernel.param, "HDMI-A-1:edid/1920x1080.bin");
        assert_eq!(leases.kernel.firmware_path, "");
        assert!(leases.kernel.firmware.is_empty());
        assert!(leases.kernel.saved.is_empty());
    }

    #[test]
    fn a_connector_someone_configured_is_left_alone() {
        let mut kernel = FakeKernel::office();
        kernel.param = "HDMI-A-1:edid/1920x1080.bin,HDMI-A-2:edid/1280x1024.bin".to_owned();
        let mut leases = leases(kernel);
        assert_eq!(reserve(&mut leases, 1).connector.name, "DP-1");
    }

    #[test]
    fn an_entry_would_reach_other_connectors_so_none_is_made() {
        let mut kernel = FakeKernel::office();
        kernel.crtcs = None;
        kernel.connectors.push(connector("card0", "DP-10", false, 120, "i915"));
        kernel.connectors.push(connector("card1", "HDMI-A-2", false, 130, "i915"));
        kernel.connectors.push(connector("card1", "DP-3", true, 131, "i915"));
        let leases = leases(kernel);
        // DP-1 would stand for DP-10 too; card0's HDMI-A-2 for card1's; DP-10 is free alone.
        assert_eq!(
            leases
                .candidates(false)
                .iter()
                .map(|(c, _)| format!("{}-{}", c.card, c.name))
                .collect::<Vec<_>>(),
            vec!["card0-DP-10"]
        );
    }

    #[test]
    fn a_connector_the_command_line_turned_off_is_left_alone() {
        let mut kernel = FakeKernel::office();
        kernel.cmdline = "quiet video=HDMI-A-1:e video=HDMI-A-2:d".to_owned();
        let mut leases = leases(kernel);
        assert_eq!(reserve(&mut leases, 1).connector.name, "DP-1");
    }

    #[test]
    fn lighting_over_and_over_is_refused_for_a_while() {
        let mut leases = leases(FakeKernel::with_vkms());
        let t0 = Instant::now();
        reserve(&mut leases, 1);
        for _ in 0..BURST {
            leases.activate(1, "Virtual-1", t0).unwrap();
            leases.release(1, "Virtual-1").unwrap();
            reserve(&mut leases, 1);
        }
        assert!(leases.activate(1, "Virtual-1", t0).is_err());
        // Connecting again does not start the count over: it goes by user.
        leases.release_client(1);
        reserve(&mut leases, 2);
        assert!(leases.activate(2, "Virtual-1", t0).is_err());
        // Another user is not held up by it.
        leases.release_client(2);
        reserve_as(&mut leases, 3, ME + 1);
        assert!(leases.activate(3, "Virtual-1", t0).is_ok());
        leases.release_client(3);
        reserve(&mut leases, 1);
        assert!(leases.activate(1, "Virtual-1", t0 + BURST_WINDOW).is_ok());
    }

    #[test]
    fn nothing_is_lit_that_could_not_be_remembered() {
        let mut kernel = FakeKernel::with_vkms();
        kernel.fail_save = true;
        let mut leases = leases(kernel);
        reserve(&mut leases, 1);
        assert!(leases.activate(1, "Virtual-1", Instant::now()).is_err());
        assert!(!leases.kernel.status.contains_key("Virtual-1"));
    }

    #[test]
    fn what_was_put_back_is_told_most_recent_last() {
        let mut leases = leases(FakeKernel::with_vkms());
        let now = Instant::now();
        reserve(&mut leases, 1);
        leases.activate(1, "Virtual-1", now).unwrap();
        reserve(&mut leases, 2);
        leases.activate(2, "HDMI-A-2", now).unwrap();
        leases.release_client(2);
        leases.release_client(1);
        let names = |leases: &Leases<FakeKernel>| -> Vec<String> {
            leases.released().into_iter().map(|g| g.connector).collect()
        };
        assert_eq!(names(&leases), vec!["HDMI-A-2", "Virtual-1"]);
        assert_eq!(leases.released()[0].kind, Kind::Connector);
        // Again: once, at the end.
        reserve(&mut leases, 2);
        leases.activate(2, "Virtual-1", now).unwrap();
        leases.release_client(2);
        assert_eq!(names(&leases), vec!["HDMI-A-2", "Virtual-1"]);
    }

    #[test]
    fn what_a_crash_left_lit_is_put_back_at_start() {
        let mut leases = leases(FakeKernel::recent());
        reserve(&mut leases, 1);
        leases.activate(1, "HDMI-A-2", Instant::now()).unwrap();
        // The service dies here; its successor starts on the same kernel.
        let kernel = std::mem::replace(&mut leases.kernel, FakeKernel::office());
        let mut successor = Leases::counted_in(kernel, Box::leak(Box::new(Held::new())));
        successor.recover();
        assert_eq!(successor.kernel.param, "HDMI-A-1:edid/1920x1080.bin");
        assert_eq!(successor.kernel.status["HDMI-A-2"], "detect");
        assert_eq!(successor.kernel.firmware_path, "");
        assert!(successor.kernel.firmware.is_empty());
        assert!(successor.kernel.saved.is_empty());
        assert_eq!(successor.status().0, 2);
        // Its predecessor's output may still be on in the X server: told so.
        assert_eq!(successor.released()[0].connector, "HDMI-A-2");
        // A vkms connector left on goes off.
        let mut leases = super::tests::leases(FakeKernel::with_vkms());
        reserve(&mut leases, 1);
        leases.activate(1, "Virtual-1", Instant::now()).unwrap();
        let kernel = std::mem::replace(&mut leases.kernel, FakeKernel::office());
        let mut successor = Leases::counted_in(kernel, Box::leak(Box::new(Held::new())));
        successor.kernel.steps.clear();
        successor.recover();
        assert_eq!(successor.kernel.steps, vec!["Virtual-1 off", "card1 change"]);
    }

    #[test]
    fn kernel_releases_compare_by_major_and_minor() {
        assert!(has_builtin_edids("6.6.127-amd64-desktop-hwe"));
        assert!(has_builtin_edids("5.15.0-105-generic"));
        assert!(!has_builtin_edids("6.9.0"));
        assert!(!has_builtin_edids("6.11.0-29-generic"));
        assert!(!has_builtin_edids(""));
        assert!(kernel_at_least("6.11.0-29-generic", (6, 11)));
        assert!(kernel_at_least("7.0.1", (6, 11)));
        assert!(!kernel_at_least("6.10.14", (6, 11)));
    }
}
