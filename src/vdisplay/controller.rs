//! The controlling side: this device's monitors, and the virtual display to ask the peer for so a
//! monitor shows the remote desktop at the size things have on it.

use base::message_proto::{
    Message, Misc, ToggleVirtualDisplay, VirtualDisplayResult, VirtualDisplaySpec,
};
use std::collections::{BTreeMap, VecDeque};

/// A monitor of this device. Its slot is its index in `local_monitors()`.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    /// Where it is, in the units windows are placed in here: points on macOS, logical pixels on
    /// Wayland, pixels elsewhere.
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Its pixels, and how many of them make one logical pixel.
    pub pixels_w: u32,
    pub pixels_h: u32,
    pub scale: f64,
    pub primary: bool,
}

/// Where a window of this session shows a display: its size in logical pixels (points on macOS),
/// and how many pixels make one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub w: f64,
    pub h: f64,
    pub scale: f64,
}

/// The virtual displays a session holds on the peer, by slot: the size each was asked for, to ask
/// again after a reconnection, and the name the peer gave it, empty until the peer answers; and the
/// view each is shown in, whose size it follows.
///
/// The user may switch a slot on and off faster than the peer answers. The peer answers every
/// request once, in turn, so each answer belongs to the oldest request not answered yet, and only
/// the answer to the last request concerning a slot says what the slot holds; the ones before it
/// are stale by the time they come. A removal of all concerns every slot.
#[derive(Debug, Default)]
pub struct HeldDisplays {
    held: BTreeMap<i32, HeldDisplay>,
    // The requests not answered yet, oldest first: each one's slot (-1 for all), number, and the
    // mode it asked for.
    unanswered: VecDeque<(i32, u64, i32)>,
    // The number of the last request concerning each slot.
    last: BTreeMap<i32, u64>,
    numbered: u64,
    // The view each slot is shown in, as the window showing it last told.
    views: BTreeMap<i32, View>,
}

#[derive(Clone, Debug, Default, PartialEq)]
struct HeldDisplay {
    spec: VirtualDisplaySpec,
    name: String,
    // The mode the peer uses it in, as its answers in turn tell, the stale ones too: the one asked
    // for when it did as asked, none (shared) for a display it removed.
    used: i32,
}

impl HeldDisplays {
    /// Notes a request: `slot` with `spec` on, or off; -1 off for every slot.
    pub fn ask(&mut self, slot: i32, on: bool, spec: VirtualDisplaySpec) {
        let n = self.number(slot, if on { spec.mode } else { SHARED });
        if slot == ALL {
            for last in self.last.values_mut() {
                *last = n;
            }
            self.held.clear();
            self.views.clear();
        } else if on {
            self.held.entry(slot).or_default().spec = spec;
        } else {
            self.held.remove(&slot);
            self.views.remove(&slot);
        }
    }

    /// Notes the view `slot` is shown in.
    pub fn set_view(&mut self, slot: i32, view: View) {
        self.views.insert(slot, view);
    }

    /// The view `slot` is shown in, if a window told.
    pub fn view(&self, slot: i32) -> Option<View> {
        self.views.get(&slot).copied()
    }

    /// The request that brings `slot` to the size `spec_for` gives its view, noted as asked. None
    /// while the slot is not held or has no view, while a request concerning it is not answered
    /// yet (the answer comes back here: one request at a time, the last view winning), or when it
    /// already has that size.
    pub fn follow(
        &mut self,
        slot: i32,
        spec_for: impl Fn(&View) -> VirtualDisplaySpec,
    ) -> Option<VirtualDisplaySpec> {
        if self
            .unanswered
            .iter()
            .any(|(asked, _, _)| *asked == slot || *asked == ALL)
        {
            return None;
        }
        let spec = spec_for(self.views.get(&slot)?);
        if self.held.get(&slot)?.spec == spec {
            return None;
        }
        self.ask(slot, true, spec.clone());
        Some(spec)
    }

    /// The requests that take every held display back after a reconnection, noted as asked.
    pub fn ask_again(&mut self) -> Vec<(i32, VirtualDisplaySpec)> {
        // What the old connection asked and never heard back about, it never will.
        self.unanswered.clear();
        let again: Vec<(i32, VirtualDisplaySpec)> = self
            .held
            .iter()
            .map(|(slot, held)| (*slot, held.spec.clone()))
            .collect();
        for (slot, spec) in &again {
            self.number(*slot, spec.mode);
        }
        again
    }

    /// Takes in the peer's answer. Some(mode) when the peer kept the slot but did not use it in the
    /// mode last asked for: it keeps the one it had, `mode`, which the slot is asked for in from
    /// then on, lest a reconnection ask again what was refused. Not so for the shared screen: the
    /// peer gives a desktop back as soon as it can.
    pub fn answer(&mut self, r: &VirtualDisplayResult) -> Option<i32> {
        // Answered in turn: the requests before this one's will not be answered any more.
        let asked = self
            .unanswered
            .iter()
            .position(|(slot, _, _)| *slot == r.slot)
            .and_then(|i| self.unanswered.drain(..=i).last());
        if r.slot == ALL {
            // Everything it removed came off when it was asked.
            return None;
        }
        // Stale or not, the peer tells how the display is used now; one from before that does not,
        // and done as asked it is used in the mode asked for, none for a removal.
        if let Some(held) = self.held.get_mut(&r.slot) {
            match asked {
                _ if r.used > 0 => held.used = r.used - 1,
                Some((_, _, mode)) if r.error.is_empty() => held.used = mode,
                _ => {}
            }
        }
        if let Some((_, n, _)) = asked {
            if self.last.get(&r.slot) != Some(&n) {
                return None;
            }
        }
        if !r.on {
            self.held.remove(&r.slot);
            self.views.remove(&r.slot);
            return None;
        }
        // Off for us but still there, a removal that failed: still ours.
        let held = self.held.entry(r.slot).or_default();
        held.name = r.name.clone();
        let (_, _, mode) = asked?;
        if mode == held.used || mode == SHARED {
            return None;
        }
        held.spec.mode = held.used;
        Some(held.used)
    }

    /// Takes in word from the peer that `slot` is a shared extra screen now, by its own doing
    /// (`VirtualDisplayResult.notice`): asked for as one from now on, after a reconnection too,
    /// lest that take the peer's desktop over again. Answers the request that says so to the
    /// peer, noted as asked, which also tells it the word was heard: until then it does not
    /// take its desktop over for the slot again. None if the slot is not held.
    pub fn now_shared(&mut self, slot: i32) -> Option<VirtualDisplaySpec> {
        let spec = VirtualDisplaySpec {
            mode: 0,
            ..self.held.get(&slot)?.spec.clone()
        };
        self.ask(slot, true, spec.clone());
        Some(spec)
    }

    /// Slot -> the name the peer gave it, empty until it answers.
    pub fn names(&self) -> BTreeMap<i32, String> {
        self.held
            .iter()
            .map(|(slot, held)| (*slot, held.name.clone()))
            .collect()
    }

    fn number(&mut self, slot: i32, mode: i32) -> u64 {
        self.numbered += 1;
        self.unanswered.push_back((slot, self.numbered, mode));
        if slot != ALL {
            self.last.insert(slot, self.numbered);
        }
        self.numbered
    }
}

const ALL: i32 = -1;
const SHARED: i32 = 0;

/// The value of the mode option (`OPTION_VIRTUAL_DISPLAY_MODE`) that asks for `mode`
/// (`VirtualDisplaySpec.mode`): empty for the shared screen.
pub fn mode_option(mode: i32) -> &'static str {
    match mode {
        1 => "privacy",
        2 => "nested",
        _ => "",
    }
}

/// What the peer's virtual displays can do.
#[derive(Clone, Debug, PartialEq)]
pub struct PeerCaps {
    pub hidpi: bool,
    /// The modes the peer offers (`VirtualDisplaySpec.mode` names). A peer from before modes lists
    /// none; it still does the shared extra screen, so that is the fallback. The menu greys out
    /// any mode not here.
    pub modes: Vec<String>,
}

impl PeerCaps {
    /// From `PeerInfo.platform_additions`; None if the peer offers no virtual displays.
    pub fn parse(platform_additions: &str) -> Option<Self> {
        let additions: serde_json::Value = serde_json::from_str(platform_additions).ok()?;
        let caps = additions.get("vdm_caps")?;
        let modes = caps
            .get("modes")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .filter(|m: &Vec<String>| !m.is_empty())
            .unwrap_or_else(|| vec!["shared".to_owned()]);
        Some(Self {
            hidpi: caps.get("hidpi").and_then(|v| v.as_bool()).unwrap_or(false),
            modes,
        })
    }

    /// Whether the peer offers a mode by its name.
    pub fn has_mode(&self, name: &str) -> bool {
        self.modes.iter().any(|m| m == name)
    }
}

/// The virtual display for monitor `m`. A HiDPI peer gets the monitor's pixels at its scale. Any
/// other peer gets the logical size, so text keeps the size it has here; with `pixels`, all the
/// pixels at scale 1 instead, for more room and smaller text.
pub fn spec_for(m: &Monitor, caps: &PeerCaps, pixels: bool) -> VirtualDisplaySpec {
    let logical = |px: u32| (((px as f64) / m.scale).round() as u32) & !1;
    let logical = (logical(m.pixels_w), logical(m.pixels_h));
    spec_of((m.pixels_w, m.pixels_h), logical, m.scale, caps, pixels)
}

/// The virtual display for `view`, by the same rule as for a monitor: one the view shows whole, so
/// rounded down, never past its edges.
pub fn spec_for_view(view: &View, caps: &PeerCaps, pixels: bool) -> VirtualDisplaySpec {
    let down = |v: f64| ((v + 1e-6).floor().max(0.0) as u32) & !1;
    let pixels_wh = (down(view.w * view.scale), down(view.h * view.scale));
    spec_of(pixels_wh, (down(view.w), down(view.h)), view.scale, caps, pixels)
}

fn spec_of(
    (pixels_w, pixels_h): (u32, u32),
    logical: (u32, u32),
    scale: f64,
    caps: &PeerCaps,
    pixels: bool,
) -> VirtualDisplaySpec {
    let (width, height, scale) = if caps.hidpi {
        (pixels_w, pixels_h, scale)
    } else if pixels || scale <= 1.0 {
        (pixels_w, pixels_h, 1.0)
    } else {
        (logical.0, logical.1, 1.0)
    };
    VirtualDisplaySpec {
        width: width as i32,
        height: height as i32,
        scale,
        ..Default::default()
    }
}

/// The request for `slot`. A removal carries an empty spec all the same: it is what tells the fork
/// protocol from the upstream one.
pub fn toggle_msg(slot: i32, on: bool, spec: VirtualDisplaySpec) -> Message {
    let mut misc = Misc::new();
    misc.set_toggle_virtual_display(ToggleVirtualDisplay {
        display: slot,
        on,
        spec: Some(spec).into(),
        ..Default::default()
    });
    let mut msg = Message::new();
    msg.set_misc(misc);
    msg
}

/// This device's monitors, in the order `main_get_displays` lists them for window placement.
#[cfg(not(any(target_os = "android", target_os = "ios")))]
pub fn local_monitors() -> Vec<Monitor> {
    #[cfg(target_os = "linux")]
    if !crate::platform::linux::is_x11() {
        return wayland_monitors();
    }
    match crate::display_service::try_get_displays() {
        Ok(displays) => displays.iter().map(monitor_of).collect(),
        Err(e) => {
            hbb_common::log::error!("Failed to list the local monitors: {}", e);
            Vec::new()
        }
    }
}

#[cfg(any(target_os = "android", target_os = "ios"))]
pub fn local_monitors() -> Vec<Monitor> {
    Vec::new()
}

#[cfg(target_os = "macos")]
fn monitor_of(d: &scrap::Display) -> Monitor {
    use core_graphics::display::CGDisplay;
    let display = CGDisplay::new(d.name().parse().unwrap_or_default());
    let bounds = display.bounds();
    let (w, h) = (bounds.size.width as i32, bounds.size.height as i32);
    // The mode's pixels, which a scaled ("looks like") mode keeps apart from its points.
    let (pixels_w, pixels_h) = display
        .display_mode()
        .map(|mode| (mode.pixel_width() as u32, mode.pixel_height() as u32))
        .filter(|&(pw, ph)| pw > 0 && ph > 0)
        .unwrap_or((w.max(0) as u32, h.max(0) as u32));
    Monitor {
        x: bounds.origin.x as i32,
        y: bounds.origin.y as i32,
        w,
        h,
        pixels_w,
        pixels_h,
        scale: if w > 0 { pixels_w as f64 / w as f64 } else { 1.0 },
        primary: display.is_main(),
    }
}

#[cfg(windows)]
fn monitor_of(d: &scrap::Display) -> Monitor {
    use winapi::{
        shared::windef::POINT,
        um::{
            shellscalingapi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
            winuser::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST},
        },
    };
    let (x, y) = d.origin();
    let (w, h) = (d.width() as i32, d.height() as i32);
    // Desktop coordinates are pixels here; the scale is the monitor's DPI setting.
    let center = POINT {
        x: x + w / 2,
        y: y + h / 2,
    };
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    let ok = unsafe {
        let monitor = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == 0
    };
    Monitor {
        x,
        y,
        w,
        h,
        pixels_w: w.max(0) as u32,
        pixels_h: h.max(0) as u32,
        scale: if ok && dpi_x > 0 {
            dpi_x as f64 / 96.0
        } else {
            1.0
        },
        primary: d.is_primary(),
    }
}

// X11 has no scale of its own per monitor.
#[cfg(target_os = "linux")]
fn monitor_of(d: &scrap::Display) -> Monitor {
    let (x, y) = d.origin();
    let (w, h) = (d.width() as i32, d.height() as i32);
    Monitor {
        x,
        y,
        w,
        h,
        pixels_w: w.max(0) as u32,
        pixels_h: h.max(0) as u32,
        scale: 1.0,
        primary: d.is_primary(),
    }
}

// Listing displays the X11 way would start a screen cast on Wayland; the outputs say enough.
#[cfg(target_os = "linux")]
fn wayland_monitors() -> Vec<Monitor> {
    let displays = scrap::wayland::display::get_displays();
    displays
        .displays
        .iter()
        .enumerate()
        .map(|(i, d)| {
            // The mode keeps its unrotated size; the logical size comes rotated.
            let (pixels_w, pixels_h) = if d.transform == 90 || d.transform == 270 {
                (d.height, d.width)
            } else {
                (d.width, d.height)
            };
            let (w, h) = d.logical_size.unwrap_or((pixels_w, pixels_h));
            Monitor {
                x: d.x,
                y: d.y,
                w,
                h,
                pixels_w: pixels_w.max(0) as u32,
                pixels_h: pixels_h.max(0) as u32,
                scale: if w > 0 {
                    pixels_w as f64 / w as f64
                } else {
                    1.0
                },
                primary: i == displays.primary,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base::message_proto::misc;

    fn monitor(pixels_w: u32, pixels_h: u32, scale: f64) -> Monitor {
        Monitor {
            x: 0,
            y: 0,
            w: (pixels_w as f64 / scale) as i32,
            h: (pixels_h as f64 / scale) as i32,
            pixels_w,
            pixels_h,
            scale,
            primary: true,
        }
    }

    fn size(spec: VirtualDisplaySpec) -> (i32, i32, f64) {
        (spec.width, spec.height, spec.scale)
    }

    #[test]
    fn a_plain_monitor_asks_for_its_pixels() {
        let caps = PeerCaps {
            hidpi: false,
            modes: vec!["shared".to_owned()],
        };
        assert_eq!(
            size(spec_for(&monitor(2560, 1440, 1.0), &caps, false)),
            (2560, 1440, 1.0)
        );
    }

    #[test]
    fn a_retina_monitor_asks_for_its_logical_size_unless_told_otherwise() {
        let retina = monitor(2880, 1800, 2.0);
        let plain_peer = PeerCaps {
            hidpi: false,
            modes: vec!["shared".to_owned()],
        };
        assert_eq!(size(spec_for(&retina, &plain_peer, false)), (1440, 900, 1.0));
        assert_eq!(size(spec_for(&retina, &plain_peer, true)), (2880, 1800, 1.0));
        // A HiDPI peer shows all the pixels at the same size.
        let hidpi_peer = PeerCaps {
            hidpi: true,
            modes: vec!["shared".to_owned()],
        };
        assert_eq!(size(spec_for(&retina, &hidpi_peer, false)), (2880, 1800, 2.0));
        // Windows at 150%: the logical size is rounded to even sides.
        let scaled = monitor(2560, 1440, 1.5);
        assert_eq!(size(spec_for(&scaled, &plain_peer, false)), (1706, 960, 1.0));
    }

    fn plain_peer() -> PeerCaps {
        PeerCaps {
            hidpi: false,
            modes: vec!["shared".to_owned()],
        }
    }

    #[test]
    fn a_view_asks_for_a_display_it_shows_whole() {
        // A Retina window, its view 2048x1031.5 points.
        let retina = View {
            w: 2048.0,
            h: 1031.5,
            scale: 2.0,
        };
        assert_eq!(size(spec_for_view(&retina, &plain_peer(), false)), (2048, 1030, 1.0));
        assert_eq!(size(spec_for_view(&retina, &plain_peer(), true)), (4096, 2062, 1.0));
        let hidpi_peer = PeerCaps {
            hidpi: true,
            modes: vec!["shared".to_owned()],
        };
        assert_eq!(size(spec_for_view(&retina, &hidpi_peer, false)), (4096, 2062, 2.0));
        // Windows at 125%: fractions of a logical pixel are dropped, not rounded up past the view.
        let scaled = View {
            w: 1535.2,
            h: 839.6,
            scale: 1.25,
        };
        assert_eq!(size(spec_for_view(&scaled, &plain_peer(), false)), (1534, 838, 1.0));
        assert_eq!(size(spec_for_view(&scaled, &plain_peer(), true)), (1918, 1048, 1.0));
    }

    #[test]
    fn only_peers_offering_virtual_displays_have_caps() {
        assert_eq!(PeerCaps::parse(""), None);
        assert_eq!(PeerCaps::parse(r#"{"is_wayland":true}"#), None);
        // A peer from before modes still does the shared extra screen.
        assert_eq!(
            PeerCaps::parse(r#"{"vdm_caps":{"v":1,"max":2}}"#),
            Some(PeerCaps {
                hidpi: false,
                modes: vec!["shared".to_owned()],
            })
        );
        assert_eq!(
            PeerCaps::parse(r#"{"vdm_caps":{"v":1,"hidpi":true}}"#),
            Some(PeerCaps {
                hidpi: true,
                modes: vec!["shared".to_owned()],
            })
        );
        // A peer that lists its modes offers exactly those.
        let caps = PeerCaps::parse(r#"{"vdm_caps":{"v":1,"modes":["shared","privacy","nested"]}}"#)
            .unwrap();
        assert_eq!(caps.modes, vec!["shared", "privacy", "nested"]);
        assert!(caps.has_mode("privacy") && caps.has_mode("nested"));
        assert!(!caps.has_mode("concurrent"));
        // An empty list falls back to the shared screen rather than offering nothing.
        let caps = PeerCaps::parse(r#"{"vdm_caps":{"v":1,"modes":[]}}"#).unwrap();
        assert_eq!(caps.modes, vec!["shared"]);
    }

    fn answer(slot: i32, name: Option<&str>, error: &str) -> VirtualDisplayResult {
        VirtualDisplayResult {
            slot,
            on: name.is_some(),
            name: name.unwrap_or_default().to_owned(),
            error: error.to_owned(),
            ..Default::default()
        }
    }

    fn sized(width: i32) -> VirtualDisplaySpec {
        VirtualDisplaySpec {
            width,
            height: 1080,
            scale: 1.0,
            ..Default::default()
        }
    }

    fn names(h: &HeldDisplays) -> Vec<(i32, String)> {
        h.names().into_iter().collect()
    }

    #[test]
    fn a_slot_is_held_from_the_request_and_named_by_the_answer() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        assert_eq!(names(&h), vec![(0, String::new())]);
        h.answer(&answer(0, Some("DP-1"), ""));
        assert_eq!(names(&h), vec![(0, "DP-1".to_owned())]);
        // A failed addition gives the slot up.
        h.ask(1, true, sized(1920));
        h.answer(&answer(1, None, "No free virtual display"));
        assert_eq!(names(&h), vec![(0, "DP-1".to_owned())]);
    }

    #[test]
    fn only_the_last_answer_for_a_slot_counts() {
        // On, then off before the peer answered: the late "on" must not bring the slot back.
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.ask(0, false, VirtualDisplaySpec::default());
        h.answer(&answer(0, Some("DP-1"), ""));
        assert!(names(&h).is_empty());
        h.answer(&answer(0, None, ""));
        assert!(names(&h).is_empty());
        // Off, then on again: the late "off" must not drop the slot asked for since.
        h.ask(0, true, sized(2560));
        h.answer(&answer(0, Some("DP-1"), ""));
        h.ask(0, false, VirtualDisplaySpec::default());
        h.ask(0, true, sized(2560));
        h.answer(&answer(0, None, ""));
        assert_eq!(names(&h), vec![(0, String::new())]);
        h.answer(&answer(0, Some("DP-2"), ""));
        assert_eq!(names(&h), vec![(0, "DP-2".to_owned())]);
    }

    #[test]
    fn a_failed_removal_leaves_the_display_held() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.answer(&answer(0, Some("DP-1"), ""));
        h.ask(0, false, VirtualDisplaySpec::default());
        h.answer(&answer(0, Some("DP-1"), "busy"));
        assert_eq!(names(&h), vec![(0, "DP-1".to_owned())]);
    }

    #[test]
    fn removing_all_and_asking_again() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.ask(1, true, sized(1920));
        h.answer(&answer(0, Some("DP-1"), ""));
        h.answer(&answer(1, Some("DP-2"), ""));
        // A request the connection died with is never answered.
        h.ask(1, true, sized(1920));
        // After a reconnection both are asked for again, with the sizes they were asked for.
        assert_eq!(h.ask_again(), vec![(0, sized(2560)), (1, sized(1920))]);
        h.answer(&answer(0, Some("DP-1"), ""));
        h.answer(&answer(1, Some("DP-2"), ""));
        h.ask(-1, false, VirtualDisplaySpec::default());
        assert!(names(&h).is_empty());
        h.answer(&answer(-1, None, ""));
        assert!(names(&h).is_empty());
        // A slot asked for after a removal of all is not affected by its answer.
        h.ask(-1, false, VirtualDisplaySpec::default());
        h.ask(2, true, sized(1280));
        h.answer(&answer(-1, None, ""));
        assert_eq!(names(&h), vec![(2, String::new())]);
        h.answer(&answer(2, Some("DP-3"), ""));
        assert_eq!(names(&h), vec![(2, "DP-3".to_owned())]);
    }

    #[test]
    fn a_removal_of_all_outdates_what_was_asked_before_it() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.ask(-1, false, VirtualDisplaySpec::default());
        // The addition's answer comes first, but the removal of all came after it.
        h.answer(&answer(0, Some("DP-1"), ""));
        assert!(names(&h).is_empty());
        h.answer(&answer(-1, None, ""));
        assert!(names(&h).is_empty());
    }

    #[test]
    fn a_peer_holding_less_than_we_think_does_not_stall_later_answers() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.answer(&answer(0, Some("DP-1"), ""));
        // The peer lost the display meanwhile; removing all is answered once, for all.
        h.ask(-1, false, VirtualDisplaySpec::default());
        h.answer(&answer(-1, None, ""));
        h.ask(0, true, sized(2560));
        h.answer(&answer(0, Some("DP-2"), ""));
        assert_eq!(names(&h), vec![(0, "DP-2".to_owned())]);
    }

    #[test]
    fn an_answer_skips_requests_that_were_never_answered() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.ask(1, true, sized(1920));
        // The answer for 0 never comes; the one for 1 does, and 0 is left as asked.
        h.answer(&answer(1, Some("DP-2"), ""));
        assert_eq!(names(&h), vec![(0, String::new()), (1, "DP-2".to_owned())]);
        h.ask(0, false, VirtualDisplaySpec::default());
        h.answer(&answer(0, None, ""));
        assert_eq!(names(&h), vec![(1, "DP-2".to_owned())]);
    }

    fn view(w: f64) -> View {
        View {
            w,
            h: 1080.0,
            scale: 1.0,
        }
    }

    #[test]
    fn a_held_display_follows_its_view_one_request_at_a_time() {
        let to_view = |v: &View| spec_for_view(v, &plain_peer(), false);
        let mut h = HeldDisplays::default();
        // Not held: nothing to follow.
        h.set_view(0, view(1600.0));
        assert_eq!(h.follow(0, to_view), None);
        h.ask(0, true, sized(2560));
        // The addition is not answered yet: its answer brings the view in.
        assert_eq!(h.follow(0, to_view), None);
        h.answer(&answer(0, Some("DP-1"), ""));
        assert_eq!(h.follow(0, to_view), Some(sized(1600)));
        // The window is resized twice meanwhile; one request, for the last size, once answered.
        h.set_view(0, view(1400.0));
        h.set_view(0, view(1200.0));
        assert_eq!(h.follow(0, to_view), None);
        h.answer(&answer(0, Some("DP-1"), ""));
        assert_eq!(h.follow(0, to_view), Some(sized(1200)));
        h.answer(&answer(0, Some("DP-1"), ""));
        // At its size already.
        assert_eq!(h.follow(0, to_view), None);
        // After a reconnection, the size last asked for is asked for again.
        assert_eq!(h.ask_again(), vec![(0, sized(1200))]);
        h.answer(&answer(0, Some("DP-1"), ""));
        // A request for another slot does not hold this one back.
        h.set_view(0, view(1000.0));
        h.ask(1, true, sized(1920));
        assert_eq!(h.follow(0, to_view), Some(sized(1000)));
    }

    #[test]
    fn a_display_the_peer_made_shared_is_asked_for_as_shared_again() {
        let privacy = VirtualDisplaySpec {
            mode: 1,
            ..sized(2560)
        };
        let mut h = HeldDisplays::default();
        h.ask(0, true, privacy);
        h.answer(&answer(0, Some("DP-1"), ""));
        // Said so to the peer, the request answered as any.
        assert_eq!(h.now_shared(0), Some(sized(2560)));
        h.answer(&answer(0, Some("DP-1"), ""));
        // Word of a slot not held changes nothing.
        assert_eq!(h.now_shared(1), None);
        assert_eq!(names(&h), vec![(0, "DP-1".to_owned())]);
        assert_eq!(h.ask_again(), vec![(0, sized(2560))]);
    }

    fn moded(width: i32, mode: i32) -> VirtualDisplaySpec {
        VirtualDisplaySpec {
            mode,
            ..sized(width)
        }
    }

    #[test]
    fn a_mode_the_peer_refused_is_put_back_to_the_one_it_uses() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "")), None);
        // Taking the desktop over refused: the display stays shared, at the size asked.
        h.ask(0, true, moded(1920, 1));
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "Recording")), Some(0));
        assert_eq!(names(&h), vec![(0, "DP-1".to_owned())]);
        assert_eq!(h.ask_again(), vec![(0, sized(1920))]);
        h.answer(&answer(0, Some("DP-1"), ""));
        // Taken over, then a nested session refused: still taken over.
        h.ask(0, true, moded(1920, 1));
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "")), None);
        h.ask(0, true, moded(1920, 2));
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "No nested")), Some(1));
        assert_eq!(h.ask_again(), vec![(0, moded(1920, 1))]);
        h.answer(&answer(0, Some("DP-1"), ""));
        // A resize that failed in the mode used puts nothing back.
        h.ask(0, true, moded(1280, 1));
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "Too big")), None);
        assert_eq!(h.ask_again(), vec![(0, moded(1280, 1))]);
        h.answer(&answer(0, Some("DP-1"), ""));
        // Giving the desktop back failed: the peer tries again, and the slot is shared for us.
        h.ask(0, true, sized(1280));
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "Busy")), None);
        assert_eq!(h.ask_again(), vec![(0, sized(1280))]);
        // A new display made an extra screen rather than taking the desktop over.
        let mut h = HeldDisplays::default();
        h.ask(1, true, moded(2560, 1));
        assert_eq!(h.answer(&answer(1, Some("DP-2"), "No privacy")), Some(0));
        assert_eq!(h.ask_again(), vec![(1, sized(2560))]);
        // One refused outright is gone, nothing to put back.
        h.ask(2, true, moded(2560, 1));
        assert_eq!(h.answer(&answer(2, None, "Recording")), None);
        assert_eq!(names(&h), vec![(1, "DP-2".to_owned())]);
    }

    #[test]
    fn the_mode_used_is_told_by_the_answers_stale_ones_too() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.answer(&answer(0, Some("DP-1"), ""));
        // Taking over asked twice (the mode chosen, then the window resized) before an answer,
        // and refused twice: back to shared, not to the mode the first request put down.
        h.ask(0, true, moded(2560, 1));
        h.ask(0, true, moded(1920, 1));
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "Recording")), None);
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "Recording")), Some(0));
        // The first done as asked, the second refused for its size: taken over still.
        h.ask(0, true, moded(2560, 1));
        h.ask(0, true, moded(1920, 1));
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "")), None);
        assert_eq!(h.answer(&answer(0, Some("DP-1"), "Too big")), None);
        assert_eq!(h.ask_again(), vec![(0, moded(1920, 1))]);
        h.answer(&answer(0, Some("DP-1"), ""));
        // Removed and asked for again before the answers: the new display was never taken over.
        h.ask(0, false, VirtualDisplaySpec::default());
        h.ask(0, true, moded(1920, 1));
        assert_eq!(h.answer(&answer(0, None, "")), None);
        assert_eq!(h.answer(&answer(0, Some("DP-2"), "No privacy")), Some(0));
        // So too when the display removed was taken over by a request the answers came in for.
        h.ask(0, true, moded(1920, 1));
        h.ask(0, false, VirtualDisplaySpec::default());
        h.ask(0, true, moded(1920, 1));
        assert_eq!(h.answer(&answer(0, Some("DP-2"), "")), None);
        assert_eq!(h.answer(&answer(0, None, "")), None);
        assert_eq!(h.answer(&answer(0, Some("DP-3"), "No privacy")), Some(0));
    }

    fn used(slot: i32, name: &str, error: &str, mode: i32) -> VirtualDisplayResult {
        VirtualDisplayResult {
            used: mode + 1,
            ..answer(slot, Some(name), error)
        }
    }

    #[test]
    fn the_mode_the_peer_tells_it_uses_wins_over_what_was_asked() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.answer(&used(0, "DP-1", "", 0));
        // Taking over failed, and so did lighting the screens again: taken over all the same.
        h.ask(0, true, moded(2560, 1));
        assert_eq!(h.answer(&used(0, "DP-1", "No screens lit", 1)), None);
        assert_eq!(h.ask_again(), vec![(0, moded(2560, 1))]);
        h.answer(&used(0, "DP-1", "", 1));
        // A nested session refused, and the desktop given back on the way: shared, not taken over.
        h.ask(0, true, moded(2560, 2));
        assert_eq!(h.answer(&used(0, "DP-1", "No nested", 0)), Some(0));
        assert_eq!(h.ask_again(), vec![(0, sized(2560))]);
        h.answer(&used(0, "DP-1", "", 0));
        // Told by a stale answer too: the first request taken over in the end, the second refused.
        h.ask(0, true, moded(2560, 1));
        h.ask(0, true, moded(1920, 1));
        assert_eq!(h.answer(&used(0, "DP-1", "Recording", 1)), None);
        assert_eq!(h.answer(&used(0, "DP-1", "Too big", 1)), None);
        // A removal that failed tells it too.
        h.ask(0, false, VirtualDisplaySpec::default());
        h.ask(0, true, moded(1920, 2));
        assert_eq!(h.answer(&used(0, "DP-1", "Busy", 1)), None);
        assert_eq!(h.answer(&used(0, "DP-1", "No nested", 1)), Some(1));
        assert_eq!(h.ask_again(), vec![(0, moded(1920, 1))]);
    }

    #[test]
    fn a_mode_asks_for_its_option() {
        assert_eq!(mode_option(0), "");
        assert_eq!(mode_option(1), "privacy");
        assert_eq!(mode_option(2), "nested");
        assert_eq!(mode_option(3), "");
    }

    #[test]
    fn a_view_goes_with_its_display() {
        let mut h = HeldDisplays::default();
        h.ask(0, true, sized(2560));
        h.set_view(0, view(1600.0));
        h.ask(0, false, VirtualDisplaySpec::default());
        assert_eq!(h.view(0), None);
        // An addition the peer refused.
        h.set_view(1, view(1600.0));
        h.ask(1, true, sized(1600));
        h.answer(&answer(0, None, ""));
        h.answer(&answer(1, None, "No free virtual display"));
        assert_eq!(h.view(1), None);
        h.set_view(2, view(1600.0));
        h.ask(-1, false, VirtualDisplaySpec::default());
        assert_eq!(h.view(2), None);
    }

    #[test]
    fn requests_always_carry_a_spec() {
        let msg = toggle_msg(-1, false, VirtualDisplaySpec::default());
        let Some(misc::Union::ToggleVirtualDisplay(t)) = msg.misc().union.clone() else {
            panic!("not a toggle");
        };
        assert!(t.spec.is_some() && t.display == -1 && !t.on);
    }
}
