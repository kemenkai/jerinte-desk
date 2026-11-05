//! The windows nested sessions (`Mode::Nested`) show in: one per virtual display, over its output
//! in the shared desktop, with `Xephyr` drawing into it (`-parent`). They are override-redirect,
//! so no window manager places, lists or focuses them; a window `Xephyr` makes of its own is
//! managed, and deepin-kwin, for one, put it full-screen on the physical monitor, focused and in
//! the taskbar. Ours to place, they show only where their output shows them alone: checked
//! against the X server's layout when shown, and hidden by a watch the moment the layout changes
//! under them, before anything puts them right.

use super::randr::alone;
pub use super::randr::Area;
pub use imp::Hosts;

/// What of `area`, asked for output `output`, may show by the layout `lit` (unreadable for none):
/// all of it, where it is the output's area and the output shows it alone; else nothing.
pub fn keep(output: &str, area: Option<Area>, lit: Option<&[(String, Area)]>) -> Option<Area> {
    area.filter(|&area| lit.and_then(|lit| alone(output, lit)) == Some(area))
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{keep, Area};
    use hbb_common::{anyhow::anyhow, libc, log, ResultType};
    use std::{
        os::unix::io::AsRawFd,
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc, Mutex,
        },
    };
    use x11rb::{
        connection::Connection,
        protocol::{
            randr::{ConnectionExt as _, NotifyMask},
            shape::{ConnectionExt as _, SK, SO},
            xproto::{
                AtomEnum, ClipOrdering, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux,
                EventMask, PropMode, StackMode, WindowClass,
            },
        },
        rust_connection::RustConnection,
        wrapper::ConnectionExt as _,
        COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
    };

    // deepin-kwin's effects on a window, off: its rounded corners, shadow, border, and the
    // animations at its start and close (`_DEEPIN_NET_EFFECT` 0x01, 0x02, 0x04, 0x10, 0x20). It
    // puts them on unmanaged windows too, and the shadow would spill onto the physical monitor
    // beside the virtual display.
    const NO_EFFECTS: u32 = 0x37;

    struct Host {
        output: String,
        window: u32,
        /// Where it shows; hidden for none.
        shown: Option<Area>,
    }

    pub struct Hosts {
        conn: RustConnection,
        root: u32,
        black: u32,
        /// Shared with the watch, which hides them; a window is shown or hidden under its lock.
        hosts: Arc<Mutex<Vec<Host>>>,
        /// The watch's own connection, shut down to end it.
        watch: Arc<RustConnection>,
        /// While the watch runs: without it nothing shows.
        watching: Arc<AtomicBool>,
    }

    impl Hosts {
        /// Connects to the desktop's X server (`DISPLAY`), and starts the watch.
        pub fn new() -> ResultType<Self> {
            let (conn, screen) = RustConnection::connect(None)?;
            let (root, black) = {
                let screen = conn
                    .setup()
                    .roots
                    .get(screen)
                    .ok_or_else(|| anyhow!("The X server has no screen {}", screen))?;
                (screen.root, screen.black_pixel)
            };
            let (watch, _) = RustConnection::connect(None)?;
            let watch = Arc::new(watch);
            watch.randr_select_input(
                root,
                NotifyMask::SCREEN_CHANGE | NotifyMask::CRTC_CHANGE | NotifyMask::OUTPUT_CHANGE,
            )?;
            watch.flush()?;
            let hosts = Arc::new(Mutex::new(Vec::new()));
            let watching = Arc::new(AtomicBool::new(true));
            let (conn2, shared, running) = (watch.clone(), hosts.clone(), watching.clone());
            std::thread::Builder::new()
                .name("vdisplay-nest-watch".to_owned())
                .spawn(move || hide_on_change(&conn2, root, &shared, &running))?;
            Ok(Self {
                conn,
                root,
                black,
                hosts,
                watch,
                watching,
            })
        }

        /// Whether the watch runs, without which nothing shows.
        pub fn watching(&self) -> bool {
            self.watching.load(Ordering::SeqCst)
        }

        /// A window over `area`, hidden, for the nested session of `output` to show in.
        pub fn create(&mut self, output: &str, area: Area) -> ResultType<u32> {
            let window = self.conn.generate_id()?;
            // The root's depth and visual: `Xephyr` makes its window of the parent's depth.
            let aux = CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(self.black)
                .event_mask(EventMask::NO_EVENT);
            self.conn.create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                self.root,
                area.0 as i16,
                area.1 as i16,
                side(area.2),
                side(area.3),
                0,
                WindowClass::INPUT_OUTPUT,
                COPY_FROM_PARENT,
                &aux,
            )?;
            let made = (|| -> ResultType<()> {
                let effect = self.atom(b"_DEEPIN_NET_EFFECT")?;
                self.conn.change_property32(
                    PropMode::REPLACE,
                    window,
                    effect,
                    AtomEnum::CARDINAL,
                    &[NO_EFFECTS],
                )?;
                let skip = self.atom(b"_KDE_NET_WM_SKIP_CLOSE_ANIMATION")?;
                self.conn.change_property32(
                    PropMode::REPLACE,
                    window,
                    skip,
                    AtomEnum::CARDINAL,
                    &[1],
                )?;
                self.conn.change_property8(
                    PropMode::REPLACE,
                    window,
                    AtomEnum::WM_NAME,
                    AtomEnum::STRING,
                    b"rustdesk-nested-host",
                )?;
                // The pointer of whoever is at the machine passes through to what is under it,
                // rather than into the nested session.
                self.conn.shape_rectangles(
                    SO::SET,
                    SK::INPUT,
                    ClipOrdering::UNSORTED,
                    window,
                    0,
                    0,
                    &[],
                )?;
                self.conn.flush()?;
                Ok(())
            })();
            if let Err(e) = made {
                self.conn.destroy_window(window).ok();
                self.conn.flush().ok();
                return Err(e);
            }
            self.hosts.lock().unwrap().push(Host {
                output: output.to_owned(),
                window,
                shown: None,
            });
            Ok(window)
        }

        /// Shows the window of `output` at `area`, if the X server has the output there, alone,
        /// now; hides it otherwise, or for none.
        pub fn place(&mut self, output: &str, area: Option<Area>) {
            let mut hosts = self.hosts.lock().unwrap();
            let Some(host) = hosts.iter_mut().find(|h| h.output == output) else {
                return;
            };
            // Under the lock the watch hides by: a change after this look is the watch's to see.
            // Without the watch, nothing shows.
            let area = if self.watching() {
                keep(output, area, layout(&self.conn, self.root).ok().as_deref())
            } else {
                None
            };
            // One shown is left as it is, rather than raised again: others' unmanaged windows (a
            // lock screen, an on-screen display) are not fought for the top. One to hide is
            // hidden again whatever it is thought to be, which costs nothing.
            if area.is_some() && area == host.shown {
                return;
            }
            host.shown = match show(&self.conn, host.window, area) {
                Ok(()) => area,
                Err(e) => {
                    log::warn!("Failed to place the nested session of {}: {}", output, e);
                    self.conn.unmap_window(host.window).ok();
                    self.conn.flush().ok();
                    None
                }
            };
        }

        /// Gets rid of the window of `output`.
        pub fn destroy(&mut self, output: &str) {
            let mut hosts = self.hosts.lock().unwrap();
            if let Some(i) = hosts.iter().position(|h| h.output == output) {
                let host = hosts.remove(i);
                self.conn.destroy_window(host.window).ok();
                self.conn.flush().ok();
            }
        }

        fn atom(&self, name: &[u8]) -> ResultType<u32> {
            Ok(self.conn.intern_atom(false, name)?.reply()?.atom)
        }
    }

    impl Drop for Hosts {
        // Ends the watch; the windows go with this connection.
        fn drop(&mut self) {
            self.watching.store(false, Ordering::SeqCst);
            unsafe {
                libc::shutdown(self.watch.stream().as_raw_fd(), libc::SHUT_RDWR);
            }
        }
    }

    // Shown at `area`, the size given to `Xephyr`'s window in it too, which resizes the nested
    // screen (it follows its own window, not ours); or hidden.
    fn show(conn: &RustConnection, window: u32, area: Option<Area>) -> ResultType<()> {
        match area {
            None => {
                conn.unmap_window(window)?;
            }
            Some((x, y, w, h)) => {
                let (w, h) = (side(w) as u32, side(h) as u32);
                let aux = ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .width(w)
                    .height(h)
                    .stack_mode(StackMode::ABOVE);
                conn.configure_window(window, &aux)?;
                if let Some(&nested) = conn.query_tree(window)?.reply()?.children.first() {
                    let aux = ConfigureWindowAux::new().x(0).y(0).width(w).height(h);
                    conn.configure_window(nested, &aux)?;
                }
                conn.map_window(window)?;
            }
        }
        // Done by the X server before the lock is let go, so it is not done after a hide the
        // watch sends meanwhile on its connection.
        conn.get_input_focus()?.reply()?;
        // No events are asked for; what comes is errors of this connection's requests, not read
        // otherwise.
        while let Ok(Some(_)) = conn.poll_for_event() {}
        Ok(())
    }

    // Hides a shown window the moment the layout no longer has its output there, alone: RandR
    // tells of every change of it. Ends with the connection: `Hosts` going, or the X server; then
    // nothing shows any more (`watching`).
    fn hide_on_change(
        conn: &RustConnection,
        root: u32,
        hosts: &Mutex<Vec<Host>>,
        watching: &AtomicBool,
    ) {
        let end = loop {
            if let Err(e) = conn.wait_for_event() {
                break e;
            }
            // One look for a burst of changes.
            while let Ok(Some(_)) = conn.poll_for_event() {}
            let mut hosts = hosts.lock().unwrap();
            // A layout that cannot be read hides everything.
            let lit = layout(conn, root).ok();
            let mut hid = false;
            for host in hosts.iter_mut() {
                if host.shown.is_some() && keep(&host.output, host.shown, lit.as_deref()).is_none()
                {
                    conn.unmap_window(host.window).ok();
                    host.shown = None;
                    hid = true;
                    log::info!(
                        "Hid the nested session of {}: the layout changed under it",
                        host.output
                    );
                }
            }
            if hid {
                conn.flush().ok();
                // Done before the lock is let go (see `show`).
                if let Ok(cookie) = conn.get_input_focus() {
                    cookie.reply().ok();
                }
            }
        };
        if watching.swap(false, Ordering::SeqCst) {
            log::warn!(
                "Stopped watching the layout for nested sessions ({}): none shows any more",
                end
            );
        }
    }

    // The outputs that are on, and where, as the X server has them now.
    fn layout(conn: &RustConnection, root: u32) -> ResultType<Vec<(String, Area)>> {
        let resources = conn.randr_get_screen_resources_current(root)?.reply()?;
        let mut lit = Vec::new();
        for crtc in resources.crtcs {
            let info = conn
                .randr_get_crtc_info(crtc, resources.config_timestamp)?
                .reply()?;
            if info.mode == 0 {
                continue;
            }
            let area = (
                info.x as i32,
                info.y as i32,
                info.width as i32,
                info.height as i32,
            );
            for output in info.outputs {
                let name = conn
                    .randr_get_output_info(output, resources.config_timestamp)?
                    .reply()?
                    .name;
                lit.push((String::from_utf8_lossy(&name).into_owned(), area));
            }
        }
        Ok(lit)
    }

    fn side(n: i32) -> u16 {
        n.clamp(1, u16::MAX as i32) as u16
    }
}

// Nested sessions need an X11 desktop, which only Linux has here: no window is ever made.
#[cfg(not(target_os = "linux"))]
mod imp {
    use super::Area;
    use hbb_common::{bail, ResultType};

    pub enum Hosts {}

    impl Hosts {
        pub fn new() -> ResultType<Self> {
            bail!("Nested sessions need an X11 desktop")
        }

        pub fn watching(&self) -> bool {
            match *self {}
        }

        pub fn create(&mut self, _output: &str, _area: Area) -> ResultType<u32> {
            match *self {}
        }

        pub fn place(&mut self, _output: &str, _area: Option<Area>) {
            match *self {}
        }

        pub fn destroy(&mut self, _output: &str) {
            match *self {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_shows_only_where_its_output_shows_alone_now() {
        let hdmi = ("HDMI-1".to_owned(), (0, 0, 1920, 1080));
        let ours = |area: Area| ("Virtual-1-1".to_owned(), area);
        let beside = (1920, 0, 2048, 1152);
        let lit = [hdmi.clone(), ours(beside)];
        assert_eq!(keep("Virtual-1-1", Some(beside), Some(&lit)), Some(beside));
        // Asked where the output no longer is: moved, resized, or put over the monitor.
        for now in [
            (2000, 0, 2048, 1152),
            (1920, 0, 1920, 1080),
            (0, 0, 2048, 1152),
        ] {
            let lit = [hdmi.clone(), ours(now)];
            assert_eq!(
                keep("Virtual-1-1", Some(beside), Some(&lit)),
                None,
                "{:?}",
                now
            );
        }
        // Off, a layout that cannot be read, or nothing asked: nothing shows.
        assert_eq!(
            keep("Virtual-1-1", Some(beside), Some(&[hdmi.clone()])),
            None
        );
        assert_eq!(keep("Virtual-1-1", Some(beside), None), None);
        assert_eq!(keep("Virtual-1-1", None, Some(&lit)), None);
    }
}
