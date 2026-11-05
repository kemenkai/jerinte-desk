//! What GNOME Mutter's `DisplayConfig` says about the monitors, what the desktop shows of them, and
//! how the capturers of a display share its stream. Free of D-Bus and GStreamer, so it builds, and
//! is tested, on macOS too: `wayland::mutter` reads the state from Mutter, `wayland::mutter_capture`
//! records with it.

use hbb_common::{bail, ResultType};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Condvar, Mutex,
    },
    time::{Duration, Instant},
};

/// The product of a virtual monitor of Mutter's remote desktop (`RecordVirtual`).
pub const VIRTUAL_PRODUCT: &str = "Virtual remote monitor";

/// A monitor as `DisplayConfig.GetCurrentState` lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    pub connector: String,
    pub vendor: String,
    pub product: String,
    pub serial: String,
    pub modes: Vec<MonitorMode>,
    pub builtin: bool,
    /// Whether it is underscanned, where it can be.
    pub underscanning: Option<bool>,
    /// Its colour mode (HDR or not), where Mutter tells (from 47).
    pub color_mode: Option<u32>,
    /// Leased to a program (a VR headset, say), out of the desktop.
    pub for_lease: bool,
}

impl Monitor {
    pub fn current_mode(&self) -> Option<&MonitorMode> {
        self.modes.iter().find(|m| m.current)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MonitorMode {
    pub id: String,
    pub width: i32,
    pub height: i32,
    pub refresh: f64,
    pub current: bool,
    pub preferred: bool,
}

/// A logical monitor: where its monitors sit in the desktop.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalMonitor {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    /// 0 normal, 1 90°, 2 180°, 3 270°, 4–7 the same flipped.
    pub transform: u32,
    pub primary: bool,
    pub connectors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DisplayState {
    /// Passed back with a configuration, which Mutter refuses once the monitors changed.
    pub serial: u32,
    pub monitors: Vec<Monitor>,
    pub logical: Vec<LogicalMonitor>,
    /// The desktop is laid out in logical pixels (a monitor of scale 2 covers half its mode), not
    /// in the pixels of the modes. GNOME's default is the latter, unless fractional scaling is on.
    pub layout_logical: bool,
    /// A configuration may say which of the two layouts it is in.
    pub layout_mode_changeable: bool,
}

/// A logical monitor to configure: each monitor of it with the id of its mode.
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalConfig {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    pub monitors: Vec<(String, String)>,
}

/// A monitor shown on the desktop, as capture and the controlling side see it.
#[derive(Debug, Clone, PartialEq)]
pub struct Shown {
    pub connector: String,
    /// Its top left corner in the desktop.
    pub position: (i32, i32),
    /// The size of its frames: its mode, turned as the monitor is.
    pub physical: (usize, usize),
    /// What it covers of the desktop.
    pub logical: (usize, usize),
    pub primary: bool,
}

impl DisplayState {
    pub fn monitor(&self, connector: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.connector == connector)
    }

    pub fn logical_of(&self, connector: &str) -> Option<&LogicalMonitor> {
        self.logical
            .iter()
            .find(|l| l.connectors.iter().any(|c| c == connector))
    }

    /// The monitors the desktop shows, in the order Mutter lists them, which a monitor moved
    /// about the desktop keeps. One turned off is in no logical monitor and not shown; of
    /// monitors mirroring each other only the first is.
    pub fn shown(&self) -> Vec<Shown> {
        self.monitors
            .iter()
            .filter_map(|m| {
                let l = self.logical_of(&m.connector)?;
                let first = self
                    .monitors
                    .iter()
                    .find(|o| l.connectors.contains(&o.connector))?;
                if first.connector != m.connector {
                    return None;
                }
                let mode = m.current_mode()?;
                let (w, h) = if l.transform % 2 == 1 {
                    (mode.height, mode.width)
                } else {
                    (mode.width, mode.height)
                };
                // As Mutter sizes the logical monitor and then the frames it records of it.
                let (logical, physical) = if self.layout_logical && l.scale > 0.0 {
                    let logical = (
                        (w as f64 / l.scale).round(),
                        (h as f64 / l.scale).round(),
                    );
                    (
                        logical,
                        ((logical.0 * l.scale).round(), (logical.1 * l.scale).round()),
                    )
                } else {
                    ((w as f64, h as f64), (w as f64, h as f64))
                };
                Some(Shown {
                    connector: m.connector.clone(),
                    position: (l.x, l.y),
                    physical: (physical.0.max(0.0) as usize, physical.1.max(0.0) as usize),
                    logical: (logical.0.max(0.0) as usize, logical.1.max(0.0) as usize),
                    primary: l.primary,
                })
            })
            .collect()
    }

    /// The connectors of virtual monitors of Mutter's remote desktop, of anyone's.
    pub fn virtual_connectors(&self) -> Vec<&str> {
        self.monitors
            .iter()
            .filter(|m| m.product == VIRTUAL_PRODUCT)
            .map(|m| m.connector.as_str())
            .collect()
    }
}

/// A stream Mutter records, anyone's, as its `Parameters` say (`*_set_parameters`, 42 to 48): a
/// monitor's place and size, an area's or a window's size alone, nothing for a virtual monitor.
/// Written once, as it starts: a monitor moved or turned off closes the streams of it.
#[derive(Debug, Clone, PartialEq)]
pub struct CastStream {
    pub path: String,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

impl CastStream {
    /// Whether it records what the desktop shows, a monitor, an area or a window, and not a
    /// virtual monitor.
    pub fn of_the_desktop(&self) -> bool {
        self.position.is_some() || self.size.is_some()
    }
}

/// The child nodes D-Bus introspection data names, `<node name="u3"/>`, in order.
pub fn child_nodes(xml: &str) -> Vec<String> {
    const OPEN: &str = "<node name=\"";
    let mut nodes = Vec::new();
    let mut rest = xml;
    while let Some(at) = rest.find(OPEN) {
        rest = &rest[at + OPEN.len()..];
        let Some(end) = rest.find('"') else {
            break;
        };
        nodes.push(rest[..end].to_owned());
        rest = &rest[end..];
    }
    nodes
}

/// How the physical monitors are recorded.
#[derive(Debug, Clone, PartialEq)]
pub enum Recording {
    /// Not at all.
    Not,
    /// These, by connector.
    Live(Vec<String>),
    /// The session is of no more use, and why.
    Ended(Ended),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ended {
    /// A monitor of it moved, was resized or went: what Mutter closes a stream for.
    Moved,
    /// Mutter went, and every session of it.
    Restarted,
    /// A stream of it failed, or sent frames of another size.
    Failed,
    /// Closed, with none of the above: by GNOME Shell, the screen locked or the sharing stopped
    /// in its indicator.
    Closed,
}

/// How long a capturer waits for the one before it on a stream to let go.
pub const LET_GO: Duration = Duration::from_secs(5);

/// A capturer found its stream taken: the one before it did not let go in time. Tried again soon;
/// nothing is wrong with the display.
#[derive(Debug)]
pub struct Busy(pub &'static str);

impl std::fmt::Display for Busy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for Busy {}

impl Busy {
    /// `e` boxed as a capturable fails, a `Busy` still telling itself: anyhow's own boxing hides
    /// the type.
    pub fn boxed(e: hbb_common::anyhow::Error) -> Box<dyn std::error::Error> {
        match e.downcast::<Busy>() {
            Ok(busy) => Box::new(busy),
            Err(e) => e.into(),
        }
    }
}

/// A display's stream as its capturers take turns at it. A stream takes one consumer at a time
/// (a pipeline started while another is on it never gets a frame, not even once that one is
/// gone), so a capturer coming asks the one there to let go and waits for it: the display moved
/// from one capturer to another, its index having changed. Each builds a pipeline of its own,
/// settling the size wanted now. Whoever changes that size, or finds the stream gone, moves it on
/// to a new generation: a capturer of an older one then fails at its next frame, to be built
/// again. Mutter keeps a virtual monitor with no consumer as it is, and resizes it only once a
/// consumer settles another size (`ensure_virtual_monitor`), so with no capturer about a resize
/// settles it itself, holding new capturers off meanwhile.
pub struct Shared {
    node: u32,
    generation: AtomicU64,
    /// Raised by each capturer finding another on the stream: one built before is to let go.
    asked: AtomicU64,
    inner: Mutex<Inner>,
    /// Told whenever a capturer stops, or the stream changes or goes.
    changed: Condvar,
}

struct Inner {
    size: Option<(i32, i32)>,
    /// Capturers with a pipeline of it.
    capturing: usize,
    /// Gone for good.
    retired: bool,
}

impl Shared {
    pub fn new(node: u32, size: Option<(i32, i32)>) -> Self {
        Self {
            node,
            generation: AtomicU64::new(0),
            asked: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                size,
                capturing: 0,
                retired: false,
            }),
            changed: Condvar::new(),
        }
    }

    /// The PipeWire node of the stream.
    pub fn node(&self) -> u32 {
        self.node
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The size pipelines settle now, where one is asked for.
    pub fn size(&self) -> Option<(i32, i32)> {
        self.inner.lock().unwrap().size
    }

    /// A capturer of `generation` starts, with the pipeline `build` makes for the node and the
    /// size wanted now, once the one there, if any, let go: asked to, waited for up to `wait`,
    /// else `Busy`. Fails as for a display changed should the stream change or go meanwhile,
    /// `gone` telling of what this does not see. Answers the pipeline and its turn, which
    /// `asked_to_let_go` takes. It calls `stopped` once that pipeline is stopped.
    pub fn start<R>(
        &self,
        generation: u64,
        wait: Duration,
        gone: impl Fn() -> bool,
        build: impl FnOnce(u32, Option<(i32, i32)>) -> ResultType<R>,
    ) -> ResultType<(R, u64)> {
        let changed = |inner: &Inner| inner.retired || self.generation() != generation || gone();
        let mut inner = self.inner.lock().unwrap();
        let until = Instant::now() + wait;
        while inner.capturing > 0 && !changed(&inner) {
            // Asked at each wake: the one there now may have got there since, after another
            // that let go.
            self.asked.fetch_add(1, Ordering::SeqCst);
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            inner = self.changed.wait_timeout(inner, left).unwrap().0;
        }
        if changed(&inner) {
            bail!("The display changed");
        }
        if inner.capturing > 0 {
            return Err(hbb_common::anyhow::Error::new(Busy(
                "The display is still captured by another capturer",
            )));
        }
        let recorder = build(self.node, inner.size)?;
        inner.capturing += 1;
        Ok((recorder, self.asked.load(Ordering::SeqCst)))
    }

    /// Whether the capturer that started at `turn` is to let go: another came since.
    pub fn asked_to_let_go(&self, turn: u64) -> bool {
        self.asked.load(Ordering::SeqCst) != turn
    }

    pub fn stopped(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.capturing = inner.capturing.saturating_sub(1);
        self.changed.notify_all();
    }

    /// Moves on to a new generation of `size`. With no capturer about, `settle` runs meanwhile,
    /// new capturers held off: it settles the size with a pipeline of its own, and stops it.
    /// Answers whether a capturer was there to settle it instead, once built again.
    pub fn resize(
        &self,
        size: Option<(i32, i32)>,
        settle: impl FnOnce(u32, Option<(i32, i32)>) -> ResultType<()>,
    ) -> ResultType<bool> {
        let mut inner = self.inner.lock().unwrap();
        if inner.retired {
            bail!("The display is gone");
        }
        inner.size = size;
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_all();
        if inner.capturing > 0 {
            return Ok(true);
        }
        settle(self.node, size)?;
        Ok(false)
    }

    /// The stream is gone: its capturers fail at their next frame, and none starts again.
    pub fn retire(&self) {
        self.inner.lock().unwrap().retired = true;
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{atomic::AtomicBool, mpsc, Arc},
        thread,
    };

    // A capturer starting as `Held::lease` starts one, with nothing gone.
    fn go<R>(
        shared: &Shared,
        generation: u64,
        build: impl FnOnce(u32, Option<(i32, i32)>) -> ResultType<R>,
    ) -> ResultType<R> {
        shared
            .start(generation, LET_GO, || false, build)
            .map(|(r, _)| r)
    }

    fn monitor(connector: &str, product: &str, modes: &[(i32, i32, bool)]) -> Monitor {
        Monitor {
            connector: connector.to_owned(),
            vendor: "V".to_owned(),
            product: product.to_owned(),
            serial: "1".to_owned(),
            modes: modes
                .iter()
                .enumerate()
                .map(|(i, &(width, height, current))| MonitorMode {
                    id: format!("{}x{}@{}", width, height, i),
                    width,
                    height,
                    refresh: 60.0,
                    current,
                    preferred: i == 0,
                })
                .collect(),
            builtin: false,
            underscanning: None,
            color_mode: None,
            for_lease: false,
        }
    }

    fn logical(
        x: i32,
        y: i32,
        scale: f64,
        transform: u32,
        primary: bool,
        c: &[&str],
    ) -> LogicalMonitor {
        LogicalMonitor {
            x,
            y,
            scale,
            transform,
            primary,
            connectors: c.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn shown_in_mutters_order_with_turned_sizes() {
        let state = DisplayState {
            serial: 1,
            monitors: vec![
                monitor("HDMI-1", "Dell", &[(1920, 1080, true)]),
                monitor("DP-1", "LG", &[(2560, 1440, true)]),
                monitor("DP-2", "LG", &[(1280, 1024, true)]),
                monitor("Meta-0", VIRTUAL_PRODUCT, &[(1600, 900, true)]),
            ],
            logical: vec![
                // DP-1 on the left, turned: its frames are 1440x2560.
                logical(0, 0, 1.0, 1, false, &["DP-1"]),
                logical(1440, 0, 1.0, 0, true, &["HDMI-1"]),
                logical(3360, 0, 1.0, 0, false, &["Meta-0"]),
            ],
            layout_logical: false,
            layout_mode_changeable: false,
        };
        let shown = state.shown();
        let names: Vec<&str> = shown.iter().map(|s| s.connector.as_str()).collect();
        // Mutter's order, not left to right; DP-2 is off.
        assert_eq!(names, ["HDMI-1", "DP-1", "Meta-0"]);
        assert_eq!(shown[0].position, (1440, 0));
        assert!(shown[0].primary);
        assert_eq!(shown[1].physical, (1440, 2560));
        assert_eq!(shown[1].logical, (1440, 2560));
        assert_eq!(state.virtual_connectors(), ["Meta-0"]);
    }

    #[test]
    fn a_scaled_monitor_covers_less_of_a_logical_desktop_only() {
        let mut state = DisplayState {
            serial: 1,
            monitors: vec![monitor("eDP-1", "BOE", &[(2880, 1800, true)])],
            logical: vec![logical(0, 0, 2.0, 0, true, &["eDP-1"])],
            layout_logical: false,
            layout_mode_changeable: false,
        };
        // GNOME's default: laid out in the pixels of the mode, the scale is the clients' to apply.
        assert_eq!(state.shown()[0].logical, (2880, 1800));
        assert_eq!(state.shown()[0].physical, (2880, 1800));
        state.layout_logical = true;
        assert_eq!(state.shown()[0].logical, (1440, 900));
        assert_eq!(state.shown()[0].physical, (2880, 1800));
        // Fractional, as Mutter would round it.
        state.logical[0].scale = 1.75;
        assert_eq!(state.shown()[0].logical, (1646, 1029));
        assert_eq!(state.shown()[0].physical, (2881, 1801));
        state.logical[0].scale = 1.5;
        assert_eq!(state.shown()[0].logical, (1920, 1200));
        assert_eq!(state.shown()[0].physical, (2880, 1800));
    }

    #[test]
    fn mirrored_monitors_show_once() {
        let state = DisplayState {
            serial: 1,
            monitors: vec![
                monitor("HDMI-1", "Dell", &[(1920, 1080, true)]),
                monitor("DP-1", "LG", &[(1920, 1080, true)]),
            ],
            logical: vec![logical(0, 0, 1.0, 0, true, &["DP-1", "HDMI-1"])],
            layout_logical: false,
            layout_mode_changeable: false,
        };
        let shown = state.shown();
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].connector, "HDMI-1");
    }

    #[test]
    fn a_monitor_without_a_current_mode_is_not_shown() {
        let state = DisplayState {
            serial: 1,
            monitors: vec![monitor("HDMI-1", "Dell", &[(1920, 1080, false)])],
            logical: vec![logical(0, 0, 1.0, 0, true, &["HDMI-1"])],
            layout_logical: false,
            layout_mode_changeable: false,
        };
        assert!(state.shown().is_empty());
    }

    #[test]
    fn a_capturer_of_an_old_generation_does_not_start() {
        let shared = Shared::new(35, Some((1600, 900)));
        let gen = shared.generation();
        let built = go(&shared, gen, |node, size| Ok((node, size))).unwrap();
        assert_eq!(built, (35, Some((1600, 900))));
        // Resized under it: it was there to settle the new size, once built again.
        assert!(shared.resize(Some((2560, 1440)), |_, _| panic!("settled twice")).unwrap());
        shared.stopped();
        assert!(go(&shared, gen, |_, _| Ok(())).is_err());
        let built = go(&shared, shared.generation(), |_, size| Ok(size)).unwrap();
        assert_eq!(built, Some((2560, 1440)));
    }

    #[test]
    fn with_no_capturer_a_resize_settles_the_size_itself() {
        let shared = Shared::new(35, Some((1600, 900)));
        let mut settled = None;
        let capturer_there = shared
            .resize(Some((1280, 720)), |node, size| {
                settled = Some((node, size));
                Ok(())
            })
            .unwrap();
        assert!(!capturer_there);
        assert_eq!(settled, Some((35, Some((1280, 720)))));
        // A capturer that came and went counts no more.
        let gen = shared.generation();
        go(&shared, gen, |_, _| Ok(())).unwrap();
        shared.stopped();
        assert!(!shared.resize(Some((800, 600)), |_, _| Ok(())).unwrap());
    }

    #[test]
    fn a_retired_stream_starts_no_capturer() {
        let shared = Shared::new(35, None);
        let gen = shared.generation();
        shared.retire();
        assert!(go(&shared, gen, |_, _| Ok(())).is_err());
        assert!(go(&shared, shared.generation(), |_, _| Ok(())).is_err());
        assert!(shared.resize(None, |_, _| Ok(())).is_err());
    }

    // As GDBus answers for /org/gnome/Mutter/ScreenCast/Stream, whose own node has no interface.
    #[test]
    fn child_nodes_are_read_from_introspection() {
        let xml = "<!DOCTYPE node PUBLIC \"-//freedesktop//DTD D-BUS Object Introspection 1.0//EN\"\n\
                   \"http://www.freedesktop.org/standards/dbus/1.0/introspect.dtd\">\n\
                   <node>\n  <node name=\"u1\"/>\n  <node name=\"u12\"/>\n</node>\n";
        assert_eq!(child_nodes(xml), vec!["u1", "u12"]);
        assert!(child_nodes("<node>\n</node>\n").is_empty());
        assert!(child_nodes("<node name=\"u1").is_empty());
    }

    #[test]
    fn a_failed_build_is_no_capturer() {
        let shared = Shared::new(35, None);
        let gen = shared.generation();
        assert!(go::<()>(&shared, gen, |_, _| hbb_common::bail!("no pipewiresrc")).is_err());
        let mut settled = false;
        shared
            .resize(None, |_, _| {
                settled = true;
                Ok(())
            })
            .unwrap();
        assert!(settled);
    }

    #[test]
    fn a_capturer_waits_for_the_one_on_the_stream_to_let_go() {
        let shared = Arc::new(Shared::new(35, Some((1600, 900))));
        let gen = shared.generation();
        let (_, first) = shared.start(gen, LET_GO, || false, |_, _| Ok(())).unwrap();
        assert!(!shared.asked_to_let_go(first));
        let let_go = Arc::new(AtomicBool::new(false));
        let second = {
            let (shared, let_go) = (shared.clone(), let_go.clone());
            thread::spawn(move || {
                shared.start(gen, LET_GO, || false, |_, _| {
                    // Built only once the first one stopped: a stream takes one consumer.
                    assert!(let_go.load(Ordering::SeqCst));
                    Ok(())
                })
            })
        };
        let asked = Instant::now();
        while !shared.asked_to_let_go(first) {
            assert!(asked.elapsed() < Duration::from_secs(1), "never asked to let go");
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(50));
        assert!(!second.is_finished());
        let_go.store(true, Ordering::SeqCst);
        let stopped = Instant::now();
        shared.stopped();
        let (_, turn) = second.join().unwrap().unwrap();
        assert!(stopped.elapsed() < Duration::from_secs(1));
        // The one built since is not asked to go by the request it made itself.
        assert!(!shared.asked_to_let_go(turn));
        assert!(shared.asked_to_let_go(first));
    }

    // Two come while one is on the stream: the one that gets it next is asked to let go by the
    // other, still waiting, which then gets it in turn.
    #[test]
    fn of_two_waiting_each_gets_the_stream_in_turn() {
        let shared = Arc::new(Shared::new(35, None));
        let gen = shared.generation();
        shared.start(gen, LET_GO, || false, |_, _| Ok(())).unwrap();
        let (tx, rx) = mpsc::channel();
        for _ in 0..2 {
            let (shared, tx) = (shared.clone(), tx.clone());
            thread::spawn(move || {
                tx.send(shared.start(gen, Duration::from_secs(3), || false, |_, _| Ok(())))
                    .unwrap()
            });
            thread::sleep(Duration::from_millis(50));
        }
        shared.stopped();
        let (_, first) = rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        let asked = Instant::now();
        while !shared.asked_to_let_go(first) {
            assert!(asked.elapsed() < Duration::from_secs(1), "the other did not ask again");
            thread::sleep(Duration::from_millis(5));
        }
        shared.stopped();
        let (_, second) = rx.recv_timeout(Duration::from_secs(1)).unwrap().unwrap();
        assert!(!shared.asked_to_let_go(second));
    }

    #[test]
    fn a_stream_gone_while_waiting_is_not_started() {
        let shared = Arc::new(Shared::new(35, None));
        let gen = shared.generation();
        shared.start(gen, LET_GO, || false, |_, _| Ok(())).unwrap();
        let gone = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (shared, gone) = (shared.clone(), gone.clone());
            thread::spawn(move || {
                shared.start(
                    gen,
                    LET_GO,
                    move || gone.load(Ordering::SeqCst),
                    |_, _| -> ResultType<()> { panic!("started on a stream gone") },
                )
            })
        };
        thread::sleep(Duration::from_millis(50));
        gone.store(true, Ordering::SeqCst);
        let woken = Instant::now();
        shared.stopped();
        let e = waiter.join().unwrap().unwrap_err();
        assert!(woken.elapsed() < Duration::from_secs(1));
        assert!(!e.is::<Busy>());
    }

    #[test]
    fn a_capturer_whose_turn_does_not_come_is_busy() {
        let shared = Shared::new(35, None);
        let gen = shared.generation();
        shared.start(gen, LET_GO, || false, |_, _| Ok(())).unwrap();
        let waited = Instant::now();
        let e = shared
            .start(gen, Duration::from_millis(100), || false, |_, _| -> ResultType<()> {
                panic!("built while another is on the stream")
            })
            .unwrap_err();
        assert!(waited.elapsed() >= Duration::from_millis(100));
        assert!(e.downcast_ref::<Busy>().is_some());
        // The one on the stream still counts: the next to come waits too.
        assert!(shared
            .start(gen, Duration::ZERO, || false, |_, _| Ok(()))
            .unwrap_err()
            .is::<Busy>());
    }

    #[test]
    fn waiting_ends_once_the_stream_changes_or_goes() {
        for change in ["resize", "retire"] {
            let shared = Arc::new(Shared::new(35, None));
            let gen = shared.generation();
            shared.start(gen, LET_GO, || false, |_, _| Ok(())).unwrap();
            let second = {
                let shared = shared.clone();
                thread::spawn(move || shared.start(gen, LET_GO, || false, |_, _| Ok(())))
            };
            thread::sleep(Duration::from_millis(50));
            let changed = Instant::now();
            match change {
                "resize" => assert!(shared.resize(Some((800, 600)), |_, _| Ok(())).unwrap()),
                _ => shared.retire(),
            }
            let e = second.join().unwrap().unwrap_err();
            assert!(changed.elapsed() < Duration::from_secs(1), "{}", change);
            assert!(!e.is::<Busy>(), "{}", change);
        }
        // Gone already: no waiting at all, nor the one there asked to let go for nothing.
        let shared = Shared::new(35, None);
        let gen = shared.generation();
        let (_, there) = shared.start(gen, LET_GO, || false, |_, _| Ok(())).unwrap();
        let waited = Instant::now();
        assert!(shared.start(gen, LET_GO, || true, |_, _| Ok(())).is_err());
        assert!(waited.elapsed() < Duration::from_secs(1));
        assert!(!shared.asked_to_let_go(there));
    }

    // As `Held::lease` fails into the capturable's `Box<dyn Error>`, for `TimedOut`.
    #[test]
    fn busy_is_still_told_in_a_boxed_error() {
        fn lease(busy: bool) -> ResultType<()> {
            if busy {
                Err(hbb_common::anyhow::Error::new(Busy("taken")))
            } else {
                bail!("gone")
            }
        }
        fn recorder(busy: bool) -> Result<(), Box<dyn std::error::Error>> {
            lease(busy).map_err(Busy::boxed)?;
            Ok(())
        }
        assert!(recorder(true).unwrap_err().downcast_ref::<Busy>().is_some());
        let other = recorder(false).unwrap_err();
        assert!(other.downcast_ref::<Busy>().is_none());
        assert_eq!(other.to_string(), "gone");
    }
}
