//! Tries scrap's capture through Mutter against a running GNOME (or headless Mutter) session, as
//! the user the session belongs to:
//!   cargo run -p scrap --features wayland --example mutter_probe -- [WxH] [W2xH2]
//! Records the physical monitors and takes frames through two capturers in turn (each with a
//! pipeline of its own, and a frame at once even of a static screen), makes a virtual monitor of
//! WxH (default 1600x900), captures it, resizes it to W2xH2 (default 1280x720) under its capturer,
//! which then fails and is built again, resizes it back with no capturer about, removes it, and
//! stops recording the physical monitors under their capturer, which then fails too. Prints what
//! it sees; `FAIL` marks the unexpected.

#[cfg(target_os = "linux")]
fn main() {
    linux::main()
}

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
mod linux {
    use dbus::blocking::SyncConnection;
    use scrap::{
        wayland::{
            mutter::{self, DisplayState, VIRTUAL_PRODUCT},
            mutter_capture::{self, Recording, VirtualMonitor},
        },
        Capturer, Display, Frame, TraitCapturer, TraitPixelBuffer,
    };
    use std::time::{Duration, Instant};

    fn size_arg(n: usize, default: (i32, i32)) -> (i32, i32) {
        std::env::args()
            .nth(n)
            .and_then(|s| {
                let (w, h) = s.split_once('x')?;
                Some((w.parse().ok()?, h.parse().ok()?))
            })
            .unwrap_or(default)
    }

    fn check(what: &str, ok: bool) {
        println!("== {} {}", if ok { "ok  " } else { "FAIL" }, what);
    }

    pub fn main() {
        let size = size_arg(1, (1600, 900));
        let resized = size_arg(2, (1280, 720));
        let conn = SyncConnection::new_session().unwrap();
        check("Mutter available", mutter::available(&conn));
        println!("== monitor config over D-Bus allowed: {}", mutter::apply_allowed(&conn));
        let before = mutter::current_state(&conn).unwrap();
        show("before", &before);

        let start = Instant::now();
        mutter_capture::record_physical(&before, &[]).unwrap();
        println!("== physical recorded in {:?}", start.elapsed());
        let recording = mutter_capture::physical_recording(&before);
        println!("== recording: {:?}", recording);
        check("physical recorded", matches!(recording, Recording::Live(_)));
        let physical = first(&before, |d| !d.name().is_empty());
        let name = physical.name();
        let want = (physical.width(), physical.height());
        println!("== physical {} {:?} at {:?}", name, want, physical.origin());
        let mut capturer = Capturer::new(physical).unwrap();
        check("first capturer frame", frame(&mut capturer, 3000) == Some(want));
        drop(capturer);
        // The next builds a pipeline of its own: a frame comes at once, screen static.
        let start = Instant::now();
        let mut capturer = Capturer::new(first(&before, |d| d.name() == name)).unwrap();
        check("second capturer frame", frame(&mut capturer, 1000) == Some(want));
        println!("== in {:?}", start.elapsed());

        let start = Instant::now();
        let mut virt = VirtualMonitor::create(size.0, size.1).unwrap();
        println!("== virtual made in {:?}", start.elapsed());
        let (connector, state) = wait_for(&conn, &before, size).expect("no virtual monitor");
        println!("== virtual monitor {}", connector);
        show("with virtual", &state);
        virt.show_as(&connector);
        let mut vcap = Capturer::new(first(&state, |d| d.name() == connector)).unwrap();
        let want_v = (size.0 as usize, size.1 as usize);
        check("virtual frame", frame(&mut vcap, 3000) == Some(want_v));
        check(
            "physical capture survives the virtual monitor",
            frame(&mut capturer, 100).is_some() || frame_err(&mut capturer).is_none(),
        );

        // As in the server: the resize on a thread of its own, the capturer capturing meanwhile
        // until it fails, and gives the pipeline back.
        let start = Instant::now();
        let virt = std::sync::Arc::new(virt);
        let resizing = {
            let virt = virt.clone();
            std::thread::spawn(move || virt.resize(resized.0, resized.1))
        };
        let err = frame_err(&mut vcap);
        println!("== old virtual capturer: {:?} after {:?}", err, start.elapsed());
        check("old virtual capturer fails", err.is_some());
        drop(vcap);
        let done = resizing.join().unwrap();
        println!("== resize asked in {:?}: {:?}", start.elapsed(), done);
        check("resize through the capturer", matches!(done, Ok(true)));
        let virt = std::sync::Arc::try_unwrap(virt).ok().unwrap();
        // As the video service does: built again, the capturer settles the new size.
        let mut vcap = Capturer::new(first(&state_now(&conn), |d| d.name() == connector)).unwrap();
        check(
            "resized frame",
            frame(&mut vcap, 3000) == Some((resized.0 as usize, resized.1 as usize)),
        );
        let state = wait_until(&conn, |s| {
            s.monitor(&connector)
                .and_then(|m| m.current_mode())
                .map(|m| (m.width, m.height))
                == Some(resized)
        })
        .expect("not resized");
        show("resized", &state);
        drop(vcap);

        // Back, nobody capturing: settled by a pipeline of its own.
        let start = Instant::now();
        let done = virt.resize(size.0, size.1);
        println!("== resized alone in {:?}: {:?}", start.elapsed(), done);
        check("resize with no capturer", matches!(done, Ok(false)));
        let state = wait_until(&conn, |s| {
            s.monitor(&connector)
                .and_then(|m| m.current_mode())
                .map(|m| (m.width, m.height))
                == Some(size)
        });
        check("resized back", state.is_some());
        let state = state.unwrap_or(state_now(&conn));
        let mut vcap = Capturer::new(first(&state, |d| d.name() == connector)).unwrap();
        check("frame after resizing back", frame(&mut vcap, 3000) == Some(want_v));
        drop(vcap);

        drop(virt);
        let state = wait_until(&conn, |s| s.monitor(&connector).is_none()).expect("not removed");
        show("removed", &state);
        println!("== physical: {:?}", mutter_capture::physical_recording(&state));

        mutter_capture::stop_physical();
        let err = frame_err(&mut capturer);
        println!("== physical capturer after stop: {:?}", err);
        check("physical capturer fails", err.is_some());
        check(
            "nothing recorded",
            mutter_capture::physical_recording(&state) == Recording::Not,
        );
        drop(capturer);
        std::thread::sleep(Duration::from_millis(500));
        show("after", &mutter::current_state(&conn).unwrap());
    }

    fn state_now(conn: &SyncConnection) -> DisplayState {
        mutter::current_state(conn).unwrap()
    }

    fn first(state: &DisplayState, pick: impl Fn(&Display) -> bool) -> Display {
        mutter_capture::displays(state)
            .into_iter()
            .find(|d| pick(d))
            .expect("no such display")
    }

    // The size of a frame, waiting up to `ms`.
    fn frame(c: &mut Capturer, ms: u64) -> Option<(usize, usize)> {
        let end = Instant::now() + Duration::from_millis(ms);
        loop {
            match c.frame(Duration::from_millis(33)) {
                Ok(Frame::PixelBuffer(p)) => return Some((p.width(), p.height())),
                Ok(_) => return None,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    println!("== frame error: {}", e);
                    return None;
                }
            }
            if Instant::now() >= end {
                return None;
            }
        }
    }

    // The error a capturer fails with within a second, if any.
    fn frame_err(c: &mut Capturer) -> Option<String> {
        let end = Instant::now() + Duration::from_secs(1);
        while Instant::now() < end {
            match c.frame(Duration::from_millis(33)) {
                Err(e) if e.kind() != std::io::ErrorKind::WouldBlock => return Some(e.to_string()),
                _ => {}
            }
        }
        None
    }

    fn wait_until(conn: &SyncConnection, done: impl Fn(&DisplayState) -> bool) -> Option<DisplayState> {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            let state = mutter::current_state(conn).ok()?;
            if done(&state) {
                return Some(state);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        None
    }

    // The virtual monitor of `size` that was not there `before`.
    fn wait_for(
        conn: &SyncConnection,
        before: &DisplayState,
        size: (i32, i32),
    ) -> Option<(String, DisplayState)> {
        let state = wait_until(conn, |s| new_one(before, s, size).is_some())?;
        Some((new_one(before, &state, size)?, state))
    }

    fn new_one(before: &DisplayState, now: &DisplayState, size: (i32, i32)) -> Option<String> {
        now.monitors
            .iter()
            .find(|m| {
                before.monitor(&m.connector).is_none()
                    && m.product == VIRTUAL_PRODUCT
                    && m.current_mode().map(|c| (c.width, c.height)) == Some(size)
            })
            .map(|m| m.connector.clone())
    }

    fn show(label: &str, s: &DisplayState) {
        let shown: Vec<_> = s
            .shown()
            .iter()
            .map(|d| (d.connector.clone(), d.position, d.physical, d.primary))
            .collect();
        println!("== {}: serial={} shown={:?}", label, s.serial, shown);
    }
}
