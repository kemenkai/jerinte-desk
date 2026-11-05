//! The nested session of a `Mode::Nested` virtual display: an `Xephyr` X server shown over the
//! virtual display's output, in a window of ours there (see `host`), with a small window manager
//! of its own inside it. The person at the machine never sees it (the output has no monitor, and
//! the window shows only where the output shows it alone), and it has its own pointer, keyboard
//! and focus, so the controlling side works it without touching the shared desktop.
//!
//! Everything here is a child of `--server`: a process group of its own, killed when the display
//! goes, with `PR_SET_PDEATHSIG` as a backstop should `--server` die without cleaning up. The
//! controlling side's input reaches the nested server through `XTEST` (see `server::input_service`
//! `nested`); its screen is captured as part of the shared desktop, since `Xephyr` draws into the
//! output's pixels through our window.

use super::host::{Area, Hosts};
use hbb_common::{bail, log, ResultType};
use std::{
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

/// Starts and stops the nested session shown on an output. A fake stands in for it in tests, so
/// the backend's orchestration is exercised without a real `Xephyr`.
pub trait Nest: Send {
    /// Starts a nested session for RandR output `output`, the size of `area`, the output's, and
    /// hidden until placed. Returns the X display it runs on, e.g. `:9`.
    fn start(&mut self, output: &str, area: Area) -> ResultType<String>;
    /// Shows the nested session of `output` over `area`, the output's, at its size; hides it for
    /// none.
    fn place(&mut self, output: &str, area: Option<Area>);
    /// The outputs whose nested session ended on its own, since last asked; gone from here.
    fn ended(&mut self) -> Vec<String>;
    /// Stops the nested session on `output`. One that is not running counts as stopped.
    fn stop(&mut self, output: &str);
    /// Stops every nested session, e.g. at shutdown.
    fn stop_all(&mut self);
}

/// The window manager run inside the nested server. Openbox gives a root menu to launch programs
/// and manages their windows; without it the nested session has no focus handling. Overridable by
/// the environment for a site that wants another one.
const DEFAULT_WM: &str = "openbox";
const WM_ENV: &str = "RUSTDESK_NESTED_WM";
// Display numbers tried for a nested server, past the ones a desktop usually holds.
const DISPLAY_RANGE: std::ops::Range<u32> = 8..64;
// How long to wait for `Xephyr` to come up (to take a client) before giving up.
const UP_TIMEOUT: Duration = Duration::from_secs(5);
const UP_POLL: Duration = Duration::from_millis(50);
// How long one try to reach it waits for its answer.
const ANSWER_TIMEOUT: Duration = Duration::from_millis(500);
// How long a stopped session has to exit on its own before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(1);
// How much of what a nested server printed goes to the log when it does not come up.
const SAID_LINES: usize = 8;

/// A running nested session: the `Xephyr` server and the window manager inside it.
struct Session {
    output: String,
    display: String,
    children: Vec<Child>,
}

impl Session {
    // Asks the children to go, then SIGKILLs what is left. They are each in their own process
    // group (`setsid`), so this also takes whatever they started inside the nested session.
    // SIGTERM first: an X server that exits on its own removes its lock file and socket, which
    // would otherwise hold its display number until something clears them.
    fn kill(&mut self) {
        for child in &self.children {
            signal_group(child, hbb_common::libc::SIGTERM);
        }
        let deadline = Instant::now() + STOP_GRACE;
        for child in &mut self.children {
            while Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                std::thread::sleep(UP_POLL);
            }
        }
        for child in &mut self.children {
            signal_group(child, hbb_common::libc::SIGKILL);
            child.kill().ok();
            child.wait().ok();
        }
        // Once: dropping it after a stop signals nothing more.
        self.children.clear();
    }

    // Whether its `Xephyr` exited on its own, and how.
    fn ended(&mut self) -> Option<std::process::ExitStatus> {
        self.children.first_mut()?.try_wait().ok().flatten()
    }
}

// Negative pid: the whole group. The children lead their own groups.
fn signal_group(child: &Child, signal: i32) {
    unsafe {
        hbb_common::libc::kill(-(child.id() as i32), signal);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.kill();
    }
}

/// The real nest, running `Xephyr`.
#[derive(Default)]
pub struct XephyrNest {
    sessions: Vec<Session>,
    /// The windows they show in, once the first is started.
    hosts: Option<Hosts>,
}

impl XephyrNest {
    // The windows, connected anew when none is in use and the old connection is gone or no
    // longer watched (the X server restarted, say): with it nothing would ever show.
    fn hosts(&mut self) -> ResultType<&mut Hosts> {
        let usable = self.hosts.as_ref().map_or(false, |h| h.watching());
        if !usable && self.sessions.is_empty() {
            self.hosts = None;
            self.hosts = Some(Hosts::new()?);
        }
        self.hosts
            .as_mut()
            .ok_or_else(|| hbb_common::anyhow::anyhow!("No windows for nested sessions"))
    }

    // `Xephyr` in `window`, up, with its window manager.
    fn run(&mut self, output: &str, window: u32) -> ResultType<String> {
        let display = free_display().ok_or_else(|| {
            hbb_common::anyhow::anyhow!("No free X display for a nested session")
        })?;
        let said = said_path(&display);
        let xephyr = spawn_xephyr(&display, window, said.as_deref())?;
        // Held in a session from here on, so an early return drops it and kills what was started.
        let mut session = Session {
            output: output.to_owned(),
            display: display.clone(),
            children: vec![xephyr],
        };
        wait_until_up(&display, &mut session.children[0], said.as_deref())?;
        match spawn_wm(&display) {
            Ok(wm) => session.children.push(wm),
            // No window manager is a poor nested session, not a failed one: windows go unmanaged,
            // but the controlling side still has the display.
            Err(e) => log::warn!("Nested session {} has no window manager: {}", display, e),
        }
        log::info!("Nested session {} on {}", display, output);
        self.sessions.push(session);
        Ok(display)
    }
}

impl Nest for XephyrNest {
    fn start(&mut self, output: &str, area: Area) -> ResultType<String> {
        self.stop(output);
        let window = match self.hosts()?.create(output, area) {
            Ok(window) => window,
            Err(e) => {
                // A connection that cannot make a window is made anew next time, if it can be.
                if self.sessions.is_empty() {
                    self.hosts = None;
                }
                return Err(e);
            }
        };
        let started = self.run(output, window);
        if started.is_err() {
            if let Some(hosts) = self.hosts.as_mut() {
                hosts.destroy(output);
            }
        }
        started
    }

    fn place(&mut self, output: &str, area: Option<Area>) {
        if let Some(hosts) = self.hosts.as_mut() {
            hosts.place(output, area);
        }
    }

    fn ended(&mut self) -> Vec<String> {
        let mut ended = Vec::new();
        self.sessions.retain_mut(|s| match s.ended() {
            Some(status) => {
                log::warn!(
                    "Nested X server {} on {} exited ({})",
                    s.display,
                    s.output,
                    status
                );
                // Its window manager, and what ran in it, with it.
                s.kill();
                ended.push(s.output.clone());
                false
            }
            None => true,
        });
        if let Some(hosts) = self.hosts.as_mut() {
            for output in &ended {
                hosts.destroy(output);
            }
        }
        ended
    }

    // Hidden first, so nothing of a session going shows; its window gone last.
    fn stop(&mut self, output: &str) {
        if let Some(hosts) = self.hosts.as_mut() {
            hosts.place(output, None);
        }
        self.sessions.retain_mut(|s| {
            if s.output == output {
                log::info!("Stopping nested session {} on {}", s.display, output);
                s.kill();
                false
            } else {
                true
            }
        });
        if let Some(hosts) = self.hosts.as_mut() {
            hosts.destroy(output);
        }
    }

    fn stop_all(&mut self) {
        let outputs: Vec<String> = self.sessions.iter().map(|s| s.output.clone()).collect();
        for output in outputs {
            self.stop(&output);
        }
    }
}

impl Drop for XephyrNest {
    fn drop(&mut self) {
        self.stop_all();
    }
}

// `Xephyr :N -parent <window>`: one screen, the size of our window, drawn in a window of its own
// inside it, at its corner. It never touches ours, and resizes its screen when its own window is
// resized (see `host`). Nothing else may define the screen: `-output` makes a window the
// desktop's window manager places (deepin-kwin put it full-screen on the physical monitor),
// `-fullscreen` covers the whole desktop, and a second screen (`-screen`) crashes it at start.
// `-no-host-grab`: Ctrl+Shift in its window would otherwise take the whole desktop's keyboard and
// pointer; the controlling side's input goes in through XTEST and needs no grab. `-glamor`
// accelerates it with the host GPU; a build without it refuses the option, which shows as the
// server not coming up.
fn spawn_xephyr(display: &str, parent: u32, said: Option<&Path>) -> ResultType<Child> {
    let mut cmd = Command::new("Xephyr");
    cmd.arg(display)
        .args(["-parent", &parent.to_string()])
        .arg("-no-host-grab")
        .arg("-glamor")
        .args(["-title", "rustdesk-nested"])
        .arg("-nolisten")
        .arg("tcp");
    own_group(&mut cmd);
    spawn(cmd, "Xephyr", said)
}

// A clean, separate session for the nested window manager: its own session bus (so it does not
// fight the desktop's notifications, single-instance apps or systemd --user), only `DISPLAY` set
// to the nested server.
fn spawn_wm(display: &str) -> ResultType<Child> {
    let wm = std::env::var(WM_ENV).unwrap_or_else(|_| DEFAULT_WM.to_owned());
    let mut cmd = Command::new("dbus-run-session");
    cmd.args(["--", &wm])
        .env_clear()
        .env("DISPLAY", display)
        .env("HOME", std::env::var_os("HOME").unwrap_or_default());
    own_group(&mut cmd);
    spawn(cmd, &wm, None)
}

// What it prints on stderr goes to `said` where given, else nowhere.
fn spawn(mut cmd: Command, what: &str, said: Option<&Path>) -> ResultType<Child> {
    let stderr = said
        .and_then(open_said)
        .map_or_else(Stdio::null, Stdio::from);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .map_err(|e| hbb_common::anyhow::anyhow!("Failed to start {}: {}", what, e))
}

// Where a nested server's complaints are kept, for when it does not come up: the user's runtime
// directory, which only they can write to. Nowhere without one, or as root, which would write
// into a directory someone else may hold.
fn said_path(display: &str) -> Option<PathBuf> {
    if unsafe { hbb_common::libc::geteuid() } == 0 {
        return None;
    }
    let dir = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    if !dir.is_absolute() {
        return None;
    }
    Some(dir.join(format!(
        "rustdesk-nested-{}.log",
        display.trim_start_matches(':')
    )))
}

// Readable by the user only, and never through a link left in its place.
fn open_said(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .custom_flags(hbb_common::libc::O_NOFOLLOW)
        .open(path)
        .ok()
}

// The last lines a nested server printed, for the log.
fn last_said(said: Option<&Path>) -> String {
    let Some(text) = said.and_then(|path| std::fs::read_to_string(path).ok()) else {
        return String::new();
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let last = &lines[lines.len().saturating_sub(SAID_LINES)..];
    if last.is_empty() {
        String::new()
    } else {
        format!(": {}", last.join(" | "))
    }
}

// Its own process group, and death with `--server` as a backstop: `PR_SET_PDEATHSIG` fires when
// the thread that started it ends, here `--server`'s virtual display worker, which lives as long
// as `--server` does; so it is a real backstop. The main cleanup is still that `--server` kills
// the group on its way out or at next start.
#[cfg(target_os = "linux")]
fn own_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            hbb_common::libc::setsid();
            hbb_common::libc::prctl(hbb_common::libc::PR_SET_PDEATHSIG, hbb_common::libc::SIGKILL);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn own_group(_cmd: &mut Command) {}

// The first free display in the range. A race with another server taking the same number right
// after is why `Xephyr` is also checked to come up.
fn free_display() -> Option<String> {
    DISPLAY_RANGE
        .map(|n| format!(":{}", n))
        .find(|d| is_free(d))
}

// Free: no running server has the number. A lock left by one that died (a crash, a SIGKILL) does
// not count: `Xephyr` takes such a lock over, and binds over the socket left with it. Anything
// unclear (a lock it cannot read, a socket without a lock) counts as taken.
fn is_free(display: &str) -> bool {
    let n = display.trim_start_matches(':');
    match std::fs::read_to_string(format!("/tmp/.X{}-lock", n)) {
        Ok(lock) => lock.trim().parse::<i32>().map_or(false, |pid| !alive(pid)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => !Path::new(&socket(display)).exists(),
        Err(_) => false,
    }
}

// As the X server judges a lock: its process is there unless signalling it says there is none.
fn alive(pid: i32) -> bool {
    pid > 0
        && (unsafe { hbb_common::libc::kill(pid, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() != Some(hbb_common::libc::ESRCH))
}

fn socket(display: &str) -> String {
    format!("/tmp/.X11-unix/X{}", display.trim_start_matches(':'))
}

// Whether the server on `socket` takes a client. It answers one only once fully set up, unlike
// its socket, which is there before its screens are (and stays should setting them up fail).
fn answers(socket: &str) -> bool {
    use std::io::{Read, Write};
    let Ok(mut stream) = std::os::unix::net::UnixStream::connect(socket) else {
        return false;
    };
    stream.set_read_timeout(Some(ANSWER_TIMEOUT)).ok();
    // Connection setup: little-endian, protocol 11.0, no authorization (none is set up).
    let setup = [b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut status = [0u8];
    // 1 is Success; 0 Failed, 2 Authenticate.
    stream.write_all(&setup).is_ok() && stream.read_exact(&mut status).is_ok() && status[0] == 1
}

fn wait_until_up(display: &str, xephyr: &mut Child, said: Option<&Path>) -> ResultType<()> {
    let socket = socket(display);
    let deadline = Instant::now() + UP_TIMEOUT;
    while Instant::now() < deadline {
        if answers(&socket) {
            return Ok(());
        }
        // Gone already: nothing to wait out.
        if let Ok(Some(status)) = xephyr.try_wait() {
            log::warn!(
                "Nested X server {} exited ({}){}",
                display,
                status,
                last_said(said)
            );
            bail!("Nested X server {} did not come up", display);
        }
        std::thread::sleep(UP_POLL);
    }
    log::warn!(
        "Nested X server {} did not come up in {:?}{}",
        display,
        UP_TIMEOUT,
        last_said(said)
    );
    bail!("Nested X server {} did not come up", display)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn a_lock_counts_while_its_process_lives() {
        assert!(alive(std::process::id() as i32));
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id() as i32;
        child.wait().unwrap();
        assert!(!alive(pid));
        assert!(!alive(0));
    }

    #[test]
    fn a_server_is_up_once_it_accepts_a_client() {
        // What a server answers the connection setup with, if anything, and whether it is up.
        for (answer, up) in [(Some(1u8), true), (Some(0), false), (Some(2), false), (None, false)] {
            let path = std::env::temp_dir().join(format!(
                "rd-nested-{}-{:?}",
                std::process::id(),
                answer
            ));
            std::fs::remove_file(&path).ok();
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            let server = std::thread::spawn(move || {
                let (mut client, _) = listener.accept().unwrap();
                let mut setup = [0u8; 12];
                client.read_exact(&mut setup).unwrap();
                match answer {
                    Some(status) => client.write_all(&[status]).unwrap(),
                    // Still setting up: answers nothing for longer than a try waits.
                    None => std::thread::sleep(ANSWER_TIMEOUT * 2),
                }
                setup
            });
            assert_eq!(answers(path.to_str().unwrap()), up, "{:?}", answer);
            // Little-endian, protocol 11.0, then no authorization.
            assert_eq!(server.join().unwrap(), [b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            std::fs::remove_file(&path).ok();
        }
        // Nothing listening: not up, and no wait.
        assert!(!answers("/nonexistent/rd-nested-socket"));
    }
}
