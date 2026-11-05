//! Virtual displays on Linux X11. Best on the connector of vkms, the kernel's virtual display
//! card, which the root service loads at boot and keeps off until it is needed; otherwise on a
//! connector without a monitor, lit by the root service with an EDID of ours, running a mode of
//! ours, or, where the port carries only a slow one, scaled by RandR to the size asked for.

mod connectors;
mod drm;
mod edid;
mod host;
mod input;
mod leases;
mod protocol;
mod randr;
mod seat;
mod session;
mod sysfs;
mod vkms;
mod xephyr;

#[cfg(target_os = "linux")]
pub use leases::held as leases_held;
#[cfg(target_os = "linux")]
pub use live::new_backend;
#[cfg(target_os = "linux")]
pub(super) use live::RootLink;
#[cfg(target_os = "linux")]
pub use service::{release_leftovers, serve_root, vkms_boot};

#[cfg(target_os = "linux")]
mod live {
    use super::{
        protocol::{self, Request, Response},
        randr::{self, Output, Provider},
        session::{Desktop, Root, X11Backend},
    };
    use crate::vdisplay::manager::Backend;
    use hbb_common::{bail, log, ResultType};
    use std::{io::BufReader, os::unix::net::UnixStream, process::Command, time::Duration};
    use x11rb::{
        connection::Connection,
        protocol::{randr::ConnectionExt as _, render::Transform, xproto::ConnectionExt as _},
        rust_connection::RustConnection,
    };

    /// The backend of this session, on X11 where the root service answers.
    pub fn new_backend() -> Option<Box<dyn Backend>> {
        if !crate::platform::linux::is_x11() {
            return None;
        }
        let mut root = match RootLink::connect() {
            Ok(root) => root,
            Err(e) => {
                log::info!("No virtual displays: {}", e);
                return None;
            }
        };
        let status = match root.call(Request::Status) {
            Ok(status) => status,
            Err(e) => {
                log::info!("No virtual displays: {}", e);
                return None;
            }
        };
        log::info!(
            "Virtual displays: {} free, {}{}",
            status.free,
            status.kind.map_or("none", |kind| kind.name()),
            if status.note.is_empty() {
                String::new()
            } else {
                format!(" ({})", status.note)
            }
        );
        Some(Box::new(X11Backend::new(
            root,
            XDesktop::default(),
            status.free,
            status.kind,
        )))
    }

    // How long the root service may take to answer: lighting a connector, longer than it takes at
    // worst (a probe of a connector without a monitor waits out its timeouts); anything else, not
    // long, the first call being made as a session logs in.
    const LIGHTING_TIMEOUT: Duration = Duration::from_secs(30);
    const ROOT_TIMEOUT: Duration = Duration::from_secs(5);

    /// The connection to the root service. The service holds our connectors for as long as it
    /// lasts: an answer that does not come in time is given up on, not the connection (it is
    /// passed over when it comes, by its number). Once the connection fails, it is closed, and
    /// the next call connects anew.
    pub(in crate::vdisplay) struct RootLink {
        stream: Option<(BufReader<UnixStream>, UnixStream)>,
        seq: u64,
        /// Counts the connections made: the service forgets a client's hold and lock with its
        /// connection.
        generation: u64,
    }

    impl RootLink {
        fn connect() -> ResultType<Self> {
            Ok(Self {
                stream: Some(Self::open()?),
                seq: 0,
                generation: 1,
            })
        }

        /// One that connects at its first call, for a backend that works without the service.
        pub(in crate::vdisplay) fn later() -> Self {
            Self {
                stream: None,
                seq: 0,
                generation: 0,
            }
        }

        /// Which connection this is, counting from 1; 0 before the first.
        pub(in crate::vdisplay) fn generation(&self) -> u64 {
            self.generation
        }

        /// Asks the service to keep the input here from the desktop this process took over by
        /// itself (`Request::LockInput`). On any failure the connection goes, as with `hold`: a
        /// lock taken by an answer too late is let go with it.
        pub(in crate::vdisplay) fn lock_input(&mut self) -> ResultType<()> {
            let locked = self.call(Request::LockInput).map(|_| ());
            if locked.is_err() {
                self.stream = None;
            }
            locked
        }

        /// Whether the person at the device took back the desktop this process took over.
        pub(in crate::vdisplay) fn taken_back(&mut self) -> ResultType<bool> {
            self.call(Request::TakenBack).map(|r| r.taken_back)
        }

        /// The desktop this process took over is given back. On any failure the connection
        /// goes, as with `hold`: the service lets go of the input with it, should its answer
        /// come too late.
        pub(in crate::vdisplay) fn restored(&mut self) -> ResultType<()> {
            let restored = self.call(Request::Restored).map(|_| ());
            if restored.is_err() {
                self.stream = None;
            }
            restored
        }

        /// Asks the service not to restart this `--server` (`on`), or no more. On any failure
        /// the connection goes, and the service lets go of the hold with it: an answer late or
        /// lost may still have taken, and the service then holds what we think it does not.
        pub(in crate::vdisplay) fn hold(&mut self, on: bool) -> ResultType<()> {
            let held = self.call(Request::Hold { on }).map(|_| ());
            if held.is_err() {
                self.stream = None;
            }
            held
        }

        fn open() -> ResultType<(BufReader<UnixStream>, UnixStream)> {
            let stream = crate::ipc::connect_vdisplay(ROOT_TIMEOUT)?;
            Ok((BufReader::new(stream.try_clone()?), stream))
        }
    }

    impl Root for RootLink {
        fn call(&mut self, request: Request) -> ResultType<Response> {
            if self.stream.is_none() {
                self.stream = Some(Self::open()?);
                self.generation += 1;
            }
            let Some((reader, writer)) = self.stream.as_mut() else {
                bail!("No connection to the root service");
            };
            let timeout = match request {
                Request::Activate { .. } => LIGHTING_TIMEOUT,
                _ => ROOT_TIMEOUT,
            };
            if let Err(e) = writer.set_read_timeout(Some(timeout)) {
                self.stream = None;
                return Err(e.into());
            }
            self.seq += 1;
            let response = match protocol::call(reader, writer, &request, self.seq) {
                Ok(response) => response,
                // Slow, not gone: the service may be busy lighting another session's display.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return Err(e.into());
                }
                Err(e) => {
                    self.stream = None;
                    return Err(e.into());
                }
            };
            if !response.error.is_empty() {
                bail!(response.error);
            }
            Ok(response)
        }

        fn lost(&self) -> bool {
            self.stream.is_none()
        }
    }

    /// The X server, through xrandr for what xrandr does, and through a connection of its own for
    /// what it cannot: the pointer, and CRTCs without outputs.
    #[derive(Default)]
    struct XDesktop {
        x: Option<(RustConnection, u32)>,
    }

    impl XDesktop {
        // The connection, made when first needed; on failure, dropped and made again next time.
        fn with<T>(&mut self, f: impl FnOnce(&RustConnection, u32) -> ResultType<T>) -> Option<T> {
            if self.x.is_none() {
                match x11rb::connect(None) {
                    Ok((conn, screen)) => {
                        let root = conn.setup().roots.get(screen).map(|s| s.root)?;
                        self.x = Some((conn, root));
                    }
                    Err(e) => {
                        log::warn!("Failed to connect to the X server: {}", e);
                        return None;
                    }
                }
            }
            let (conn, root) = self.x.as_ref()?;
            match f(conn, *root) {
                Ok(value) => Some(value),
                Err(e) => {
                    log::warn!("X server: {}", e);
                    self.x = None;
                    None
                }
            }
        }
    }

    fn identity() -> Transform {
        const ONE: i32 = 1 << 16;
        Transform {
            matrix11: ONE,
            matrix12: 0,
            matrix13: 0,
            matrix21: 0,
            matrix22: ONE,
            matrix23: 0,
            matrix31: 0,
            matrix32: 0,
            matrix33: ONE,
        }
    }

    impl Desktop for XDesktop {
        fn outputs(&mut self) -> ResultType<Vec<Output>> {
            Ok(randr::parse(&xrandr_text(&["--current", "--prop"])?))
        }

        // A probe of every connector, without --current.
        fn probe(&mut self) {
            if let Err(e) = Command::new("xrandr").output() {
                log::warn!("xrandr: {}", e);
            }
        }

        fn xrandr(&mut self, args: &[String]) -> ResultType<()> {
            let out = Command::new("xrandr").arg("--current").args(args).output()?;
            if !out.status.success() {
                bail!(
                    "xrandr {}: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        }

        // Deepin keeps an idea of the primary output of its own, and puts it back on RandR;
        // told through it, it keeps ours. Elsewhere RandR's is the one.
        fn set_primary(&mut self, name: &str) -> ResultType<()> {
            match deepin_set_primary(name) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => log::warn!("Failed to tell Deepin the primary output: {}", e),
            }
            self.xrandr(&["--output", name, "--primary"].map(str::to_owned))
        }

        // Deepin's: its dde-daemon turns it down for good on an output too small for it.
        fn global_scale(&mut self) -> Option<f64> {
            match deepin_scale() {
                Ok(scale) => scale,
                Err(e) => {
                    log::warn!("Failed to ask Deepin for its scale: {}", e);
                    None
                }
            }
        }

        // The way dde-daemon set it: the same call, the same settings written.
        fn set_global_scale(&mut self, scale: f64) -> ResultType<()> {
            let conn = dbus::blocking::Connection::new_session()?;
            let proxy = conn.with_proxy(XSETTINGS, "/org/deepin/dde/XSettings1", Duration::from_secs(5));
            proxy.method_call::<(), _, _, _>(XSETTINGS, "SetScaleFactor", (scale,))?;
            Ok(())
        }

        fn providers(&mut self) -> ResultType<Vec<Provider>> {
            Ok(randr::parse_providers(&xrandr_text(&["--listproviders"])?))
        }

        fn pointer(&mut self) -> Option<(i32, i32)> {
            self.with(|conn, root| {
                let reply = conn.query_pointer(root)?.reply()?;
                Ok((reply.root_x as i32, reply.root_y as i32))
            })
        }

        fn warp(&mut self, (x, y): (i32, i32)) {
            self.with(|conn, root| {
                conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, x as i16, y as i16)?;
                conn.flush()?;
                Ok(())
            });
        }

        // The transform a CRTC without an output has pending is the one it takes when next set.
        fn clear_transforms(&mut self) {
            self.with(|conn, root| {
                let resources = conn.randr_get_screen_resources_current(root)?.reply()?;
                for crtc in resources.crtcs {
                    let info = conn
                        .randr_get_crtc_info(crtc, resources.config_timestamp)?
                        .reply()?;
                    if !info.outputs.is_empty() {
                        continue;
                    }
                    let transform = conn.randr_get_crtc_transform(crtc)?.reply()?;
                    if transform.pending_transform == identity()
                        && transform.pending_filter_name.is_empty()
                    {
                        continue;
                    }
                    log::info!("Dropping the transform left on CRTC {}", crtc);
                    conn.randr_set_crtc_transform(crtc, identity(), b"", &[])?;
                }
                conn.flush()?;
                Ok(())
            });
        }

        fn sleep(&mut self, duration: Duration) {
            std::thread::sleep(duration)
        }
    }

    fn xrandr_text(args: &[&str]) -> ResultType<String> {
        let out = Command::new("xrandr").args(args).output()?;
        if !out.status.success() {
            bail!(
                "xrandr {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    const XSETTINGS: &str = "org.deepin.dde.XSettings1";

    // None where there is no Deepin settings service to ask.
    fn deepin_scale() -> ResultType<Option<f64>> {
        let conn = dbus::blocking::Connection::new_session()?;
        let proxy = conn.with_proxy(XSETTINGS, "/org/deepin/dde/XSettings1", Duration::from_secs(5));
        match proxy.method_call::<(f64,), _, _, _>(XSETTINGS, "GetScaleFactor", ()) {
            Ok((scale,)) => Ok(Some(scale)),
            Err(e) if e.name() == Some("org.freedesktop.DBus.Error.ServiceUnknown") => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // False where there is no Deepin display service to tell.
    fn deepin_set_primary(name: &str) -> ResultType<bool> {
        const SERVICE: &str = "org.deepin.dde.Display1";
        let conn = dbus::blocking::Connection::new_session()?;
        let proxy = conn.with_proxy(SERVICE, "/org/deepin/dde/Display1", Duration::from_secs(5));
        match proxy.method_call::<(), _, _, _>(SERVICE, "SetPrimary", (name,)) {
            Ok(()) => Ok(true),
            Err(e) if e.name() == Some("org.freedesktop.DBus.Error.ServiceUnknown") => Ok(false),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(target_os = "linux")]
mod service {
    use super::{
        leases::Leases,
        protocol::{self, Request, Response},
        seat,
        sysfs::SysfsKernel,
        vkms,
    };
    use hbb_common::log;
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    // How often the session on the screen is looked at; how long a graphical one must have been
    // there for the desktop to count as up.
    const SEAT_LOOK: Duration = Duration::from_millis(250);
    const DESKTOP_UP: Duration = Duration::from_secs(10);

    /// The root service's side, for the life of the service: lights connectors for the session's
    /// `--server`, and lets them go when it does.
    pub fn serve_root() {
        let leases = Arc::new(Mutex::new(Leases::new(SysfsKernel::system())));
        leases.lock().unwrap().recover();
        let followed = leases.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("vdisplay-seat".into())
            .spawn(move || follow_seat(followed))
        {
            log::warn!("Virtual displays will not follow the session on the screen: {}", e);
        }
        let result = crate::ipc::listen_vdisplay(move |stream, client, uid, pid| {
            protocol::serve(std::io::BufReader::new(&stream), &stream, |request| {
                handle(&mut leases.lock().unwrap(), client, uid, pid, request)
            });
            leases.lock().unwrap().release_client(client);
        });
        if let Err(e) = result {
            log::error!("Virtual displays will not be lit: {}", e);
        }
    }

    /// Puts back what was lit, the service being gone (`--release-virtual-displays`, run by
    /// systemd after the service stops however it stopped).
    pub fn release_leftovers() {
        Leases::new(SysfsKernel::system()).recover();
    }

    /// Loads vkms at boot, before the desktop (`--vkms-boot`, run by systemd).
    pub fn vkms_boot() {
        let word = vkms::boot(&mut vkms::SystemHost);
        log::info!("vkms at boot: {}", word);
        println!("vkms at boot: {}", word);
    }

    // Connectors of a session that is not on the screen go dark meanwhile; and once a graphical
    // session has been on it a while, a boot that loaded vkms reached the desktop.
    fn follow_seat(leases: Arc<Mutex<Leases<SysfsKernel>>>) {
        let mut graphical_since: Option<Instant> = None;
        let mut confirmed = false;
        loop {
            std::thread::sleep(SEAT_LOOK);
            let (session, uid) = seat::active(&std::fs::read_to_string(seat::SEAT0).unwrap_or_default());
            {
                let mut leases = leases.lock().unwrap();
                leases.follow(uid);
                leases.heartbeat();
            }
            if confirmed {
                continue;
            }
            let graphical = session.map_or(false, |session| {
                let path = format!("{}/{}", seat::SESSIONS, session);
                seat::graphical(&std::fs::read_to_string(path).unwrap_or_default())
            });
            match (graphical, graphical_since) {
                (false, _) => graphical_since = None,
                (true, None) => graphical_since = Some(Instant::now()),
                (true, Some(since)) if since.elapsed() >= DESKTOP_UP => {
                    vkms::confirm(&mut vkms::SystemHost);
                    confirmed = true;
                }
                _ => {}
            }
        }
    }

    fn handle(
        leases: &mut Leases<SysfsKernel>,
        client: u64,
        uid: u32,
        pid: u32,
        request: Request,
    ) -> Response {
        let answer = match request {
            Request::Status => {
                let (free, kind, note) = leases.status();
                Ok(Response {
                    free,
                    kind,
                    note,
                    released: leases.released(),
                    taken_back: leases.taken_back_by(client, pid),
                    ..Default::default()
                })
            }
            Request::Reserve { skip_vkms } => {
                leases.reserve(client, uid, skip_vkms).map(|r| Response {
                    kind: Some(r.kind),
                    connector: r.connector.name,
                    connector_id: r.connector.id,
                    max_khz: r.max_khz,
                    ..Default::default()
                })
            }
            Request::Activate { connector } => leases
                .activate(client, &connector, Instant::now())
                .map(|modes| Response {
                    modes,
                    ..Default::default()
                }),
            Request::Release { connector } => leases
                .release(client, &connector)
                .map(|()| Response::default()),
            Request::Takeover { restore } => leases
                .take_over(client, pid, restore)
                .map(|()| Response::default()),
            Request::Restored => {
                leases.restored(client);
                leases.escape_over(pid);
                Ok(Response::default())
            }
            Request::Hold { on } => {
                leases.hold(client, on);
                Ok(Response::default())
            }
            Request::LockInput => leases
                .lock_input(client, uid, pid, Instant::now())
                .map(|()| Response::default()),
            Request::TakenBack => Ok(Response {
                taken_back: leases.taken_back_by(client, pid),
                ..Default::default()
            }),
        };
        answer.unwrap_or_else(Response::failed)
    }
}
