//! The kernel settings the root service changes, through sysfs and debugfs, and what it reads of
//! the cards through their device nodes.

use super::{
    connectors::{self, Connector},
    drm, input,
    leases::{IntelDisplay, Kernel, Lit, Taken, XServer},
};
use hbb_common::{bail, ResultType};
use std::path::{Path, PathBuf};

/// Where the loader of vkms at boot leaves word of what it did (see `vkms`).
pub const VKMS_BOOT: &str = "vkms-boot";

pub struct SysfsKernel {
    drm: PathBuf,
    dev: PathBuf,
    param: PathBuf,
    firmware_path: PathBuf,
    debugfs: PathBuf,
    /// Our files, the state and the firmware directory in it.
    run: PathBuf,
    proc: PathBuf,
    /// The lock on the keyboards and pointers here, started the first time it is wanted.
    input: Option<input::Lock>,
    // Started once: a lock that would not start is said so once, not at every look.
    input_tried: bool,
}

impl SysfsKernel {
    pub fn system() -> Self {
        Self {
            drm: "/sys/class/drm".into(),
            dev: "/dev/dri".into(),
            param: "/sys/module/drm/parameters/edid_firmware".into(),
            firmware_path: "/sys/module/firmware_class/parameters/path".into(),
            debugfs: "/sys/kernel/debug/dri".into(),
            run: "/run/rustdesk".into(),
            proc: "/proc".into(),
            input: None,
            input_tried: false,
        }
    }

    #[cfg(test)]
    fn under(root: &Path) -> Self {
        Self {
            drm: root.join("drm"),
            dev: root.join("dev"),
            param: root.join("module/edid_firmware"),
            firmware_path: root.join("module/firmware_class_path"),
            debugfs: root.join("debug"),
            run: root.join("run/rustdesk"),
            proc: root.join("proc"),
            input: None,
            input_tried: false,
        }
    }

    fn state(&self) -> PathBuf {
        self.run.join("vdisplay-lit.json")
    }

    fn taken_state(&self) -> PathBuf {
        self.run.join("vdisplay-taken.json")
    }

    fn connector_dir(&self, card: &str, name: &str) -> PathBuf {
        self.drm.join(format!("{}-{}", card, name))
    }

    // debugfs names a card by its minor: card0 is dri/0.
    fn debugfs_dir(&self, card: &str) -> Option<PathBuf> {
        let minor = card.strip_prefix("card")?;
        minor
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| self.debugfs.join(minor))
    }

    fn card(&self, card: &str) -> Option<drm::Card> {
        if !card.starts_with("card") || card.contains('/') {
            return None;
        }
        drm::Card::open(&self.dev.join(card)).ok()
    }
}

impl Kernel for SysfsKernel {
    fn connectors(&self) -> Vec<Connector> {
        connectors::scan(&self.drm)
    }

    fn release(&self) -> String {
        read_trimmed(&self.proc.join("sys/kernel/osrelease")).unwrap_or_default()
    }

    fn cmdline(&self) -> String {
        read_trimmed(&self.proc.join("cmdline")).unwrap_or_default()
    }

    fn edid_param(&self) -> ResultType<String> {
        read_trimmed(&self.param)
    }

    fn set_edid_param(&mut self, value: &str) -> ResultType<()> {
        write_param(&self.param, value)
    }

    fn firmware_path(&self) -> ResultType<String> {
        read_trimmed(&self.firmware_path)
    }

    fn set_firmware_path(&mut self, value: &str) -> ResultType<()> {
        write_param(&self.firmware_path, value)
    }

    fn our_firmware_dir(&self) -> String {
        self.run.join("firmware").to_string_lossy().into_owned()
    }

    fn put_firmware(&mut self, name: &str, bytes: Option<&[u8]>) -> ResultType<()> {
        let path = Path::new(&self.our_firmware_dir()).join(name);
        match bytes {
            Some(bytes) => write_private(&path, bytes),
            None => match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            },
        }
    }

    fn set_status(&mut self, card: &str, name: &str, status: &str) -> ResultType<()> {
        Ok(std::fs::write(
            self.connector_dir(card, name).join("status"),
            status,
        )?)
    }

    // A "change" event of the card, as a hotplug sends; the X server's modesetting driver looks
    // at its connectors again, and its udev configuration leaves the card where it is.
    fn hotplug(&mut self, card: &str) -> ResultType<()> {
        if !card.starts_with("card") || card.contains('/') {
            bail!("not a card: {}", card);
        }
        Ok(std::fs::write(self.drm.join(card).join("uevent"), "change")?)
    }

    fn modes(&self, card: &str, name: &str) -> Vec<String> {
        std::fs::read_to_string(self.connector_dir(card, name).join("modes"))
            .map(|modes| modes.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn has_edid(&self, card: &str, name: &str) -> bool {
        std::fs::read(self.connector_dir(card, name).join("edid"))
            .map_or(false, |edid| !edid.is_empty())
    }

    fn override_edid(&mut self, card: &str, name: &str, edid: Option<&[u8]>) -> ResultType<()> {
        let Some(dir) = self.debugfs_dir(card) else {
            bail!("not a card: {}", card);
        };
        let path = dir.join(name).join("edid_override");
        match edid {
            Some(edid) => std::fs::write(&path, edid)?,
            None => std::fs::write(&path, "reset")?,
        }
        Ok(())
    }

    fn crtcs(&self, card: &str) -> Option<usize> {
        self.card(card)?.crtcs().ok()
    }

    fn encoders(&self, card: &str, id: u32) -> Option<Vec<u32>> {
        self.card(card)?.encoders(id).ok()
    }

    // "platform: DG2" in i915_capabilities; "display version: 12" there since 5.15, and in
    // i915_display_capabilities since 6.4; "gen: 9" before.
    fn intel_display(&self, card: &str) -> Option<IntelDisplay> {
        let dir = self.debugfs_dir(card)?;
        let text: String = ["i915_display_capabilities", "i915_capabilities"]
            .iter()
            .filter_map(|file| std::fs::read_to_string(dir.join(file)).ok())
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return None;
        }
        Some(intel_display(&text))
    }

    fn save_lit(&mut self, lit: &[Lit]) -> ResultType<()> {
        if lit.is_empty() {
            match std::fs::remove_file(self.state()) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
                _ => return Ok(()),
            }
        }
        write_private(&self.state(), &serde_json::to_vec(lit)?)
    }

    fn load_lit(&self) -> Vec<Lit> {
        std::fs::read(self.state())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn vkms_word(&self) -> Option<String> {
        read_trimmed(&self.run.join(VKMS_BOOT)).ok()
    }

    // The session's display, and its X authority copied: a file of the process's own user, as
    // its environment names it or in its home. Only a local display.
    fn x_of(&mut self, pid: u32) -> ResultType<XServer> {
        use std::os::unix::fs::MetadataExt;
        let dir = self.proc.join(pid.to_string());
        let owner = std::fs::metadata(&dir)?.uid();
        let env = X11Environment::read(&std::fs::read(dir.join("environ"))?);
        let Some(display) = env.display.filter(|d| local_display(d)) else {
            bail!("process {} has no local X display", pid);
        };
        let xauthority = env
            .xauthority
            .or_else(|| env.home.map(|home| format!("{}/.Xauthority", home)))
            .and_then(|path| read_own_file(Path::new(&path), owner));
        let copy = match xauthority {
            Some(bytes) => {
                let copy = self.run.join(format!("xauth-{}", pid));
                write_private(&copy, &bytes)?;
                Some(copy.to_string_lossy().into_owned())
            }
            None => None,
        };
        Ok(XServer {
            display,
            xauthority: copy,
        })
    }

    fn drop_x(&mut self, x: &XServer) {
        if let Some(copy) = &x.xauthority {
            let copy = Path::new(copy);
            if copy.parent() == Some(self.run.as_path()) {
                std::fs::remove_file(copy).ok();
            }
        }
    }

    // `--current` with arguments, so that only the change is made; none, a probe. Given up on
    // after `XRANDR_TIMEOUT`: an X server that does not answer must not hold the service.
    fn xrandr(&mut self, x: &XServer, args: &[String]) -> ResultType<()> {
        use std::process::{Command, Stdio};
        let mut command = Command::new("xrandr");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("DISPLAY", &x.display)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(copy) = &x.xauthority {
            command.env("XAUTHORITY", copy);
        }
        if !args.is_empty() {
            command.arg("--current").args(args);
        }
        let mut child = command.spawn()?;
        let deadline = std::time::Instant::now() + XRANDR_TIMEOUT;
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    bail!("xrandr {} failed: {}", args.join(" "), status);
                }
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                child.kill().ok();
                child.wait().ok();
                bail!("xrandr {} did not finish", args.join(" "));
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn save_taken(&mut self, taken: &[Taken]) -> ResultType<()> {
        let path = self.taken_state();
        if taken.is_empty() {
            return match std::fs::remove_file(&path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
                _ => Ok(()),
            };
        }
        write_private(&path, &serde_json::to_vec(taken)?)
    }

    fn load_taken(&self) -> Vec<Taken> {
        std::fs::read(self.taken_state())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn lock_input(&mut self, on: bool) {
        if !self.input_tried && on {
            self.input_tried = true;
            #[cfg(target_os = "linux")]
            match input::Lock::start(input::System) {
                Ok(lock) => self.input = Some(lock),
                Err(e) => {
                    hbb_common::log::error!("The keyboards and pointers here will not be kept: {}", e)
                }
            }
        }
        if let Some(lock) = &self.input {
            lock.set(on);
        }
    }

    fn input_escaped(&mut self) -> bool {
        self.input.as_ref().map_or(false, |lock| lock.escaped())
    }

    fn process_start(&self, pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(self.proc.join(pid.to_string()).join("stat")).ok()?;
        stat_start(&stat)
    }

    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    fn end_process(&mut self, pid: u32) {
        #[cfg(target_os = "linux")]
        if unsafe { hbb_common::libc::kill(pid as i32, hbb_common::libc::SIGKILL) } != 0 {
            hbb_common::log::warn!(
                "Failed to end process {}: {}",
                pid,
                std::io::Error::last_os_error()
            );
        }
    }
}

// When a process started, from its /proc/<pid>/stat: the 22nd field, counted after the name in
// parentheses, which may hold spaces and parentheses of its own.
fn stat_start(stat: &str) -> Option<u64> {
    let (_, after_name) = stat.rsplit_once(") ")?;
    // The fields after the name start at the 3rd, the state.
    after_name.split_whitespace().nth(22 - 3)?.parse().ok()
}

// An X authority file is a few hundred bytes.
const MAX_XAUTHORITY: u64 = 64 * 1024;
const XRANDR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

// The bytes of file `path` if it is a plain file of user `owner`, and small: opened once, without
// waiting (a FIFO named there must not hold root), and checked on what was opened, not on the
// name, which could point elsewhere by then.
fn read_own_file(path: &Path, owner: u32) -> Option<Vec<u8>> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt},
    };
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(hbb_common::libc::O_NONBLOCK | hbb_common::libc::O_NOCTTY)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.uid() != owner || meta.len() > MAX_XAUTHORITY {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(MAX_XAUTHORITY).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// What a process's environment says of its X server.
#[derive(Debug, Default, PartialEq)]
struct X11Environment {
    display: Option<String>,
    xauthority: Option<String>,
    home: Option<String>,
}

impl X11Environment {
    // /proc/<pid>/environ: NUL-separated NAME=value.
    fn read(environ: &[u8]) -> Self {
        let mut env = Self::default();
        for entry in environ.split(|b| *b == 0) {
            let entry = String::from_utf8_lossy(entry);
            let Some((name, value)) = entry.split_once('=') else {
                continue;
            };
            let value = Some(value.to_owned()).filter(|v| !v.is_empty());
            match name {
                "DISPLAY" => env.display = value,
                "XAUTHORITY" => env.xauthority = value,
                "HOME" => env.home = value,
                _ => {}
            }
        }
        env
    }
}

// ":0", ":1.0": a display of this machine.
fn local_display(display: &str) -> bool {
    let Some(rest) = display.strip_prefix(':') else {
        return false;
    };
    let (number, screen) = rest.split_once('.').unwrap_or((rest, "0"));
    !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit())
        && !screen.is_empty()
        && screen.bytes().all(|b| b.is_ascii_digit())
}

fn intel_display(text: &str) -> IntelDisplay {
    let value = |key: &str| {
        text.lines()
            .find_map(|line| Some(line.trim().strip_prefix(key)?.trim().to_owned()))
    };
    let number = |key: &str| {
        let value = value(key)?;
        value.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
    };
    IntelDisplay {
        version: number("display version:").or_else(|| number("gen:")),
        platform: value("platform:").unwrap_or_default(),
    }
}

fn read_trimmed(path: &Path) -> ResultType<String> {
    Ok(std::fs::read_to_string(path)?.trim_end().to_owned())
}

// A module parameter takes what is written, a newline included, so none is written; and sysfs
// ignores a write of nothing, so an empty value is written as a NUL, which the parameter reads
// as the end of the string.
fn write_param(path: &Path, value: &str) -> ResultType<()> {
    let bytes: &[u8] = if value.is_empty() {
        b"\0"
    } else {
        value.as_bytes()
    };
    Ok(std::fs::write(path, bytes)?)
}

// In a directory only root reaches, and whole or not at all.
fn write_private(path: &Path, bytes: &[u8]) -> ResultType<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let Some(dir) = path.parent() else {
        bail!("no directory for {}", path.display());
    };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The 22nd field, whatever the name in parentheses holds.
    #[test]
    fn a_process_start_is_read_past_its_name() {
        let tail = "S 1 4242 4242 0 -1 4194560 1234 0 0 0 10 5 0 0 20 0 12 0 98765 1000 300";
        assert_eq!(stat_start(&format!("4242 (rustdesk) {}", tail)), Some(98765));
        assert_eq!(stat_start(&format!("4242 (a) b (c)) {}", tail)), Some(98765));
        assert_eq!(stat_start("4242 (rustdesk) S 1 2"), None);
        assert_eq!(stat_start(""), None);
    }

    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("vdisplay-{}-{}", name, std::process::id()));
            std::fs::remove_dir_all(&root).ok();
            let connector = |entry: &str, status: &str, modes: &str| {
                let dir = root.join("drm").join(entry);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("status"), status).unwrap();
                std::fs::write(dir.join("enabled"), "disabled").unwrap();
                std::fs::write(dir.join("modes"), modes).unwrap();
            };
            connector("card0-DP-1", "disconnected\n", "");
            connector("card0-HDMI-A-1", "connected\n", "1920x1080\n");
            std::fs::create_dir_all(root.join("module")).unwrap();
            std::fs::write(
                root.join("module/edid_firmware"),
                "HDMI-A-1:edid/1920x1080.bin\n",
            )
            .unwrap();
            std::fs::write(root.join("module/firmware_class_path"), "\n").unwrap();
            std::fs::create_dir_all(root.join("debug/0/DP-1")).unwrap();
            std::fs::create_dir_all(root.join("proc/sys/kernel")).unwrap();
            std::fs::write(root.join("proc/sys/kernel/osrelease"), "6.6.127-amd64\n").unwrap();
            std::fs::write(root.join("proc/cmdline"), "quiet video=HDMI-A-1:e\n").unwrap();
            std::fs::write(root.join("drm/card0-HDMI-A-1/edid"), [0u8, 255, 255]).unwrap();
            std::fs::write(root.join("drm/card0-DP-1/edid"), "").unwrap();
            std::fs::create_dir_all(root.join("dev")).unwrap();
            Self(root)
        }

        fn kernel(&self) -> SysfsKernel {
            SysfsKernel::under(&self.0)
        }

        fn read(&self, path: &str) -> String {
            std::fs::read_to_string(self.0.join(path)).unwrap_or_default()
        }
    }

    #[test]
    fn a_process_environment_tells_its_x_server() {
        let env = X11Environment::read(
            b"HOME=/home/user\0DISPLAY=:0\0XAUTHORITY=/home/user/.Xauthority\0LANG=zh_CN.UTF-8\0",
        );
        assert_eq!(env.display.as_deref(), Some(":0"));
        assert_eq!(env.xauthority.as_deref(), Some("/home/user/.Xauthority"));
        assert_eq!(env.home.as_deref(), Some("/home/user"));
        assert_eq!(X11Environment::read(b"DISPLAY=\0"), X11Environment::default());
        for local in [":0", ":10", ":1.0"] {
            assert!(local_display(local), "{}", local);
        }
        for other in ["", ":", "localhost:0", "host:10.0", ":0.", ":a", "0"] {
            assert!(!local_display(other), "{}", other);
        }
    }

    #[test]
    fn root_keeps_its_own_copy_of_a_session_x_authority() {
        let tree = Tree::new("xauth");
        let proc = tree.0.join("proc/4242");
        std::fs::create_dir_all(&proc).unwrap();
        let cookie = tree.0.join("home/.Xauthority");
        std::fs::create_dir_all(cookie.parent().unwrap()).unwrap();
        std::fs::write(&cookie, b"cookie").unwrap();
        std::fs::write(
            proc.join("environ"),
            format!("DISPLAY=:0\0HOME={}\0", cookie.parent().unwrap().display()),
        )
        .unwrap();
        let mut kernel = tree.kernel();
        let x = kernel.x_of(4242).unwrap();
        assert_eq!(x.display, ":0");
        let copy = x.xauthority.clone().unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"cookie");
        assert!(copy.ends_with("run/rustdesk/xauth-4242"));
        // The session's own may go; root's copy stays until dropped.
        std::fs::remove_file(&cookie).unwrap();
        assert!(Path::new(&copy).exists());
        kernel.drop_x(&x);
        assert!(!Path::new(&copy).exists());
        // Without a local display, nothing.
        std::fs::write(proc.join("environ"), "DISPLAY=remote:0\0").unwrap();
        assert!(kernel.x_of(4242).is_err());
        assert!(kernel.x_of(4343).is_err());
    }

    #[test]
    fn only_a_plain_small_file_of_the_user_is_read_and_a_fifo_never_waited_on() {
        use std::os::unix::fs::MetadataExt;
        let tree = Tree::new("ownfile");
        let me = std::fs::metadata(&tree.0).unwrap().uid();
        let plain = tree.0.join("cookie");
        std::fs::write(&plain, b"cookie").unwrap();
        assert_eq!(read_own_file(&plain, me).as_deref(), Some(&b"cookie"[..]));
        assert_eq!(read_own_file(&plain, me + 1), None);
        let big = tree.0.join("big");
        std::fs::write(&big, vec![0u8; MAX_XAUTHORITY as usize + 1]).unwrap();
        assert_eq!(read_own_file(&big, me), None);
        let fifo = tree.0.join("fifo");
        let c = std::ffi::CString::new(fifo.to_string_lossy().as_bytes()).unwrap();
        assert_eq!(unsafe { hbb_common::libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert_eq!(read_own_file(&fifo, me), None);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(read_own_file(&tree.0.join("nothing"), me), None);
    }

    #[test]
    fn desktops_taken_over_are_remembered_by_root_alone() {
        use super::super::protocol::{Back, Restore};
        let tree = Tree::new("taken");
        let mut kernel = tree.kernel();
        assert!(kernel.load_taken().is_empty());
        let taken = vec![Taken {
            restore: Restore {
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
            },
            x: XServer {
                display: ":0".to_owned(),
                xauthority: None,
            },
        }];
        kernel.save_taken(&taken).unwrap();
        assert_eq!(kernel.load_taken(), taken);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(tree.0.join("run/rustdesk/vdisplay-taken.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        kernel.save_taken(&[]).unwrap();
        assert!(kernel.load_taken().is_empty());
        assert!(!tree.0.join("run/rustdesk/vdisplay-taken.json").exists());
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn settings_go_where_the_kernel_reads_them() {
        let tree = Tree::new("settings");
        let mut k = tree.kernel();
        assert_eq!(k.edid_param().unwrap(), "HDMI-A-1:edid/1920x1080.bin");
        k.set_edid_param("HDMI-A-1:edid/1920x1080.bin,DP-1:edid/1024x768.bin")
            .unwrap();
        assert_eq!(
            tree.read("module/edid_firmware"),
            "HDMI-A-1:edid/1920x1080.bin,DP-1:edid/1024x768.bin"
        );
        k.set_status("card0", "DP-1", "on").unwrap();
        assert_eq!(tree.read("drm/card0-DP-1/status"), "on");
        assert_eq!(k.modes("card0", "HDMI-A-1"), vec!["1920x1080"]);
        assert!(k.modes("card0", "DP-1").is_empty());
        k.override_edid("card0", "DP-1", Some(&[1, 2, 3])).unwrap();
        assert_eq!(
            std::fs::read(tree.0.join("debug/0/DP-1/edid_override")).unwrap(),
            vec![1, 2, 3]
        );
        k.override_edid("card0", "DP-1", None).unwrap();
        assert_eq!(tree.read("debug/0/DP-1/edid_override"), "reset");
        assert_eq!(k.connectors().len(), 2);
        assert!(k.has_edid("card0", "HDMI-A-1") && !k.has_edid("card0", "DP-1"));
        assert_eq!(k.release(), "6.6.127-amd64");
        assert_eq!(k.cmdline(), "quiet video=HDMI-A-1:e");
        std::fs::create_dir_all(tree.0.join("drm/card0")).unwrap();
        k.hotplug("card0").unwrap();
        assert_eq!(tree.read("drm/card0/uevent"), "change");
        assert!(k.hotplug("../card0").is_err());
        // No device nodes here.
        assert_eq!(k.crtcs("card0"), None);
        assert_eq!(k.encoders("card0", 95), None);
    }

    #[test]
    fn intels_display_is_read_from_debugfs() {
        let tree = Tree::new("version");
        let k = tree.kernel();
        assert_eq!(k.intel_display("card0"), None);
        std::fs::create_dir_all(tree.0.join("debug/0")).unwrap();
        std::fs::write(
            tree.0.join("debug/0/i915_capabilities"),
            "pch: 6\nplatform: DG2\ngraphics version: 12.55\nmedia version: 12\n\
             display version: 13\n",
        )
        .unwrap();
        let display = k.intel_display("card0").unwrap();
        assert_eq!((display.version, display.platform.as_str()), (Some(13), "DG2"));
        // 6.4 on: a file of its own, which comes first.
        std::fs::write(
            tree.0.join("debug/0/i915_display_capabilities"),
            "display version: 14.01\nhas_hdcp: yes\n",
        )
        .unwrap();
        assert_eq!(k.intel_display("card0").unwrap().version, Some(14));
        assert_eq!(intel_display("platform: KABYLAKE\ngen: 9\n").version, Some(9));
        assert_eq!(intel_display("platform: KABYLAKE\n").version, None);
    }

    #[test]
    fn the_word_of_the_vkms_loader_is_read() {
        let tree = Tree::new("note");
        let k = tree.kernel();
        assert_eq!(k.vkms_word(), None);
        std::fs::create_dir_all(tree.0.join("run/rustdesk")).unwrap();
        std::fs::write(tree.0.join("run/rustdesk").join(VKMS_BOOT), "blacklisted\n").unwrap();
        assert_eq!(k.vkms_word().as_deref(), Some("blacklisted"));
    }

    #[test]
    fn emptied_parameters_are_written_as_a_nul() {
        let tree = Tree::new("params");
        let mut k = tree.kernel();
        k.set_edid_param("").unwrap();
        assert_eq!(tree.read("module/edid_firmware"), "\0");
        assert_eq!(k.firmware_path().unwrap(), "");
        let dir = k.our_firmware_dir();
        k.set_firmware_path(&dir).unwrap();
        assert_eq!(k.firmware_path().unwrap(), dir);
        k.put_firmware("rustdesk-vdisplay.bin", Some(&[1, 2])).unwrap();
        assert_eq!(
            std::fs::read(Path::new(&dir).join("rustdesk-vdisplay.bin")).unwrap(),
            vec![1, 2]
        );
        k.put_firmware("rustdesk-vdisplay.bin", None).unwrap();
        k.put_firmware("rustdesk-vdisplay.bin", None).unwrap();
        assert!(!Path::new(&dir).join("rustdesk-vdisplay.bin").exists());
    }

    #[test]
    fn what_is_lit_is_kept_privately_and_dropped_when_nothing_is() {
        let tree = Tree::new("state");
        let mut k = tree.kernel();
        assert!(k.load_lit().is_empty());
        let lit = vec![Lit {
            card: "card0".to_owned(),
            name: "DP-1".to_owned(),
            source: super::super::leases::Source::Firmware,
            id: Some(95),
            kind: super::super::protocol::Kind::Scaled,
        }];
        k.save_lit(&lit).unwrap();
        assert_eq!(k.load_lit(), lit);
        // A record from before the kind was kept reads as a connector's.
        std::fs::write(
            tree.0.join("run/rustdesk/vdisplay-lit.json"),
            r#"[{"card":"card0","name":"DP-1","source":"builtin"}]"#,
        )
        .unwrap();
        assert_eq!(k.load_lit()[0].kind, super::super::protocol::Kind::Connector);
        k.save_lit(&lit).unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str| {
            std::fs::metadata(tree.0.join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("run/rustdesk"), 0o700);
        assert_eq!(mode("run/rustdesk/vdisplay-lit.json"), 0o600);
        k.save_lit(&[]).unwrap();
        assert!(!tree.0.join("run/rustdesk/vdisplay-lit.json").exists());
        k.save_lit(&[]).unwrap();
    }
}
