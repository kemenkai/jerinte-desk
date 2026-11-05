//! The keyboards and pointers at the device, kept from a desktop taken over (`Mode::Privacy`): the
//! root service holds each input device of the seat to itself (`EVIOCGRAB`), and the X server
//! hears nothing of them. What the controlling side types comes through XTEST, which no device
//! carries, and is untouched. The kernel lets a device go when the file holding it closes, the
//! service's death included; a service alive but stuck stops saying the lock is still wanted, and
//! the lock lets go by itself.
//!
//! Ctrl+P on a keyboard held ends the takeover, as it ends upstream's privacy mode on Windows
//! (src/privacy_mode/win_input.rs): whoever sits at the device can always have it back.

use hbb_common::log;
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    time::{Duration, Instant},
};

// linux/input-event-codes.h
const KEY_P: u16 = 25;
const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_POWER: u16 = 116;
const KEY_SLEEP: u16 = 142;
const KEY_WAKEUP: u16 = 143;
const KEY_SUSPEND: u16 = 205;
const KEY_POWER2: u16 = 0x164;

// A device with these keys alone is a power or sleep button, logind's: left alone, so the device
// can always be turned off.
const POWER_KEYS: [u16; 5] = [KEY_POWER, KEY_SLEEP, KEY_WAKEUP, KEY_SUSPEND, KEY_POWER2];

// Our own uinput devices, which carry what the controlling side types where XTEST does not
// (Wayland, src/server/uinput.rs), by the start of their names.
const OURS: [&str; 2] = ["RustDesk ", "mouce-library-fake-mouse"];

// linux/input-event-codes.h
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

/// How often the devices are looked at while held: one plugged in meanwhile is held at most this
/// long after it appears.
pub const TICK: Duration = Duration::from_millis(100);
/// How long the lock holds without being told it is still wanted.
pub const STALE: Duration = Duration::from_secs(5);

/// The input devices of the machine.
pub trait Seat {
    /// The event devices there are, each with its device number.
    fn devices(&self) -> Vec<(PathBuf, u64)>;
    fn open(&self, path: &Path) -> io::Result<Box<dyn Device>>;
    /// The seat udev put device `rdev` on, seat0 unless said otherwise; None while udev has not
    /// said yet, which it does before the X server hears of the device.
    fn seat_of(&self, rdev: u64) -> Option<String>;
}

/// An event device, open.
pub trait Device: Send {
    fn name(&self) -> String;
    /// Its key and button codes.
    fn keys(&self) -> Vec<u16>;
    /// Whether it moves a pointer: a relative or absolute X axis.
    fn pointer(&self) -> bool;
    /// The keys and buttons down now (`EVIOCGKEY`). The kernel drops the key events waiting to be
    /// read on asking (`evdev_handle_get_val`, so a reader does not see them after the state they
    /// led to): not for a device whose events are still to be read.
    fn down(&self) -> io::Result<Vec<u16>>;
    /// Whether it is still there (`EVIOCGVERSION`, which every device answers until it goes),
    /// without touching the events waiting.
    fn alive(&self) -> bool;
    fn grab(&mut self) -> io::Result<()>;
    fn ungrab(&mut self) -> io::Result<()>;
    /// The keys pressed (true) and let go (false) since last asked, in order, repeats left out
    /// (`key_changes`). Of a device held, whose events come to the holder alone.
    fn keys_since(&mut self) -> Vec<(u16, bool)>;
}

/// The keys pressed (true) and let go (false) in `events` (type, code, value), repeats left out.
/// Where the kernel dropped events (`SYN_DROPPED`, its buffer full, as a pointer on the same device
/// fills it), those up to the next report are not to be trusted, and the Ctrl keys and P are said
/// as they were there (`as_before`): a Ctrl+P among the events dropped is seen while P is still
/// down.
pub fn key_changes(
    events: impl IntoIterator<Item = (u16, u16, i32)>,
    now: impl Fn() -> Vec<u16>,
) -> Vec<(u16, bool)> {
    let events: Vec<_> = events.into_iter().collect();
    let mut keys = Vec::new();
    let mut dropped = false;
    for (i, &(kind, code, value)) in events.iter().enumerate() {
        match (kind, code) {
            (EV_SYN, SYN_DROPPED) => dropped = true,
            (EV_SYN, SYN_REPORT) if dropped => {
                dropped = false;
                keys.extend(as_before(&events[i + 1..], &now()));
            }
            (EV_KEY, _) if !dropped && value != 2 => keys.push((code, value == 1)),
            _ => {}
        }
    }
    if dropped {
        keys.extend(as_before(&[], &now()));
    }
    keys
}

/// The Ctrl keys and P as they were just before the events `after`, from the keys down `now`, after
/// them all: one changed in `after` as its first change there has it not, any other as it is now.
fn as_before(after: &[(u16, u16, i32)], now: &[u16]) -> [(u16, bool); 3] {
    [KEY_LEFTCTRL, KEY_RIGHTCTRL, KEY_P].map(|key| {
        let first = after
            .iter()
            .find(|&&(kind, code, value)| kind == EV_KEY && code == key && value != 2);
        (key, first.map_or(now.contains(&key), |&(_, _, value)| value == 0))
    })
}

/// Why a device is not to be held, if it is not.
pub fn not_held(name: &str, keys: &[u16], pointer: bool) -> Option<&'static str> {
    if OURS.iter().any(|ours| name.starts_with(ours)) {
        return Some("ours");
    }
    if keys.is_empty() && !pointer {
        return Some("neither keys nor a pointer");
    }
    if !pointer && keys.iter().all(|k| POWER_KEYS.contains(k)) {
        return Some("a power button");
    }
    None
}

struct Held {
    path: PathBuf,
    rdev: u64,
    name: String,
    device: Box<dyn Device>,
    // One with P and a Ctrl key, watched for Ctrl+P.
    keyboard: bool,
    ctrl: [bool; 2],
}

/// The devices held, and those looked at and left alone.
#[derive(Default)]
pub struct Grabs {
    held: Vec<Held>,
    // Left alone, until another device takes its path.
    left: Vec<(PathBuf, u64)>,
    // Said once each: waiting for its keys to be let go; held by someone else.
    waiting: HashSet<PathBuf>,
    refused: HashSet<PathBuf>,
}

impl Grabs {
    /// Holds every device of the seat not held yet, and forgets those gone. Answers whether
    /// Ctrl+P was pressed on a keyboard held.
    pub fn look(&mut self, seat: &dyn Seat) -> bool {
        let devices = seat.devices();
        let there = |path: &Path, rdev: u64| devices.iter().any(|(p, r)| p == path && *r == rdev);
        self.held.retain(|h| {
            // Gone, or gone and back at the same path between two looks: the file held is the
            // old device's, which answers no more.
            let kept = there(&h.path, h.rdev) && h.device.alive();
            if !kept {
                log::info!("Input device {} went", h.name);
            }
            kept
        });
        self.left.retain(|(path, rdev)| there(path, *rdev));
        for (path, rdev) in &devices {
            let known = self.held.iter().any(|h| &h.path == path)
                || self.left.iter().any(|(p, _)| p == path);
            if !known {
                self.hold(seat, path, *rdev);
            }
        }
        let mut escape = false;
        for h in self.held.iter_mut().filter(|h| h.keyboard) {
            for (key, down) in h.device.keys_since() {
                match key {
                    KEY_LEFTCTRL => h.ctrl[0] = down,
                    KEY_RIGHTCTRL => h.ctrl[1] = down,
                    KEY_P if down && h.ctrl.contains(&true) => escape = true,
                    _ => {}
                }
            }
        }
        escape
    }

    /// The names of the devices held.
    #[cfg(test)]
    pub fn held(&self) -> Vec<String> {
        self.held.iter().map(|h| h.name.clone()).collect()
    }

    /// Lets every device go.
    pub fn release(&mut self) {
        for mut h in self.held.drain(..) {
            // Closing it would do as well.
            h.device.ungrab().ok();
        }
        self.left.clear();
        self.waiting.clear();
        self.refused.clear();
    }

    fn hold(&mut self, seat: &dyn Seat, path: &Path, rdev: u64) {
        match seat.seat_of(rdev) {
            // Looked at again next time.
            None => return,
            Some(other) if other != "seat0" => {
                self.left.push((path.to_owned(), rdev));
                return;
            }
            Some(_) => {}
        }
        // Gone meanwhile, or not ready: looked at again next time.
        let mut device = match seat.open(path) {
            Ok(device) => device,
            Err(e) => {
                log::debug!("Input device {} not opened: {}", path.display(), e);
                return;
            }
        };
        let name = device.name();
        let keys = device.keys();
        if let Some(why) = not_held(&name, &keys, device.pointer()) {
            log::info!("Leaving input device {} ({}) alone: {}", name, path.display(), why);
            self.left.push((path.to_owned(), rdev));
            return;
        }
        // The X server saw whatever is down go down, and would never see it come up: a key would
        // repeat, a button stay down, for as long as the device is held.
        let idle = |device: &dyn Device| matches!(device.down(), Ok(down) if down.is_empty());
        if !idle(device.as_ref()) {
            if self.waiting.insert(path.to_owned()) {
                log::info!("Waiting for the keys of input device {} to be let go", name);
            }
            return;
        }
        if let Err(e) = device.grab() {
            if self.refused.insert(path.to_owned()) {
                log::warn!("Failed to keep input device {} from the desktop: {}", name, e);
            }
            return;
        }
        // One pressed in the moment between is let go, and held once let go.
        if !idle(device.as_ref()) {
            device.ungrab().ok();
            return;
        }
        self.waiting.remove(path);
        self.refused.remove(path);
        log::info!("Keeping input device {} ({}) from the desktop", name, path.display());
        let keyboard = keys.contains(&KEY_P)
            && (keys.contains(&KEY_LEFTCTRL) || keys.contains(&KEY_RIGHTCTRL));
        self.held.push(Held {
            path: path.to_owned(),
            rdev,
            name,
            device,
            keyboard,
            ctrl: [false; 2],
        });
    }
}

/// The lock, on a thread of its own started with it: told `true`, it holds the seat's input, and
/// must be told so again within `STALE`; told `false`, it lets go.
pub struct Lock {
    tx: mpsc::Sender<bool>,
    escaped: Arc<AtomicBool>,
}

impl Lock {
    pub fn start(seat: impl Seat + Send + 'static) -> io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let escaped = Arc::new(AtomicBool::new(false));
        let pressed = escaped.clone();
        std::thread::Builder::new()
            .name("vdisplay-input".into())
            .spawn(move || run(rx, pressed, seat))?;
        Ok(Self { tx, escaped })
    }

    pub fn set(&self, on: bool) {
        self.tx.send(on).ok();
    }

    /// Whether Ctrl+P was pressed on a keyboard held since last asked.
    pub fn escaped(&self) -> bool {
        self.escaped.swap(false, Ordering::SeqCst)
    }
}

fn run(rx: mpsc::Receiver<bool>, escaped: Arc<AtomicBool>, seat: impl Seat) {
    let mut grabs = Grabs::default();
    // When last told to hold.
    let mut wanted: Option<Instant> = None;
    let mut looked: Option<Instant> = None;
    loop {
        let told = match wanted {
            Some(_) => rx.recv_timeout(TICK),
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
        };
        match told {
            Ok(true) => {
                if wanted.is_none() {
                    log::info!("Keeping the keyboards and pointers here from the desktop");
                }
                wanted = Some(Instant::now());
            }
            Ok(false) => {
                if wanted.take().is_some() {
                    grabs.release();
                    log::info!("The keyboards and pointers here are the desktop's again");
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                grabs.release();
                return;
            }
        }
        if wanted.map_or(false, |at| at.elapsed() >= STALE) {
            log::error!(
                "Not told for {:?} to keep the keyboards and pointers here; letting them go",
                STALE
            );
            wanted = None;
            grabs.release();
            continue;
        }
        if looked.map_or(false, |at| at.elapsed() < TICK) {
            continue;
        }
        looked = Some(Instant::now());
        if grabs.look(&seat) {
            log::warn!("Ctrl+P pressed here: the desktop is to be given back");
            escaped.store(true, Ordering::SeqCst);
        }
    }
}

/// The seat udev put a device on, from its record ("E:ID_SEAT=seat1").
pub fn udev_seat(record: &str) -> Option<String> {
    record
        .lines()
        .find_map(|line| line.strip_prefix("E:ID_SEAT="))
        .map(|seat| seat.trim().to_owned())
}

// glibc's gnu_dev_major / gnu_dev_minor.
fn major_minor(rdev: u64) -> (u64, u64) {
    (
        ((rdev >> 8) & 0xfff) | ((rdev >> 32) & !0xfff),
        (rdev & 0xff) | ((rdev >> 12) & !0xff),
    )
}

/// The machine's own devices.
#[cfg(target_os = "linux")]
pub struct System;

#[cfg(target_os = "linux")]
impl Seat for System {
    fn devices(&self) -> Vec<(PathBuf, u64)> {
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let Ok(dir) = std::fs::read_dir("/dev/input") else {
            return Vec::new();
        };
        dir.filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                meta.file_type()
                    .is_char_device()
                    .then(|| (e.path(), meta.rdev()))
            })
            .collect()
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn Device>> {
        use hbb_common::libc;
        use std::os::unix::io::AsRawFd;
        // Opened close-on-exec, as std does: a child of the service holding the file would keep
        // the device held after the service went.
        let device = evdev::raw_stream::RawDevice::open(path)?;
        let fd = device.as_raw_fd();
        // Only what is there is read.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Box::new(Evdev(device)))
    }

    fn seat_of(&self, rdev: u64) -> Option<String> {
        const UDEV: &str = "/run/udev/data";
        let (major, minor) = major_minor(rdev);
        match std::fs::read_to_string(format!("{}/c{}:{}", UDEV, major, minor)) {
            Ok(record) => Some(udev_seat(&record).unwrap_or_else(|| "seat0".to_owned())),
            // Without udev there is but the one seat.
            Err(_) if !Path::new(UDEV).is_dir() => Some("seat0".to_owned()),
            Err(_) => None,
        }
    }
}

#[cfg(target_os = "linux")]
struct Evdev(evdev::raw_stream::RawDevice);

#[cfg(target_os = "linux")]
impl Device for Evdev {
    fn name(&self) -> String {
        self.0.name().unwrap_or_default().to_owned()
    }

    fn keys(&self) -> Vec<u16> {
        self.0
            .supported_keys()
            .map_or_else(Vec::new, |keys| keys.iter().map(|k| k.code()).collect())
    }

    fn pointer(&self) -> bool {
        use evdev::{AbsoluteAxisType, RelativeAxisType};
        self.0
            .supported_relative_axes()
            .map_or(false, |axes| axes.contains(RelativeAxisType::REL_X))
            || self.0.supported_absolute_axes().map_or(false, |axes| {
                axes.contains(AbsoluteAxisType::ABS_X)
                    || axes.contains(AbsoluteAxisType::ABS_MT_POSITION_X)
            })
    }

    fn down(&self) -> io::Result<Vec<u16>> {
        Ok(self.0.get_key_state()?.iter().map(|k| k.code()).collect())
    }

    fn alive(&self) -> bool {
        use hbb_common::libc;
        use std::os::unix::io::AsRawFd;
        // EVIOCGVERSION, _IOR('E', 0x01, int).
        const EVIOCGVERSION: libc::c_ulong = 0x8004_4501;
        let mut version: libc::c_int = 0;
        let answered = unsafe { libc::ioctl(self.0.as_raw_fd(), EVIOCGVERSION as _, &mut version) };
        // Gone is ENODEV alone: a signal in the middle (EINTR) is not.
        answered == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ENODEV)
    }

    fn grab(&mut self) -> io::Result<()> {
        self.0.grab()
    }

    fn ungrab(&mut self) -> io::Result<()> {
        self.0.ungrab()
    }

    fn keys_since(&mut self) -> Vec<(u16, bool)> {
        let mut events = Vec::new();
        // Until there is nothing more to read: the device does not wait (`O_NONBLOCK`).
        while let Ok(read) = self.0.fetch_events() {
            let before = events.len();
            events.extend(read.map(|e| (e.event_type().0, e.code(), e.value())));
            if events.len() == before {
                break;
            }
        }
        key_changes(events, || self.down().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    const KEY_A: u16 = 30;
    const BTN_LEFT: u16 = 0x110;
    const BTN_TOUCH: u16 = 0x14a;

    #[derive(Clone, Default)]
    struct Fake {
        name: String,
        keys: Vec<u16>,
        pointer: bool,
        // Down now; down once grabbed (pressed in the moment between).
        down: Vec<u16>,
        down_after_grab: Vec<u16>,
        busy: bool,
        typed: Vec<(u16, bool)>,
        // Plugged in again at the same path: a file opened before is the old device's.
        generation: u32,
    }

    #[derive(Default)]
    struct State {
        devices: Vec<(PathBuf, u64)>,
        fakes: HashMap<PathBuf, Fake>,
        seats: HashMap<u64, String>,
        // Not on udev's record yet.
        unrecorded: HashSet<u64>,
        // Every grab and ungrab, in order.
        steps: Vec<String>,
    }

    #[derive(Clone, Default)]
    struct FakeSeat(Arc<Mutex<State>>);

    struct Open {
        path: PathBuf,
        fake: Fake,
        generation: u32,
        grabbed: bool,
        state: Arc<Mutex<State>>,
    }

    impl Seat for FakeSeat {
        fn devices(&self) -> Vec<(PathBuf, u64)> {
            self.0.lock().unwrap().devices.clone()
        }
        fn open(&self, path: &Path) -> io::Result<Box<dyn Device>> {
            let fake = self.0.lock().unwrap().fakes.get(path).cloned();
            let fake = fake.ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
            Ok(Box::new(Open {
                path: path.to_owned(),
                generation: fake.generation,
                fake,
                grabbed: false,
                state: self.0.clone(),
            }))
        }
        fn seat_of(&self, rdev: u64) -> Option<String> {
            let s = self.0.lock().unwrap();
            if s.unrecorded.contains(&rdev) {
                return None;
            }
            Some(s.seats.get(&rdev).cloned().unwrap_or_else(|| "seat0".to_owned()))
        }
    }

    impl Device for Open {
        fn name(&self) -> String {
            self.fake.name.clone()
        }
        fn keys(&self) -> Vec<u16> {
            self.fake.keys.clone()
        }
        fn pointer(&self) -> bool {
            self.fake.pointer
        }
        fn down(&self) -> io::Result<Vec<u16>> {
            let mut state = self.state.lock().unwrap();
            let fake = state.fakes.get_mut(&self.path).unwrap();
            if fake.generation != self.generation {
                return Err(io::Error::from_raw_os_error(19));
            }
            // As the kernel does: the key events waiting are dropped.
            fake.typed.clear();
            Ok(if self.grabbed {
                fake.down_after_grab.clone()
            } else {
                fake.down.clone()
            })
        }
        fn alive(&self) -> bool {
            self.state.lock().unwrap().fakes[&self.path].generation == self.generation
        }
        fn grab(&mut self) -> io::Result<()> {
            if self.fake.busy {
                return Err(io::Error::from_raw_os_error(16));
            }
            self.grabbed = true;
            self.state.lock().unwrap().steps.push(format!("grab {}", self.fake.name));
            Ok(())
        }
        fn ungrab(&mut self) -> io::Result<()> {
            self.grabbed = false;
            self.state.lock().unwrap().steps.push(format!("ungrab {}", self.fake.name));
            Ok(())
        }
        fn keys_since(&mut self) -> Vec<(u16, bool)> {
            std::mem::take(&mut self.state.lock().unwrap().fakes.get_mut(&self.path).unwrap().typed)
        }
    }

    impl FakeSeat {
        fn plug(&self, n: u64, fake: Fake) {
            let path = PathBuf::from(format!("/dev/input/event{}", n));
            let mut s = self.0.lock().unwrap();
            s.devices.push((path.clone(), 0xd00 + n));
            s.fakes.insert(path, fake);
        }
        // Out and in again between two looks: the same path, the same device number.
        fn replug(&self, n: u64, mut fake: Fake) {
            let path = PathBuf::from(format!("/dev/input/event{}", n));
            let mut s = self.0.lock().unwrap();
            fake.generation = s.fakes[&path].generation + 1;
            s.fakes.insert(path, fake);
        }
        fn unplug(&self, n: u64) {
            let path = PathBuf::from(format!("/dev/input/event{}", n));
            self.0.lock().unwrap().devices.retain(|(p, _)| *p != path);
        }
        fn with<T>(&self, n: u64, f: impl FnOnce(&mut Fake) -> T) -> T {
            let path = PathBuf::from(format!("/dev/input/event{}", n));
            f(self.0.lock().unwrap().fakes.get_mut(&path).unwrap())
        }
        fn steps(&self) -> Vec<String> {
            std::mem::take(&mut self.0.lock().unwrap().steps)
        }
    }

    fn keyboard(name: &str) -> Fake {
        Fake {
            name: name.to_owned(),
            keys: vec![KEY_P, KEY_LEFTCTRL, KEY_RIGHTCTRL, KEY_A, KEY_POWER],
            ..Default::default()
        }
    }

    fn mouse(name: &str) -> Fake {
        Fake {
            name: name.to_owned(),
            keys: vec![BTN_LEFT],
            pointer: true,
            ..Default::default()
        }
    }

    fn sorted(mut v: Vec<String>) -> Vec<String> {
        v.sort();
        v
    }

    #[test]
    fn keyboards_and_pointers_are_held_power_buttons_switches_and_ours_are_not() {
        let seat = FakeSeat::default();
        seat.plug(0, keyboard("AT keyboard"));
        seat.plug(1, mouse("USB mouse"));
        seat.plug(
            2,
            Fake {
                name: "Power Button".to_owned(),
                keys: vec![KEY_POWER],
                ..Default::default()
            },
        );
        // A lid switch, a speaker: no keys, no pointer.
        seat.plug(
            3,
            Fake {
                name: "Lid Switch".to_owned(),
                ..Default::default()
            },
        );
        seat.plug(
            4,
            Fake {
                name: "RustDesk UInput Keyboard".to_owned(),
                keys: vec![KEY_A],
                ..Default::default()
            },
        );
        // A touchpad, a touchscreen: buttons and an absolute axis.
        seat.plug(
            5,
            Fake {
                name: "Touchpad".to_owned(),
                keys: vec![BTN_TOUCH, BTN_LEFT],
                pointer: true,
                ..Default::default()
            },
        );
        // Brightness and display-switch keys.
        seat.plug(
            6,
            Fake {
                name: "Video Bus".to_owned(),
                keys: vec![224, 225, 227],
                ..Default::default()
            },
        );
        let mut grabs = Grabs::default();
        assert!(!grabs.look(&seat));
        assert_eq!(
            sorted(grabs.held()),
            vec!["AT keyboard", "Touchpad", "USB mouse", "Video Bus"]
        );
        // Looked at once: those left alone are not opened again.
        seat.steps();
        grabs.look(&seat);
        assert!(seat.steps().is_empty());
        grabs.release();
        assert_eq!(seat.steps().len(), 4);
        assert!(grabs.held().is_empty());
    }

    #[test]
    fn a_device_is_held_only_once_nothing_on_it_is_down() {
        let seat = FakeSeat::default();
        seat.plug(0, keyboard("keyboard"));
        seat.plug(1, mouse("mouse"));
        seat.with(0, |k| k.down = vec![KEY_A]);
        seat.with(1, |m| m.down = vec![BTN_LEFT]);
        let mut grabs = Grabs::default();
        grabs.look(&seat);
        assert!(grabs.held().is_empty());
        assert!(seat.steps().is_empty());
        seat.with(0, |k| k.down.clear());
        grabs.look(&seat);
        assert_eq!(grabs.held(), vec!["keyboard"]);
        // Pressed in the moment between the look and the grab: let go again, held later.
        seat.with(1, |m| {
            m.down.clear();
            m.down_after_grab = vec![BTN_LEFT];
        });
        grabs.look(&seat);
        assert_eq!(seat.steps(), vec!["grab keyboard", "grab mouse", "ungrab mouse"]);
        assert_eq!(grabs.held(), vec!["keyboard"]);
        seat.with(1, |m| m.down_after_grab.clear());
        grabs.look(&seat);
        assert_eq!(grabs.held(), vec!["keyboard", "mouse"]);
    }

    #[test]
    fn devices_plugged_in_are_held_and_those_gone_forgotten() {
        let seat = FakeSeat::default();
        seat.plug(0, keyboard("keyboard"));
        let mut grabs = Grabs::default();
        grabs.look(&seat);
        // A KVM switch back: the mouse comes, the keyboard comes again under its old path.
        seat.unplug(0);
        grabs.look(&seat);
        assert!(grabs.held().is_empty());
        seat.plug(0, keyboard("keyboard again"));
        seat.plug(1, mouse("mouse"));
        grabs.look(&seat);
        assert_eq!(grabs.held(), vec!["keyboard again", "mouse"]);
        // Out and in again between two looks.
        seat.steps();
        seat.replug(1, mouse("mouse again"));
        grabs.look(&seat);
        assert_eq!(grabs.held(), vec!["keyboard again", "mouse again"]);
        assert_eq!(seat.steps(), vec!["grab mouse again"]);
    }

    #[test]
    fn a_device_on_another_seat_or_held_by_another_is_left() {
        let seat = FakeSeat::default();
        seat.plug(0, keyboard("seat1 keyboard"));
        seat.0.lock().unwrap().seats.insert(0xd00, "seat1".to_owned());
        seat.plug(1, keyboard("seat0 keyboard"));
        seat.0.lock().unwrap().seats.insert(0xd01, "seat0".to_owned());
        seat.plug(
            2,
            Fake {
                busy: true,
                ..keyboard("keyd's keyboard")
            },
        );
        // Plugged in a moment ago: udev has not said which seat yet.
        seat.plug(3, keyboard("new keyboard"));
        seat.0.lock().unwrap().unrecorded.insert(0xd03);
        let mut grabs = Grabs::default();
        grabs.look(&seat);
        assert_eq!(grabs.held(), vec!["seat0 keyboard"]);
        // Tried again: someone else may let it go, and udev has said.
        seat.with(2, |k| k.busy = false);
        seat.0.lock().unwrap().unrecorded.clear();
        grabs.look(&seat);
        assert_eq!(grabs.held(), vec!["seat0 keyboard", "keyd's keyboard", "new keyboard"]);
    }

    #[test]
    fn ctrl_p_on_a_keyboard_held_is_the_way_out() {
        let seat = FakeSeat::default();
        seat.plug(0, keyboard("keyboard"));
        seat.plug(1, mouse("mouse"));
        let mut grabs = Grabs::default();
        grabs.look(&seat);
        // P alone, Ctrl alone, Ctrl let go before P: not it.
        seat.with(0, |k| {
            k.typed = vec![(KEY_P, true), (KEY_P, false), (KEY_LEFTCTRL, true), (KEY_LEFTCTRL, false)]
        });
        assert!(!grabs.look(&seat));
        seat.with(0, |k| k.typed = vec![(KEY_P, true)]);
        assert!(!grabs.look(&seat));
        // Ctrl held across looks, then P.
        seat.with(0, |k| k.typed = vec![(KEY_P, false), (KEY_RIGHTCTRL, true)]);
        assert!(!grabs.look(&seat));
        seat.with(0, |k| k.typed = vec![(KEY_P, true)]);
        assert!(grabs.look(&seat));
        // One Ctrl let go, the other still down.
        seat.with(0, |k| {
            k.typed = vec![(KEY_P, false), (KEY_LEFTCTRL, true), (KEY_RIGHTCTRL, false), (KEY_P, true)]
        });
        assert!(grabs.look(&seat));
    }

    #[test]
    fn keys_the_kernel_dropped_are_made_up_for_by_what_is_down() {
        const EV_REL: u16 = 2;
        let dropped = |rest: &[(u16, u16, i32)]| {
            let mut events = vec![(EV_SYN, SYN_DROPPED, 0), (EV_SYN, SYN_REPORT, 0)];
            events.extend_from_slice(rest);
            events
        };
        // Ctrl let go among the events dropped: up, as it is now; the rest up to the report not
        // trusted.
        let events = vec![
            (EV_KEY, KEY_LEFTCTRL, 1),
            (EV_SYN, SYN_REPORT, 0),
            (EV_REL, 0, 5),
            (EV_SYN, SYN_DROPPED, 0),
            (EV_KEY, KEY_A, 1),
            (EV_SYN, SYN_REPORT, 0),
            (EV_KEY, KEY_P, 1),
            (EV_KEY, KEY_P, 2),
            (EV_SYN, SYN_REPORT, 0),
        ];
        assert_eq!(
            key_changes(events, || vec![KEY_P]),
            vec![
                (KEY_LEFTCTRL, true),
                (KEY_LEFTCTRL, false),
                (KEY_RIGHTCTRL, false),
                (KEY_P, false),
                (KEY_P, true)
            ]
        );
        // Ctrl down since before, let go after the drop: down there, as what came after says,
        // though up now; so P meanwhile is Ctrl+P.
        assert_eq!(
            key_changes(
                dropped(&[(EV_KEY, KEY_P, 1), (EV_KEY, KEY_P, 0), (EV_KEY, KEY_LEFTCTRL, 0)]),
                Vec::new
            ),
            vec![
                (KEY_LEFTCTRL, true),
                (KEY_RIGHTCTRL, false),
                (KEY_P, false),
                (KEY_P, true),
                (KEY_P, false),
                (KEY_LEFTCTRL, false)
            ]
        );
        // Ctrl pressed after the drop: up there, though down now; so P before it is not.
        assert_eq!(
            key_changes(
                dropped(&[(EV_KEY, KEY_P, 1), (EV_KEY, KEY_P, 0), (EV_KEY, KEY_LEFTCTRL, 1)]),
                || vec![KEY_LEFTCTRL]
            ),
            vec![
                (KEY_LEFTCTRL, false),
                (KEY_RIGHTCTRL, false),
                (KEY_P, false),
                (KEY_P, true),
                (KEY_P, false),
                (KEY_LEFTCTRL, true)
            ]
        );
        // Ctrl+P among the events dropped, both still down: seen.
        assert_eq!(
            key_changes(dropped(&[]), || vec![KEY_LEFTCTRL, KEY_P]),
            vec![(KEY_LEFTCTRL, true), (KEY_RIGHTCTRL, false), (KEY_P, true)]
        );
        // Dropped, the report not read yet.
        assert_eq!(
            key_changes(vec![(EV_SYN, SYN_DROPPED, 0)], || vec![KEY_RIGHTCTRL]),
            vec![(KEY_LEFTCTRL, false), (KEY_RIGHTCTRL, true), (KEY_P, false)]
        );
    }

    #[test]
    fn our_own_devices_are_never_held() {
        assert_eq!(not_held("RustDesk UInput Keyboard", &[KEY_A], false), Some("ours"));
        assert_eq!(not_held("mouce-library-fake-mouse", &[BTN_LEFT], true), Some("ours"));
        assert_eq!(not_held("rd-test keyboard", &[KEY_A], false), None);
    }

    #[test]
    fn the_lock_holds_while_told_and_tells_of_ctrl_p() {
        let seat = FakeSeat::default();
        seat.plug(0, keyboard("keyboard"));
        let lock = Lock::start(seat.clone()).unwrap();
        let wait_for = |want: &[&str]| {
            for _ in 0..100 {
                if seat.0.lock().unwrap().steps == want {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            false
        };
        lock.set(true);
        assert!(wait_for(&["grab keyboard"]));
        lock.set(false);
        assert!(wait_for(&["grab keyboard", "ungrab keyboard"]));
        seat.steps();
        // Ctrl+P is told.
        lock.set(true);
        assert!(wait_for(&["grab keyboard"]));
        seat.with(0, |k| k.typed = vec![(KEY_LEFTCTRL, true), (KEY_P, true)]);
        let mut escaped = false;
        for _ in 0..100 {
            if lock.escaped() {
                escaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(escaped);
        assert!(!lock.escaped());
        lock.set(false);
    }

    #[test]
    fn udev_says_which_seat() {
        assert_eq!(
            udev_seat("I:123\nE:ID_INPUT=1\nE:ID_SEAT=seat1\nG:seat\n").as_deref(),
            Some("seat1")
        );
        assert_eq!(udev_seat("E:ID_INPUT=1\n"), None);
        // /dev/input/event13 is 13:77.
        assert_eq!(major_minor(0xd4d), (13, 77));
        assert_eq!(major_minor((13 << 8) | (300 & 0xff) | ((300 & !0xff) << 12)), (13, 300));
    }
}
