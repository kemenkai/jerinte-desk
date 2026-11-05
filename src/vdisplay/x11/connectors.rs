//! The DRM connectors a virtual display can be lit on, as sysfs shows them, and the kernel's
//! `drm.edid_firmware` parameter that gives a connector without a monitor an EDID to light with.

use super::protocol::Kind;
use std::{collections::HashMap, path::Path};

/// A connector of a DRM card.
#[derive(Clone, Debug, PartialEq)]
pub struct Connector {
    /// "card0".
    pub card: String,
    /// The kernel's name, "DP-1" or "HDMI-A-1"; the parameter and the X server's output both go by
    /// it (the X server may spell it differently, see `randr::output_for`).
    pub name: String,
    /// The DRM connector id, where the kernel shows it.
    pub id: Option<u32>,
    pub connected: bool,
    pub enabled: bool,
    /// The card's driver, "i915" or "amdgpu"; "vkms" for the kernel's virtual display card.
    pub driver: String,
}

impl Connector {
    /// How a display on it would be made, if one can be: by what its driver does with a connector
    /// that has no monitor.
    pub fn kind(&self) -> Option<Kind> {
        match (self.driver.as_str(), connector_type(&self.name)) {
            ("vkms", "Virtual") => Some(Kind::Vkms),
            // Intel's HDMI ports train no link: only the clock of the port limits them. Its
            // DisplayPort ports train at the lowest rate without a monitor, about 72 MHz.
            ("i915" | "xe", "HDMI-A") => Some(Kind::Connector),
            ("i915" | "xe", "DP") => Some(Kind::Scaled),
            // Not amdgpu's: a connector forced on keeps the sink amdgpu made for it, and shows as
            // connected after it is let go, until a hotplug (and 6.6 then reads the EDID it
            // freed). Its cards have vkms on every distribution that builds it.
            // nouveau's DisplayPort ports have no modes at all without a monitor.
            ("nouveau", "HDMI-A") => Some(Kind::Connector),
            _ => None,
        }
    }
}

/// The connectors of every card under `drm_root` (/sys/class/drm).
pub fn scan(drm_root: &Path) -> Vec<Connector> {
    let Ok(entries) = std::fs::read_dir(drm_root) else {
        return Vec::new();
    };
    let mut drivers: HashMap<String, String> = HashMap::new();
    let mut connectors: Vec<Connector> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let file_name = entry.file_name().into_string().ok()?;
            let (card, name) = split_entry(&file_name)?;
            let dir = entry.path();
            let read = |file: &str| {
                std::fs::read_to_string(dir.join(file))
                    .map(|s| s.trim().to_owned())
                    .unwrap_or_default()
            };
            let driver = drivers
                .entry(card.to_owned())
                .or_insert_with(|| card_driver(drm_root, card))
                .clone();
            Some(Connector {
                card: card.to_owned(),
                name: name.to_owned(),
                id: read("connector_id").parse().ok(),
                connected: read("status") == "connected",
                enabled: read("enabled") == "enabled",
                driver,
            })
        })
        .collect();
    connectors.sort_by(|a, b| (&a.card, &a.name).cmp(&(&b.card, &b.name)));
    connectors
}

// The driver of `card`. vkms by its device's name: a platform device until 6.17, a faux one
// since, whose driver is "faux_driver".
fn card_driver(drm_root: &Path, card: &str) -> String {
    let device = drm_root.join(card).join("device");
    let name = |path: &Path| {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    if std::fs::canonicalize(&device).map_or(false, |path| name(&path) == "vkms") {
        return "vkms".to_owned();
    }
    std::fs::read_link(device.join("driver"))
        .map(|path| name(&path))
        .unwrap_or_default()
}

/// The connectors a virtual display may take, best first: of a kind a display can be made on,
/// with nothing attached, not in use and not `taken` already (by card and name). A physical one
/// on a card that drives a connected monitor (the one the desktop is on); vkms's, only while it
/// is turned off, as we keep it until it is needed: one that shows as connected is someone
/// else's.
pub fn usable<'a>(connectors: &'a [Connector], taken: &[(String, String)]) -> Vec<&'a Connector> {
    let mut usable: Vec<&Connector> = connectors
        .iter()
        .filter(|c| {
            !c.connected
                && !c.enabled
                && !taken.iter().any(|(card, name)| *card == c.card && *name == c.name)
                && match c.kind() {
                    None => false,
                    Some(Kind::Vkms) => true,
                    Some(_) => connectors.iter().any(|o| o.card == c.card && o.connected),
                }
        })
        .collect();
    usable.sort_by_key(|c| c.kind());
    usable
}

/// `param` (the value of /sys/module/drm/parameters/edid_firmware) with `connector` given `edid`
/// and every other entry kept: other connectors' and an entry for all connectors. Ours goes first:
/// the kernel takes the first entry whose name starts the connector's, and someone's `DP:` or `DP-1:`
/// would otherwise give DP-1 or DP-10 their EDID instead of ours.
pub fn with_edid(param: &str, connector: &str, edid: &str) -> String {
    let mut entries = vec![format!("{}:{}", connector, edid)];
    entries.extend(entries_but(param, connector));
    entries.join(",")
}

/// `param` without the entry for `connector`.
pub fn without_edid(param: &str, connector: &str) -> String {
    entries_but(param, connector).join(",")
}

/// Whether `param` has an entry for `connector`.
pub fn names(param: &str, connector: &str) -> bool {
    param
        .trim()
        .split(',')
        .any(|entry| entry_connector(entry) == Some(connector))
}

/// The connectors the kernel command line turns off: `video=DP-1:d`, or a mode ending in `d`.
pub fn turned_off(cmdline: &str) -> Vec<String> {
    cmdline
        .split_whitespace()
        .filter_map(|arg| {
            let (connector, options) = arg.strip_prefix("video=")?.split_once(':')?;
            options.ends_with('d').then(|| connector.to_owned())
        })
        .collect()
}

// "card0-DP-1" -> ("card0", "DP-1"); not the card itself, nor its render node.
fn split_entry(file_name: &str) -> Option<(&str, &str)> {
    let (card, name) = file_name.split_once('-')?;
    let number = card.strip_prefix("card")?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) || name.is_empty() {
        return None;
    }
    Some((card, name))
}

// "HDMI-A-1" -> "HDMI-A", "DP-1" -> "DP".
fn connector_type(name: &str) -> &str {
    name.rsplit_once('-').map_or(name, |(kind, _)| kind)
}

fn entry_connector(entry: &str) -> Option<&str> {
    entry.split_once(':').map(|(connector, _)| connector)
}

fn entries_but(param: &str, connector: &str) -> Vec<String> {
    param
        .trim()
        .split(',')
        .filter(|entry| !entry.is_empty() && entry_connector(entry) != Some(connector))
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector(card: &str, name: &str, connected: bool, driver: &str) -> Connector {
        Connector {
            card: card.to_owned(),
            name: name.to_owned(),
            id: None,
            connected,
            enabled: connected,
            driver: driver.to_owned(),
        }
    }

    fn sysfs(dir: &Path, entry: &str, status: &str, enabled: &str, id: Option<u32>) {
        let path = dir.join(entry);
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("status"), format!("{}\n", status)).unwrap();
        std::fs::write(path.join("enabled"), format!("{}\n", enabled)).unwrap();
        if let Some(id) = id {
            std::fs::write(path.join("connector_id"), format!("{}\n", id)).unwrap();
        }
    }

    #[test]
    fn sysfs_is_read_as_the_kernel_lays_it_out() {
        let dir = std::env::temp_dir().join(format!("vdisplay-sysfs-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        sysfs(&dir, "card0-HDMI-A-1", "connected", "enabled", Some(103));
        sysfs(&dir, "card0-DP-1", "disconnected", "disabled", Some(95));
        sysfs(&dir, "card1-Virtual-1", "disconnected", "disabled", Some(36));
        // Not connectors: the cards, a render node, the version file.
        std::fs::create_dir_all(dir.join("renderD128")).unwrap();
        std::fs::write(dir.join("version"), "drm 1.1.0\n").unwrap();
        // card0 is a PCI device bound to i915; card1 the vkms device, faux since 6.17.
        let devices = dir.join("devices");
        std::fs::create_dir_all(devices.join("0000:00:02.0")).unwrap();
        std::fs::create_dir_all(devices.join("faux/vkms")).unwrap();
        std::fs::create_dir_all(dir.join("card0")).unwrap();
        std::fs::create_dir_all(dir.join("card1")).unwrap();
        std::os::unix::fs::symlink(devices.join("0000:00:02.0"), dir.join("card0/device")).unwrap();
        std::os::unix::fs::symlink("../../bus/pci/drivers/i915", devices.join("0000:00:02.0/driver"))
            .unwrap();
        std::os::unix::fs::symlink(devices.join("faux/vkms"), dir.join("card1/device")).unwrap();
        std::os::unix::fs::symlink("../../bus/faux/drivers/faux_driver", devices.join("faux/vkms/driver"))
            .unwrap();
        let found = scan(&dir);
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(
            found,
            vec![
                Connector {
                    card: "card0".to_owned(),
                    name: "DP-1".to_owned(),
                    id: Some(95),
                    connected: false,
                    enabled: false,
                    driver: "i915".to_owned(),
                },
                Connector {
                    card: "card0".to_owned(),
                    name: "HDMI-A-1".to_owned(),
                    id: Some(103),
                    connected: true,
                    enabled: true,
                    driver: "i915".to_owned(),
                },
                Connector {
                    card: "card1".to_owned(),
                    name: "Virtual-1".to_owned(),
                    id: Some(36),
                    connected: false,
                    enabled: false,
                    driver: "vkms".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn each_driver_gives_its_own_kind() {
        let kind = |driver: &str, name: &str| connector("card0", name, false, driver).kind();
        assert_eq!(kind("vkms", "Virtual-1"), Some(Kind::Vkms));
        assert_eq!(kind("i915", "HDMI-A-2"), Some(Kind::Connector));
        assert_eq!(kind("xe", "HDMI-A-1"), Some(Kind::Connector));
        assert_eq!(kind("i915", "DP-1"), Some(Kind::Scaled));
        assert_eq!(kind("amdgpu", "DP-2"), None);
        assert_eq!(kind("amdgpu", "HDMI-A-1"), None);
        assert_eq!(kind("nouveau", "HDMI-A-1"), Some(Kind::Connector));
        assert_eq!(kind("nouveau", "DP-1"), None);
        assert_eq!(kind("radeon", "HDMI-A-1"), None);
        assert_eq!(kind("i915", "DVI-I-1"), None);
        assert_eq!(kind("i915", "eDP-1"), None);
        // virtio-gpu, qxl, vmwgfx name theirs Virtual too: never ours.
        assert_eq!(kind("virtio_gpu", "Virtual-1"), None);
        assert_eq!(kind("", "HDMI-A-1"), None);
    }

    #[test]
    fn only_free_connectors_a_display_can_be_made_on_are_usable() {
        let all = vec![
            connector("card0", "HDMI-A-1", true, "i915"),
            connector("card0", "DP-1", false, "i915"),
            connector("card0", "eDP-1", false, "i915"),
            connector("card0", "VGA-1", false, "i915"),
            connector("card0", "Writeback-1", false, "i915"),
            connector("card0", "HDMI-A-2", false, "i915"),
            // A second card with nothing connected drives no desktop.
            connector("card2", "DP-2", false, "amdgpu"),
            // vkms drives none either, and needs not.
            connector("card1", "Virtual-1", false, "vkms"),
        ];
        let names = |all: &[Connector], taken: &[(String, String)]| -> Vec<String> {
            usable(all, taken).iter().map(|c| c.name.clone()).collect()
        };
        // Best first: vkms, a real mode, scaling.
        assert_eq!(names(&all, &[]), vec!["Virtual-1", "HDMI-A-2", "DP-1"]);
        let vkms = ("card1".to_owned(), "Virtual-1".to_owned());
        assert_eq!(names(&all, &[vkms]), vec!["HDMI-A-2", "DP-1"]);
        // Taken on another card is not taken here.
        let elsewhere = ("card3".to_owned(), "HDMI-A-2".to_owned());
        assert_eq!(names(&all, &[elsewhere]), vec!["Virtual-1", "HDMI-A-2", "DP-1"]);
        // Enabled without a monitor: someone forced it on already.
        let mut forced = all.clone();
        forced[5].enabled = true;
        assert_eq!(names(&forced, &[]), vec!["Virtual-1", "DP-1"]);
        // vkms connected: someone else's, as we keep ours off until it is needed.
        let mut theirs = all.clone();
        theirs[7].connected = true;
        assert_eq!(names(&theirs, &[]), vec!["HDMI-A-2", "DP-1"]);
    }

    #[test]
    fn the_parameter_keeps_every_other_entry() {
        let office = "HDMI-A-1:edid/1920x1080.bin";
        let lit = with_edid(office, "DP-1", "edid/1024x768.bin");
        assert_eq!(lit, "DP-1:edid/1024x768.bin,HDMI-A-1:edid/1920x1080.bin");
        assert!(names(&lit, "DP-1") && names(&lit, "HDMI-A-1") && !names(&lit, "DP-2"));
        assert_eq!(without_edid(&lit, "DP-1"), office);
        // Lit again: one entry, not two.
        assert_eq!(
            with_edid(&lit, "DP-1", "edid/800x600.bin"),
            "DP-1:edid/800x600.bin,HDMI-A-1:edid/1920x1080.bin"
        );
        // Empty, with the newline sysfs reads back, and with an entry for every connector.
        assert_eq!(
            with_edid("\n", "DP-1", "edid/1024x768.bin"),
            "DP-1:edid/1024x768.bin"
        );
        assert_eq!(without_edid("DP-1:edid/1024x768.bin\n", "DP-1"), "");
        assert_eq!(
            with_edid("edid/1920x1080.bin", "DP-1", "edid/1024x768.bin"),
            "DP-1:edid/1024x768.bin,edid/1920x1080.bin"
        );
        // Ahead of an entry whose name starts ours, which the kernel would take first.
        assert_eq!(
            with_edid("DP:edid/x.bin", "DP-10", "edid/1024x768.bin"),
            "DP-10:edid/1024x768.bin,DP:edid/x.bin"
        );
        // A name that only starts like ours is someone else's.
        assert_eq!(
            without_edid("DP-10:edid/a.bin,DP-1:edid/b.bin", "DP-1"),
            "DP-10:edid/a.bin"
        );
    }

    #[test]
    fn the_command_line_may_turn_connectors_off() {
        let cmdline = "BOOT_IMAGE=/vmlinuz video=HDMI-A-1:e video=DP-1:d \
                       video=DP-2:1920x1080@60d video=efifb:off video=HDMI-A-2:1280x720D";
        assert_eq!(turned_off(cmdline), vec!["DP-1", "DP-2"]);
        assert!(turned_off("quiet splash").is_empty());
    }
}
