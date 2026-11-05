//! Virtual displays the controlling side asks for, so it sees this desktop at the size of its own
//! monitors instead of a scaled copy of ours. The controlling side names each display by a slot,
//! one of its own monitors, and gives the size it wants (`ToggleVirtualDisplay.spec`).
//!
//! On the controlled side, this module is the platform-independent part: the requests, which
//! session owns which display and when the displays go (`manager.rs`). Each platform plugs
//! displays in and out through a `Backend`. The backends are slow (seconds on Linux), so they run
//! on a thread of their own and never on a connection's loop; the rest of the server reads a
//! published snapshot instead. The controlling side's part, its monitors and what to ask for
//! each, is `controller.rs`.

mod controller;
// Built on macOS too, for its tests: only Linux uses it.
#[cfg(any(target_os = "linux", all(test, target_os = "macos")))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod gnome;
mod manager;
#[cfg(any(windows, test))]
#[cfg_attr(not(windows), allow(dead_code))]
mod win;
// Built on other Unix systems too, for its tests: only Linux uses it.
#[cfg(any(target_os = "linux", all(test, unix)))]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod x11;

#[cfg(target_os = "linux")]
pub use x11::{leases_held, release_leftovers, serve_root, vkms_boot};
#[cfg(windows)]
pub use win::{ctrl_p, taking_over};

pub use controller::{
    local_monitors, mode_option, spec_for, spec_for_view, toggle_msg, HeldDisplays, Monitor,
    PeerCaps, View,
};
pub use manager::{NestedTarget, Owner};

use base::{
    config::keys,
    message_proto::{ToggleVirtualDisplay, VirtualDisplayResult},
};
use hbb_common::{log, ResultType};
use manager::{Backend, Caps, Manager, Mode, Notice, Spec, ALL_SLOTS};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Mutex, OnceLock, RwLock,
    },
    time::{Duration, Instant},
};

/// Hands a result to the connection that asked.
pub type Reply = Box<dyn FnMut(VirtualDisplayResult) + Send>;

/// The longest one backend call may hold the display list back; see `layout_settling`.
const SETTLE_LIMIT: Duration = Duration::from_secs(30);
// How often the manager looks after the displays there are.
const MAINTAIN: Duration = Duration::from_secs(3);
// How often while the backend is watchful (`Backend::watchful`).
const WATCHFUL: Duration = Duration::from_millis(500);
// The longest the server waits as it ends for its displays to go.
#[cfg(windows)]
const SHUTDOWN_WAIT: Duration = Duration::from_secs(3);

enum Cmd {
    Toggle {
        conn_id: i32,
        owner: Owner,
        toggle: ToggleVirtualDisplay,
        refusal: Option<String>,
        privacy_allowed: bool,
        reply: Reply,
    },
    /// The connection may no longer have the desktop to itself.
    Demote {
        conn_id: i32,
    },
    ConnEnded {
        conn_id: i32,
        closed: bool,
    },
    Resize {
        owner: Owner,
        name: String,
        width: u32,
        height: u32,
    },
    /// The server is about to end: every display goes, answered on `done`.
    #[cfg(windows)]
    Shutdown {
        done: mpsc::Sender<()>,
    },
}

struct Worker {
    tx: mpsc::Sender<Cmd>,
    caps: Caps,
}

/// What the rest of the server reads without waiting for the manager.
struct Published {
    /// Our displays, oldest first, and one being plugged in once its name is announced.
    names: Vec<String>,
    /// Since when the backend has been changing displays.
    changing: Option<Instant>,
    /// The sessions whose displays wait out their grace (`Manager::waiting`).
    waiting: Vec<Owner>,
}

static PUBLISHED: RwLock<Published> = RwLock::new(Published {
    names: Vec::new(),
    changing: None,
    waiting: Vec::new(),
});
// The nested-session displays, for input routing. Read on the input path for every event, so it
// is kept apart from `PUBLISHED` and read without blocking the manager.
static NESTED: RwLock<Vec<NestedTarget>> = RwLock::new(Vec::new());
static WORKER: OnceLock<Worker> = OnceLock::new();
// When the manager last failed to start (the root service not up yet, say): it is tried again
// after `RETRY`, not given up on for the life of the server, nor tried on every login.
static FAILED: Mutex<Option<Instant>> = Mutex::new(None);
const RETRY: Duration = Duration::from_secs(60);
// Cleared if the manager's thread ever ends: nothing it held is ours any more.
static RUNNING: AtomicBool = AtomicBool::new(false);
// The displays there were before the backend began plugging one in, while it does: one that
// appears meanwhile is listed once announced, not before. Windows lists a display before it can
// be told ours.
static BEFORE_PLUG: RwLock<Option<Vec<String>>> = RwLock::new(None);

/// `vdm_caps` for `PeerInfo.platform_additions` of a connection of session `owner`: what its
/// controlling side may ask for. None where there is no backend, or virtual displays are turned
/// off on this device.
pub fn caps(owner: &Owner) -> Option<Value> {
    if !enabled() {
        return None;
    }
    let caps = &worker().filter(|_| RUNNING.load(Ordering::SeqCst))?.caps;
    let free = {
        let published = PUBLISHED.read().unwrap();
        free_for(caps.max, published.names.len(), &published.waiting, owner)
    };
    Some(json!({
        "v": 1,
        "kind": caps.kind,
        "max": caps.max,
        "free": free,
        "custom": caps.custom,
        "hidpi": caps.hidpi,
        "max_w": caps.max_width,
        "max_h": caps.max_height,
        "modes": caps.modes.iter().map(|m| m.name()).collect::<Vec<_>>(),
    }))
}

/// Adds or removes virtual displays for connection `conn_id`, whose session is `owner`. The
/// answer comes through `reply`, once the backend is done, and later word of its displays it did
/// not ask about (`VirtualDisplayResult.notice`). `refusal`: why this connection may not add a
/// display; it still takes back a display its session already has. `privacy_allowed`: whether it
/// may have the desktop to itself (`Mode::Privacy`), which the device's privacy mode permission
/// says; if not, it gets an extra screen, told why.
pub fn toggle(
    conn_id: i32,
    owner: Owner,
    toggle: ToggleVirtualDisplay,
    refusal: Option<&str>,
    privacy_allowed: bool,
    mut reply: Reply,
) {
    let refusal = match refusal {
        Some(refusal) => Some(refusal.to_owned()),
        None if toggle.on && !enabled() => {
            Some("Virtual displays are turned off on the remote device".to_owned())
        }
        None => None,
    };
    // Only a display to add needs the manager started; one not running holds nothing.
    let worker = if toggle.on && refusal.is_none() {
        worker()
    } else {
        running()
    };
    let Some(worker) = worker else {
        let error = match refusal {
            _ if !toggle.on => String::new(),
            Some(refusal) => refusal,
            None => "Virtual displays are not supported on the remote device".to_owned(),
        };
        reply(result(toggle.display, None, &error));
        return;
    };
    let cmd = Cmd::Toggle {
        conn_id,
        owner,
        toggle,
        refusal,
        privacy_allowed,
        reply,
    };
    if let Err(mpsc::SendError(Cmd::Toggle {
        toggle, mut reply, ..
    })) = worker.tx.send(cmd)
    {
        reply(result(
            toggle.display,
            None,
            "Virtual displays are not available on the remote device",
        ));
    }
}

/// Connection `conn_id` may no longer have the desktop to itself, its privacy mode permission
/// taken: a display of it that has becomes an extra screen, the desktop given back, and its
/// controlling side told.
pub fn privacy_revoked(conn_id: i32) {
    if let Some(worker) = running() {
        worker.tx.send(Cmd::Demote { conn_id }).ok();
    }
}

/// A connection ended. `closed`: its session ended for good, so its displays go now; otherwise
/// they are kept a while for the session to come back.
pub fn conn_ended(conn_id: i32, closed: bool) {
    // Without a manager there is nothing to remove, and no reason to start one.
    if let Some(worker) = running() {
        worker.tx.send(Cmd::ConnEnded { conn_id, closed }).ok();
    }
}

/// The server is about to end, the service stopping or the session changing: our displays go
/// first, a desktop taken over given back, so the person at the device is not left with dark
/// monitors. Waits `SHUTDOWN_WAIT` at most, the manager being maybe busy.
#[cfg(windows)]
pub fn shutdown() {
    let Some(worker) = running() else {
        return;
    };
    let (done, wait) = mpsc::channel();
    if worker.tx.send(Cmd::Shutdown { done }).is_ok()
        && wait.recv_timeout(SHUTDOWN_WAIT).is_err()
    {
        log::warn!("The virtual displays did not go in time");
    }
}

// How many more displays `owner` may have: those free, and those of its device waiting out the
// grace of a session that ended, which a new session of the device takes back. Counted as taken,
// the controlling side would not even ask after its client was restarted.
fn free_for(max: usize, used: usize, waiting: &[Owner], owner: &Owner) -> usize {
    let reclaimable = waiting
        .iter()
        .filter(|w| w.same_device(owner) && *w != owner)
        .count();
    (max.saturating_sub(used) + reclaimable).min(max)
}

/// Whether `name` is one of our virtual displays.
pub fn is_virtual(name: &str) -> bool {
    PUBLISHED.read().unwrap().names.iter().any(|n| n == name)
}

/// Whether display `name` is listed: not while the backend is plugging a display in, appeared
/// meanwhile and not announced yet. Whatever indexes displays reads them through this.
pub fn listed(name: &str) -> bool {
    listed_given(BEFORE_PLUG.read().unwrap().as_deref(), name, is_virtual(name))
}

fn listed_given(before: Option<&[String]>, name: &str, announced: bool) -> bool {
    before.map_or(true, |before| announced || before.iter().any(|n| n == name))
}

/// `Some(the displays there now)` as the backend begins plugging one in, `None` once it is done.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn hide_new_displays(before: Option<Vec<String>>) {
    *BEFORE_PLUG.write().unwrap() = before;
}

/// Takes back the virtual display a previous run of the server left, where the backend can tell
/// it is ours. Run as the server starts, before any display is listed.
#[cfg(windows)]
pub fn clean_leftover() {
    if win::new_backend().is_some() {
        win::clean_leftover();
    }
}

/// The nested-session display the desktop point `(x, y)` lands in, if any: input there goes to its
/// own X session, not the shared desktop. `(x, y)` are the controlling side's coordinates in the
/// shared desktop, the same ones the input path uses. Empty where no nested display is in use, so
/// this is read on every input event without touching the manager.
pub fn nested_at(x: i32, y: i32) -> Option<NestedTarget> {
    let nested = NESTED.read().unwrap();
    if nested.is_empty() {
        return None;
    }
    nested
        .iter()
        .find(|t| x >= t.x && y >= t.y && x < t.x + t.w && y < t.y + t.h)
        .cloned()
}

/// Whether any display runs as its own nested session right now.
pub fn any_nested() -> bool {
    !NESTED.read().unwrap().is_empty()
}

/// Resizes `owner`'s virtual display `name`. Answers whether `name` is ours, which the caller must
/// leave alone then, even if it belongs to another session and so keeps its size.
pub fn resize(owner: Owner, name: &str, width: i32, height: i32) -> bool {
    if !is_virtual(name) {
        return false;
    }
    if let Some(worker) = running() {
        worker
            .tx
            .send(Cmd::Resize {
                owner,
                name: name.to_owned(),
                width: width.max(0) as u32,
                height: height.max(0) as u32,
            })
            .ok();
    }
    true
}

/// Puts our virtual displays last, oldest first, and leaves the other displays in the order they
/// came. Plugging a display in or out then never renumbers a physical display.
pub fn stable_order<T>(displays: Vec<T>, name: impl Fn(&T) -> String) -> Vec<T> {
    stable_order_by(&PUBLISHED.read().unwrap().names, displays, name)
}

/// True while the backend is plugging displays in or out, resizing or clearing them. The display
/// list waits meanwhile, so it never shows a display half made. Bounded, so a stuck backend
/// cannot freeze the list.
pub fn layout_settling() -> bool {
    PUBLISHED
        .read()
        .unwrap()
        .changing
        .map_or(false, |since| since.elapsed() < SETTLE_LIMIT)
}

// A permission like the others: the access mode overrides it.
fn enabled() -> bool {
    #[cfg(not(target_os = "ios"))]
    return crate::server::Connection::is_permission_enabled_locally(
        keys::OPTION_ENABLE_REMOTE_VIRTUAL_DISPLAY,
    );
    #[cfg(target_os = "ios")]
    false
}

// The manager, started on first need.
fn worker() -> Option<&'static Worker> {
    if let Some(worker) = WORKER.get() {
        return Some(worker);
    }
    let mut failed = FAILED.lock().unwrap();
    if let Some(worker) = WORKER.get() {
        return Some(worker);
    }
    if failed.map_or(false, |at| at.elapsed() < RETRY) {
        return None;
    }
    match start() {
        Some(worker) => {
            *failed = None;
            Some(WORKER.get_or_init(|| worker))
        }
        None => {
            *failed = Some(Instant::now());
            None
        }
    }
}

fn start() -> Option<Worker> {
    let backend = new_backend()?;
    let caps = backend.caps();
    let (tx, rx) = mpsc::channel();
    RUNNING.store(true, Ordering::SeqCst);
    let manager = Manager::new(Box::new(Settling(backend)));
    match std::thread::Builder::new()
        .name("vdisplay".to_owned())
        .spawn(move || run(manager, rx))
    {
        Ok(_) => Some(Worker { tx, caps }),
        Err(e) => {
            RUNNING.store(false, Ordering::SeqCst);
            log::error!("Failed to start the virtual display manager: {}", e);
            None
        }
    }
}

// The manager, if it was started and still runs.
fn running() -> Option<&'static Worker> {
    WORKER.get().filter(|_| RUNNING.load(Ordering::SeqCst))
}

/// This platform's backend, where there is one.
fn new_backend() -> Option<Box<dyn Backend>> {
    #[cfg(target_os = "linux")]
    return if crate::platform::linux::is_x11() {
        x11::new_backend()
    } else {
        gnome::new_backend()
    };
    #[cfg(windows)]
    return win::new_backend();
    #[cfg(not(any(target_os = "linux", windows)))]
    None
}

fn run(mut manager: Manager, rx: mpsc::Receiver<Cmd>) {
    let _stopped = Stopped;
    log::info!("Virtual display manager started: {:?}", manager.caps());
    manager.reset();
    publish(&manager);
    // Each connection's way to its controlling side, the one its last request came with: for word
    // of its displays it did not ask about.
    let mut notifiers: HashMap<i32, Reply> = HashMap::new();
    let mut next_maintain = Instant::now() + MAINTAIN;
    loop {
        let maintain_at = manager.has_displays().then_some(next_maintain);
        let deadline = match (manager.next_deadline(), maintain_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let cmd = match deadline {
            Some(deadline) => {
                match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(cmd) => Some(cmd),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            },
        };
        if let Some(cmd) = cmd {
            handle(&mut manager, cmd, &mut notifiers);
        }
        let now = Instant::now();
        manager.expire(now);
        if now >= next_maintain {
            manager.maintain();
            next_maintain = now + maintain_every(&manager);
        } else {
            // Watchful since the last look: the next one comes sooner.
            next_maintain = next_maintain.min(now + maintain_every(&manager));
        }
        // After any answer it follows from.
        notify(&mut notifiers, manager.word());
        publish(&manager);
    }
}

fn maintain_every(manager: &Manager) -> Duration {
    if manager.watchful() {
        WATCHFUL
    } else {
        MAINTAIN
    }
}

fn handle(manager: &mut Manager, cmd: Cmd, notifiers: &mut HashMap<i32, Reply>) {
    match cmd {
        Cmd::Toggle {
            conn_id,
            owner,
            toggle,
            refusal,
            privacy_allowed,
            mut reply,
        } => {
            answer(
                manager,
                conn_id,
                &owner,
                toggle,
                refusal,
                privacy_allowed,
                &mut reply,
            );
            if manager.serves(conn_id) {
                notifiers.insert(conn_id, reply);
            } else {
                notifiers.remove(&conn_id);
            }
        }
        Cmd::Demote { conn_id } => manager.demote(conn_id),
        Cmd::ConnEnded { conn_id, closed } => {
            notifiers.remove(&conn_id);
            manager.conn_ended(conn_id, closed, Instant::now());
        }
        Cmd::Resize {
            owner,
            name,
            width,
            height,
        } => {
            manager.resize(&owner, &name, width, height);
        }
        #[cfg(windows)]
        Cmd::Shutdown { done } => {
            manager.reset();
            done.send(()).ok();
        }
    }
}

// Word for the sessions of their displays, through the way of the connection last heard from. A
// connection gone has none: the manager keeps the word until its session takes the display back.
fn notify(notifiers: &mut HashMap<i32, Reply>, notices: Vec<Notice>) {
    for n in notices {
        if let Some(reply) = notifiers.get_mut(&n.conn_id) {
            let mut word = result(n.slot, Some(n.name), &n.why);
            word.notice = true;
            reply(word);
        }
    }
}

// A request of connection `conn_id` answered through `reply`.
fn answer(
    manager: &mut Manager,
    conn_id: i32,
    owner: &Owner,
    toggle: ToggleVirtualDisplay,
    refusal: Option<String>,
    privacy_allowed: bool,
    reply: &mut Reply,
) {
    let slot = toggle.display;
    if !toggle.on {
        let removed = manager.plug_out(owner, slot);
        if slot == ALL_SLOTS {
            // One answer to one request, whatever it removed: the first failure, if any.
            let error = removed
                .into_iter()
                .find_map(|(_, _, res)| res.err())
                .unwrap_or_default();
            reply(result(ALL_SLOTS, None, &error));
            return;
        }
        for (slot, name, res) in removed {
            match res {
                Ok(()) => reply(result(slot, None, "")),
                Err(e) => reply(held(slot, manager.used(&name), name, &e)),
            }
        }
        return;
    }
    let spec = toggle.spec.as_ref().map_or(
        Spec {
            width: 0,
            height: 0,
            scale: 1.0,
            mode: Mode::Shared,
        },
        |s| Spec {
            width: s.width.max(0) as u32,
            height: s.height.max(0) as u32,
            scale: s.scale,
            mode: Mode::from_wire(s.mode),
        },
    );
    if let Some(refusal) = refusal {
        // Taking back a display the session already has adds nothing, and resizes nothing.
        match manager.take_back(conn_id, owner, slot, privacy_allowed, spec.mode) {
            Some(name) => reply(held(slot, manager.used(&name), name, "")),
            None => reply(result(slot, None, &refusal)),
        }
        return;
    }
    // Listed as ours, last, from the moment it has a name.
    let mut announce = |name: &str| {
        PUBLISHED.write().unwrap().names.push(name.to_owned());
    };
    match manager.plug_in(conn_id, owner, slot, spec, privacy_allowed, &mut announce) {
        Ok((name, note)) => reply(held(
            slot,
            manager.used(&name),
            name,
            note.as_deref().unwrap_or(""),
        )),
        Err(e) => {
            // The controlling side only shows it: here is the one record of why.
            log::warn!(
                "Failed to plug in a virtual display ({}x{}@{}, {:?}) for slot {} of {} \
                 on connection {}: {}",
                spec.width,
                spec.height,
                spec.scale,
                spec.mode,
                slot,
                owner.peer_id,
                conn_id,
                e
            );
            reply(result(slot, None, &e))
        }
    }
}

// Marks the display list as changing around every backend call that changes displays; the
// manager's thread ends the change by publishing what came of it.
struct Settling(Box<dyn Backend>);

impl Backend for Settling {
    fn caps(&self) -> Caps {
        self.0.caps()
    }

    fn plug_in(&mut self, spec: &Spec, announce: &mut dyn FnMut(&str)) -> ResultType<String> {
        set_changing();
        self.0.plug_in(spec, announce)
    }

    fn set_mode(&mut self, name: &str, spec: &Spec) -> ResultType<()> {
        set_changing();
        self.0.set_mode(name, spec)
    }

    fn usage(&self, name: &str) -> Option<Mode> {
        self.0.usage(name)
    }

    fn lost_why(&mut self, name: &str) -> Option<String> {
        self.0.lost_why(name)
    }

    fn plug_out(&mut self, name: &str) -> ResultType<()> {
        set_changing();
        self.0.plug_out(name)
    }

    fn reset_all(&mut self) -> ResultType<()> {
        set_changing();
        self.0.reset_all()
    }

    // Not marked as changing: mostly it changes nothing, and what it puts back was already
    // wrong in the list.
    fn maintain(&mut self) -> Vec<String> {
        self.0.maintain()
    }

    fn left(&mut self, name: &str) {
        self.0.left(name)
    }

    fn nested(&self) -> Vec<NestedTarget> {
        self.0.nested()
    }

    fn watchful(&self) -> bool {
        self.0.watchful()
    }
}

// Ends with the manager's thread, however it ends.
struct Stopped;

impl Drop for Stopped {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
        let mut published = PUBLISHED.write().unwrap_or_else(|e| e.into_inner());
        published.names.clear();
        published.changing = None;
        published.waiting.clear();
        *NESTED.write().unwrap_or_else(|e| e.into_inner()) = Vec::new();
        log::error!("The virtual display manager stopped");
    }
}

fn set_changing() {
    PUBLISHED.write().unwrap().changing = Some(Instant::now());
}

// Names and the end of the change, together: whoever sees the change over sees its names.
fn publish(manager: &Manager) {
    let mut published = PUBLISHED.write().unwrap();
    published.names = manager.names();
    published.waiting = manager.waiting();
    published.changing = None;
    drop(published);
    *NESTED.write().unwrap() = manager.nested();
}

// What a slot holds now: `name` if a display, and why the request failed, if it did.
fn result(slot: i32, name: Option<String>, error: &str) -> VirtualDisplayResult {
    VirtualDisplayResult {
        slot,
        on: name.is_some(),
        name: name.unwrap_or_default(),
        error: error.to_owned(),
        ..Default::default()
    }
}

// A slot still holding `name`, and how it is used now, which a failed request may not have changed.
fn held(slot: i32, used: Option<Mode>, name: String, error: &str) -> VirtualDisplayResult {
    VirtualDisplayResult {
        used: used.map_or(0, |mode| mode.wire() + 1),
        ..result(slot, Some(name), error)
    }
}

fn stable_order_by<T>(names: &[String], displays: Vec<T>, name: impl Fn(&T) -> String) -> Vec<T> {
    if names.is_empty() {
        return displays;
    }
    let (mut ours, mut others): (Vec<T>, Vec<T>) = displays
        .into_iter()
        .partition(|d| names.contains(&name(d)));
    ours.sort_by_key(|d| names.iter().position(|n| *n == name(d)));
    others.extend(ours);
    others
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_display_appearing_while_one_is_plugged_in_waits_for_its_announcement() {
        let before = vec![r"\\.\DISPLAY1".to_owned()];
        assert!(listed_given(None, r"\\.\DISPLAY2", false), "nothing being plugged in");
        assert!(listed_given(Some(&before), r"\\.\DISPLAY1", false), "there before");
        assert!(!listed_given(Some(&before), r"\\.\DISPLAY2", false), "new, not announced");
        assert!(listed_given(Some(&before), r"\\.\DISPLAY2", true), "announced");
    }

    use super::*;
    use base::message_proto::VirtualDisplaySpec;

    fn order(names: &[&str], displays: &[&str]) -> Vec<String> {
        let names: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        let displays: Vec<String> = displays.iter().map(|s| s.to_string()).collect();
        stable_order_by(&names, displays, |d| d.clone())
    }

    #[test]
    fn displays_waiting_for_an_ended_session_of_the_device_are_free_for_its_new_one() {
        let owner = |id: &str, session_id| Owner {
            peer_id: id.to_owned(),
            peer_name: "mac".to_owned(),
            session_id,
        };
        let (a1, a2, b) = (owner("A", 1), owner("A", 2), owner("B", 3));
        assert_eq!(free_for(1, 1, &[], &a2), 0, "full");
        assert_eq!(free_for(1, 1, &[a1.clone()], &a2), 1, "its device's ended session");
        assert_eq!(free_for(1, 1, &[b], &a2), 0, "another device's");
        assert_eq!(free_for(1, 1, &[a2.clone()], &a2), 0, "its own, which it still has");
        assert_eq!(free_for(2, 1, &[a1.clone(), a1], &a2), 2, "never more than there are");
        assert_eq!(free_for(2, 0, &[], &a2), 2);
    }

    #[test]
    fn virtual_displays_go_last_in_the_order_they_were_made() {
        assert_eq!(
            order(&["DP-2", "DP-1"], &["DP-1", "HDMI-1", "DP-2", "eDP-1"]),
            vec!["HDMI-1", "eDP-1", "DP-2", "DP-1"]
        );
        // Without virtual displays nothing moves.
        assert_eq!(order(&[], &["DP-1", "HDMI-1"]), vec!["DP-1", "HDMI-1"]);
        // A virtual display the platform does not list (yet, or any more) is just absent.
        assert_eq!(order(&["DP-3"], &["HDMI-1"]), vec!["HDMI-1"]);
    }

    #[test]
    fn results_say_what_the_slot_holds() {
        let r = result(2, Some("DP-1".to_owned()), "");
        assert!(r.on && r.name == "DP-1" && r.slot == 2 && r.error.is_empty());
        let r = result(1, None, "No free virtual display");
        assert!(!r.on && r.name.is_empty() && r.error == "No free virtual display");
        // A display kept tells how it is used, its mode plus one, which a removal has none of.
        let r = held(3, Some(Mode::Privacy), "DP-2".to_owned(), "Refused");
        assert!(r.on && r.name == "DP-2" && r.used == 2 && r.error == "Refused");
        assert_eq!(held(3, Some(Mode::Shared), "DP-2".to_owned(), "").used, 1);
        assert_eq!(held(3, None, "DP-2".to_owned(), "").used, 0);
        assert_eq!(result(2, Some("DP-1".to_owned()), "").used, 0);
    }

    // Takes a desktop over and then fails, so taken over it stays; removes nothing.
    struct Stuck(Mode);

    impl Backend for Stuck {
        fn caps(&self) -> Caps {
            Caps {
                kind: "stuck",
                max: 1,
                custom: true,
                hidpi: false,
                max_width: 4096,
                max_height: 4096,
                modes: vec![Mode::Shared, Mode::Privacy],
            }
        }
        fn plug_in(&mut self, _: &Spec, _: &mut dyn FnMut(&str)) -> ResultType<String> {
            Ok("V-1".to_owned())
        }
        fn set_mode(&mut self, _: &str, spec: &Spec) -> ResultType<()> {
            self.0 = spec.mode;
            hbb_common::bail!("No screens lit")
        }
        fn plug_out(&mut self, _: &str) -> ResultType<()> {
            hbb_common::bail!("Busy")
        }
        fn reset_all(&mut self) -> ResultType<()> {
            Ok(())
        }
        fn usage(&self, _: &str) -> Option<Mode> {
            Some(self.0)
        }
    }

    #[test]
    fn every_answer_keeping_a_display_tells_how_it_is_used() {
        let owner = Owner {
            peer_id: "A".to_owned(),
            peer_name: "mac".to_owned(),
            session_id: 1,
        };
        let mut m = Manager::new(Box::new(Stuck(Mode::Shared)));
        let shared = Spec {
            width: 1920,
            height: 1080,
            scale: 1.0,
            mode: Mode::Shared,
        };
        m.plug_in(1, &owner, 0, shared, true, &mut |_| {}).unwrap();
        let answers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut reply = {
            let answers = answers.clone();
            Box::new(move |r: VirtualDisplayResult| answers.lock().unwrap().push(r)) as Reply
        };
        let ask = |on: bool, mode: i32| ToggleVirtualDisplay {
            display: 0,
            on,
            spec: Some(VirtualDisplaySpec {
                width: 1920,
                height: 1080,
                scale: 1.0,
                mode,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        };
        // Asked to take the desktop over, which failed half way: taken over, not shared.
        answer(&mut m, 1, &owner, ask(true, 1), None, true, &mut reply);
        // Taken back by a session that may not add displays, and failed to be removed.
        answer(&mut m, 1, &owner, ask(true, 1), Some("No".to_owned()), true, &mut reply);
        answer(&mut m, 1, &owner, ask(false, 0), None, true, &mut reply);
        let answers = answers.lock().unwrap();
        let told: Vec<(bool, &str, i32, &str)> = answers
            .iter()
            .map(|r| (r.on, r.name.as_str(), r.used, r.error.as_str()))
            .collect();
        assert_eq!(
            told,
            vec![
                (true, "V-1", 2, "No screens lit"),
                (true, "V-1", 2, ""),
                (true, "V-1", 2, "Busy"),
            ]
        );
    }

    #[test]
    fn a_spec_marks_the_new_protocol_and_survives_the_wire() {
        use hbb_common::protobuf::Message as _;
        let mut t = ToggleVirtualDisplay {
            display: 0,
            on: false,
            ..Default::default()
        };
        // Removal carries an empty spec only to be told apart from the upstream request.
        t.spec = Some(Default::default()).into();
        let parsed = ToggleVirtualDisplay::parse_from_bytes(&t.write_to_bytes().unwrap()).unwrap();
        assert!(parsed.spec.is_some());
        // The upstream request has none.
        let upstream = ToggleVirtualDisplay {
            display: 1,
            on: true,
            ..Default::default()
        };
        let parsed =
            ToggleVirtualDisplay::parse_from_bytes(&upstream.write_to_bytes().unwrap()).unwrap();
        assert!(parsed.spec.is_none());
    }

    #[test]
    fn without_a_manager_requests_are_answered_at_once() {
        let owner = Owner {
            peer_id: "A".to_owned(),
            peer_name: "mac".to_owned(),
            session_id: 1,
        };
        let answers = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let reply = |answers: &std::sync::Arc<std::sync::Mutex<Vec<VirtualDisplayResult>>>| {
            let answers = answers.clone();
            Box::new(move |r| answers.lock().unwrap().push(r)) as Reply
        };
        let ask = |on: bool| ToggleVirtualDisplay {
            display: 0,
            on,
            spec: Some(Default::default()).into(),
            ..Default::default()
        };
        // No platform has a backend yet, so nothing ever starts.
        toggle(1, owner.clone(), ask(false), None, true, reply(&answers));
        toggle(1, owner.clone(), ask(true), Some("refused"), true, reply(&answers));
        let answers = answers.lock().unwrap();
        assert_eq!(answers.len(), 2);
        assert!(!answers[0].on && answers[0].error.is_empty());
        assert!(!answers[1].on && answers[1].error == "refused");
        assert!(running().is_none());
    }
}
