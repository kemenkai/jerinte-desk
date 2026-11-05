//! What the X server's RandR says about its outputs, as `xrandr` prints it, and the `xrandr`
//! arguments that lay our virtual displays out.

use super::protocol::Kind;

/// An output of the X screen.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Output {
    pub name: String,
    pub connected: bool,
    pub primary: bool,
    /// Where it shows the screen, (x, y, width, height), if it is on. With `--scale-from` this is
    /// the scaled size, the area of the screen it shows.
    pub area: Option<(i32, i32, i32, i32)>,
    /// Its modes, preferred first as `xrandr` lists them.
    pub modes: Vec<String>,
    /// The mode it shows, and its refresh rate, if it is on.
    pub current: Option<String>,
    pub rate: Option<f64>,
    /// The DRM connector it is on, where the driver says (`CONNECTOR_ID`).
    pub connector_id: Option<u32>,
    /// How it is turned, if on and not `normal`: `left`, `right` or `inverted`.
    pub rotation: Option<String>,
}

const ROTATIONS: [&str; 3] = ["left", "right", "inverted"];

/// The outputs in the output of `xrandr --current`, or of `xrandr --current --prop`, which adds
/// each output's properties. Never plain `xrandr`: that probes every connector, and a lit
/// connector without a monitor answers a probe slowly, the X server waiting meanwhile.
pub fn parse(text: &str) -> Vec<Output> {
    let mut outputs: Vec<Output> = Vec::new();
    for line in text.lines() {
        if line.starts_with("Screen ") {
            continue;
        }
        if !line.starts_with(char::is_whitespace) {
            let mut words = line.split_whitespace();
            let (Some(name), Some(state)) = (words.next(), words.next()) else {
                continue;
            };
            let mut output = Output {
                name: name.to_owned(),
                connected: state == "connected",
                ..Default::default()
            };
            for word in words.by_ref() {
                if word == "primary" {
                    output.primary = true;
                } else if let Some(area) = parse_area(word) {
                    output.area = Some(area);
                    break;
                } else if word.starts_with('(') {
                    break;
                }
            }
            // Right after the area, how it is turned; nothing there when it is not.
            if output.area.is_some() {
                output.rotation = words
                    .next()
                    .filter(|w| ROTATIONS.contains(w))
                    .map(str::to_owned);
            }
            outputs.push(output);
            continue;
        }
        let Some(output) = outputs.last_mut() else {
            continue;
        };
        if let Some(value) = line.trim().strip_prefix("CONNECTOR_ID:") {
            output.connector_id = value.trim().parse().ok();
        } else if line.starts_with("   ") {
            // A mode, "   1920x1080     60.00*+  59.94"; properties are indented with tabs. The
            // rate marked * is the one shown.
            let mut words = line.split_whitespace();
            if let Some(mode) = words.next() {
                if parse_size(mode).is_some() {
                    output.modes.push(mode.to_owned());
                    if let Some(rate) = words.find(|w| w.contains('*')) {
                        output.rate = rate.trim_end_matches(['*', '+']).parse().ok();
                        output.current = Some(mode.to_owned());
                    }
                }
            }
        }
    }
    outputs
}

/// The output on the DRM connector `name` (sysfs's spelling) with id `id`, a display of `kind`.
///
/// vkms's is an output of a card added to the screen, which the modesetting driver names after
/// that card, `Virtual-1-1`: one of those with the id, ids being counted by card, so that
/// virtio-gpu's own `Virtual-1` never is it; where those outputs show no ids, the only one there
/// is.
///
/// Another, by id where both sides know ids, of the same type of connector (ids of another card
/// could be the same); otherwise by the name the driver gives it, which way it names told by the
/// names it gave. Never a guess across namings: the amdgpu driver's HDMI-A-1 is the kernel's
/// HDMI-A-2.
pub fn output_for<'a>(
    outputs: &'a [Output],
    name: &str,
    id: Option<u32>,
    kind: Kind,
) -> Option<&'a Output> {
    let (connector_type, number) = name.rsplit_once('-')?;
    let number: u32 = number.parse().ok()?;
    let with_ids = outputs.iter().any(|o| o.connector_id.is_some());
    if kind == Kind::Vkms {
        let added = |o: &&Output| {
            let rest = o.name.strip_prefix("Virtual-")?;
            let (card, n) = rest.split_once('-')?;
            card.parse::<u32>().ok()?;
            n.parse::<u32>().ok()
        };
        let theirs: Vec<&Output> = outputs.iter().filter(|o| added(o).is_some()).collect();
        let theirs_with_ids = theirs.iter().any(|o| o.connector_id.is_some());
        return match (id, theirs_with_ids) {
            (Some(id), true) => theirs.into_iter().find(|o| o.connector_id == Some(id)),
            _ => {
                let mut numbered = theirs.into_iter().filter(|o| added(o) == Some(number));
                let found = numbered.next()?;
                numbered.next().is_none().then_some(found)
            }
        };
    }
    if let (Some(id), true) = (id, with_ids) {
        return outputs
            .iter()
            .find(|o| o.connector_id == Some(id) && same_type(connector_type, &o.name));
    }
    let kind = connector_type;
    let spelled = match Naming::of(outputs) {
        // The kernel's DP-1 and HDMI-A-1 are DisplayPort-0 and HDMI-A-0.
        Naming::FromZero => format!(
            "{}-{}",
            if kind == "DP" { "DisplayPort" } else { kind },
            number.checked_sub(1)?
        ),
        // HDMI1, DP1, DVI1.
        Naming::RunOn => format!(
            "{}{}",
            match kind {
                "HDMI-A" | "HDMI-B" => "HDMI",
                "DVI-D" | "DVI-I" => "DVI",
                kind => kind,
            },
            number
        ),
        // HDMI-1, DP-1, DVI-D-1.
        Naming::Modesetting => {
            format!("{}-{}", if kind == "HDMI-A" { "HDMI" } else { kind }, number)
        }
    };
    outputs.iter().find(|o| o.name == spelled)
}

// Whether RandR's `output` names a connector of the kernel's `connector_type`.
fn same_type(connector_type: &str, output: &str) -> bool {
    let prefixes: &[&str] = match connector_type {
        "DP" => &["DP", "DisplayPort"],
        "HDMI-A" | "HDMI-B" => &["HDMI"],
        "DVI-D" | "DVI-I" => &["DVI"],
        "Virtual" => &["Virtual"],
        other => return output.starts_with(other),
    };
    prefixes.iter().any(|p| output.starts_with(p))
        && !(connector_type == "DP" && output.starts_with("DPI"))
}

/// A RandR provider, as `xrandr --listproviders` prints it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Provider {
    pub id: String,
    pub source_output: bool,
    pub sink_output: bool,
    /// Providers it is bound to.
    pub associated: usize,
    pub name: String,
}

/// "Provider 1: id: 0x9b cap: 0x2, Sink Output crtcs: 1 outputs: 1 associated providers: 0
/// name:modesetting".
pub fn parse_providers(text: &str) -> Vec<Provider> {
    text.lines()
        .filter(|line| line.starts_with("Provider "))
        .filter_map(|line| {
            let id = line.split("id: ").nth(1)?.split_whitespace().next()?;
            let associated = line
                .split("associated providers: ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(0);
            Some(Provider {
                id: id.to_owned(),
                source_output: line.contains("Source Output"),
                sink_output: line.contains("Sink Output"),
                associated,
                name: line
                    .split("name:")
                    .nth(1)
                    .map_or(String::new(), |n| n.trim().to_owned()),
            })
        })
        .collect()
}

// How an X driver names its outputs.
enum Naming {
    /// The modesetting driver's: the kernel's, HDMI-A shortened to HDMI.
    Modesetting,
    /// The amdgpu and radeon drivers', counting each kind from 0 (NVIDIA's too, whose
    /// connectors are none of ours to light).
    FromZero,
    /// The intel driver's, the number run on.
    RunOn,
}

impl Naming {
    fn of(outputs: &[Output]) -> Self {
        if outputs
            .iter()
            .any(|o| o.name.ends_with("-0") || o.name.starts_with("DisplayPort-"))
        {
            return Naming::FromZero;
        }
        let run_on = outputs.iter().any(|o| {
            let stem = o.name.trim_end_matches(|c: char| c.is_ascii_digit());
            stem.len() < o.name.len() && !stem.ends_with('-')
        });
        if run_on {
            Naming::RunOn
        } else {
            Naming::Modesetting
        }
    }
}

/// Where our displays go: in a row to the right of every other output that is on, top-aligned
/// with them, in the order given (oldest first). `ours` holds each display's name and size.
pub fn layout(outputs: &[Output], ours: &[(String, i32, i32)]) -> Vec<(String, i32, i32)> {
    let others: Vec<(i32, i32, i32, i32)> = outputs
        .iter()
        .filter(|o| !ours.iter().any(|(name, _, _)| *name == o.name))
        .filter_map(|o| o.area)
        .collect();
    let mut x = others.iter().map(|(x, _, w, _)| x + w).max().unwrap_or(0);
    let y = others.iter().map(|(_, y, _, _)| *y).min().unwrap_or(0);
    ours.iter()
        .map(|(name, width, _)| {
            let place = (name.clone(), x, y);
            x += width;
            place
        })
        .collect()
}

/// How an output shows its part of the screen.
#[derive(Clone, Debug, PartialEq)]
pub enum Look {
    /// Someone else's output: only moved, whatever it shows.
    Moved,
    /// One of ours, at the size of its mode.
    Mode(String, (i32, i32)),
    /// One of ours, its mode scaled to the size: for a connector without a monitor that can only
    /// carry a slow mode.
    Scaled(String, (i32, i32)),
    /// Someone else's output, off while ours have the desktop (`Mode::Privacy`); or one of ours,
    /// off as the desktop is given back.
    Off,
    /// Someone else's output on again as it was before: its mode at its rate, turned as it was.
    Back {
        mode: String,
        rotation: Option<String>,
        rate: Option<f64>,
    },
}

/// Where an output goes.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    pub name: String,
    pub at: (i32, i32),
    pub look: Look,
}

impl Place {
    /// Whether `outputs` show it where it goes, and as it should look: with its mode (two of
    /// ours can be of one size, at two rates) and at its size.
    pub fn is_in(&self, outputs: &[Output]) -> bool {
        outputs.iter().any(|o| {
            o.name == self.name
                && match (o.area, &self.look) {
                    (area, Look::Off) => area.is_none(),
                    (None, _) => false,
                    (Some((x, y, w, h)), look) => {
                        (x, y) == self.at
                            && match look {
                                Look::Moved => true,
                                Look::Mode(mode, size) | Look::Scaled(mode, size) => {
                                    (w, h) == *size && o.current.as_deref() == Some(mode.as_str())
                                }
                                Look::Back { mode, rotation, .. } => {
                                    o.current.as_deref() == Some(mode.as_str())
                                        && o.rotation == *rotation
                                }
                                Look::Off => false,
                            }
                    }
                }
        })
    }
}

/// One `xrandr` call that puts each output of `places` in its place and makes `primary` primary:
/// one change for the desktop to see, not one per output.
pub fn arrange_args(places: &[Place], primary: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    for place in places {
        args.extend(["--output".to_owned(), place.name.clone()]);
        match &place.look {
            Look::Moved => {}
            Look::Mode(mode, _) => args.extend(["--mode".to_owned(), mode.clone()]),
            Look::Scaled(mode, (width, height)) => args.extend([
                "--mode".to_owned(),
                mode.clone(),
                "--scale-from".to_owned(),
                format!("{}x{}", width, height),
            ]),
            Look::Off => {
                args.push("--off".to_owned());
                continue;
            }
            Look::Back {
                mode,
                rotation,
                rate,
            } => {
                args.extend(["--mode".to_owned(), mode.clone()]);
                if let Some(rate) = rate {
                    args.extend(["--rate".to_owned(), format!("{:.2}", rate)]);
                }
                args.extend([
                    "--rotate".to_owned(),
                    rotation.clone().unwrap_or_else(|| "normal".to_owned()),
                ]);
            }
        }
        args.extend(["--pos".to_owned(), format!("{}x{}", place.at.0, place.at.1)]);
        if primary == Some(place.name.as_str()) {
            args.push("--primary".to_owned());
        }
    }
    if let Some(primary) = primary {
        if !places.iter().any(|p| p.name == primary) {
            args.extend(["--output".to_owned(), primary.to_owned(), "--primary".to_owned()]);
        }
    }
    args
}

/// The `xrandr` calls that turn output `name` off, of a screen `screen` big. A CRTC keeps the
/// transform it was last given through `--off` and puts it on whatever it drives next, a monitor
/// plugged in later included: the transform goes first, while the output is still on, and at the
/// size the screen has, so the screen is resized once, not twice.
pub fn off_args(name: &str, screen: Option<(i32, i32)>) -> Vec<Vec<String>> {
    let mut identity = vec![
        "--output".to_owned(),
        name.to_owned(),
        "--transform".to_owned(),
        "none".to_owned(),
    ];
    if let Some((width, height)) = screen {
        identity.extend(["--fb".to_owned(), format!("{}x{}", width, height)]);
    }
    vec![
        identity,
        vec!["--output".to_owned(), name.to_owned(), "--off".to_owned()],
    ]
}

/// The lowest refresh rate of the outputs that are on but `ours`, 60 if none says. A compositor
/// such as KWin runs at the lowest rate of all outputs: a virtual display slower than that would
/// slow the whole desktop down.
pub fn lowest_rate(outputs: &[Output], ours: &[&str]) -> f64 {
    outputs
        .iter()
        .filter(|o| o.area.is_some() && !ours.contains(&o.name.as_str()))
        .filter_map(|o| o.rate)
        .fold(None, |low: Option<f64>, rate| Some(low.map_or(rate, |low| low.min(rate))))
        .unwrap_or(60.0)
}

/// A mode's timings, as `xrandr --newmode` takes them.
#[derive(Clone, Debug, PartialEq)]
pub struct Modeline {
    pub name: String,
    pub clock_khz: u32,
    /// Display, sync start, sync end, total.
    pub h: [u32; 4],
    pub v: [u32; 4],
}

impl Modeline {
    /// CVT reduced-blanking timings (VESA CVT 1.1, as `cvt -r` gives them) for `width`x`height`
    /// at `hz` or a hair more: the clock is rounded up, not down, so the mode is never slower than
    /// asked. The width goes down to a multiple of 8, as CVT has it.
    pub fn cvt_rb(width: u32, height: u32, hz: f64) -> Self {
        const MIN_VBLANK_US: f64 = 460.0;
        const H_BLANK: u32 = 160;
        const H_SYNC: u32 = 32;
        const V_FRONT_PORCH: u32 = 3;
        const MIN_V_BACK_PORCH: u32 = 6;
        let (w, h) = (width - width % 8, height);
        let v_sync = match (w, h) {
            (w, h) if h % 3 == 0 && h * 4 / 3 == w => 4,
            (w, h) if h % 9 == 0 && h * 16 / 9 == w => 5,
            (w, h) if h % 10 == 0 && h * 16 / 10 == w => 6,
            (w, h) if (h % 4 == 0 && h * 5 / 4 == w) || (h % 9 == 0 && h * 15 / 9 == w) => 7,
            _ => 10,
        };
        let line_us = (1_000_000.0 / hz - MIN_VBLANK_US) / h as f64;
        let v_blank = ((MIN_VBLANK_US / line_us) as u32 + 1).max(V_FRONT_PORCH + v_sync + MIN_V_BACK_PORCH);
        let (h_total, v_total) = (w + H_BLANK, h + v_blank);
        let h_sync_end = w + H_BLANK / 2;
        Self {
            // Its size and rate in its name, so a mode of that name left by a server that went
            // away is the same mode.
            name: format!("{}x{}_{:.2}_rustdesk", w, h, hz),
            clock_khz: (hz * h_total as f64 * v_total as f64 / 1000.0).ceil() as u32,
            h: [w, h_sync_end - H_SYNC, h_sync_end, h_total],
            v: [h, h + V_FRONT_PORCH, h + V_FRONT_PORCH + v_sync, v_total],
        }
    }

    pub fn size(&self) -> (i32, i32) {
        (self.h[0] as i32, self.v[0] as i32)
    }

    pub fn rate(&self) -> f64 {
        self.clock_khz as f64 * 1000.0 / (self.h[3] as f64 * self.v[3] as f64)
    }

    /// Whether `name` is the name of one of ours.
    pub fn is_ours(name: &str) -> bool {
        name.ends_with("_rustdesk")
    }

    /// `xrandr --newmode` arguments for it.
    pub fn newmode_args(&self) -> Vec<String> {
        let mut args = vec![
            "--newmode".to_owned(),
            self.name.clone(),
            format!("{:.3}", self.clock_khz as f64 / 1000.0),
        ];
        args.extend(self.h.iter().chain(&self.v).map(|n| n.to_string()));
        args.extend(["+hsync".to_owned(), "-vsync".to_owned()]);
        args
    }
}

/// The size of the screen the outputs that are on show, from the origin.
pub fn extent(outputs: &[Output]) -> Option<(i32, i32)> {
    outputs
        .iter()
        .filter_map(|o| o.area)
        .map(|(x, y, w, h)| (x + w, y + h))
        .reduce(|(w, h), (x, y)| (w.max(x), h.max(y)))
}

/// An area of the screen, (x, y, width, height).
pub type Area = (i32, i32, i32, i32);

/// The outputs that are on, and the area each shows.
pub fn lit(outputs: &[Output]) -> Vec<(String, Area)> {
    outputs
        .iter()
        .filter_map(|o| Some((o.name.clone(), o.area?)))
        .collect()
}

/// The area output `name` shows, so long as it shows it alone: no other output that is on shows
/// any of it (a mirror, or one the desktop moved over it), whose monitor would show what is put
/// there too. None while the output is off.
pub fn alone(name: &str, lit: &[(String, Area)]) -> Option<Area> {
    let (x, y, w, h) = lit.iter().find(|(n, _)| n == name)?.1;
    let overlaps =
        |&(ox, oy, ow, oh): &Area| ox < x + w && x < ox + ow && oy < y + h && y < oy + oh;
    (!lit.iter().any(|(n, area)| n != name && overlaps(area))).then_some((x, y, w, h))
}

// "2560x1440+1920+0".
fn parse_area(word: &str) -> Option<(i32, i32, i32, i32)> {
    let (size, rest) = word.split_once('+')?;
    let (x, y) = rest.split_once('+')?;
    let (w, h) = parse_size(size)?;
    Some((x.parse().ok()?, y.parse().ok()?, w, h))
}

// "1920x1080", "1920x1080i", or a mode named after its size, "2560x1440_rustdesk".
/// The size a mode named `mode` shows: "1920x1080", "2560x1440_60.00_rustdesk".
pub fn mode_size(mode: &str) -> Option<(i32, i32)> {
    parse_size(mode)
}

fn parse_size(word: &str) -> Option<(i32, i32)> {
    let (w, h) = word.split_once('x')?;
    let h = h.split(|c: char| !c.is_ascii_digit()).next()?;
    Some((w.parse().ok()?, h.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_output_shows_alone_unless_another_shows_some_of_it() {
        let hdmi = ("HDMI-1".to_owned(), (0, 0, 1920, 1080));
        let ours = |area: Area| ("Virtual-1-1".to_owned(), area);
        // Beside the monitor, touching its edge: alone.
        let beside = [hdmi.clone(), ours((1920, 0, 2048, 1152))];
        assert_eq!(alone("Virtual-1-1", &beside), Some((1920, 0, 2048, 1152)));
        // Mirrored, or moved over it, even partly: not.
        for area in [(0, 0, 2048, 1152), (1000, 500, 2048, 1152), (-100, 0, 200, 200)] {
            assert_eq!(alone("Virtual-1-1", &[hdmi.clone(), ours(area)]), None, "{:?}", area);
        }
        // Off: nowhere.
        assert_eq!(alone("Virtual-1-1", &[hdmi.clone()]), None);
        // The others show alone or not by the same rule.
        assert_eq!(alone("HDMI-1", &beside), Some((0, 0, 1920, 1080)));
        let off = Output {
            name: "DP-1".to_owned(),
            ..Default::default()
        };
        let on = Output {
            name: "HDMI-1".to_owned(),
            area: Some((0, 0, 1920, 1080)),
            ..Default::default()
        };
        assert_eq!(lit(&[off, on]), vec![hdmi]);
    }

    // As the office machine printed it during the manual test.
    const QUERY: &str = "Screen 0: minimum 320 x 200, current 4480 x 1440, maximum 16384 x 16384
HDMI-1 connected 1920x1080+0+0 (normal left inverted right x axis y axis) 500mm x 281mm
   1920x1080     60.00*+
DP-1 connected primary 2560x1440+1920+0 (normal left inverted right x axis y axis) 355mm x 266mm
   1024x768      60.00*+  60.00
DP-2 disconnected (normal left inverted right x axis y axis)
";

    const PROP: &str = "Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 16384 x 16384
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 500mm x 281mm
\tEDID:
\t\t00ffffffffffff00
\tCONNECTOR_ID: 103
\t\tsupported: 103
\tnon-desktop: 0
\t\trange: (0, 1)
   1920x1080     60.00*+
DP-1 disconnected (normal left inverted right x axis y axis)
\tCONNECTOR_ID: 95
\t\tsupported: 95
";

    #[test]
    fn how_an_output_is_turned_is_read() {
        let outputs = parse(
            "HDMI-1 connected primary 1080x1920+0+0 left (normal left inverted right x axis y \
             axis) 500mm x 281mm\n   1920x1080     60.00*+\nDP-1 connected 1920x1080+1080+0 \
             (normal left inverted right x axis y axis) 500mm x 281mm\n   1920x1080     60.00*+\n\
             DP-2 disconnected (normal left inverted right x axis y axis)\n",
        );
        assert_eq!(outputs[0].rotation.as_deref(), Some("left"));
        assert_eq!(outputs[0].area, Some((0, 0, 1080, 1920)));
        assert_eq!(outputs[1].rotation, None);
        assert_eq!(outputs[2].rotation, None);
    }

    #[test]
    fn the_desktop_is_taken_over_and_given_back_in_one_call_each() {
        let over = vec![
            Place {
                name: "HDMI-1".to_owned(),
                at: (0, 0),
                look: Look::Off,
            },
            Place {
                name: "Virtual-1-1".to_owned(),
                at: (0, 0),
                look: Look::Mode("2560x1600_60.00_rustdesk".to_owned(), (2560, 1600)),
            },
        ];
        assert_eq!(
            arrange_args(&over, Some("Virtual-1-1")).join(" "),
            "--output HDMI-1 --off \
             --output Virtual-1-1 --mode 2560x1600_60.00_rustdesk --pos 0x0 --primary"
        );
        let taken = parse(
            "HDMI-1 connected (normal left inverted right x axis y axis)\n   1920x1080     60.00 \
             +\nVirtual-1-1 connected primary 2560x1600+0+0 (normal left inverted right x axis y \
             axis) 0mm x 0mm\n   2560x1600_60.00_rustdesk  59.99*\n",
        );
        assert!(over.iter().all(|p| p.is_in(&taken)));

        let back = vec![
            Place {
                name: "HDMI-1".to_owned(),
                at: (0, 0),
                look: Look::Back {
                    mode: "1920x1080".to_owned(),
                    rotation: Some("left".to_owned()),
                    rate: Some(144.0),
                },
            },
            Place {
                name: "Virtual-1-1".to_owned(),
                at: (0, 0),
                look: Look::Off,
            },
        ];
        assert_eq!(
            arrange_args(&back, Some("HDMI-1")).join(" "),
            "--output HDMI-1 --mode 1920x1080 --rate 144.00 --rotate left --pos 0x0 --primary \
             --output Virtual-1-1 --off"
        );
        assert!(!back[0].is_in(&taken));
        assert!(!back[1].is_in(&taken));
        let given_back = parse(
            "HDMI-1 connected primary 1080x1920+0+0 left (normal left inverted right x axis y \
             axis) 500mm x 281mm\n   1920x1080     60.00*+\nVirtual-1-1 connected (normal left \
             inverted right x axis y axis)\n   2560x1600_60.00_rustdesk  59.99\n",
        );
        assert!(back.iter().all(|p| p.is_in(&given_back)));
    }

    #[test]
    fn query_output_is_read() {
        let outputs = parse(QUERY);
        assert_eq!(outputs.len(), 3);
        assert_eq!(
            outputs[0],
            Output {
                name: "HDMI-1".to_owned(),
                connected: true,
                primary: false,
                area: Some((0, 0, 1920, 1080)),
                modes: vec!["1920x1080".to_owned()],
                current: Some("1920x1080".to_owned()),
                rate: Some(60.0),
                connector_id: None,
                rotation: None,
            }
        );
        assert!(outputs[1].primary);
        assert_eq!(outputs[1].area, Some((1920, 0, 2560, 1440)));
        // Both refresh rates of 1024x768 are on one line: one mode.
        assert_eq!(outputs[1].modes, vec!["1024x768"]);
        assert!(!outputs[2].connected && outputs[2].area.is_none());
    }

    #[test]
    fn connector_ids_come_with_prop() {
        let outputs = parse(PROP);
        assert_eq!(outputs[0].connector_id, Some(103));
        assert_eq!(outputs[0].modes, vec!["1920x1080"]);
        assert_eq!(outputs[1].connector_id, Some(95));
        let found = |name: &str, id: Option<u32>| {
            output_for(&outputs, name, id, Kind::Connector).map(|o| o.name.clone())
        };
        // By id, whatever the spelling.
        assert_eq!(found("DP-1", Some(95)).unwrap(), "DP-1");
        assert_eq!(found("HDMI-A-1", Some(103)).unwrap(), "HDMI-1");
        // A driver that shows ids and shows none for this connector: not this screen's.
        assert!(found("DP-1", Some(96)).is_none());
        // The same id on another card, for another type of connector: not it.
        assert!(found("DP-1", Some(103)).is_none());
        // A kernel too old to show ids: by name.
        assert_eq!(found("DP-1", None).unwrap(), "DP-1");
    }

    #[test]
    fn vkms_is_an_output_of_a_card_added_to_the_screen() {
        let outputs = parse(
            "Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 16384 x 16384
Virtual-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 0mm x 0mm
\tCONNECTOR_ID: 36
   1920x1080     60.00*+
Virtual-1-1 disconnected (normal left inverted right x axis y axis)
\tCONNECTOR_ID: 36
",
        );
        // virtio-gpu's Virtual-1 has the same id, on its own card: never vkms's.
        let found = output_for(&outputs, "Virtual-1", Some(36), Kind::Vkms).unwrap();
        assert_eq!(found.name, "Virtual-1-1");
        assert!(output_for(&outputs, "Virtual-1", Some(37), Kind::Vkms).is_none());
        // Without ids, the only one there is; so too where only the screen's own outputs show
        // ids.
        let mut without = outputs.clone();
        without[1].connector_id = None;
        assert_eq!(
            output_for(&without, "Virtual-1", Some(36), Kind::Vkms).unwrap().name,
            "Virtual-1-1"
        );
        without.iter_mut().for_each(|o| o.connector_id = None);
        assert_eq!(
            output_for(&without, "Virtual-1", None, Kind::Vkms).unwrap().name,
            "Virtual-1-1"
        );
        without.push(Output {
            name: "Virtual-2-1".to_owned(),
            ..Default::default()
        });
        assert!(output_for(&without, "Virtual-1", None, Kind::Vkms).is_none());
        // Never a connector's.
        assert!(output_for(&outputs, "DP-1", Some(36), Kind::Connector).is_none());
    }

    #[test]
    fn providers_are_read() {
        let providers = parse_providers(
            "Providers: number : 2
Provider 0: id: 0x47 cap: 0xf, Source Output, Sink Output, Source Offload, Sink Offload crtcs: 3 outputs: 2 associated providers: 1 name:modesetting
Provider 1: id: 0x9b cap: 0x2, Sink Output crtcs: 1 outputs: 1 associated providers: 0 name:modesetting
",
        );
        assert_eq!(
            providers,
            vec![
                Provider {
                    id: "0x47".to_owned(),
                    source_output: true,
                    sink_output: true,
                    associated: 1,
                    name: "modesetting".to_owned(),
                },
                Provider {
                    id: "0x9b".to_owned(),
                    source_output: false,
                    sink_output: true,
                    associated: 0,
                    name: "modesetting".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn names_match_without_ids() {
        let named = |names: &[&str]| -> Vec<Output> {
            names
                .iter()
                .map(|n| Output {
                    name: n.to_string(),
                    ..Default::default()
                })
                .collect()
        };
        let found = |outputs: &[Output], name: &str| {
            output_for(outputs, name, None, Kind::Connector).map(|o| o.name.clone())
        };
        let modesetting = named(&["HDMI-1", "DP-1"]);
        assert_eq!(found(&modesetting, "HDMI-A-1").unwrap(), "HDMI-1");
        assert_eq!(found(&modesetting, "DP-1").unwrap(), "DP-1");
        let intel = named(&["HDMI1", "DP1", "eDP1"]);
        assert_eq!(found(&intel, "HDMI-A-1").unwrap(), "HDMI1");
        assert_eq!(found(&intel, "DP-1").unwrap(), "DP1");
        assert!(found(&intel, "DP-2").is_none());
        // Two HDMI ports counted from 0: the kernel's HDMI-A-1 is HDMI-A-0, never their HDMI-A-1,
        // which is the kernel's HDMI-A-2.
        let amdgpu = named(&["HDMI-A-0", "HDMI-A-1", "DisplayPort-0", "DisplayPort-1"]);
        assert_eq!(found(&amdgpu, "HDMI-A-1").unwrap(), "HDMI-A-0");
        assert_eq!(found(&amdgpu, "HDMI-A-2").unwrap(), "HDMI-A-1");
        assert_eq!(found(&amdgpu, "DP-2").unwrap(), "DisplayPort-1");
        assert!(found(&amdgpu, "DP-3").is_none());
        // NVIDIA's, also from 0 but spelled otherwise: nothing.
        assert!(found(&named(&["DP-0", "DP-1", "HDMI-0"]), "DP-1").is_none());
        // GPU screens' outputs do not make the naming look run on.
        let shared = named(&["HDMI-1", "DP-1", "Virtual-1-1"]);
        assert_eq!(found(&shared, "DP-1").unwrap(), "DP-1");
    }

    #[test]
    fn our_displays_go_in_a_row_right_of_the_others() {
        let outputs = parse(QUERY);
        // DP-1 is ours; DP-3 comes next.
        let ours = vec![
            ("DP-1".to_owned(), 2560, 1440),
            ("DP-3".to_owned(), 1920, 1080),
        ];
        assert_eq!(
            layout(&outputs, &ours),
            vec![("DP-1".to_owned(), 1920, 0), ("DP-3".to_owned(), 4480, 0)]
        );
        // Once DP-1 is off and gone, DP-3 moves up to where it was.
        let mut without = outputs.clone();
        without[1].area = None;
        assert_eq!(
            layout(&without, &ours[1..]),
            vec![("DP-3".to_owned(), 1920, 0)]
        );
        // Nothing else on: from the origin.
        assert_eq!(layout(&[], &ours[..1]), vec![("DP-1".to_owned(), 0, 0)]);
    }

    #[test]
    fn one_call_lays_everything_out() {
        let places = vec![
            Place {
                name: "HDMI-1".to_owned(),
                at: (0, 0),
                look: Look::Moved,
            },
            Place {
                name: "DP-1".to_owned(),
                at: (1920, 0),
                look: Look::Scaled("1024x768".to_owned(), (2560, 1440)),
            },
        ];
        assert_eq!(
            arrange_args(&places, Some("HDMI-1")).join(" "),
            "--output HDMI-1 --pos 0x0 --primary \
             --output DP-1 --mode 1024x768 --scale-from 2560x1440 --pos 1920x0"
        );
        // The primary output need not move to be made primary.
        assert_eq!(
            arrange_args(&places[1..], Some("HDMI-1")).join(" "),
            "--output DP-1 --mode 1024x768 --scale-from 2560x1440 --pos 1920x0 \
             --output HDMI-1 --primary"
        );
        let outputs = parse(QUERY);
        assert!(places.iter().all(|p| p.is_in(&outputs)));
        let smaller = Place {
            look: Look::Scaled("1024x768".to_owned(), (1920, 1080)),
            ..places[1].clone()
        };
        assert!(!smaller.is_in(&outputs));
        // A display at the size of its mode: no scaling.
        let real = Place {
            name: "Virtual-1-1".to_owned(),
            at: (1920, 0),
            look: Look::Mode("2560x1440_60.00_rustdesk".to_owned(), (2560, 1440)),
        };
        assert_eq!(
            arrange_args(&[real], None).join(" "),
            "--output Virtual-1-1 --mode 2560x1440_60.00_rustdesk --pos 1920x0"
        );
    }

    #[test]
    fn rates_are_read_from_the_mode_shown() {
        let outputs = parse(
            "Screen 0: minimum 320 x 200, current 4480 x 1440, maximum 16384 x 16384
HDMI-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 500mm x 281mm
   1920x1080     60.00 +  74.97*   50.00
   1280x720      60.00
Virtual-1-1 connected 2560x1440+1920+0 (normal left inverted right x axis y axis) 0mm x 0mm
   1024x768      60.00 +
   2560x1440_60.00_rustdesk  60.00*
DP-1 disconnected (normal left inverted right x axis y axis)
",
        );
        assert_eq!(outputs[0].rate, Some(74.97));
        assert_eq!(outputs[0].current.as_deref(), Some("1920x1080"));
        assert_eq!(outputs[1].modes, vec!["1024x768", "2560x1440_60.00_rustdesk"]);
        assert_eq!(outputs[1].rate, Some(60.0));
        assert_eq!(outputs[1].current.as_deref(), Some("2560x1440_60.00_rustdesk"));
        assert_eq!(outputs[2].current, None);
        // Of the size, but in another mode: not as it should look.
        let place = Place {
            name: "Virtual-1-1".to_owned(),
            at: (1920, 0),
            look: Look::Mode("2560x1440_59.95_rustdesk".to_owned(), (2560, 1440)),
        };
        assert!(!place.is_in(&outputs));
        assert_eq!(outputs[2].rate, None);
        assert_eq!(lowest_rate(&outputs, &["Virtual-1-1"]), 74.97);
        assert_eq!(lowest_rate(&outputs, &[]), 60.0);
        assert_eq!(lowest_rate(&[], &[]), 60.0);
    }

    #[test]
    fn cvt_reduced_blanking_matches_cvt_but_never_runs_slower() {
        // cvt -r 2560 1440 60: 241.50  2560 2608 2640 2720  1440 1443 1448 1481 +hsync -vsync
        let m = Modeline::cvt_rb(2560, 1440, 60.0);
        assert_eq!(m.name, "2560x1440_60.00_rustdesk");
        assert!(Modeline::is_ours(&m.name) && !Modeline::is_ours("2560x1440"));
        assert_eq!(m.h, [2560, 2608, 2640, 2720]);
        assert_eq!(m.v, [1440, 1443, 1448, 1481]);
        // CVT rounds the clock down to 241.50 MHz (59.95 Hz); ours goes up to stay at 60.
        assert_eq!(m.clock_khz, 241_700);
        assert!(m.rate() >= 60.0 && m.rate() < 60.01);
        assert_eq!(
            m.newmode_args().join(" "),
            "--newmode 2560x1440_60.00_rustdesk 241.700 2560 2608 2640 2720 1440 1443 1448 1481 \
             +hsync -vsync"
        );
        // cvt -r 1920 1080 60: 138.50  1920 1968 2000 2080  1080 1083 1088 1111
        let m = Modeline::cvt_rb(1920, 1080, 60.0);
        assert_eq!(m.h, [1920, 1968, 2000, 2080]);
        assert_eq!(m.v, [1080, 1083, 1088, 1111]);
        assert!(m.rate() >= 60.0);
        // 16:10 and other aspects take their own sync widths; odd widths round down to 8.
        assert_eq!(Modeline::cvt_rb(1440, 900, 60.0).v[2] - Modeline::cvt_rb(1440, 900, 60.0).v[1], 6);
        assert_eq!(Modeline::cvt_rb(1366, 768, 60.0).size(), (1360, 768));
        let fast = Modeline::cvt_rb(1920, 1080, 144.0);
        assert!(fast.rate() >= 144.0);
    }

    #[test]
    fn turning_off_drops_the_transform_first_at_the_same_size() {
        let outputs = parse(QUERY);
        assert_eq!(extent(&outputs), Some((4480, 1440)));
        assert_eq!(extent(&[]), None);
        let calls: Vec<String> = off_args("DP-1", extent(&outputs))
            .iter()
            .map(|args| args.join(" "))
            .collect();
        assert_eq!(
            calls,
            vec![
                "--output DP-1 --transform none --fb 4480x1440",
                "--output DP-1 --off"
            ]
        );
    }
}
