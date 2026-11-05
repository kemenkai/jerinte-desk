//! The Windows calls the backend makes: the outputs there are, their modes, the driver's table
//! of modes, and the desktop's layout.

use super::plan::{self, View};
use hbb_common::{bail, ResultType};
use std::{io, mem, ptr};
use winapi::{
    shared::{
        minwindef::FALSE,
        ntdef::{LONG, NULL},
        winerror::{ERROR_ACCESS_DENIED, ERROR_INSUFFICIENT_BUFFER},
    },
    um::{
        wingdi::{
            DEVMODEW, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
            DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE, DISPLAYCONFIG_PATH_INFO,
            DISPLAYCONFIG_PATH_MODE_IDX_INVALID, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
            DISPLAYCONFIG_SOURCE_MODE, DISPLAYCONFIG_TOPOLOGY_ID, DISPLAY_DEVICEW,
            DISPLAY_DEVICE_ATTACHED_TO_DESKTOP, DISPLAY_DEVICE_MIRRORING_DRIVER, DM_PELSHEIGHT,
            DM_PELSWIDTH, QDC_ONLY_ACTIVE_PATHS, SDC_ALLOW_CHANGES, SDC_APPLY,
            SDC_USE_DATABASE_CURRENT, SDC_USE_SUPPLIED_DISPLAY_CONFIG,
        },
        processthreadsapi::{GetCurrentProcessId, ProcessIdToSessionId},
        winbase::WTSGetActiveConsoleSessionId,
        winuser::{
            ChangeDisplaySettingsExW, EnumDisplayDevicesW, EnumDisplaySettingsExW, CDS_GLOBAL,
            CDS_RESET, CDS_UPDATEREGISTRY, DISP_CHANGE_SUCCESSFUL, EDD_GET_DEVICE_INTERFACE_NAME,
            ENUM_CURRENT_SETTINGS,
        },
    },
};
use winreg::{enums::HKEY_LOCAL_MACHINE, RegKey};

/// Where the driver reads the modes a monitor offers, when the monitor is plugged in.
const TABLE_KEY: &str =
    r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\WUDF\Services\usbmmIdd\Parameters\Monitors";

/// One display output of the desktop.
#[derive(Clone, Debug)]
pub struct Output {
    /// `\\.\DISPLAYn`, the `DisplayInfo.name` of the display shown on it.
    pub name: String,
    /// Served by the amyuni driver.
    pub amyuni: bool,
    /// Part of the desktop; a detached one (Win+P) still holds its monitor.
    pub attached: bool,
    /// The interface path of the monitor on it, where there is one: `…&UID256#{…}`.
    pub monitor: Option<String>,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

fn text(buf: &[u16]) -> String {
    let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}

fn amyuni_device_string() -> &'static str {
    crate::virtual_display_manager::AMYUNI_IDD_DEVICE_STRING.trim_end_matches('\0')
}

fn devmode() -> DEVMODEW {
    let mut dm: DEVMODEW = unsafe { mem::zeroed() };
    dm.dmSize = mem::size_of::<DEVMODEW>() as _;
    dm
}

fn current(name: &[u16]) -> Option<DEVMODEW> {
    let mut dm = devmode();
    let ok = unsafe { EnumDisplaySettingsExW(name.as_ptr(), ENUM_CURRENT_SETTINGS, &mut dm, 0) };
    (ok != FALSE).then_some(dm)
}

/// Every display output with a monitor on it or part of the desktop, mirroring drivers left out.
pub fn outputs() -> Vec<Output> {
    let mut all = Vec::new();
    for i in 0.. {
        let mut dd: DISPLAY_DEVICEW = unsafe { mem::zeroed() };
        dd.cb = mem::size_of::<DISPLAY_DEVICEW>() as _;
        if unsafe { EnumDisplayDevicesW(ptr::null(), i, &mut dd, 0) } == FALSE {
            break;
        }
        if dd.StateFlags & DISPLAY_DEVICE_MIRRORING_DRIVER != 0 {
            continue;
        }
        let name = text(&dd.DeviceName);
        let wname = wide(&name);
        let mut mon: DISPLAY_DEVICEW = unsafe { mem::zeroed() };
        mon.cb = mem::size_of::<DISPLAY_DEVICEW>() as _;
        let monitor = (unsafe {
            EnumDisplayDevicesW(wname.as_ptr(), 0, &mut mon, EDD_GET_DEVICE_INTERFACE_NAME)
        } != FALSE)
            .then(|| text(&mon.DeviceID))
            .filter(|id| !id.is_empty());
        let attached = dd.StateFlags & DISPLAY_DEVICE_ATTACHED_TO_DESKTOP != 0;
        if attached || monitor.is_some() {
            all.push(Output {
                amyuni: text(&dd.DeviceString) == amyuni_device_string(),
                name,
                attached,
                monitor,
            });
        }
    }
    all
}

/// Whether this process runs in the session on the console. From another one (a session
/// disconnected, as when the console switches to another user, or a remote one) the displays and
/// the layout seen are not the machine's: our monitor, still plugged in, looks gone.
pub fn on_console() -> bool {
    let mut session = 0;
    let found = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } != FALSE;
    found && session == unsafe { WTSGetActiveConsoleSessionId() }
}

/// The sizes display `name` offers.
pub fn modes(name: &str) -> Vec<(u32, u32)> {
    let wname = wide(name);
    let mut sizes = Vec::new();
    for i in 0.. {
        let mut dm = devmode();
        if unsafe { EnumDisplaySettingsExW(wname.as_ptr(), i, &mut dm, 0) } == FALSE {
            break;
        }
        let size = (dm.dmPelsWidth, dm.dmPelsHeight);
        if !sizes.contains(&size) {
            sizes.push(size);
        }
    }
    sizes
}

/// Display `name`'s size now.
pub fn size(name: &str) -> Option<(u32, u32)> {
    current(&wide(name)).map(|dm| (dm.dmPelsWidth, dm.dmPelsHeight))
}

/// Gives display `name` size `size`, the way upstream's resolution changes do: saved for all
/// users, the server running as SYSTEM, which has no settings of its own to save them in.
pub fn set(name: &str, size: (u32, u32)) -> ResultType<()> {
    let wname = wide(name);
    let mut dm = devmode();
    dm.dmPelsWidth = size.0;
    dm.dmPelsHeight = size.1;
    dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
    let res = unsafe {
        ChangeDisplaySettingsExW(
            wname.as_ptr(),
            &mut dm,
            NULL as _,
            CDS_UPDATEREGISTRY | CDS_GLOBAL | CDS_RESET,
            NULL,
        )
    };
    if res != DISP_CHANGE_SUCCESSFUL {
        bail!(
            "Failed to set {} to {}x{}: {} ({})",
            name,
            size.0,
            size.1,
            res,
            io::Error::last_os_error()
        );
    }
    Ok(())
}

/// The driver's table of modes as it is, to be put back with `put_table`: entries "0" to "9" and
/// the default one, named "", which is the mode the monitor prefers.
pub type Table = Vec<(String, String)>;

fn table_key() -> ResultType<RegKey> {
    Ok(RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(
        TABLE_KEY,
        winreg::enums::KEY_READ | winreg::enums::KEY_WRITE,
    )?)
}

pub fn table() -> ResultType<Table> {
    let key = table_key()?;
    Ok(std::iter::once(String::new())
        .chain((0..plan::TABLE_LEN).map(|i| i.to_string()))
        .filter_map(|name| key.get_value::<String, _>(&name).ok().map(|v| (name, v)))
        .collect())
}

/// Makes entries "0" to "9" `entries`, and the default entry too where `entries` has one. Written
/// over in place, never emptied first: the driver may read the table at any moment while a monitor
/// arrives.
pub fn put_table(entries: &Table) -> ResultType<()> {
    let key = table_key()?;
    for (name, value) in entries {
        key.set_value(name, value)?;
    }
    for i in 0..plan::TABLE_LEN {
        let name = i.to_string();
        if !entries.iter().any(|(n, _)| *n == name) {
            key.delete_value(&name).ok();
        }
    }
    Ok(())
}

/// `sizes` as table entries, "0" onwards, the first also the preferred mode: one the monitor does
/// not offer leaves the system unable to make it part of the desktop.
pub fn entries(sizes: &[(u32, u32)]) -> Table {
    sizes
        .first()
        .map(|s| (String::new(), plan::table_value(*s)))
        .into_iter()
        .chain(
            sizes
                .iter()
                .take(plan::TABLE_LEN)
                .enumerate()
                .map(|(i, s)| (i.to_string(), plan::table_value(*s))),
        )
        .collect()
}

#[link(name = "user32")]
extern "system" {
    fn GetDisplayConfigBufferSizes(flags: u32, paths: *mut u32, modes: *mut u32) -> LONG;
    fn QueryDisplayConfig(
        flags: u32,
        num_paths: *mut u32,
        paths: *mut DISPLAYCONFIG_PATH_INFO,
        num_modes: *mut u32,
        modes: *mut DISPLAYCONFIG_MODE_INFO,
        topology: *mut DISPLAYCONFIG_TOPOLOGY_ID,
    ) -> LONG;
    fn SetDisplayConfig(
        num_paths: u32,
        paths: *const DISPLAYCONFIG_PATH_INFO,
        num_modes: u32,
        modes: *const DISPLAYCONFIG_MODE_INFO,
        flags: u32,
    ) -> LONG;
    fn DisplayConfigGetDeviceInfo(packet: *mut DISPLAYCONFIG_DEVICE_INFO_HEADER) -> LONG;
}

/// Why a layout could not be put in force.
#[derive(Debug)]
pub enum Refused {
    /// No access to the console now, as on the secure desktop.
    Denied,
    Failed(String),
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Refused::Denied => write!(f, "Access to the display layout denied"),
            Refused::Failed(why) => write!(f, "{}", why),
        }
    }
}

impl std::error::Error for Refused {}

fn refused(what: &str, rc: LONG) -> Refused {
    if rc as u32 == ERROR_ACCESS_DENIED {
        Refused::Denied
    } else {
        Refused::Failed(format!("{} failed: {}", what, rc))
    }
}

/// The desktop's layout in force: the displays that are part of it (the active paths), and their
/// modes. Put back in force as it is, it is the desktop as it was.
#[derive(Clone)]
pub struct Layout {
    paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

impl Layout {
    pub fn now() -> ResultType<Self> {
        // The layout may change between the two calls, the sizes with it.
        for _ in 0..3 {
            let (mut num_paths, mut num_modes) = (0u32, 0u32);
            let rc = unsafe {
                GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut num_paths, &mut num_modes)
            };
            if rc != 0 {
                bail!("GetDisplayConfigBufferSizes failed: {}", rc);
            }
            let mut paths = vec![unsafe { mem::zeroed() }; num_paths as usize];
            let mut modes = vec![unsafe { mem::zeroed() }; num_modes as usize];
            let rc = unsafe {
                QueryDisplayConfig(
                    QDC_ONLY_ACTIVE_PATHS,
                    &mut num_paths,
                    paths.as_mut_ptr(),
                    &mut num_modes,
                    modes.as_mut_ptr(),
                    ptr::null_mut(),
                )
            };
            if rc as u32 == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if rc != 0 {
                bail!("QueryDisplayConfig failed: {}", rc);
            }
            paths.truncate(num_paths as usize);
            modes.truncate(num_modes as usize);
            return Ok(Layout { paths, modes });
        }
        bail!("The display layout kept changing")
    }

    /// The displays that are part of the desktop.
    pub fn views(&self) -> Vec<View> {
        self.paths
            .iter()
            .filter_map(|p| {
                let mode = self.source_mode(p)?;
                Some(View {
                    name: source_name(p)?,
                    x: mode.position.x,
                    y: mode.position.y,
                    size: (mode.width, mode.height),
                })
            })
            .collect()
    }

    /// This layout with display `name` alone in it, at the top left and of size `size`: the
    /// desktop taken over.
    pub fn alone(&self, name: &str, size: (u32, u32)) -> ResultType<Layout> {
        let Some(path) = self.path_of(name) else {
            bail!("{} is not part of the layout", name);
        };
        let mut path = self.paths[path];
        let mut source = self.modes[path.sourceInfo.modeInfoIdx as usize];
        let mode = unsafe { source.u.sourceMode_mut() };
        mode.position.x = 0;
        mode.position.y = 0;
        (mode.width, mode.height) = size;
        path.sourceInfo.modeInfoIdx = 0;
        let mut modes = vec![source];
        let target = path.targetInfo.modeInfoIdx;
        if target != DISPLAYCONFIG_PATH_MODE_IDX_INVALID && (target as usize) < self.modes.len() {
            modes.push(self.modes[target as usize]);
            path.targetInfo.modeInfoIdx = 1;
        }
        Ok(Layout {
            paths: vec![path],
            modes,
        })
    }

    /// This layout with display `name` of size `size`, nothing else changed.
    pub fn resized(&self, name: &str, size: (u32, u32)) -> ResultType<Layout> {
        let Some(path) = self.path_of(name) else {
            bail!("{} is not part of the layout", name);
        };
        let mut layout = self.clone();
        let i = layout.paths[path].sourceInfo.modeInfoIdx as usize;
        let mode = unsafe { layout.modes[i].u.sourceMode_mut() };
        (mode.width, mode.height) = size;
        Ok(layout)
    }

    /// Puts this layout in force, for now: the layouts the system saved are left as they are, so
    /// the one it falls back to when a display comes or goes is the one from before.
    pub fn apply(&self) -> Result<(), Refused> {
        let rc = unsafe {
            SetDisplayConfig(
                self.paths.len() as _,
                self.paths.as_ptr(),
                self.modes.len() as _,
                self.modes.as_ptr(),
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
            )
        };
        if rc != 0 {
            return Err(refused("SetDisplayConfig", rc));
        }
        Ok(())
    }

    fn source_mode(&self, path: &DISPLAYCONFIG_PATH_INFO) -> Option<&DISPLAYCONFIG_SOURCE_MODE> {
        let mode = self.modes.get(path.sourceInfo.modeInfoIdx as usize)?;
        (mode.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE)
            .then(|| unsafe { mode.u.sourceMode() })
    }

    fn path_of(&self, name: &str) -> Option<usize> {
        self.paths.iter().position(|p| {
            self.source_mode(p).is_some()
                && source_name(p).map_or(false, |n| n.eq_ignore_ascii_case(name))
        })
    }
}

/// The `\\.\DISPLAYn` a path shows.
fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut source: DISPLAYCONFIG_SOURCE_DEVICE_NAME = unsafe { mem::zeroed() };
    source.header._type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    source.header.size = mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as _;
    source.header.adapterId = path.sourceInfo.adapterId;
    source.header.id = path.sourceInfo.id;
    let rc = unsafe { DisplayConfigGetDeviceInfo(&mut source.header) };
    (rc == 0).then(|| text(&source.viewGdiDeviceName))
}

/// Puts in force the layout the system saved for the displays there are now: the one it falls
/// back to by itself when a display comes or goes.
pub fn apply_saved() -> Result<(), Refused> {
    let rc = unsafe {
        SetDisplayConfig(
            0,
            ptr::null(),
            0,
            ptr::null(),
            SDC_APPLY | SDC_USE_DATABASE_CURRENT,
        )
    };
    if rc != 0 {
        return Err(refused("SetDisplayConfig (saved layout)", rc));
    }
    Ok(())
}
