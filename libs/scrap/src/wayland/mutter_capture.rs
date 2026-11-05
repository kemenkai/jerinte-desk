//! Capture through Mutter's own sessions (`mutter`), without the portal: the physical monitors
//! recorded in one session, and each virtual monitor of ours in a session of its own (`mutter` says
//! why). Each capturer builds a pipeline of its own for its display's stream, and stops it when
//! done (`Shared` says how they share it).
//!
//! How a stream ends depends on Mutter. Up to 44, a recorded monitor moved, resized or turned off
//! closes its stream and the whole session with it, which says `Closed`. From 45 the session of a
//! remote desktop stays and only the stream goes, which nothing on D-Bus says: so the physical
//! monitors are checked against where they were when recorded, as Mutter does
//! (`meta_screen_cast_monitor_stream`'s `update_monitor`), whenever its monitors changed, and a
//! pipeline that saw its stream end says so too.

use std::{
    error::Error,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use dbus::blocking::SyncConnection;
use hbb_common::{anyhow, bail, ResultType};
use tracing::{info, warn};

use super::{
    capturable::{PixelProvider, Recorder},
    mutter::{self, DisplayState, LogicalConfig, Session, Shown, Source, Watch},
    pipewire::{PipeWireCapturable, PipeWireRecorder},
};
pub use crate::mutter_model::{CastStream, Ended, Recording};
use crate::mutter_model::{Shared, LET_GO};

/// How long the first frame of a stream may take to come: it says the size of a physical
/// monitor's frames, and that a virtual monitor of the size asked for is made.
const FIRST_FRAME: Duration = Duration::from_secs(3);
/// How long a capturer's pipeline may go without a first frame before it is built again: longer
/// than `FIRST_FRAME`, a virtual monitor resized for it coming first.
const NO_FRAME: Duration = Duration::from_secs(5);

/// Raised on every change to what is recorded, for whoever lists the displays.
static CHANGES: AtomicU64 = AtomicU64::new(0);
/// Raised whenever Mutter's remote desktop changes owner: Mutter restarted, its sessions gone.
static EPOCH: AtomicU64 = AtomicU64::new(0);
/// The connection `state` asks on, made again should it fail.
static ASKING: Mutex<Option<Arc<SyncConnection>>> = Mutex::new(None);
/// Mutter's `MonitorsChanged` so far, and when the last came.
static MONITORS_CHANGED: Mutex<(u64, Option<Instant>)> = Mutex::new((0, None));
/// Until when a session closed is taken to be closed by a change of ours (`expect_relayout`).
static OURS_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);
/// How long after a change of ours Mutter may close sessions for it.
const OURS_FOR: Duration = Duration::from_secs(10);
static PHYSICAL: Mutex<Option<Physical>> = Mutex::new(None);
/// Ours, by connector, in the order they were shown.
static VIRTUAL: Mutex<Vec<(String, Recorded)>> = Mutex::new(Vec::new());

/// What a capturer fails with once its display changed under it: built again, it captures the
/// display as it is now.
#[derive(Debug)]
pub struct Changed;

impl std::fmt::Display for Changed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "The display changed")
    }
}

impl Error for Changed {}

/// A stream recorded, and how to tell it is no more.
#[derive(Clone)]
struct Recorded {
    shared: Arc<Shared>,
    watch: Watch,
    /// Raised once its session is of no more use, for all its streams: one ended, or sent frames
    /// of another size, or a monitor of it moved.
    broken: Arc<AtomicBool>,
    /// `EPOCH` when recorded.
    epoch: u64,
}

impl Recorded {
    fn new(shared: Shared, watch: Watch, broken: Arc<AtomicBool>) -> Self {
        Self {
            shared: Arc::new(shared),
            watch,
            broken,
            epoch: EPOCH.load(Ordering::SeqCst),
        }
    }

    fn gone(&self) -> bool {
        self.end().is_some()
    }

    // Whether it is of no more use, by what is known without asking D-Bus (which may run a signal
    // handler): safe under its stream's lock.
    fn known_gone(&self) -> bool {
        self.epoch != EPOCH.load(Ordering::SeqCst)
            || self.broken.load(Ordering::SeqCst)
            || self.watch.closed_seen()
    }

    fn end(&self) -> Option<Ended> {
        if self.epoch != EPOCH.load(Ordering::SeqCst) {
            Some(Ended::Restarted)
        } else if self.broken.load(Ordering::SeqCst) {
            Some(Ended::Failed)
        } else if self.watch.is_closed() {
            Some(Ended::Closed)
        } else {
            None
        }
    }
}

/// A display's stream as a capturer reaches it, of the generation it was listed at.
#[derive(Clone)]
pub(crate) struct Held {
    recorded: Recorded,
    generation: u64,
    /// The size of its frames.
    size: (usize, usize),
}

impl Held {
    pub(crate) fn lease(&self) -> ResultType<Lease> {
        if self.recorded.gone() {
            return Err(anyhow::Error::new(Changed));
        }
        let recorded = &self.recorded;
        let (recorder, turn) = recorded.shared.start(
            self.generation,
            LET_GO,
            || recorded.known_gone(),
            |node, size| PipeWireRecorder::build(None, node as u64, size),
        )?;
        Ok(Lease {
            held: self.clone(),
            recorder: Some(recorder),
            turn,
            built: Instant::now(),
            played: false,
        })
    }
}

/// A capturer's pipeline of a display's stream.
pub(crate) struct Lease {
    held: Held,
    recorder: Option<PipeWireRecorder>,
    /// When it started, for `Shared::asked_to_let_go`.
    turn: u64,
    built: Instant,
    /// A frame reached its pipeline.
    played: bool,
}

impl Lease {
    // The pipeline stopped, and the stream told: before another may be built.
    fn stop(&mut self) {
        if let Some(recorder) = self.recorder.take() {
            drop(recorder);
            self.held.recorded.shared.stopped();
        }
    }
}

// Whenever a capturer fails as for a display changed, its pipeline is stopped there and then: the
// stream is free for the next one at once, not once the video service is done with this one.
impl Recorder for Lease {
    fn capture(&mut self, timeout_ms: u64) -> Result<PixelProvider<'_>, Box<dyn Error>> {
        let recorded = &self.held.recorded;
        if recorded.shared.generation() != self.held.generation || recorded.gone() {
            self.stop();
            return Err(Box::new(Changed));
        }
        if recorded.shared.asked_to_let_go(self.turn) {
            // Another capturer has the display, its index having changed.
            hbb_common::throttled_log!(
                Duration::from_secs(10),
                info,
                "A Mutter stream goes to another capturer"
            );
            self.stop();
            return Err(Box::new(Changed));
        }
        // Asked of the pipeline apart: the one a frame comes from is borrowed for as long as the
        // frame, which bars stopping on the way.
        let (ended, played) = {
            let recorder = self.recorder.as_mut().ok_or("No pipeline")?;
            (recorder.ended(), self.played || recorder.playing())
        };
        if ended {
            warn!("A Mutter stream ended");
            recorded.broken.store(true, Ordering::SeqCst);
            self.stop();
            return Err(Box::new(Changed));
        }
        if played {
            self.played = true;
        } else if self.built.elapsed() > NO_FRAME {
            // Started with another consumer on the stream, say, which it never gets a frame
            // after: built again, not left capturing nothing for good.
            hbb_common::throttled_log!(
                Duration::from_secs(60),
                warn,
                "A Mutter stream sent its capturer no frame: built again"
            );
            self.stop();
            return Err(Box::new(Changed));
        }
        let recorder = self.recorder.as_mut().ok_or("No pipeline")?;
        let frame = recorder.capture(timeout_ms)?;
        let size = frame.size();
        if size != (0, 0) && size != self.held.size {
            // Stopped as the capturer goes, the frame still borrowing the pipeline here: the
            // session is recorded again anyway.
            warn!(
                "Mutter stream frames of {:?}, not {:?}: recording again",
                size, self.held.size
            );
            recorded.broken.store(true, Ordering::SeqCst);
            return Err(Box::new(Changed));
        }
        Ok(frame)
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.stop();
    }
}

// The physical monitors' session.
struct Physical {
    _session: Session,
    streams: Vec<PhysicalStream>,
}

// Its streams go with it: a capturer waiting for a turn at one is told at once.
impl Drop for Physical {
    fn drop(&mut self) {
        for stream in &self.streams {
            stream.recorded.shared.retire();
        }
    }
}

struct PhysicalStream {
    connector: String,
    recorded: Recorded,
    /// The size of its frames.
    size: (usize, usize),
    /// Where its monitor was in the desktop when recorded, and how much of it it covered.
    at: ((i32, i32), (usize, usize)),
}

/// Records the physical monitors `state` shows, all but `ours` (by connector), in a session of
/// their own, in place of any recorded before.
pub fn record_physical(state: &DisplayState, ours: &[String]) -> ResultType<()> {
    stop_physical();
    let shown: Vec<Shown> = state
        .shown()
        .into_iter()
        .filter(|s| !ours.contains(&s.connector))
        .collect();
    if shown.is_empty() {
        bail!("No monitor to record");
    }
    let mut session = Session::open()?;
    let sources: Vec<Source> = shown.iter().map(|s| Source::Monitor(&s.connector)).collect();
    let streams = session.record(&sources)?;
    let watch = session.watch();
    let broken = Arc::new(AtomicBool::new(false));
    let mut recorded = Vec::with_capacity(shown.len());
    for (shown, stream) in shown.iter().zip(streams) {
        // A first frame says the size, which with fractional scaling is not the mode's.
        let size = first_frame(stream.node, None)
            .map_err(|e| warn!("No first frame of {}: {}", shown.connector, e))
            .unwrap_or(shown.physical);
        recorded.push(PhysicalStream {
            connector: shown.connector.clone(),
            recorded: Recorded::new(Shared::new(stream.node, None), watch.clone(), broken.clone()),
            size,
            at: (shown.position, shown.logical),
        });
    }
    *PHYSICAL.lock().unwrap() = Some(Physical {
        _session: session,
        streams: recorded,
    });
    CHANGES.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// Stops recording the physical monitors. A capturer of one fails at its next frame.
pub fn stop_physical() {
    let physical = PHYSICAL.lock().unwrap().take();
    if physical.is_some() {
        drop(physical);
        CHANGES.fetch_add(1, Ordering::SeqCst);
    }
}

/// How the physical monitors are recorded, as `state` shows them now.
pub fn physical_recording(state: &DisplayState) -> Recording {
    let physical = PHYSICAL.lock().unwrap();
    let Some(physical) = physical.as_ref() else {
        return Recording::Not;
    };
    if physical.moved(state) {
        return Recording::Ended(Ended::Moved);
    }
    match physical.streams.iter().find_map(|s| s.recorded.end()) {
        // A session closed while a change of ours went on was closed for it, whatever the
        // monitors look like by now: we may have put them back meanwhile.
        Some(Ended::Closed) if OURS_UNTIL.lock().unwrap().map_or(false, |t| Instant::now() < t) => {
            physical.break_all();
            Recording::Ended(Ended::Moved)
        }
        Some(end) => Recording::Ended(end),
        None => Recording::Live(physical.streams.iter().map(|s| s.connector.clone()).collect()),
    }
}

/// A change of ours is about to make Mutter lay the monitors out again (a virtual monitor added,
/// resized or removed), which may close the physical monitors' session: for a while, a session
/// closed is taken for that, not for someone at the device stopping it.
pub fn expect_relayout() {
    *OURS_UNTIL.lock().unwrap() = Some(Instant::now() + OURS_FOR);
}

impl Physical {
    // Whether a monitor recorded moved, was resized or went, as Mutter tells (`update_monitor`):
    // what it closes a stream for, or will. From 45 nothing else would say, so it is noted.
    fn moved(&self, state: &DisplayState) -> bool {
        let shown = state.shown();
        let moved = self.streams.iter().any(|s| {
            shown
                .iter()
                .find(|d| d.connector == s.connector)
                .map_or(true, |d| (d.position, d.logical) != s.at)
        });
        if moved {
            self.break_all();
        }
        moved
    }

    fn break_all(&self) {
        for s in &self.streams {
            s.recorded.broken.store(true, Ordering::SeqCst);
        }
    }
}

/// Raised on every change to what is recorded, and every change of the monitors Mutter said: the
/// display list may differ.
pub fn changes() -> u64 {
    CHANGES.load(Ordering::SeqCst) + monitors_changed().0
}

/// Whether Mutter's interfaces are on this process's session bus.
pub fn available() -> bool {
    match SyncConnection::new_session() {
        Ok(conn) => mutter::available(&conn),
        Err(e) => {
            info!("No session bus to find Mutter on: {}", e);
            false
        }
    }
}

/// What Mutter's `DisplayConfig` says now.
pub fn state() -> ResultType<DisplayState> {
    let conn = asking()?;
    mutter::current_state(&conn).map_err(|e| {
        // Asked again on a new connection, should this one be the trouble.
        *ASKING.lock().unwrap() = None;
        e
    })
}

/// Whether Mutter refuses new sessions for now, the screen locked.
pub fn creation_inhibited() -> bool {
    asking().map_or(false, |conn| mutter::creation_inhibited(&conn))
}

/// Whether Mutter takes monitor configurations over D-Bus now.
pub fn apply_allowed() -> bool {
    asking().map_or(false, |conn| mutter::apply_allowed(&conn))
}

/// Lays the monitors out as `config` says, for as long as nothing changes them (`mutter`'s
/// `apply_temporary`), against `state`.
pub fn apply_temporary(
    state: &DisplayState,
    config: &[LogicalConfig],
    layout_logical: Option<bool>,
) -> ResultType<()> {
    let conn = asking()?;
    mutter::apply_temporary(&conn, state, config, layout_logical)
}

/// Every stream Mutter records now, anyone's (`mutter::streams`).
pub fn streams() -> ResultType<Vec<CastStream>> {
    let conn = asking()?;
    mutter::streams(&conn).map_err(|e| {
        *ASKING.lock().unwrap() = None;
        e
    })
}

fn asking() -> ResultType<Arc<SyncConnection>> {
    let mut asking = ASKING.lock().unwrap();
    Ok(match asking.as_ref() {
        Some(conn) => conn.clone(),
        None => asking
            .insert(Arc::new(SyncConnection::new_session()?))
            .clone(),
    })
}

/// How many times Mutter said its monitors changed, and when it last did, since first asked: a
/// thread of its own follows `MonitorsChanged` from then on, and Mutter changing owner.
pub fn monitors_changed() -> (u64, Option<Instant>) {
    static FOLLOWING: OnceLock<()> = OnceLock::new();
    FOLLOWING.get_or_init(|| {
        if let Err(e) = std::thread::Builder::new()
            .name("mutter-monitors".to_owned())
            .spawn(follow_monitors)
        {
            warn!("Monitor changes will not be followed: {}", e);
        }
    });
    *MONITORS_CHANGED.lock().unwrap()
}

fn changed_now() {
    let mut changed = MONITORS_CHANGED.lock().unwrap();
    changed.0 += 1;
    changed.1 = Some(Instant::now());
}

fn follow_monitors() {
    loop {
        let followed: ResultType<()> = SyncConnection::new_session()
            .map_err(anyhow::Error::from)
            .and_then(|conn| {
                let changed = Arc::new(AtomicBool::new(false));
                let raised = changed.clone();
                mutter::on_monitors_changed(&conn, move || {
                    changed_now();
                    raised.store(true, Ordering::SeqCst);
                })?;
                mutter::on_owner_changed(&conn, || {
                    warn!("Mutter's remote desktop changed owner: its sessions are gone");
                    EPOCH.fetch_add(1, Ordering::SeqCst);
                    changed_now();
                })?;
                loop {
                    conn.process(Duration::from_secs(60))?;
                    // At once, before anyone puts the monitors back: a move seen later may be
                    // gone by then, the physical monitors' session closed all the same.
                    if changed.swap(false, Ordering::SeqCst) {
                        if let Ok(state) = mutter::current_state(&conn) {
                            if let Some(physical) = PHYSICAL.lock().unwrap().as_ref() {
                                physical.moved(&state);
                            }
                        }
                    }
                }
            });
        if let Err(e) = followed {
            warn!("Following Mutter's monitor changes: {}", e);
        }
        // Missed meanwhile, maybe: the displays are looked at again.
        changed_now();
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// The connectors of the virtual monitors of ours shown among the displays.
pub fn virtual_connectors() -> Vec<String> {
    VIRTUAL
        .lock()
        .unwrap()
        .iter()
        .map(|(c, _)| c.clone())
        .collect()
}

/// The displays to capture, as `state` shows them: the physical monitors recorded, in the order
/// Mutter lists them, then the virtual monitors of ours, in the order they were shown. A monitor
/// recorded but not shown any more (turned off) is left out, and so is one shown but not recorded
/// (plugged in since): `physical_recording` tells whether to record again.
pub fn displays(state: &DisplayState) -> Vec<crate::Display> {
    let shown = state.shown();
    let at = |connector: &str| shown.iter().find(|s| s.connector == connector);
    let mut displays = Vec::new();
    if let Some(physical) = PHYSICAL.lock().unwrap().as_ref() {
        for s in &physical.streams {
            if let Some(d) = at(&s.connector) {
                displays.push(display(d, &s.recorded, s.size));
            }
        }
    }
    for (connector, recorded) in VIRTUAL.lock().unwrap().iter() {
        if let Some(d) = at(connector) {
            let size = recorded
                .shared
                .size()
                .map_or(d.physical, |(w, h)| (w.max(0) as usize, h.max(0) as usize));
            displays.push(display(d, recorded, size));
        }
    }
    displays
}

fn display(shown: &Shown, recorded: &Recorded, size: (usize, usize)) -> crate::Display {
    let held = Held {
        recorded: recorded.clone(),
        generation: recorded.shared.generation(),
        size,
    };
    crate::Display::from_pipewire(PipeWireCapturable::of_mutter(
        recorded.watch.connection(),
        recorded.shared.node(),
        shown,
        size,
        held,
    ))
}

// The size of the first frame of `node`'s stream, through a pipeline of `size` stopped after.
fn first_frame(node: u32, size: Option<(i32, i32)>) -> ResultType<(usize, usize)> {
    let mut recorder = PipeWireRecorder::build(None, node as u64, size)?;
    let came = recorder
        .capture(FIRST_FRAME.as_millis() as u64)
        .map(|frame| frame.size())
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    match came {
        (0, 0) => bail!("No frame came"),
        size => Ok(size),
    }
}

/// A virtual monitor of ours, in a session of its own. Mutter makes the monitor once a consumer of
/// its stream settles a size, and keeps it with none (only another size changes it), so capturers
/// come and go as for any display; `DisplayConfig` lists it a moment after it is made.
pub struct VirtualMonitor {
    session: Session,
    recorded: Recorded,
    connector: Option<String>,
}

impl VirtualMonitor {
    pub fn create(width: i32, height: i32) -> ResultType<Self> {
        let mut session = Session::open()?;
        let mut streams = session.record(&[Source::Virtual])?;
        let stream = streams.remove(0);
        let size = Some((width, height));
        let recorded = Recorded::new(
            Shared::new(stream.node, size),
            session.watch(),
            Default::default(),
        );
        // Made once a frame of it came.
        first_frame(stream.node, size)?;
        Ok(Self {
            session,
            recorded,
            connector: None,
        })
    }

    /// Lists it among the displays to capture, as `connector`, which Mutter gave it.
    pub fn show_as(&mut self, connector: &str) {
        let mut ours = VIRTUAL.lock().unwrap();
        ours.retain(|(c, _)| c != connector);
        ours.push((connector.to_owned(), self.recorded.clone()));
        drop(ours);
        self.connector = Some(connector.to_owned());
        CHANGES.fetch_add(1, Ordering::SeqCst);
    }

    /// Settles another size with the stream, which Mutter resizes the monitor in place to: through
    /// its capturer, built again for it, or with no capturer about, through a pipeline of its own.
    /// Answers whether its capturer is to do it.
    pub fn resize(&self, width: i32, height: i32) -> ResultType<bool> {
        let by_capturer = self
            .recorded
            .shared
            .resize(Some((width, height)), |node, size| first_frame(node, size).map(|_| ()))?;
        CHANGES.fetch_add(1, Ordering::SeqCst);
        Ok(by_capturer)
    }

    pub fn size(&self) -> (i32, i32) {
        self.recorded.shared.size().unwrap_or_default()
    }

    /// Whether its session is of no more use: Mutter closed it (someone stopped the sharing, the
    /// screen locked) or went, or its stream ended.
    pub fn is_gone(&self) -> bool {
        self.recorded.gone()
    }
}

impl Drop for VirtualMonitor {
    fn drop(&mut self) {
        if let Some(connector) = self.connector.take() {
            VIRTUAL.lock().unwrap().retain(|(c, _)| *c != connector);
            CHANGES.fetch_add(1, Ordering::SeqCst);
        }
        self.recorded.shared.retire();
        self.session.stop();
    }
}
