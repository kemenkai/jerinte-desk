//! Where the monitors go while virtual monitors of ours come and go. Mutter looks its stored layouts
//! up by every monitor, ours included, so each time one of ours is added or resized it finds none
//! and lays everything out side by side; the other monitors are put back as they were, ours to
//! their right. Plain, for its tests: `super` reads Mutter and applies what this says.

use scrap::mutter_model::{DisplayState, LogicalConfig, Monitor, MonitorMode};

/// The layout of every monitor that is not ours, as it was. None remembered (`default`): every
/// monitor not ours taken for one plugged in since (`give_back`).
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct Remembered {
    /// Moved together so that the top left one is at 0,0, should ours have been above or left of
    /// them.
    logical: Vec<LogicalConfig>,
    layout_logical: bool,
    /// Every monitor not ours, on or off: one plugged in or out since makes this layout stale.
    monitors: Vec<String>,
    /// How many groups joined through shared edges all the monitors, ours too, were in: Mutter
    /// took that many.
    groups: usize,
}

impl Remembered {
    /// The layout `state` shows of the monitors not `ours`. None where Mutter shows a monitor
    /// without its mode.
    pub(super) fn of(state: &DisplayState, ours: &[String]) -> Option<Self> {
        let all = current(state)?;
        let groups = groups(&rects(&all, state, state.layout_logical)?);
        let mut logical: Vec<LogicalConfig> = all
            .into_iter()
            .filter(|l| l.monitors.iter().all(|(c, _)| !ours.contains(c)))
            .collect();
        let left = logical.iter().map(|l| l.x).min().unwrap_or_default();
        let top = logical.iter().map(|l| l.y).min().unwrap_or_default();
        for l in &mut logical {
            l.x -= left;
            l.y -= top;
        }
        Some(Self {
            logical,
            layout_logical: state.layout_logical,
            monitors: others(state, ours),
            groups,
        })
    }
}

/// How the monitors are to be laid out, `ours` in the order they were made: the others as
/// remembered, ours each on its own to their right, along the top of the rightmost of the others,
/// so each shares an edge with the one before it. With none of the others primary (there are none,
/// or one of ours was made primary in Settings), the one of ours Mutter has primary stays so, else
/// the first. None where that cannot be: the other monitors are not those remembered or are laid
/// out in the other mode, a remembered mode is gone, or one of ours has no mode yet.
pub(super) fn wanted(
    remembered: &Remembered,
    state: &DisplayState,
    ours: &[String],
) -> Option<Vec<LogicalConfig>> {
    if others(state, ours) != remembered.monitors
        || state.layout_logical != remembered.layout_logical
    {
        return None;
    }
    let mut wanted = remembered.logical.clone();
    // Where the first of ours goes: along the top of the logical monitor reaching furthest right.
    let mut right = 0;
    let mut top = 0;
    for l in &wanted {
        let (w, _) = size(l, state, remembered.layout_logical)?;
        if l.x + w > right || (l.x + w == right && l.y < top) {
            right = l.x + w;
            top = l.y;
        }
    }
    let primary = (!wanted.iter().any(|l| l.primary)).then(|| {
        ours.iter()
            .position(|c| {
                state
                    .logical
                    .iter()
                    .any(|l| l.primary && l.connectors.contains(c))
            })
            .unwrap_or(0)
    });
    for (i, connector) in ours.iter().enumerate() {
        let mode = usable_mode(state.monitor(connector)?)?;
        let l = LogicalConfig {
            x: right,
            y: top,
            scale: 1.0,
            transform: 0,
            primary: primary == Some(i),
            monitors: vec![(connector.clone(), mode.id.clone())],
        };
        right += size(&l, state, remembered.layout_logical)?.0;
        wanted.push(l);
    }
    Some(wanted)
}

/// The layout `state` shows. None where Mutter shows a monitor without its mode.
pub(super) fn current(state: &DisplayState) -> Option<Vec<LogicalConfig>> {
    state
        .logical
        .iter()
        .map(|l| {
            let monitors = l
                .connectors
                .iter()
                .map(|c| Some((c.clone(), state.monitor(c)?.current_mode()?.id.clone())))
                .collect::<Option<Vec<_>>>()?;
            Some(LogicalConfig {
                x: l.x,
                y: l.y,
                scale: l.scale,
                transform: l.transform,
                primary: l.primary,
                monitors,
            })
        })
        .collect()
}

/// Whether two layouts are the same, in whatever order they list the logical monitors.
pub(super) fn same(a: &[LogicalConfig], b: &[LogicalConfig]) -> bool {
    let sorted = |c: &[LogicalConfig]| {
        let mut c = c.to_vec();
        for l in &mut c {
            l.monitors.sort();
        }
        c.sort_by(|p, q| (p.x, p.y).cmp(&(q.x, q.y)));
        c
    };
    sorted(a) == sorted(b)
}

/// Whether Mutter would take `config`, laid out from `remembered` (`meta_verify_monitors_config`):
/// logical monitors that do not overlap, each sharing an edge with another, in no more groups
/// joined through shared edges than all were in when remembered (so in one, from Mutter 48 on,
/// which takes no more: what it took then is taken again), the top left one at 0,0, and one of
/// them primary.
pub(super) fn valid(
    config: &[LogicalConfig],
    remembered: &Remembered,
    state: &DisplayState,
) -> bool {
    fits(config, state, remembered.layout_logical, remembered.groups)
}

// Whether Mutter would take `config` (`valid`), laid out in the layout mode given and in no more
// than `groups` groups (at least one).
fn fits(config: &[LogicalConfig], state: &DisplayState, layout_logical: bool, groups: usize) -> bool {
    let Some(rects) = rects(config, state, layout_logical) else {
        return false;
    };
    let overlap = |a: &Rect, b: &Rect| {
        a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
    };
    let apart = rects
        .iter()
        .enumerate()
        .all(|(i, a)| rects[i + 1..].iter().all(|b| !overlap(a, b)));
    let alone = rects.len() > 1
        && (0..rects.len())
            .any(|i| (0..rects.len()).all(|j| j == i || !adjacent(&rects[i], &rects[j])));
    !rects.is_empty()
        && rects.iter().all(|r| r.2 > 0 && r.3 > 0)
        && apart
        && !alone
        && self::groups(&rects) <= groups.max(1)
        && rects.iter().map(|r| r.0).min() == Some(0)
        && rects.iter().map(|r| r.1).min() == Some(0)
        && config.iter().filter(|l| l.primary).count() == 1
}

/// The desktop taken over by `ours` (`Mode::Privacy`): ours alone on, in the order they were made,
/// side by side from 0,0 along the top, `primary` (an index into `ours`) the primary one; every
/// other monitor off, one of ours turned off (in Settings, say) on again. None where one of ours
/// has no mode.
pub(super) fn alone(state: &DisplayState, ours: &[String], primary: usize) -> Option<Vec<LogicalConfig>> {
    let mut config = Vec::with_capacity(ours.len());
    let mut x = 0;
    for (i, connector) in ours.iter().enumerate() {
        let mode = usable_mode(state.monitor(connector)?)?;
        let l = LogicalConfig {
            x,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: i == primary,
            monitors: vec![(connector.clone(), mode.id.clone())],
        };
        x += size(&l, state, state.layout_logical)?.0;
        config.push(l);
    }
    (!config.is_empty()).then_some(config)
}

/// Whether `config` turns off a monitor `state` shows on.
pub(super) fn turns_off(config: &[LogicalConfig], state: &DisplayState) -> bool {
    !turned_off(config, state).is_empty()
}

/// The logical monitors of `state` with a monitor `config` turns off.
pub(super) fn turned_off<'a>(
    config: &[LogicalConfig],
    state: &'a DisplayState,
) -> Vec<&'a scrap::mutter_model::LogicalMonitor> {
    state
        .logical
        .iter()
        .filter(|l| {
            l.connectors.iter().any(|c| {
                !config
                    .iter()
                    .any(|l| l.monitors.iter().any(|(k, _)| k == c))
            })
        })
        .collect()
}

/// Whether `state` shows every monitor `config` has on.
pub(super) fn shows(state: &DisplayState, config: &[LogicalConfig]) -> bool {
    config
        .iter()
        .flat_map(|l| &l.monitors)
        .all(|(c, _)| state.logical.iter().any(|l| l.connectors.contains(c)))
}

/// The step before `config` that turns monitors off: `config` with each of those still on, as it
/// is on now, in a row to the right of the rest, along the top of the one reaching furthest right
/// but lowered by a pixel or two so that none is where it is now. Mutter closes the recordings of
/// a monitor moved, cleanly, and the turning off after cannot meet one: Mutter 42.9 crashes
/// turning off a monitor recorded with the cursor as metadata. None where `config` turns nothing
/// on off, or Mutter would not take the step.
pub(super) fn moving_off(config: &[LogicalConfig], state: &DisplayState) -> Option<Vec<LogicalConfig>> {
    let kept = |c: &String| config.iter().any(|l| l.monitors.iter().any(|(k, _)| k == c));
    // What goes off, by the logical monitor it is on now: mirrored monitors move together.
    let going: Vec<(&scrap::mutter_model::LogicalMonitor, Vec<String>)> = state
        .logical
        .iter()
        .filter_map(|l| {
            let off: Vec<String> = l.connectors.iter().filter(|c| !kept(c)).cloned().collect();
            (!off.is_empty()).then_some((l, off))
        })
        .collect();
    if going.is_empty() {
        return None;
    }
    let rects = rects(config, state, state.layout_logical)?;
    let (mut right, mut top) = (0, 0);
    for r in &rects {
        if r.0 + r.2 > right || (r.0 + r.2 == right && r.1 < top) {
            right = r.0 + r.2;
            top = r.1;
        }
    }
    let lower = (1..)
        .find(|k| going.iter().all(|(l, _)| l.y != top + k))
        .unwrap_or(1);
    let mut step = config.to_vec();
    for (l, off) in going {
        let monitors = off
            .iter()
            .map(|c| Some((c.clone(), state.monitor(c)?.current_mode()?.id.clone())))
            .collect::<Option<Vec<_>>>()?;
        let moved = LogicalConfig {
            x: right,
            y: top + lower,
            scale: l.scale,
            transform: l.transform,
            primary: false,
            monitors,
        };
        right += size(&moved, state, state.layout_logical)?.0;
        step.push(moved);
    }
    let groups = groups(&rects);
    fits(&step, state, state.layout_logical, groups).then_some(step)
}

/// Where the desktop goes back to, the monitors not ours having changed while it was taken over
/// (`wanted` has none): those on when remembered and still there, as they were set (at the mode
/// they like best should theirs be gone), then any
/// plugged in since, at the mode they are at or like best, side by side from 0,0 along the top, the
/// one primary then primary again, else the first; ours to their right. Leased monitors and those
/// off when remembered stay off. None where there is no monitor to give it back to, or Mutter would
/// not take it.
pub(super) fn give_back(
    remembered: &Remembered,
    state: &DisplayState,
    ours: &[String],
) -> Option<Vec<LogicalConfig>> {
    let usable = |c: &str| state.monitor(c).map_or(false, |m| !m.for_lease);
    let mut back: Vec<LogicalConfig> = remembered
        .logical
        .iter()
        .filter(|l| l.monitors.iter().all(|(c, _)| usable(c)))
        .cloned()
        .map(|mut l| {
            // A monitor swapped meanwhile may no longer have the mode it was at.
            for (c, mode) in &mut l.monitors {
                let m = state.monitor(c)?;
                if !m.modes.iter().any(|listed| listed.id == *mode) {
                    *mode = usable_mode(m)?.id.clone();
                }
            }
            Some(l)
        })
        .collect::<Option<_>>()?;
    back.sort_by_key(|l| (l.x, l.y));
    for m in &state.monitors {
        if ours.contains(&m.connector) || remembered.monitors.contains(&m.connector) || m.for_lease {
            continue;
        }
        let Some(mode) = usable_mode(m) else {
            continue;
        };
        back.push(LogicalConfig {
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: false,
            monitors: vec![(m.connector.clone(), mode.id.clone())],
        });
    }
    if back.is_empty() {
        return None;
    }
    if !back.iter().any(|l| l.primary) {
        back[0].primary = true;
    }
    for c in ours {
        let mode = usable_mode(state.monitor(c)?)?;
        back.push(LogicalConfig {
            x: 0,
            y: 0,
            scale: 1.0,
            transform: 0,
            primary: false,
            monitors: vec![(c.clone(), mode.id.clone())],
        });
    }
    let mut x = 0;
    for l in &mut back {
        l.y = 0;
        l.x = x;
        x += size(l, state, state.layout_logical)?.0;
    }
    fits(&back, state, state.layout_logical, 1).then_some(back)
}

// The mode a monitor is at, or else would come on at: the one it likes best, or its first.
fn usable_mode(m: &Monitor) -> Option<&MonitorMode> {
    m.current_mode()
        .or_else(|| m.modes.iter().find(|m| m.preferred))
        .or_else(|| m.modes.first())
}

// What each logical monitor covers of the desktop. None where a mode is not listed.
fn rects(
    config: &[LogicalConfig],
    state: &DisplayState,
    layout_logical: bool,
) -> Option<Vec<Rect>> {
    config
        .iter()
        .map(|l| size(l, state, layout_logical).map(|(w, h)| (l.x, l.y, w, h)))
        .collect()
}

// `meta_rectangle_is_adjacent_to`: a side of one on a side of the other, the two overlapping along
// it.
fn adjacent(a: &Rect, b: &Rect) -> bool {
    ((a.0 == b.0 + b.2 || a.0 + a.2 == b.0) && a.1 < b.1 + b.3 && b.1 < a.1 + a.3)
        || ((a.1 == b.1 + b.3 || a.1 + a.3 == b.1) && a.0 < b.0 + b.2 && b.0 < a.0 + a.2)
}

// How many groups the rectangles make, joined through edges they share.
fn groups(rects: &[Rect]) -> usize {
    let mut reached = vec![false; rects.len()];
    let mut groups = 0;
    for first in 0..rects.len() {
        if reached[first] {
            continue;
        }
        groups += 1;
        let mut next = vec![first];
        while let Some(i) = next.pop() {
            if !std::mem::replace(&mut reached[i], true) {
                next.extend(
                    (0..rects.len()).filter(|&j| !reached[j] && adjacent(&rects[i], &rects[j])),
                );
            }
        }
    }
    groups
}

type Rect = (i32, i32, i32, i32);

// What a logical monitor covers of the desktop, as Mutter sizes it (`derive_logical_monitor_size`):
// the mode of its first monitor, turned, and in a logical layout divided by its scale.
fn size(l: &LogicalConfig, state: &DisplayState, layout_logical: bool) -> Option<(i32, i32)> {
    let (connector, mode) = l.monitors.first()?;
    let mode = state
        .monitor(connector)?
        .modes
        .iter()
        .find(|m| m.id == *mode)?;
    let (w, h) = if l.transform % 2 == 1 {
        (mode.height, mode.width)
    } else {
        (mode.width, mode.height)
    };
    Some(if layout_logical {
        // In single precision, as Mutter does.
        let scale = l.scale as f32;
        (
            (w as f32 / scale).round() as i32,
            (h as f32 / scale).round() as i32,
        )
    } else {
        (w, h)
    })
}

// Every monitor not ours, on or off.
fn others(state: &DisplayState, ours: &[String]) -> Vec<String> {
    state
        .monitors
        .iter()
        .map(|m| m.connector.clone())
        .filter(|c| !ours.contains(c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use scrap::mutter_model::{LogicalMonitor, Monitor, MonitorMode, VIRTUAL_PRODUCT};

    fn monitor(connector: &str, product: &str, modes: &[(i32, i32, bool)]) -> Monitor {
        Monitor {
            connector: connector.to_owned(),
            vendor: String::new(),
            product: product.to_owned(),
            serial: String::new(),
            modes: modes
                .iter()
                .map(|&(width, height, current)| MonitorMode {
                    id: format!("{}x{}", width, height),
                    width,
                    height,
                    refresh: 60.0,
                    current,
                    preferred: false,
                })
                .collect(),
            builtin: false,
            underscanning: None,
            color_mode: None,
            for_lease: false,
        }
    }

    fn logical(x: i32, y: i32, primary: bool, connectors: &[&str]) -> LogicalMonitor {
        LogicalMonitor {
            x,
            y,
            scale: 1.0,
            transform: 0,
            primary,
            connectors: connectors.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn config(x: i32, y: i32, primary: bool, monitors: &[(&str, &str)]) -> LogicalConfig {
        LogicalConfig {
            x,
            y,
            scale: 1.0,
            transform: 0,
            primary,
            monitors: monitors
                .iter()
                .map(|(c, m)| (c.to_string(), m.to_string()))
                .collect(),
        }
    }

    // DP-1 (1280x1024) on the left, HDMI-1 (1920x1080, at its second mode) on the right and
    // primary, 100 lower: not what Mutter's side-by-side layout makes. DP-2 is off.
    fn stored() -> DisplayState {
        DisplayState {
            serial: 7,
            monitors: vec![
                monitor("HDMI-1", "Dell", &[(2560, 1440, false), (1920, 1080, true)]),
                monitor("DP-1", "LG", &[(1280, 1024, true)]),
                monitor("DP-2", "LG", &[(1600, 900, true)]),
            ],
            logical: vec![
                logical(1280, 100, true, &["HDMI-1"]),
                logical(0, 0, false, &["DP-1"]),
            ],
            layout_logical: false,
            layout_mode_changeable: false,
        }
    }

    // Ours added, Mutter laid everything out side by side, at preferred modes, ours included.
    fn side_by_side(ours: &[(&str, i32, i32)]) -> DisplayState {
        let mut s = stored();
        s.serial = 8;
        s.monitors[0] = monitor("HDMI-1", "Dell", &[(2560, 1440, true), (1920, 1080, false)]);
        s.logical = vec![
            logical(0, 0, true, &["HDMI-1"]),
            logical(2560, 0, false, &["DP-1"]),
        ];
        let mut x = 2560 + 1280;
        for &(c, w, h) in ours {
            s.monitors
                .push(monitor(c, VIRTUAL_PRODUCT, &[(w, h, true)]));
            s.logical.push(logical(x, 0, false, &[c]));
            x += w;
        }
        s
    }

    fn names(ours: &[&str]) -> Vec<String> {
        ours.iter().map(|c| c.to_string()).collect()
    }

    #[test]
    fn the_others_go_back_as_they_were_and_ours_to_their_right() {
        let remembered = Remembered::of(&stored(), &[]).unwrap();
        let ours = names(&["Meta-0", "Meta-1"]);
        let now = side_by_side(&[("Meta-0", 2560, 1440), ("Meta-1", 1600, 900)]);
        let wanted = wanted(&remembered, &now, &ours).unwrap();
        assert_eq!(
            wanted,
            [
                config(1280, 100, true, &[("HDMI-1", "1920x1080")]),
                config(0, 0, false, &[("DP-1", "1280x1024")]),
                // Along the top of HDMI-1, which reaches furthest right.
                config(3200, 100, false, &[("Meta-0", "2560x1440")]),
                config(5760, 100, false, &[("Meta-1", "1600x900")]),
            ]
        );
        assert!(valid(&wanted, &remembered, &now));
        assert!(!same(&current(&now).unwrap(), &wanted));
        // As wanted already: the same, whatever the order.
        let mut put_back = now.clone();
        put_back.logical = vec![
            logical(5760, 100, false, &["Meta-1"]),
            logical(0, 0, false, &["DP-1"]),
            logical(1280, 100, true, &["HDMI-1"]),
            logical(3200, 100, false, &["Meta-0"]),
        ];
        put_back.monitors[0] = stored().monitors[0].clone();
        assert!(same(&current(&put_back).unwrap(), &wanted));
        // One of ours turned off: placed all the same, at the mode it has.
        let mut off = now.clone();
        off.monitors.last_mut().unwrap().modes[0].current = false;
        off.logical.pop();
        assert_eq!(super::wanted(&remembered, &off, &ours).as_ref(), Some(&wanted));
    }

    #[test]
    fn ours_are_left_out_of_what_is_remembered() {
        let now = side_by_side(&[("Meta-0", 2560, 1440)]);
        let remembered = Remembered::of(&now, &names(&["Meta-0"])).unwrap();
        assert_eq!(remembered.logical.len(), 2);
        assert_eq!(remembered.monitors, names(&["HDMI-1", "DP-1", "DP-2"]));
        assert_eq!(remembered.groups, 1);
        // Ours put on the left by someone: the others moved together to 0,0, still as they were
        // to each other, and ours to their right again.
        let mut left = now.clone();
        left.logical = vec![
            logical(2560, 0, true, &["HDMI-1"]),
            logical(5120, 0, false, &["DP-1"]),
            logical(0, 0, false, &["Meta-0"]),
        ];
        let remembered = Remembered::of(&left, &names(&["Meta-0"])).unwrap();
        let wanted = wanted(&remembered, &left, &names(&["Meta-0"])).unwrap();
        assert_eq!(
            wanted,
            [
                config(0, 0, true, &[("HDMI-1", "2560x1440")]),
                config(2560, 0, false, &[("DP-1", "1280x1024")]),
                config(3840, 0, false, &[("Meta-0", "2560x1440")]),
            ]
        );
        assert!(valid(&wanted, &remembered, &left));
    }

    #[test]
    fn nothing_is_wanted_once_the_others_changed() {
        let remembered = Remembered::of(&stored(), &[]).unwrap();
        let ours = names(&["Meta-0"]);
        // DP-1 unplugged.
        let mut now = side_by_side(&[("Meta-0", 2560, 1440)]);
        now.monitors.retain(|m| m.connector != "DP-1");
        now.logical.retain(|l| l.connectors != ["DP-1"]);
        assert_eq!(wanted(&remembered, &now, &ours), None);
        // Laid out in logical pixels now.
        let mut now = side_by_side(&[("Meta-0", 2560, 1440)]);
        now.layout_logical = true;
        assert_eq!(wanted(&remembered, &now, &ours), None);
        // HDMI-1's remembered mode gone.
        let mut now = side_by_side(&[("Meta-0", 2560, 1440)]);
        now.monitors[0] = monitor("HDMI-1", "Dell", &[(2560, 1440, true)]);
        assert_eq!(wanted(&remembered, &now, &ours), None);
        // Ours not listed yet.
        let now = side_by_side(&[]);
        assert_eq!(wanted(&remembered, &now, &ours), None);
    }

    #[test]
    fn sizes_are_turned_and_scaled_as_mutter_does() {
        // HDMI-1 turned a quarter, DP-1 at scale 1.75 in a logical layout: 1280/1.75 = 731.4.
        let mut s = stored();
        s.layout_logical = true;
        s.logical = vec![
            LogicalMonitor {
                transform: 1,
                ..logical(0, 0, true, &["HDMI-1"])
            },
            LogicalMonitor {
                scale: 1.75,
                ..logical(1080, 0, false, &["DP-1"])
            },
        ];
        let remembered = Remembered::of(&s, &[]).unwrap();
        let mut now = s.clone();
        now.monitors
            .push(monitor("Meta-0", VIRTUAL_PRODUCT, &[(1600, 900, true)]));
        now.logical.push(logical(0, 1920, false, &["Meta-0"]));
        let wanted = wanted(&remembered, &now, &names(&["Meta-0"])).unwrap();
        assert_eq!((wanted[2].x, wanted[2].y), (1080 + 731, 0));
        assert!(valid(&wanted, &remembered, &now));
    }

    #[test]
    fn with_no_others_the_first_of_ours_is_primary() {
        let mut s = stored();
        s.monitors.clear();
        s.logical.clear();
        let remembered = Remembered::of(&s, &[]).unwrap();
        s.monitors
            .push(monitor("Meta-0", VIRTUAL_PRODUCT, &[(1600, 900, true)]));
        s.monitors
            .push(monitor("Meta-1", VIRTUAL_PRODUCT, &[(1280, 720, true)]));
        s.logical = vec![
            logical(0, 0, true, &["Meta-0"]),
            logical(1600, 0, false, &["Meta-1"]),
        ];
        let wanted = wanted(&remembered, &s, &names(&["Meta-0", "Meta-1"])).unwrap();
        assert_eq!(
            wanted,
            [
                config(0, 0, true, &[("Meta-0", "1600x900")]),
                config(1600, 0, false, &[("Meta-1", "1280x720")]),
            ]
        );
        assert!(valid(&wanted, &remembered, &s));
        assert!(same(&current(&s).unwrap(), &wanted));
    }

    #[test]
    fn one_of_ours_made_primary_stays_so() {
        let mut s = stored();
        s.monitors
            .push(monitor("Meta-0", VIRTUAL_PRODUCT, &[(1600, 900, true)]));
        s.monitors
            .push(monitor("Meta-1", VIRTUAL_PRODUCT, &[(1280, 720, true)]));
        s.logical = vec![
            logical(1280, 100, false, &["HDMI-1"]),
            logical(0, 0, false, &["DP-1"]),
            logical(3200, 100, false, &["Meta-0"]),
            logical(4800, 100, true, &["Meta-1"]),
        ];
        let ours = names(&["Meta-0", "Meta-1"]);
        let remembered = Remembered::of(&s, &ours).unwrap();
        let wanted = wanted(&remembered, &s, &ours).unwrap();
        assert!(!wanted[2].primary && wanted[3].primary);
        assert!(valid(&wanted, &remembered, &s));
        assert!(same(&current(&s).unwrap(), &wanted));
    }

    #[test]
    fn only_what_mutter_takes_is_valid() {
        let s = side_by_side(&[("Meta-0", 1600, 900)]);
        let remembered = Remembered::of(&s, &names(&["Meta-0"])).unwrap();
        let good = current(&s).unwrap();
        assert!(valid(&good, &remembered, &s));
        let with = |f: &dyn Fn(&mut Vec<LogicalConfig>)| {
            let mut c = good.clone();
            f(&mut c);
            c
        };
        // Overlapping.
        assert!(!valid(&with(&|c| c[2].x -= 1), &remembered, &s));
        // Apart: not sharing an edge with any other.
        assert!(!valid(&with(&|c| c[2].x += 1), &remembered, &s));
        // Corners touching only.
        assert!(!valid(&with(&|c| c[2].y = 1024), &remembered, &s));
        // Not starting at 0,0.
        assert!(!valid(
            &with(&|c| c.iter_mut().for_each(|l| l.y += 10)),
            &remembered,
            &s
        ));
        // Two primaries, or none.
        assert!(!valid(&with(&|c| c[1].primary = true), &remembered, &s));
        assert!(!valid(&with(&|c| c[0].primary = false), &remembered, &s));
        // A mode Mutter does not list.
        assert!(!valid(
            &with(&|c| c[2].monitors[0].1 = "640x480".to_owned()),
            &remembered,
            &s
        ));
    }

    #[test]
    fn groups_apart_are_valid_only_where_they_were_before() {
        let mut four = side_by_side(&[("Meta-0", 1600, 900), ("Meta-1", 1600, 900)]);
        four.logical.clear();
        // Two pairs, each sharing an edge, apart from each other.
        let pairs = vec![
            config(0, 0, true, &[("HDMI-1", "2560x1440")]),
            config(2560, 0, false, &[("DP-1", "1280x1024")]),
            config(0, 2000, false, &[("Meta-0", "1600x900")]),
            config(1600, 2000, false, &[("Meta-1", "1600x900")]),
        ];
        // Mutter 48 refuses it: where the others were in one group, as from 48 on, so is it.
        let one = Remembered {
            logical: pairs[..2].to_vec(),
            layout_logical: false,
            monitors: names(&["HDMI-1", "DP-1", "DP-2"]),
            groups: 1,
        };
        assert!(!valid(&pairs, &one, &four));
        // Mutter 42 took it, each sharing an edge with another: where all were in two groups
        // already, so is it.
        let two = Remembered { groups: 2, ..one };
        assert!(valid(&pairs, &two, &four));
        // Not one on its own, though, in no more groups as it is.
        let mut alone = pairs.clone();
        alone[3].x = 3840;
        alone[3].y = 0;
        assert!(!valid(&alone, &two, &four));
    }

    // As the desktop is while ours have it to themselves: the others listed, off, without a
    // current mode; ours alone on, side by side, the first primary.
    fn taken_over(ours: &[(&str, i32, i32)]) -> DisplayState {
        let mut s = stored();
        for m in &mut s.monitors {
            for mode in &mut m.modes {
                mode.current = false;
            }
        }
        s.logical = Vec::new();
        let mut x = 0;
        for (i, &(c, w, h)) in ours.iter().enumerate() {
            s.monitors.push(monitor(c, VIRTUAL_PRODUCT, &[(w, h, true)]));
            s.logical.push(logical(x, 0, i == 0, &[c]));
            x += w;
        }
        s
    }

    #[test]
    fn ours_alone_take_the_desktop_side_by_side_from_the_left() {
        let now = side_by_side(&[("Meta-0", 1600, 900), ("Meta-1", 1280, 720)]);
        let ours = names(&["Meta-0", "Meta-1"]);
        let taken = alone(&now, &ours, 1).unwrap();
        assert_eq!(
            taken,
            [
                config(0, 0, false, &[("Meta-0", "1600x900")]),
                config(1600, 0, true, &[("Meta-1", "1280x720")]),
            ]
        );
        assert!(fits(&taken, &now, false, 1));
        // One of ours turned off: on again, at the mode it has.
        let mut off = now.clone();
        off.monitors.last_mut().unwrap().modes[0].current = false;
        off.logical.pop();
        assert_eq!(alone(&off, &ours, 1).as_ref(), Some(&taken));
        // Without any: none.
        off.monitors.last_mut().unwrap().modes.clear();
        assert_eq!(alone(&off, &ours, 1), None);
    }

    // Each monitor going off is first moved, a pixel lower than the one it joins, so that none
    // stays where it is: Mutter then closes whatever records it, before it goes off.
    #[test]
    fn what_goes_off_is_first_moved_where_it_is_not() {
        let ours = names(&["Meta-0"]);
        let mut now = side_by_side(&[("Meta-0", 1600, 900)]);
        now.monitors[0] = stored().monitors[0].clone();
        now.logical = vec![
            logical(1280, 100, true, &["HDMI-1"]),
            logical(0, 0, false, &["DP-1"]),
            logical(3200, 100, false, &["Meta-0"]),
        ];
        let taken = alone(&now, &ours, 0).unwrap();
        assert!(turns_off(&taken, &now));
        assert!(!turns_off(&current(&now).unwrap(), &now));
        // Ours on now, the others to be seen on only once given back.
        let dark = taken_over(&[("Meta-0", 1600, 900)]);
        assert!(shows(&dark, &taken));
        assert!(!shows(&dark, &current(&now).unwrap()));
        let step = moving_off(&taken, &now).unwrap();
        assert_eq!(
            step,
            [
                config(0, 0, true, &[("Meta-0", "1600x900")]),
                config(1600, 1, false, &[("HDMI-1", "1920x1080")]),
                config(3520, 1, false, &[("DP-1", "1280x1024")]),
            ]
        );
        // One a pixel lower already: two then.
        now.logical[1].y = 1;
        let step = moving_off(&taken, &now).unwrap();
        assert!(step[1..].iter().all(|l| l.y == 2));
        // Nothing going off: no step.
        assert_eq!(moving_off(&current(&now).unwrap(), &now), None);
    }

    // The others changed while ours had the desktop: those still there as they were set, and one
    // plugged in since, side by side; one off when remembered stays off, and a leased one too.
    #[test]
    fn the_desktop_goes_back_to_what_is_still_there() {
        let remembered = Remembered::of(&stored(), &[]).unwrap();
        let ours = names(&["Meta-0"]);
        let mut now = taken_over(&[("Meta-0", 1600, 900)]);
        now.monitors.retain(|m| m.connector != "DP-1");
        now.monitors
            .push(monitor("HDMI-2", "AOC", &[(1280, 720, false)]));
        let mut headset = monitor("DP-3", "Valve", &[(2880, 1600, false)]);
        headset.for_lease = true;
        now.monitors.push(headset);
        assert_eq!(wanted(&remembered, &now, &ours), None);
        assert_eq!(
            give_back(&remembered, &now, &ours).unwrap(),
            [
                config(0, 0, true, &[("HDMI-1", "1920x1080")]),
                config(1920, 0, false, &[("HDMI-2", "1280x720")]),
                config(3200, 0, false, &[("Meta-0", "1600x900")]),
            ]
        );
        // HDMI-1 swapped for one without the mode it was at: at the one it has.
        let mut swapped = now.clone();
        swapped.monitors[0].modes.retain(|m| m.width != 1920);
        assert_eq!(
            give_back(&remembered, &swapped, &ours).unwrap(),
            [
                config(0, 0, true, &[("HDMI-1", "2560x1440")]),
                config(2560, 0, false, &[("HDMI-2", "1280x720")]),
                config(3840, 0, false, &[("Meta-0", "1600x900")]),
            ]
        );
        // Ours turned off meanwhile: on again with them.
        let mut ours_off = now.clone();
        ours_off.monitors.iter_mut().find(|m| m.connector == "Meta-0").unwrap().modes[0].current =
            false;
        ours_off.logical.clear();
        assert_eq!(
            give_back(&remembered, &ours_off, &ours),
            give_back(&remembered, &now, &ours)
        );
        // Nothing remembered: every monitor not ours taken for one plugged in since, at its first
        // mode short of a current or preferred one, DP-2 off before on too.
        assert_eq!(
            give_back(&Remembered::default(), &now, &ours).unwrap(),
            [
                config(0, 0, true, &[("HDMI-1", "2560x1440")]),
                config(2560, 0, false, &[("DP-2", "1600x900")]),
                config(4160, 0, false, &[("HDMI-2", "1280x720")]),
                config(5440, 0, false, &[("Meta-0", "1600x900")]),
            ]
        );
        // Nothing left to give it back to.
        let mut bare = taken_over(&[("Meta-0", 1600, 900)]);
        bare.monitors.retain(|m| m.product == VIRTUAL_PRODUCT);
        assert_eq!(give_back(&remembered, &bare, &ours), None);
    }
}
