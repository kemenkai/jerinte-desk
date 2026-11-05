//! vkms, the kernel's virtual display card, loaded at boot before the desktop starts and its
//! connector turned off at once (`rustdesk --vkms-boot`, run by systemd before the display
//! manager). The X server then has it as a card of its own from the start, with the hardware
//! cursor, which a card added later never gets, and nothing shows until a virtual display turns
//! the connector on. Loaded with an X server running instead, it would take that X server down
//! when it lets the card go, at logout or shutdown, or when udev replays its events: X before
//! 21.1.25 crashes on a card removed that it did not start with.
//!
//! Should the desktop not come up after a boot that loaded it, the next boot does without: the
//! mark left before loading is cleared by the root service only once a graphical session has been
//! on the screen a while (`confirm`). The boot after tries again; should that one not come up
//! either, vkms is not loaded any more. Once is not enough, as a machine shut down at the login
//! screen right after boot leaves the mark too.

use hbb_common::{log, ResultType};
use std::time::Duration;

/// What the loader leaves word of when it loaded vkms.
pub const LOADED: &str = "loaded";

// How long the driver of the graphics card the firmware booted with may take to come up, and
// vkms's connector, once loaded. Both are there long before the display manager, which waits for
// us.
const BOOT_VGA_WAIT: Duration = Duration::from_secs(5);
const CONNECTOR_WAIT: Duration = Duration::from_secs(2);
const LOOK: Duration = Duration::from_millis(100);

/// The files the loader keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mark {
    /// Loaded at a boot whose desktop has not been seen up yet.
    Pending,
    /// A boot that loaded it did not reach the desktop, or was not seen to: one more and it is
    /// `Failed`.
    Suspect,
    /// Not to be loaded again: two boots in a row that loaded it did not reach the desktop.
    Failed,
    /// The loader's word on this boot, for the root service: `LOADED`, or why not.
    Word,
}

/// The graphics card the firmware booted with (boot_vga).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vga {
    /// None, as on some ARM boards and virtual machines: the X server could take vkms for its
    /// main card.
    Missing,
    /// There, its driver not loaded yet.
    Coming,
    /// There, with its DRM card.
    Up,
}

/// The machine, as the loader sees it.
pub trait Host {
    /// Whether virtual displays are turned on in the settings.
    fn enabled(&self) -> bool;
    /// Whether the desktop is up already: a display manager or an X server runs. Then this is no
    /// boot, but someone starting us by hand, or a package being set up.
    fn desktop_running(&self) -> bool;
    fn vga(&self) -> Vga;
    fn module_loaded(&self) -> bool;
    /// What keeps vkms from being loaded, if anything does: modules turned off, a blacklist
    /// entry, an install command, no such module.
    fn blocked(&self) -> Option<String>;
    /// Whether vkms takes parameter `name`.
    fn has_param(&self, name: &str) -> bool;
    fn modprobe(&mut self, params: &[&str]) -> ResultType<()>;
    /// Unloads it again (`modprobe -r`), which nothing holds yet before the desktop starts.
    fn unload(&mut self) -> ResultType<()>;
    /// vkms's connectors: (card, name, connected).
    fn connectors(&self) -> Vec<(String, String, bool)>;
    fn set_status(&mut self, card: &str, name: &str, status: &str) -> ResultType<()>;
    fn has(&self, mark: Mark) -> bool;
    /// Writes `mark`, or removes it with None.
    fn set(&mut self, mark: Mark, text: Option<&str>);
    fn sleep(&mut self, duration: Duration);
}

/// Loads vkms if it should be, and leaves word of what was done.
pub fn boot(host: &mut impl Host) -> String {
    let word = load(host);
    host.set(Mark::Word, Some(&word));
    word
}

fn load(host: &mut impl Host) -> String {
    if !host.enabled() {
        return "virtual displays are turned off in the settings".to_owned();
    }
    if host.has(Mark::Failed) {
        return "vkms is not loaded any more: two boots with it in a row did not reach the \
                desktop (remove /var/lib/rustdesk/vkms-failed to try again)"
            .to_owned();
    }
    if host.has(Mark::Pending) {
        host.set(Mark::Pending, None);
        if host.has(Mark::Suspect) {
            host.set(Mark::Suspect, None);
            host.set(Mark::Failed, Some("two boots with vkms in a row did not reach the desktop"));
            return "the last two boots with vkms did not reach the desktop: vkms is not loaded \
                    from now on"
                .to_owned();
        }
        host.set(Mark::Suspect, Some("a boot with vkms did not reach the desktop"));
        return "the last boot with vkms did not reach the desktop: vkms is not loaded this time"
            .to_owned();
    }
    if host.desktop_running() {
        return "the desktop was up already: vkms is loaded at the next boot".to_owned();
    }
    // Without a card the machine booted with, the X server could have taken vkms for its main
    // card: not used then, even if it is there already.
    let vga = host.vga();
    if vga == Vga::Missing {
        return "no graphics card the machine booted with: vkms is not used".to_owned();
    }
    if host.module_loaded() {
        // Built into the kernel, or loaded by someone else: theirs while it shows connected.
        return if host.connectors().iter().any(|(_, _, connected)| *connected) {
            "vkms was there already, and in use".to_owned()
        } else {
            LOADED.to_owned()
        };
    }
    // The real card's driver first, so that the X server finds it ready.
    if vga == Vga::Coming && !wait(host, BOOT_VGA_WAIT, |host| host.vga() == Vga::Up) {
        return "the graphics card the machine booted with did not come up in time".to_owned();
    }
    if let Some(why) = host.blocked() {
        return why;
    }
    host.set(Mark::Pending, Some("loaded at boot; cleared once the desktop is up"));
    // No writeback connector, which nothing here needs (the parameter came in 5.12; an older
    // kernel would only warn of it, but it is not given).
    let params: &[&str] = if host.has_param("enable_writeback") {
        &["enable_writeback=0"]
    } else {
        &[]
    };
    if let Err(e) = host.modprobe(params) {
        host.set(Mark::Pending, None);
        return format!("modprobe vkms failed: {}", e);
    }
    if !wait(host, CONNECTOR_WAIT, |host| !host.connectors().is_empty()) {
        return "vkms was loaded, but has no connector".to_owned();
    }
    for (card, name, _) in host.connectors() {
        if let Err(e) = host.set_status(&card, &name, "off") {
            log::error!("Failed to turn vkms's {} off: {}", name, e);
        }
    }
    // Left on, the desktop would come up on it too.
    if host.connectors().iter().any(|(_, _, connected)| *connected) {
        let unloaded = host.unload();
        host.set(Mark::Pending, None);
        return match unloaded {
            Ok(()) => "vkms's connector would not turn off: vkms was unloaded".to_owned(),
            Err(e) => format!("vkms's connector would not turn off, nor vkms unload: {}", e),
        };
    }
    LOADED.to_owned()
}

// Whether `ready` holds within `limit`.
fn wait<H: Host>(host: &mut H, limit: Duration, ready: impl Fn(&H) -> bool) -> bool {
    let mut waited = Duration::ZERO;
    loop {
        if ready(host) {
            return true;
        }
        if waited >= limit {
            return false;
        }
        host.sleep(LOOK);
        waited += LOOK;
    }
}

/// The desktop came up: a boot that loaded vkms is fine. Not after a boot that did without it,
/// which says nothing of vkms.
pub fn confirm(host: &mut impl Host) {
    if host.has(Mark::Pending) {
        log::info!("The desktop is up with vkms loaded");
        host.set(Mark::Pending, None);
        host.set(Mark::Suspect, None);
    }
}

#[cfg(target_os = "linux")]
pub use system::SystemHost;

#[cfg(target_os = "linux")]
mod system {
    use super::{Host, Mark, Vga};
    use hbb_common::{bail, ResultType};
    use std::{path::Path, process::Command, time::Duration};

    const STATE: &str = "/var/lib/rustdesk";
    const RUN: &str = "/run/rustdesk";
    const DRM: &str = "/sys/class/drm";

    pub struct SystemHost;

    impl SystemHost {
        fn path(mark: Mark) -> String {
            match mark {
                Mark::Pending => format!("{}/vkms-pending", STATE),
                Mark::Suspect => format!("{}/vkms-suspect", STATE),
                Mark::Failed => format!("{}/vkms-failed", STATE),
                Mark::Word => format!("{}/{}", RUN, super::super::sysfs::VKMS_BOOT),
            }
        }
    }

    fn run(program: &str, args: &[&str]) -> Option<String> {
        let out = Command::new(program).args(args).output().ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    impl Host for SystemHost {
        fn enabled(&self) -> bool {
            crate::server::Connection::is_permission_enabled_locally(
                base::config::keys::OPTION_ENABLE_REMOTE_VIRTUAL_DISPLAY,
            )
        }

        fn desktop_running(&self) -> bool {
            let manager = Command::new("systemctl")
                .args(["is-active", "--quiet", "display-manager.service"])
                .status()
                .map_or(false, |s| s.success());
            manager
                || std::fs::read_dir("/proc").map_or(false, |entries| {
                    entries.flatten().any(|entry| {
                        let comm = std::fs::read_to_string(entry.path().join("comm"))
                            .unwrap_or_default();
                        matches!(comm.trim(), "Xorg" | "X" | "Xwayland")
                    })
                })
        }

        // The PCI device is there from the start; its DRM card once its driver is loaded.
        fn vga(&self) -> Vga {
            let Ok(devices) = std::fs::read_dir("/sys/bus/pci/devices") else {
                return Vga::Missing;
            };
            let Some(device) = devices.flatten().map(|d| d.path()).find(|d| {
                std::fs::read_to_string(d.join("boot_vga")).map_or(false, |v| v.trim() == "1")
            }) else {
                return Vga::Missing;
            };
            let card = std::fs::read_dir(device.join("drm")).map_or(false, |entries| {
                entries
                    .flatten()
                    .any(|e| e.file_name().to_string_lossy().starts_with("card"))
            });
            if card {
                Vga::Up
            } else {
                Vga::Coming
            }
        }

        fn module_loaded(&self) -> bool {
            Path::new("/sys/module/vkms").exists()
        }

        fn blocked(&self) -> Option<String> {
            if std::fs::read_to_string("/proc/sys/kernel/modules_disabled")
                .map_or(false, |v| v.trim() == "1")
            {
                return Some("loading kernel modules is turned off".to_owned());
            }
            let config = run("modprobe", &["--showconfig"]).unwrap_or_default();
            for line in config.lines() {
                let words: Vec<&str> = line.split_whitespace().collect();
                match words.as_slice() {
                    ["blacklist", "vkms", ..] => return Some("vkms is blacklisted".to_owned()),
                    ["install", "vkms", command @ ..] => {
                        return Some(format!(
                            "vkms is set to run {} instead of loading",
                            command.join(" ")
                        ))
                    }
                    _ => {}
                }
            }
            if run("modprobe", &["--dry-run", "vkms"]).is_none() {
                return Some("this kernel has no vkms module".to_owned());
            }
            None
        }

        fn has_param(&self, name: &str) -> bool {
            run("modinfo", &["--parameters", "vkms"]).map_or(false, |params| {
                params
                    .lines()
                    .any(|line| line.split(':').next() == Some(name))
            })
        }

        fn modprobe(&mut self, params: &[&str]) -> ResultType<()> {
            let out = Command::new("modprobe").arg("vkms").args(params).output()?;
            if !out.status.success() {
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(())
        }

        fn unload(&mut self) -> ResultType<()> {
            let out = Command::new("modprobe").args(["-r", "vkms"]).output()?;
            if !out.status.success() {
                bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
            }
            Ok(())
        }

        fn connectors(&self) -> Vec<(String, String, bool)> {
            super::super::connectors::scan(Path::new(DRM))
                .into_iter()
                .filter(|c| c.driver == "vkms")
                .map(|c| (c.card, c.name, c.connected))
                .collect()
        }

        fn set_status(&mut self, card: &str, name: &str, status: &str) -> ResultType<()> {
            Ok(std::fs::write(
                Path::new(DRM).join(format!("{}-{}", card, name)).join("status"),
                status,
            )?)
        }

        fn has(&self, mark: Mark) -> bool {
            Path::new(&Self::path(mark)).exists()
        }

        fn set(&mut self, mark: Mark, text: Option<&str>) {
            let path = Self::path(mark);
            let result = match text {
                Some(text) => Path::new(&path)
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|()| std::fs::write(&path, format!("{}\n", text))),
                None => match std::fs::remove_file(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    result => result,
                },
            };
            if let Err(e) = result {
                hbb_common::log::error!("Failed to update {}: {}", path, e);
            }
        }

        fn sleep(&mut self, duration: Duration) {
            std::thread::sleep(duration)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Machine {
        disabled: bool,
        desktop: bool,
        // The boot card's driver is up after this long; no boot card at all with None.
        vga_after: Option<Duration>,
        loaded: bool,
        blocked: Option<String>,
        writeback: bool,
        fail_modprobe: bool,
        // Its connector will not turn off.
        stuck_on: bool,
        // vkms's connector once loaded: connected unless turned off.
        connector: Option<bool>,
        marks: HashMap<Mark, String>,
        modprobed: Vec<String>,
        waited: Duration,
    }

    impl Machine {
        fn office() -> Self {
            Self {
                vga_after: Some(Duration::from_secs(1)),
                writeback: true,
                ..Default::default()
            }
        }
    }

    impl Host for Machine {
        fn enabled(&self) -> bool {
            !self.disabled
        }
        fn desktop_running(&self) -> bool {
            self.desktop
        }
        fn vga(&self) -> Vga {
            match self.vga_after {
                None => Vga::Missing,
                Some(after) if self.waited >= after => Vga::Up,
                Some(_) => Vga::Coming,
            }
        }
        fn module_loaded(&self) -> bool {
            self.loaded
        }
        fn blocked(&self) -> Option<String> {
            self.blocked.clone()
        }
        fn has_param(&self, name: &str) -> bool {
            self.writeback && name == "enable_writeback"
        }
        fn modprobe(&mut self, params: &[&str]) -> ResultType<()> {
            if self.fail_modprobe {
                hbb_common::bail!("Operation not permitted");
            }
            self.modprobed.push(params.join(" "));
            self.loaded = true;
            self.connector = Some(true);
            Ok(())
        }
        fn unload(&mut self) -> ResultType<()> {
            self.loaded = false;
            Ok(())
        }
        fn connectors(&self) -> Vec<(String, String, bool)> {
            self.connector
                .filter(|_| self.loaded)
                .map(|connected| ("card1".to_owned(), "Virtual-1".to_owned(), connected))
                .into_iter()
                .collect()
        }
        fn set_status(&mut self, _card: &str, _name: &str, status: &str) -> ResultType<()> {
            if !self.stuck_on {
                self.connector = Some(status != "off");
            }
            Ok(())
        }
        fn has(&self, mark: Mark) -> bool {
            self.marks.contains_key(&mark)
        }
        fn set(&mut self, mark: Mark, text: Option<&str>) {
            match text {
                Some(text) => self.marks.insert(mark, text.to_owned()),
                None => self.marks.remove(&mark),
            };
        }
        fn sleep(&mut self, duration: Duration) {
            self.waited += duration;
        }
    }

    #[test]
    fn at_boot_vkms_is_loaded_with_its_connector_off() {
        let mut m = Machine::office();
        assert_eq!(boot(&mut m), LOADED);
        assert_eq!(m.modprobed, vec!["enable_writeback=0"]);
        assert_eq!(m.connector, Some(false));
        assert!(m.has(Mark::Pending));
        assert_eq!(m.marks[&Mark::Word], LOADED);
        // The desktop came up: the mark goes.
        confirm(&mut m);
        assert!(!m.has(Mark::Pending));
        // A kernel whose vkms has no such parameter is given none.
        let mut m = Machine {
            writeback: false,
            ..Machine::office()
        };
        boot(&mut m);
        assert_eq!(m.modprobed, vec![""]);
    }

    #[test]
    fn two_boots_in_a_row_that_did_not_reach_the_desktop_turn_it_off_for_good() {
        let mut m = Machine::office();
        let reboot = |m: &mut Machine| {
            m.loaded = false;
            m.modprobed.clear();
            boot(m)
        };
        boot(&mut m);
        // Rebooted without the desktop having come up: this boot does without.
        assert!(reboot(&mut m).contains("this time"));
        assert!(m.modprobed.is_empty());
        assert!(!m.has(Mark::Pending) && m.has(Mark::Suspect) && !m.has(Mark::Failed));
        // The desktop came up without it: nothing said of vkms, which is tried again.
        confirm(&mut m);
        assert!(m.has(Mark::Suspect));
        assert_eq!(reboot(&mut m), LOADED);
        // And again not up: for good.
        assert!(reboot(&mut m).contains("from now on"));
        assert!(m.has(Mark::Failed) && !m.has(Mark::Suspect));
        assert!(reboot(&mut m).contains("vkms-failed"));
        assert!(m.modprobed.is_empty());
        // Up once with it: all is forgiven.
        let mut m = Machine::office();
        boot(&mut m);
        reboot(&mut m);
        reboot(&mut m);
        confirm(&mut m);
        assert!(!m.has(Mark::Suspect) && !m.has(Mark::Pending));
    }

    #[test]
    fn a_connector_that_will_not_turn_off_takes_vkms_away() {
        let mut m = Machine {
            stuck_on: true,
            ..Machine::office()
        };
        assert!(boot(&mut m).contains("unloaded"));
        assert!(!m.loaded);
        assert!(!m.has(Mark::Pending));
    }

    #[test]
    fn nothing_is_loaded_but_at_boot_and_where_it_is_safe() {
        let refused = |m: &mut Machine| {
            let word = boot(m);
            assert_ne!(word, LOADED);
            assert!(m.modprobed.is_empty(), "{}", word);
            assert!(!m.has(Mark::Pending), "{}", word);
            assert_eq!(m.marks[&Mark::Word], word);
            word
        };
        let mut m = Machine {
            disabled: true,
            ..Machine::office()
        };
        assert!(refused(&mut m).contains("turned off"));
        let mut m = Machine {
            desktop: true,
            ..Machine::office()
        };
        assert!(refused(&mut m).contains("next boot"));
        // No boot card: at once, the desktop not held up.
        let mut m = Machine {
            vga_after: None,
            ..Machine::office()
        };
        assert!(refused(&mut m).contains("booted with"));
        assert_eq!(m.waited, Duration::ZERO);
        // One whose driver never comes: given up on in time.
        let mut m = Machine {
            vga_after: Some(Duration::from_secs(3600)),
            ..Machine::office()
        };
        assert!(refused(&mut m).contains("in time"));
        assert!(m.waited >= BOOT_VGA_WAIT && m.waited < BOOT_VGA_WAIT * 2);
        let mut m = Machine {
            blocked: Some("vkms is blacklisted".to_owned()),
            ..Machine::office()
        };
        assert_eq!(refused(&mut m), "vkms is blacklisted");
        let mut m = Machine {
            fail_modprobe: true,
            ..Machine::office()
        };
        assert!(refused(&mut m).contains("Operation not permitted"));
    }

    #[test]
    fn a_vkms_there_already_is_used_only_while_it_is_off() {
        let mut m = Machine {
            loaded: true,
            connector: Some(false),
            ..Machine::office()
        };
        assert_eq!(boot(&mut m), LOADED);
        assert!(m.modprobed.is_empty());
        // Someone's, on: left as it is.
        m.connector = Some(true);
        assert!(boot(&mut m).contains("in use"));
        assert_eq!(m.connector, Some(true));
        // Without a card the machine booted with, not at all: it may be the X server's own.
        let mut m = Machine {
            loaded: true,
            connector: Some(false),
            vga_after: None,
            ..Machine::office()
        };
        assert!(boot(&mut m).contains("not used"));
    }
}
