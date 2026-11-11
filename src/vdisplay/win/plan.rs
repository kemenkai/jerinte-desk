//! What the amyuni backend decides, apart from the Windows calls that carry it out, so that it is
//! tested on any host.

/// The driver's mode table holds ten entries, named "0" to "9".
pub const TABLE_LEN: usize = 10;
/// The smallest and largest sizes put in the table.
pub const MIN: (u32, u32) = (640, 480);
pub const MAX: (u32, u32) = (3840, 2160);
/// Sizes around the one asked for, of its aspect: a later resize of the session's view lands on
/// one of them in place, the display's modes being fixed once it is plugged in.
const STEPS: [f64; 9] = [1.5, 1.33, 1.2, 1.1, 0.9, 0.8, 0.75, 0.67, 0.5];

fn even(v: f64) -> u32 {
    ((v / 2.0).round() as u32) * 2
}

/// `w`×`h` made fit for the table: within the bounds, the aspect kept where the largest allows,
/// both sides even.
pub fn normalize(w: u32, h: u32) -> (u32, u32) {
    let (w, h) = (w.max(1) as f64, h.max(1) as f64);
    let down = (MAX.0 as f64 / w).min(MAX.1 as f64 / h).min(1.0);
    let (w, h) = (w * down, h * down);
    (
        even(w).clamp(MIN.0, MAX.0),
        even(h).clamp(MIN.1, MAX.1),
    )
}

/// The sizes written to the table for a display asked for at `w`×`h`: that size first, then the
/// steps around it that stay within the bounds, none twice, at most `TABLE_LEN`.
pub fn ladder(w: u32, h: u32) -> Vec<(u32, u32)> {
    let first = normalize(w, h);
    let mut sizes = vec![first];
    for step in STEPS {
        let size = (even(first.0 as f64 * step), even(first.1 as f64 * step));
        let within = (MIN.0..=MAX.0).contains(&size.0) && (MIN.1..=MAX.1).contains(&size.1);
        if within && !sizes.contains(&size) && sizes.len() < TABLE_LEN {
            sizes.push(size);
        }
    }
    sizes
}

/// How much larger than the view a mode may be on a side, in percent. A desktop client shows a
/// display at its original size unless told otherwise, cropping what is over: a mode a few pixels
/// larger, as a step of the ladder is when its rounding or the frame of a restored window shifts
/// the aspect a little, shows far more than the next step down, half the size maybe. A view a
/// little smaller keeps its mode as well, sparing the display a change.
const OVER_PERCENT: u64 = 1;

/// Of the modes a display has, the one showing the most of the view `w`×`h`, among those within
/// it or at most `OVER_PERCENT` larger on each side; the smallest when there is none such.
pub fn best_fit(modes: &[(u32, u32)], w: u32, h: u32) -> Option<(u32, u32)> {
    let near = |m: &&(u32, u32)| {
        m.0 as u64 * 100 <= w as u64 * (100 + OVER_PERCENT)
            && m.1 as u64 * 100 <= h as u64 * (100 + OVER_PERCENT)
    };
    let shown = |m: &(u32, u32)| m.0.min(w) as u64 * m.1.min(h) as u64;
    let area = |m: &(u32, u32)| m.0 as u64 * m.1 as u64;
    modes
        .iter()
        .filter(near)
        // Of two showing as much, the one cropped less.
        .max_by_key(|m| (shown(m), std::cmp::Reverse(area(m))))
        .or_else(|| modes.iter().min_by_key(|m| area(m)))
        .copied()
}

/// The number in a monitor's interface path, `…&UID256#{…}`: amyuni numbers its monitors in the
/// order they are plugged in.
pub fn uid(interface_path: &str) -> Option<u32> {
    let upper = interface_path.to_ascii_uppercase();
    let rest = &upper[upper.find("UID")? + 3..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// What to do with the display a previous run left behind.
#[derive(Debug, PartialEq)]
pub enum Leftover {
    /// None of ours is there: forget the record.
    Gone,
    /// Ours is there and the newest: unplugging takes it.
    Unplug,
    /// Ours is there but another was plugged in after it, which unplugging would take instead.
    Stuck,
}

/// `ours`, the interface path recorded for our display, against those of the amyuni monitors
/// present, detached ones included.
pub fn leftover(ours: &str, present: &[String]) -> Leftover {
    if !present.iter().any(|p| p.eq_ignore_ascii_case(ours)) {
        return Leftover::Gone;
    }
    let newest = present.iter().filter_map(|p| uid(p)).max();
    match (uid(ours), newest) {
        (Some(o), Some(n)) if o == n => Leftover::Unplug,
        _ => Leftover::Stuck,
    }
}

/// A display that is part of the desktop, as the layout in force has it.
#[derive(Clone, Debug, PartialEq)]
pub struct View {
    /// `\\.\DISPLAYn`.
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub size: (u32, u32),
}

/// Whether `views` is the desktop taken over by display `ours`: ours alone, at the top left.
pub fn taken_over(views: &[View], ours: &str) -> bool {
    matches!(views, [v] if v.name.eq_ignore_ascii_case(ours) && (v.x, v.y) == (0, 0))
}

/// Whether a display other than `ours` is part of the desktop, for the person at the device to see.
pub fn others_shown(views: &[View], ours: &str) -> bool {
    views.iter().any(|v| !v.name.eq_ignore_ascii_case(ours))
}

/// How many looks in a row the desktop may fail to be taken over again before its display goes,
/// as on Linux.
pub const TAKEOVER_TRIES: u32 = 3;

/// What a look at the desktop taken over finds, the first that holds of these.
#[derive(Debug, PartialEq)]
pub enum Look {
    /// Our display went away, and the takeover with it.
    Gone,
    /// The person at the device pressed Ctrl+P: the desktop goes back to them.
    TakenBack,
    /// Local input is no longer held: held again, or the desktop goes back.
    Unhooked,
    /// The layout is not ours alone any more, a monitor plugged in or the system laying the
    /// desktop out anew: taken over again. Never the person at the device, who cannot press
    /// Win+P while input is held, only Ctrl+P.
    Moved,
    Kept,
}

pub fn look(present: bool, ctrl_p: bool, hooked: bool, arranged: bool) -> Look {
    if !present {
        Look::Gone
    } else if ctrl_p {
        Look::TakenBack
    } else if !hooked {
        Look::Unhooked
    } else if !arranged {
        Look::Moved
    } else {
        Look::Kept
    }
}

/// How taking the desktop over again went.
#[derive(Debug, PartialEq)]
pub enum Retake {
    Done,
    /// Refused for want of access to the console, as on the secure desktop: not held against it.
    Denied,
    Failed,
}

/// The looks in a row the desktop failed to be taken over again.
#[derive(Debug, Default)]
pub struct Slips(u32);

impl Slips {
    /// Counts how taking it over again went: true once that failed `TAKEOVER_TRIES` times in a
    /// row, when the display goes.
    pub fn after(&mut self, retake: Retake) -> bool {
        match retake {
            Retake::Done => self.0 = 0,
            Retake::Denied => {}
            Retake::Failed => self.0 += 1,
        }
        self.0 >= TAKEOVER_TRIES
    }

    pub fn kept(&mut self) {
        self.0 = 0;
    }
}

/// One step of a change of use or size, in the order `steps` gives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Step {
    /// The layout before the takeover put back, local input let go.
    GiveBack,
    /// Resized the way of an extra screen: saved, and laid out with the others.
    Resize,
    /// Resized as the desktop taken over, nothing else in the layout changed. The way of an extra
    /// screen would bring the other displays back.
    ResizeTakenOver,
    TakeOver,
    /// Refused: the person at the device took the desktop back since the last look, and it is not
    /// taken over again in the same breath.
    TakenBack,
}

/// The steps to a display used as the desktop taken over (`privacy`) or as an extra screen, from
/// `taking_over` or not, Ctrl+P pressed since the last look, and `resize` to another size. The
/// size changes while the display is an extra screen where it can: the way of an extra screen is
/// the one that saves it.
pub fn steps(taking_over: bool, ctrl_p: bool, privacy: bool, resize: bool) -> Vec<Step> {
    let taken_back = taking_over && ctrl_p;
    let kept = taking_over && !taken_back;
    let mut steps = Vec::new();
    if taken_back || (kept && !privacy) {
        steps.push(Step::GiveBack);
    }
    if resize {
        steps.push(if kept && privacy {
            Step::ResizeTakenOver
        } else {
            Step::Resize
        });
    }
    if privacy && !kept {
        steps.push(if taken_back {
            Step::TakenBack
        } else {
            Step::TakeOver
        });
    }
    steps
}

/// A table entry's text, "W,H".
pub fn table_value(size: (u32, u32)) -> String {
    format!("{},{}", size.0, size.1)
}

pub fn parse_value(text: &str) -> Option<(u32, u32)> {
    let (w, h) = text.trim().split_once(',')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_are_made_even_and_kept_within_the_bounds() {
        assert_eq!(normalize(1513, 983), (1514, 984));
        assert_eq!(normalize(200, 100), MIN);
        // Too large: scaled down whole, so the aspect survives.
        assert_eq!(normalize(5120, 2880), (3840, 2160));
        assert_eq!(normalize(7680, 2160), (3840, 1080));
    }

    #[test]
    fn the_ladder_starts_with_the_size_asked_and_stays_within_the_table() {
        let l = ladder(2048, 1152);
        assert_eq!(l[0], (2048, 1152));
        assert!(l.len() <= TABLE_LEN);
        assert!(l.contains(&(1024, 576)), "half of it: {:?}", l);
        assert!(l.contains(&(3072, 1728)), "one and a half: {:?}", l);
        for (w, h) in &l {
            assert!(w % 2 == 0 && h % 2 == 0);
            assert!((MIN.0..=MAX.0).contains(w) && (MIN.1..=MAX.1).contains(h));
            // Same aspect, give or take the rounding to even sides.
            assert!(((*w as f64 / *h as f64) - 16.0 / 9.0).abs() < 0.01, "{}x{}", w, h);
        }
        let mut seen = l.clone();
        seen.dedup();
        assert_eq!(seen.len(), l.len());
    }

    #[test]
    fn a_ladder_at_the_largest_size_only_steps_down() {
        let l = ladder(3840, 2160);
        assert_eq!(l[0], (3840, 2160));
        assert!(l.iter().skip(1).all(|(w, _)| *w < 3840));
    }

    #[test]
    fn a_resize_takes_the_largest_mode_near_enough_to_the_view() {
        let modes = ladder(2048, 1152);
        assert_eq!(best_fit(&modes, 2048, 1152), Some((2048, 1152)));
        // Smaller than a mode by more than a percent (2.4% narrower): the next step down.
        assert_eq!(best_fit(&modes, 2000, 1150), Some((1844, 1036)));
        // A view of another aspect: the largest that fits both ways.
        assert_eq!(best_fit(&modes, 3000, 1200), Some((2048, 1152)));
        // Nothing fits: the smallest there is.
        assert_eq!(best_fit(&modes, 100, 100), Some((1024, 576)));
        assert_eq!(best_fit(&[], 100, 100), None);
    }

    #[test]
    fn a_mode_a_few_pixels_over_the_view_beats_a_much_smaller_one() {
        // Made for a maximized window, then the window restored: the step for it is 4 pixels
        // taller than its view (0.6%), the next one down half the size. Seen in the test VM.
        let modes = ladder(1920, 1052);
        assert!(modes.contains(&(1286, 704)) && modes.contains(&(960, 526)));
        assert_eq!(best_fit(&modes, 1288, 700), Some((1286, 704)));
        // A view a little smaller keeps the mode it has: no change of the display at all.
        assert_eq!(best_fit(&modes, 1916, 1050), Some((1920, 1052)));
        // Over by more than a percent: cropped too much, the step down it is.
        assert_eq!(best_fit(&modes, 1288, 690), Some((960, 526)));
        // The percent itself: 704 is within 1% of 698 (704.98), not of 697 (703.97).
        assert_eq!(best_fit(&modes, 1288, 698), Some((1286, 704)));
        assert_eq!(best_fit(&modes, 1288, 697), Some((960, 526)));
        // Exactly a percent over still counts.
        assert_eq!(best_fit(&[(1010, 505), (800, 400)], 1000, 500), Some((1010, 505)));
        // Of two showing all of the view, the one cropped less, whichever comes first.
        assert_eq!(best_fit(&[(1004, 503), (1008, 504)], 1000, 500), Some((1004, 503)));
        assert_eq!(best_fit(&[(1008, 504), (1004, 503)], 1000, 500), Some((1004, 503)));
    }

    #[test]
    fn the_monitor_number_comes_from_the_interface_path() {
        let p = r"\\?\DISPLAY#Default_Monitor#1&28a6823a&0&UID256#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}";
        assert_eq!(uid(p), Some(256));
        assert_eq!(uid(&p.to_ascii_lowercase()), Some(256));
        assert_eq!(uid(r"\\?\DISPLAY#Default_Monitor#4&27b1e55b&0&UID0#{x}"), Some(0));
        assert_eq!(uid("no number here"), None);
    }

    #[test]
    fn a_leftover_is_unplugged_only_when_it_is_the_newest() {
        let path = |n: u32| format!(r"\\?\DISPLAY#Default_Monitor#1&28a6823a&0&UID{}#{{e6f0}}", n);
        let ours = path(257);
        assert_eq!(leftover(&ours, &[path(256)]), Leftover::Gone);
        assert_eq!(leftover(&ours, &[path(256), path(257)]), Leftover::Unplug);
        assert_eq!(leftover(&ours.to_ascii_uppercase(), &[path(257)]), Leftover::Unplug);
        assert_eq!(leftover(&ours, &[path(257), path(258)]), Leftover::Stuck);
    }

    fn view(name: &str, x: i32, y: i32) -> View {
        View {
            name: name.to_owned(),
            x,
            y,
            size: (1600, 900),
        }
    }

    #[test]
    fn the_desktop_is_taken_over_with_ours_alone_at_the_top_left() {
        let ours = r"\\.\DISPLAY6";
        assert!(taken_over(&[view(ours, 0, 0)], ours));
        assert!(taken_over(&[view(&ours.to_ascii_lowercase(), 0, 0)], ours));
        assert!(!taken_over(&[view(ours, 1280, 0)], ours));
        assert!(!taken_over(&[view(r"\\.\DISPLAY1", 0, 0), view(ours, 1280, 0)], ours));
        assert!(!taken_over(&[view(r"\\.\DISPLAY1", 0, 0)], ours));
        assert!(!taken_over(&[], ours));
        assert!(others_shown(&[view(r"\\.\DISPLAY1", 0, 0)], ours));
        assert!(!others_shown(&[view(ours, 0, 0)], ours));
        assert!(!others_shown(&[], ours));
    }

    #[test]
    fn a_look_sees_to_the_first_thing_wrong() {
        assert_eq!(look(false, true, false, false), Look::Gone);
        assert_eq!(look(true, true, false, false), Look::TakenBack);
        assert_eq!(look(true, false, false, false), Look::Unhooked);
        assert_eq!(look(true, false, true, false), Look::Moved);
        assert_eq!(look(true, false, true, true), Look::Kept);
    }

    #[test]
    fn the_display_goes_after_failing_to_take_the_desktop_over_again_three_times_in_a_row() {
        let mut slips = Slips::default();
        assert!(!slips.after(Retake::Failed));
        assert!(!slips.after(Retake::Failed));
        // Done in between: counted from nothing again.
        assert!(!slips.after(Retake::Done));
        assert!(!slips.after(Retake::Failed));
        assert!(!slips.after(Retake::Failed));
        // Refused for want of access: neither a failure nor a success.
        assert!(!slips.after(Retake::Denied));
        assert!(slips.after(Retake::Failed));
        let mut slips = Slips::default();
        slips.after(Retake::Failed);
        slips.after(Retake::Failed);
        slips.kept();
        assert!(!slips.after(Retake::Failed));
    }

    #[test]
    fn a_change_of_use_and_size_resizes_while_an_extra_screen() {
        use Step::*;
        // Taken over: resized first, the way that saves the size.
        assert_eq!(steps(false, false, true, true), vec![Resize, TakeOver]);
        assert_eq!(steps(false, false, true, false), vec![TakeOver]);
        // Given back: resized after.
        assert_eq!(steps(true, false, false, true), vec![GiveBack, Resize]);
        assert_eq!(steps(true, false, false, false), vec![GiveBack]);
        // Resized while taken over: without bringing the other displays back.
        assert_eq!(steps(true, false, true, true), vec![ResizeTakenOver]);
        assert_eq!(steps(true, false, true, false), Vec::<Step>::new());
        // An extra screen, resized or not.
        assert_eq!(steps(false, false, false, true), vec![Resize]);
        assert_eq!(steps(false, false, false, false), Vec::<Step>::new());
    }

    #[test]
    fn ctrl_p_since_the_last_look_gives_the_desktop_back_before_anything_else() {
        use Step::*;
        // Asked to keep it: given back, resized as an extra screen, refused.
        assert_eq!(steps(true, true, true, true), vec![GiveBack, Resize, TakenBack]);
        assert_eq!(steps(true, true, true, false), vec![GiveBack, TakenBack]);
        // Asked to give it back anyway.
        assert_eq!(steps(true, true, false, true), vec![GiveBack, Resize]);
        // Not taken over: nothing to give back.
        assert_eq!(steps(false, true, true, false), vec![TakeOver]);
    }

    #[test]
    fn table_values_read_back_as_written() {
        assert_eq!(table_value((1512, 982)), "1512,982");
        assert_eq!(parse_value("1512,982"), Some((1512, 982)));
        assert_eq!(parse_value(" 1920 , 1080 "), Some((1920, 1080)));
        assert_eq!(parse_value("1920x1080"), None);
    }
}
