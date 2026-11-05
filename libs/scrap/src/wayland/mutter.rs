//! GNOME Mutter's own remote-desktop, screen-cast and display-config D-Bus interfaces
//! (`org.gnome.Mutter.*`), the ones gnome-remote-desktop uses: capture without the portal's consent
//! dialog, and virtual monitors (`RecordVirtual`) sized by whoever consumes their PipeWire stream.
//! Mutter's private API, checked against 42.9 and 48.8.
//!
//! Each `Session` is a remote-desktop session with a screen-cast session tied to it, on a D-Bus
//! connection of its own. Mutter up to 44 closes the whole session when any one of its streams
//! closes (a recorded monitor disabled, moved or resized), and 42 cannot stop a single stream, so
//! callers keep the physical monitors in one session and every virtual monitor in a session of its
//! own. Mutter closes a session by itself, its virtual monitors with it, when this process leaves
//! the bus.

use std::{
    collections::HashMap,
    convert::TryFrom,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use dbus::{
    arg::{self, PropMap, RefArg, Variant},
    blocking::{
        stdintf::org_freedesktop_dbus::{Introspectable, Properties},
        Proxy, SyncConnection,
    },
    channel::MatchingReceiver,
    message::MatchRule,
    Path,
};
use hbb_common::{anyhow::anyhow, bail, ResultType};
use tracing::{debug, warn};

pub use crate::mutter_model::*;

const REMOTE_DESKTOP: &str = "org.gnome.Mutter.RemoteDesktop";
const SCREEN_CAST: &str = "org.gnome.Mutter.ScreenCast";
const DISPLAY_CONFIG: &str = "org.gnome.Mutter.DisplayConfig";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a stream takes to show up on PipeWire once started.
const STREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Mutter's cursor modes are numbered unlike the portal's: 0 hidden, 1 embedded, 2 metadata.
/// Hidden, as the portal path records: the controlling side draws its own cursor.
const CURSOR_HIDDEN: u32 = 0;

/// Whether Mutter's remote-desktop, screen-cast and display-config interfaces are all on the
/// session bus of `conn`.
pub fn available(conn: &SyncConnection) -> bool {
    let bus = conn.with_proxy("org.freedesktop.DBus", "/org/freedesktop/DBus", CALL_TIMEOUT);
    [REMOTE_DESKTOP, SCREEN_CAST, DISPLAY_CONFIG].iter().all(|name| {
        let has: Result<(bool,), _> =
            bus.method_call("org.freedesktop.DBus", "NameHasOwner", (*name,));
        matches!(has, Ok((true,)))
    })
}

/// Whether Mutter takes monitor configurations over D-Bus: a system `monitors.xml` policy can
/// turn it off.
pub fn apply_allowed(conn: &SyncConnection) -> bool {
    display_config(conn)
        .get(DISPLAY_CONFIG, "ApplyMonitorsConfigAllowed")
        .unwrap_or(false)
}

/// Whether Mutter refuses new sessions for now: GNOME Shell inhibits remote access while the
/// screen is locked, and Mutter closes every session then. A session created to tell is stopped at
/// once, never started, so nothing shows for it.
pub fn creation_inhibited(conn: &SyncConnection) -> bool {
    let created: Result<(Path<'static>,), dbus::Error> = proxy(
        conn,
        REMOTE_DESKTOP,
        "/org/gnome/Mutter/RemoteDesktop",
    )
    .method_call(REMOTE_DESKTOP, "CreateSession", ());
    match created {
        Ok((path,)) => {
            let _: Result<(), _> = proxy(conn, REMOTE_DESKTOP, path)
                .method_call(format!("{}.Session", REMOTE_DESKTOP), "Stop", ());
            false
        }
        Err(e) => e.name() == Some("org.freedesktop.DBus.Error.AccessDenied"),
    }
}

/// Calls `f` whenever the owner of Mutter's remote-desktop interface changes on the bus of `conn`,
/// which must be pumped: Mutter restarted, every session of it gone.
pub fn on_owner_changed(
    conn: &SyncConnection,
    f: impl Fn() + Send + Sync + 'static,
) -> ResultType<()> {
    let mut rule = MatchRule::new_signal("org.freedesktop.DBus", "NameOwnerChanged");
    rule.sender = Some("org.freedesktop.DBus".into());
    receive(conn, rule, move |msg| {
        if msg.read1::<&str>().ok() == Some(REMOTE_DESKTOP) {
            f();
        }
    })
}

/// What a session records.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source<'a> {
    /// A monitor by its connector, e.g. `HDMI-1`.
    Monitor(&'a str),
    /// A new virtual monitor. Mutter makes it only once the stream's consumer has settled a size,
    /// and gives it that size.
    Virtual,
}

/// A stream being recorded: its PipeWire node, and where and how big Mutter says it is, where it
/// says (a virtual monitor has neither until its consumer settles a size).
#[derive(Debug, Clone)]
pub struct Stream {
    pub path: Path<'static>,
    pub node: u32,
    pub position: Option<(i32, i32)>,
    pub size: Option<(i32, i32)>,
}

/// A remote-desktop session with its screen-cast session.
pub struct Session {
    conn: Arc<SyncConnection>,
    remote_desktop: Path<'static>,
    screen_cast: Path<'static>,
    closed: Arc<AtomicBool>,
    nodes: Arc<Mutex<HashMap<Path<'static>, u32>>>,
}

impl Session {
    pub fn open() -> ResultType<Self> {
        let conn = Arc::new(SyncConnection::new_session()?);
        let closed = Arc::new(AtomicBool::new(false));
        let nodes: Arc<Mutex<HashMap<Path<'static>, u32>>> = Default::default();
        // Listened for before anything is created, so no signal is missed.
        let found = nodes.clone();
        receive(
            &conn,
            MatchRule::new_signal(format!("{}.Stream", SCREEN_CAST), "PipeWireStreamAdded"),
            move |msg| {
                if let (Some(path), Ok(node)) = (msg.path(), msg.read1::<u32>()) {
                    found.lock().unwrap().insert(path.into_static(), node);
                }
            },
        )?;
        let (rd,): (Path<'static>,) =
            proxy(&conn, REMOTE_DESKTOP, "/org/gnome/Mutter/RemoteDesktop").method_call(
                REMOTE_DESKTOP,
                "CreateSession",
                (),
            )?;
        let gone = closed.clone();
        receive(
            &conn,
            MatchRule::new_signal(format!("{}.Session", REMOTE_DESKTOP), "Closed")
                .with_path(rd.clone()),
            move |_| gone.store(true, Ordering::SeqCst),
        )?;
        let id: String = proxy(&conn, REMOTE_DESKTOP, rd.clone()).get(
            &format!("{}.Session", REMOTE_DESKTOP),
            "SessionId",
        )?;
        let mut props = PropMap::new();
        props.insert(
            "remote-desktop-session-id".into(),
            Variant(Box::new(id) as Box<dyn RefArg>),
        );
        let (sc,): (Path<'static>,) = proxy(&conn, SCREEN_CAST, "/org/gnome/Mutter/ScreenCast")
            .method_call(SCREEN_CAST, "CreateSession", (props,))?;
        debug!("Mutter session {} with screen cast {}", rd, sc);
        Ok(Self {
            conn,
            remote_desktop: rd,
            screen_cast: sc,
            closed,
            nodes,
        })
    }

    /// Records `sources` and starts the session, which records nothing more after: one stream
    /// each, in order.
    pub fn record(&mut self, sources: &[Source]) -> ResultType<Vec<Stream>> {
        let mut paths = Vec::with_capacity(sources.len());
        for source in sources {
            let (path,): (Path<'static>,) = match source {
                Source::Monitor(connector) => self.screen_cast_proxy().method_call(
                    format!("{}.Session", SCREEN_CAST),
                    "RecordMonitor",
                    (*connector, cursor_props()),
                )?,
                Source::Virtual => self.screen_cast_proxy().method_call(
                    format!("{}.Session", SCREEN_CAST),
                    "RecordVirtual",
                    (cursor_props(),),
                )?,
            };
            paths.push(path);
        }
        proxy(&self.conn, REMOTE_DESKTOP, self.remote_desktop.clone())
            .method_call::<(), _, _, _>(format!("{}.Session", REMOTE_DESKTOP), "Start", ())?;
        let deadline = Instant::now() + STREAM_TIMEOUT;
        loop {
            {
                let nodes = self.nodes.lock().unwrap();
                if paths.iter().all(|p| nodes.contains_key(p)) {
                    break;
                }
            }
            if self.closed.load(Ordering::SeqCst) {
                bail!("Mutter closed the session before its streams started");
            }
            if Instant::now() >= deadline {
                bail!("Mutter streams did not start");
            }
            self.conn.process(Duration::from_millis(100))?;
        }
        let nodes = self.nodes.lock().unwrap().clone();
        Ok(paths
            .into_iter()
            .map(|path| {
                let params: PropMap = proxy(&self.conn, SCREEN_CAST, path.clone())
                    .get(&format!("{}.Stream", SCREEN_CAST), "Parameters")
                    .unwrap_or_default();
                Stream {
                    node: nodes[&path],
                    position: pair(&params, "position"),
                    size: pair(&params, "size"),
                    path,
                }
            })
            .collect())
    }

    /// Whether Mutter closed the session (or this side stopped it).
    pub fn is_closed(&self) -> bool {
        self.watch().is_closed()
    }

    /// Tells whether the session closed, from any thread, and keeps its connection alive.
    pub fn watch(&self) -> Watch {
        Watch {
            conn: self.conn.clone(),
            closed: self.closed.clone(),
        }
    }

    pub fn stop(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let stopped: Result<(), _> = proxy(&self.conn, REMOTE_DESKTOP, self.remote_desktop.clone())
            .method_call(format!("{}.Session", REMOTE_DESKTOP), "Stop", ());
        if let Err(e) = stopped {
            // Gone already, most likely: Mutter closed it.
            debug!("Stopping Mutter session {}: {}", self.remote_desktop, e);
        }
    }

    fn screen_cast_proxy(&self) -> Proxy<'_, &SyncConnection> {
        proxy(&self.conn, SCREEN_CAST, self.screen_cast.clone())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Whether a session closed, and its connection kept alive with it.
#[derive(Clone)]
pub struct Watch {
    conn: Arc<SyncConnection>,
    closed: Arc<AtomicBool>,
}

impl Watch {
    /// Handles what came on the connection, without waiting, and answers.
    pub fn is_closed(&self) -> bool {
        while let Ok(true) = self.conn.process(Duration::ZERO) {}
        self.closed.load(Ordering::SeqCst)
    }

    /// Whether it closed as known so far (this side stopped it, or `is_closed` found Mutter did),
    /// handling nothing that came: no signal handler runs.
    pub fn closed_seen(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    pub fn connection(&self) -> Arc<SyncConnection> {
        self.conn.clone()
    }
}

type RawMonitor = (
    (String, String, String, String),
    Vec<(String, i32, i32, f64, f64, Vec<f64>, PropMap)>,
    PropMap,
);
type RawLogical = (i32, i32, f64, u32, bool, Vec<(String, String, String, String)>, PropMap);

pub fn current_state(conn: &SyncConnection) -> ResultType<DisplayState> {
    let (serial, monitors, logical, props): (u32, Vec<RawMonitor>, Vec<RawLogical>, PropMap) =
        display_config(conn).method_call(DISPLAY_CONFIG, "GetCurrentState", ())?;
    Ok(DisplayState {
        serial,
        // 1 logical, 2 physical.
        layout_logical: arg::prop_cast::<u32>(&props, "layout-mode").copied() == Some(1),
        layout_mode_changeable: flag(&props, "supports-changing-layout-mode"),
        monitors: monitors
            .into_iter()
            .map(|((connector, vendor, product, serial), modes, props)| Monitor {
                connector,
                vendor,
                product,
                serial,
                modes: modes
                    .into_iter()
                    .map(|(id, width, height, refresh, _scale, _scales, props)| MonitorMode {
                        id,
                        width,
                        height,
                        refresh,
                        current: flag(&props, "is-current"),
                        preferred: flag(&props, "is-preferred"),
                    })
                    .collect(),
                builtin: flag(&props, "is-builtin"),
                underscanning: arg::prop_cast::<bool>(&props, "is-underscanning").copied(),
                color_mode: arg::prop_cast::<u32>(&props, "color-mode").copied(),
                for_lease: flag(&props, "is-for-lease"),
            })
            .collect(),
        logical: logical
            .into_iter()
            .map(
                |(x, y, scale, transform, primary, monitors, _props)| LogicalMonitor {
                    x,
                    y,
                    scale,
                    transform,
                    primary,
                    connectors: monitors.into_iter().map(|m| m.0).collect(),
                },
            )
            .collect(),
    })
}

/// Applies `config` against `state` until the monitors change or the session ends, without
/// storing it or asking the person at the device to keep it (`method` 1, temporary). Monitors left
/// out are turned off. Each monitor keeps what `state` lists of its underscanning and colour mode,
/// and leased ones stay leased: Mutter takes leaving any of that out for turning it off.
/// `layout_logical`: the layout mode to keep, where Mutter takes one
/// (`DisplayState::layout_mode_changeable`).
pub fn apply_temporary(
    conn: &SyncConnection,
    state: &DisplayState,
    config: &[LogicalConfig],
    layout_logical: Option<bool>,
) -> ResultType<()> {
    let kept = |connector: &str| {
        let mut props = PropMap::new();
        if let Some(m) = state.monitor(connector) {
            if let Some(on) = m.underscanning {
                props.insert("underscanning".into(), Variant(Box::new(on) as Box<dyn RefArg>));
            }
            if let Some(mode) = m.color_mode {
                props.insert("color-mode".into(), Variant(Box::new(mode) as Box<dyn RefArg>));
            }
        }
        props
    };
    let logical: Vec<(i32, i32, f64, u32, bool, Vec<(String, String, PropMap)>)> = config
        .iter()
        .map(|l| {
            (
                l.x,
                l.y,
                l.scale,
                l.transform,
                l.primary,
                l.monitors
                    .iter()
                    .map(|(connector, mode)| (connector.clone(), mode.clone(), kept(connector)))
                    .collect(),
            )
        })
        .collect();
    let mut props = PropMap::new();
    if let Some(layout_logical) = layout_logical {
        // 1 logical, 2 physical.
        let mode: u32 = if layout_logical { 1 } else { 2 };
        props.insert("layout-mode".into(), Variant(Box::new(mode) as Box<dyn RefArg>));
    }
    let leased: Vec<(String, String, String, String)> = state
        .monitors
        .iter()
        .filter(|m| m.for_lease)
        .map(|m| {
            (
                m.connector.clone(),
                m.vendor.clone(),
                m.product.clone(),
                m.serial.clone(),
            )
        })
        .collect();
    if !leased.is_empty() {
        props.insert(
            "monitors-for-lease".into(),
            Variant(Box::new(leased) as Box<dyn RefArg>),
        );
    }
    display_config(conn)
        .method_call::<(), _, _, _>(
            DISPLAY_CONFIG,
            "ApplyMonitorsConfig",
            (state.serial, 1u32, logical, props),
        )
        .map_err(|e| anyhow!("Mutter refused the monitor configuration: {}", e))
}

/// Every stream Mutter records now, anyone's: each is an object of its own under
/// `/org/gnome/Mutter/ScreenCast/Stream`, whose `Parameters` anyone may read (only the methods are
/// kept to the session's owner). One gone between the listing and the reading is left out.
pub fn streams(conn: &SyncConnection) -> ResultType<Vec<CastStream>> {
    const STREAMS: &str = "/org/gnome/Mutter/ScreenCast/Stream";
    let xml = proxy(conn, SCREEN_CAST, STREAMS).introspect()?;
    let mut streams = Vec::new();
    for node in child_nodes(&xml) {
        let path = format!("{}/{}", STREAMS, node);
        let read: Result<PropMap, dbus::Error> = proxy(conn, SCREEN_CAST, path.clone())
            .get(&format!("{}.Stream", SCREEN_CAST), "Parameters");
        match read {
            Ok(params) => streams.push(CastStream {
                path,
                position: pair(&params, "position"),
                size: pair(&params, "size"),
            }),
            // GDBus answers a call to an object no longer there as an unknown method.
            Err(e)
                if matches!(
                    e.name(),
                    Some("org.freedesktop.DBus.Error.UnknownObject")
                        | Some("org.freedesktop.DBus.Error.UnknownMethod")
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(streams)
}

/// Calls `f` on every `MonitorsChanged` that comes on `conn`, which must be pumped
/// (`SyncConnection::process`) for it to be seen.
pub fn on_monitors_changed(
    conn: &SyncConnection,
    f: impl Fn() + Send + Sync + 'static,
) -> ResultType<()> {
    receive(
        conn,
        MatchRule::new_signal(DISPLAY_CONFIG, "MonitorsChanged"),
        move |_| f(),
    )
}

fn display_config(conn: &SyncConnection) -> Proxy<'_, &SyncConnection> {
    proxy(conn, DISPLAY_CONFIG, "/org/gnome/Mutter/DisplayConfig")
}

fn proxy<'a, 'b>(
    conn: &'b SyncConnection,
    dest: &'a str,
    path: impl Into<Path<'a>>,
) -> Proxy<'a, &'b SyncConnection> {
    conn.with_proxy(dest, path, CALL_TIMEOUT)
}

fn receive(
    conn: &SyncConnection,
    rule: MatchRule<'static>,
    f: impl Fn(&dbus::Message) + Send + Sync + 'static,
) -> ResultType<()> {
    conn.add_match_no_cb(&rule.match_str())?;
    conn.start_receive(
        rule,
        Box::new(move |msg, _| {
            f(&msg);
            true
        }),
    );
    Ok(())
}

fn cursor_props() -> PropMap {
    let mut props = PropMap::new();
    props.insert(
        "cursor-mode".into(),
        Variant(Box::new(CURSOR_HIDDEN) as Box<dyn RefArg>),
    );
    props
}

fn flag(props: &PropMap, key: &str) -> bool {
    arg::prop_cast::<bool>(props, key).copied().unwrap_or(false)
}

/// An `(ii)` property, e.g. a stream's `position` or `size`.
fn pair(props: &PropMap, key: &str) -> Option<(i32, i32)> {
    let mut it = props.get(key)?.0.as_iter()?;
    let a = it.next()?.as_i64()?;
    let b = it.next()?.as_i64()?;
    match (i32::try_from(a), i32::try_from(b)) {
        (Ok(a), Ok(b)) => Some((a, b)),
        _ => {
            warn!("Mutter stream {} out of range: {}, {}", key, a, b);
            None
        }
    }
}
