//! GNOME on Wayland, captured through Mutter's own sessions (`scrap::wayland::mutter_capture`)
//! instead of the portal: no consent dialog for the person at the device, and a display list that
//! changes, virtual monitors and all. For the `--server` root starts (its input goes through
//! uinput) where Mutter's interfaces are on the session bus. Builds with DRM capture keep their
//! own Wayland path, unchanged.
//!
//! Deciding opens no session: one open shows GNOME Shell's "screen being shared" indicator, with
//! nobody connected. The physical monitors are recorded from a login on, and no more once the last
//! remote connection ends, not even by a service still winding down (`mutter_policy` says when
//! else).
#![cfg_attr(feature = "drm", allow(dead_code, unused_imports))]

use super::{
    display_service,
    mutter_policy::{Policy, Step},
    video_service::CapturerInfo,
};
use base::message_proto::{DisplayInfo, Message, MessageBox};
use hbb_common::{bail, log, ResultType};
use scrap::{
    wayland::{
        display::DisplayRect,
        mutter::DisplayState,
        mutter_capture,
    },
    Capturer, Display,
};
use std::{
    sync::{
        atomic::{AtomicU8, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

const UNDECIDED: u8 = 0;
const YES: u8 = 1;
const NO: u8 = 2;
static DECIDED: AtomicU8 = AtomicU8::new(UNDECIDED);
static DECIDING: Mutex<()> = Mutex::new(());
/// Held while deciding whether to record the physical monitors and recording them, so a decision
/// never outlives the last connection ending.
static POLICY: Mutex<Option<Policy>> = Mutex::new(None);
/// `mutter_capture::changes()` when the synced display list was last built from Mutter's, and the
/// uinput range that list calls for.
static SYNCED: Mutex<Option<(u64, Option<(i32, i32, i32, i32)>)>> = Mutex::new(None);
/// How long a capturer of a physical monitor that cannot be recorded now waits before it fails:
/// the video service tries again at once, and would every second otherwise.
const UNAVAILABLE_PAUSE: Duration = Duration::from_secs(2);

/// Decides on a thread of its own, early, so no connection waits for it.
pub(super) fn init() {
    if crate::is_server() {
        std::thread::spawn(active);
    }
}

/// Whether this process captures through Mutter.
pub fn active() -> bool {
    #[cfg(feature = "drm")]
    {
        false
    }
    #[cfg(not(feature = "drm"))]
    match DECIDED.load(Ordering::SeqCst) {
        YES => true,
        NO => false,
        _ => decide(false),
    }
}

/// Asks again should the answer have been no: at a login with nothing captured, GNOME Shell may
/// have come up only since.
pub(super) fn decide_again() {
    #[cfg(not(feature = "drm"))]
    if DECIDED.load(Ordering::SeqCst) == NO {
        decide(true);
    }
}

#[cfg(not(feature = "drm"))]
fn decide(again: bool) -> bool {
    let _one_at_a_time = DECIDING.lock().unwrap();
    match DECIDED.load(Ordering::SeqCst) {
        YES => return true,
        NO if !again => return false,
        _ => {}
    }
    let yes = !crate::platform::linux::is_x11()
        && crate::is_server()
        && mutter_capture::available();
    if yes {
        log::info!("Capturing through GNOME Mutter's own sessions");
    } else if !again {
        log::info!("Not capturing through GNOME Mutter: its interfaces are not on the session bus");
    }
    DECIDED.store(if yes { YES } else { NO }, Ordering::SeqCst);
    yes
}

fn policy<R>(f: impl FnOnce(&mut Policy) -> R) -> R {
    f(POLICY.lock().unwrap().get_or_insert_with(Policy::default))
}

/// The displays as Mutter shows them now, the physical monitors recorded first should they need
/// to be and can be: otherwise those recorded before, if any, and why not.
pub(super) fn displays() -> ResultType<(Vec<Display>, Option<&'static str>)> {
    let state = mutter_capture::state()?;
    let unavailable = record_if_needed(&state).err();
    let displays = crate::vdisplay::stable_order(mutter_capture::displays(&state), |d| d.name());
    Ok((displays, unavailable))
}

/// Every display, for whoever wants them all.
pub(super) fn all_displays() -> ResultType<Vec<Display>> {
    displays().map(|(displays, _)| displays)
}

// Records the physical monitors, should they need to be and can be; answers why not, if not.
fn record_if_needed(state: &DisplayState) -> Result<(), &'static str> {
    let ours = mutter_capture::virtual_connectors();
    let wanted: Vec<String> = state
        .shown()
        .into_iter()
        .map(|s| s.connector)
        .filter(|c| !ours.contains(c))
        .collect();
    let settling = crate::vdisplay::layout_settling();
    let mut policy = POLICY.lock().unwrap();
    let policy = policy.get_or_insert_with(Policy::default);
    let recording = mutter_capture::physical_recording(state);
    let step = policy.step(&recording, &wanted, settling, Instant::now(), || {
        mutter_capture::creation_inhibited()
    });
    match step {
        Step::Idle => Err(super::mutter_policy::IDLE),
        Step::Keep => Ok(()),
        Step::Wait(why) => Err(why),
        Step::Stop => {
            log::warn!("Screen sharing was stopped at the device: not recorded before the next login");
            mutter_capture::stop_physical();
            Err(super::mutter_policy::STOPPED)
        }
        Step::Record => {
            let recorded = mutter_capture::record_physical(state, &ours);
            policy.recorded(recorded.is_ok(), Instant::now());
            match recorded {
                Ok(()) => {
                    log::info!("Recording the monitors {:?} through Mutter", wanted);
                    Ok(())
                }
                Err(e) => {
                    hbb_common::throttled_log!(
                        Duration::from_secs(60),
                        warn,
                        "Failed to record the monitors through Mutter: {}",
                        e
                    );
                    Err(super::mutter_policy::SOON)
                }
            }
        }
    }
}

/// A connection logs in: the displays it is told of, and the primary one. Blocking.
pub(super) fn login() -> ResultType<(Vec<DisplayInfo>, usize)> {
    policy(|p| p.login());
    // Followed from now on, so no change is missed.
    mutter_capture::monitors_changed();
    let (displays, unavailable) = displays()?;
    if displays.is_empty() {
        bail!(unavailable.unwrap_or("No display found on the remote device"));
    }
    let primary = displays.iter().position(|d| d.is_primary()).unwrap_or(0);
    sync(&mut SYNCED.lock().unwrap(), &displays);
    Ok((display_service::get_sync_displays(), primary))
}

/// Brings the synced display list up to date with Mutter's, should anything have changed since
/// it was built or the uinput range not be the one it calls for.
pub(super) fn refresh() {
    let mut synced = SYNCED.lock().unwrap();
    if let Some((changes, rect)) = *synced {
        if changes == mutter_capture::changes() && display_service::wayland_uinput_rect() == rect
        {
            return;
        }
    }
    match displays() {
        Ok((displays, _)) => sync(&mut synced, &displays),
        Err(e) => hbb_common::throttled_log!(
            Duration::from_secs(60),
            warn,
            "Failed to list the displays through Mutter: {}",
            e
        ),
    }
}

// The uinput range first, then the layout the controlling side's coordinates are measured
// against, then the list itself: a coordinate in the new layout never meets the old range. The
// list follows every layout change, so the baseline is always the list's, and nothing is
// remapped (rustdesk#15601 is for a list that does not).
fn sync(synced: &mut Option<(u64, Option<(i32, i32, i32, i32)>)>, displays: &Vec<Display>) {
    // Read before the list: a change landing meanwhile is seen at the next refresh.
    let changes = mutter_capture::changes();
    let (rect, rects) = uinput_layout(displays);
    if crate::input_service::wayland_use_uinput() {
        if let Some(rect) = rect {
            display_service::apply_wayland_layout(rect, rects);
        }
    }
    display_service::check_update_displays(displays);
    *synced = Some((changes, rect));
}

// The uinput range and the per-display rects of `displays`, by the rules the portal path's come
// from (one display: its frames' size; more: what each covers of the desktop).
fn uinput_layout(displays: &[Display]) -> (Option<(i32, i32, i32, i32)>, Vec<DisplayRect>) {
    let infos: Vec<_> = displays
        .iter()
        .map(|d| base::platform::linux::WaylandDisplayInfo {
            name: d.name(),
            x: d.origin().0,
            y: d.origin().1,
            width: d.width() as _,
            height: d.height() as _,
            logical_size: Some((d.logical_width() as _, d.logical_height() as _)),
            refresh_rate: 0,
            // The sizes are turned already.
            transform: 0,
        })
        .collect();
    (
        scrap::wayland::display::desktop_rect_of_displays(&infos),
        scrap::wayland::display::logical_rects_of_displays(&infos),
    )
}

/// A capturer of display `idx` of the synced list, the one the controlling side knows. Only this
/// display's capture fails should it not be capturable now.
pub(super) fn capturer_for(idx: usize) -> ResultType<CapturerInfo> {
    let synced = display_service::get_sync_displays();
    let Some(wanted) = synced.get(idx).map(|d| d.name.clone()) else {
        bail!("No display {} among {}", idx, synced.len());
    };
    let (displays, unavailable) = displays()?;
    let made = displays
        .into_iter()
        .find(|d| d.name() == wanted)
        .ok_or_else(|| hbb_common::anyhow::anyhow!("Display {} is gone", wanted))
        .and_then(|display| {
            let (origin, width, height) = (display.origin(), display.width(), display.height());
            Ok((origin, width, height, Capturer::new(display)?))
        });
    match made {
        Ok((origin, width, height, capturer)) => Ok(CapturerInfo {
            origin,
            width,
            height,
            ndisplay: synced.len(),
            current: idx,
            privacy_mode_id: 0,
            _capturer_privacy_mode_id: 0,
            capturer: Box::new(capturer),
        }),
        Err(e) => match unavailable {
            // Its stream busy a moment, the display moving between capturers: told as it is, and
            // tried again soon.
            Some(why) if !busy(&e) => {
                std::thread::sleep(UNAVAILABLE_PAUSE);
                bail!(why)
            }
            _ => Err(e),
        },
    }
}

// Whether a capturer failed for its display's stream being busy (`scrap`'s `Busy`, `TimedOut` by
// then), not for the display.
fn busy(e: &hbb_common::anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .map_or(false, |e| e.kind() == std::io::ErrorKind::TimedOut)
}

/// While a virtual display has the desktop to itself (`on`), the physical monitors are recorded no
/// more, and not again until let go: under the lock deciding to record them, so no recording
/// begins after this stopped it.
pub(crate) fn hold_physical(on: bool) {
    let mut policy = POLICY.lock().unwrap();
    policy.get_or_insert_with(Policy::default).hold(on);
    if on {
        mutter_capture::stop_physical();
    }
}

/// The last remote connection ended: the physical monitors are recorded no more, and GNOME
/// Shell's indicator goes.
pub(crate) fn last_connection_ended() {
    if active() {
        {
            let mut policy = POLICY.lock().unwrap();
            policy.get_or_insert_with(Policy::default).ended();
            mutter_capture::stop_physical();
        }
        *SYNCED.lock().unwrap() = None;
    }
}

/// What to tell the controlling side of the capture, once: the screen locked, the sharing
/// stopped at the device.
pub(super) fn take_notice() -> Option<Message> {
    let text = policy(|p| p.take_notice())?;
    let mut msg_out = Message::new();
    msg_out.set_message_box(MessageBox {
        msgtype: "nook-nocancel-hasclose".to_owned(),
        title: "Wayland".to_owned(),
        text: text.to_owned(),
        link: "".to_owned(),
        ..Default::default()
    });
    Some(msg_out)
}
