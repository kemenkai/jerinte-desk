//! What the user's `--server` asks the root service for, one JSON object a line each way.

use serde_derive::{Deserialize, Serialize};
use std::io::{BufRead, Write};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// How many displays could still be given out, and of which kind.
    Status,
    /// A connector to light later; its name is known before its display is there. `skip_vkms`:
    /// none of the vkms driver's, which this session's X server did not show.
    Reserve {
        #[serde(default)]
        skip_vkms: bool,
    },
    /// Lights a connector this client reserved.
    Activate { connector: String },
    /// Lets a connector this client holds go.
    Release { connector: String },
    /// This client is about to take the desktop over (`Mode::Privacy`), turning the outputs of
    /// `restore.back` off. Should it go without `Restored` (it crashed, or was killed), the
    /// service gives the desktop back as `restore` says, on this client's X server, before
    /// letting its connectors go. Sent again whenever `restore` changes.
    Takeover { restore: Restore },
    /// The desktop is given back: nothing for the service to do any more.
    Restored,
    /// This client's `--server` keeps virtual displays of its own (`on`), or no more: it is not
    /// restarted meanwhile. For as long as the connection lasts.
    Hold { on: bool },
    /// This client took its desktop over by itself, laying it out as the service cannot (GNOME's):
    /// the keyboards and pointers here are kept from the desktop until `Restored`, Ctrl+P, or the
    /// connection goes. Refused while a desktop taken back from this process is not given back.
    LockInput,
    /// Whether the person at the device took back the desktop this client's process took over
    /// (Ctrl+P), as `Status` says, without looking at the connectors: asked often.
    TakenBack,
}

/// How a taken-over desktop is given back, in one `xrandr` call.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Restore {
    /// The outputs to turn on again, as they were.
    pub back: Vec<Back>,
    /// Ours, turned off in the same call.
    #[serde(default)]
    pub off: Vec<String>,
}

/// An output as it was before the desktop was taken over.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Back {
    pub name: String,
    pub mode: String,
    pub x: i32,
    pub y: i32,
    /// `left`, `right` or `inverted`; none for `normal`.
    #[serde(default)]
    pub rotation: Option<String>,
    /// The refresh rate it ran at: a mode name can stand for several.
    #[serde(default)]
    pub rate: Option<f64>,
    #[serde(default)]
    pub primary: bool,
}

// A sane bound on the outputs of one X screen.
const MAX_RESTORED: usize = 16;

impl Restore {
    /// Whether it is something `xrandr` takes as meant and nothing else, the service running what
    /// is built of it as root: names and modes of a few plain characters, a known rotation, a
    /// place on the screen, at most one primary, no output twice, and something to turn on.
    pub fn valid(&self) -> bool {
        let plain = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && !s.starts_with('-')
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.:-".contains(&b))
        };
        let mut names: Vec<&str> = self
            .back
            .iter()
            .map(|b| b.name.as_str())
            .chain(self.off.iter().map(String::as_str))
            .collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        !self.back.is_empty()
            && count <= MAX_RESTORED
            && names.len() == count
            && names.iter().all(|n| plain(n))
            && self.back.iter().all(|b| {
                plain(&b.mode)
                    && (0..=32767).contains(&b.x)
                    && (0..=32767).contains(&b.y)
                    && b.rotation
                        .as_deref()
                        .map_or(true, |r| ["left", "right", "inverted"].contains(&r))
                    && b.rate.map_or(true, |r| r.is_finite() && (1.0..=1000.0).contains(&r))
            })
            && self.back.iter().filter(|b| b.primary).count() <= 1
    }

    /// The `xrandr` arguments that give the desktop back.
    pub fn args(&self) -> Vec<String> {
        use super::randr::{arrange_args, Look, Place};
        let places: Vec<Place> = self
            .back
            .iter()
            .map(|b| Place {
                name: b.name.clone(),
                at: (b.x, b.y),
                look: Look::Back {
                    mode: b.mode.clone(),
                    rotation: b.rotation.clone(),
                    rate: b.rate,
                },
            })
            .chain(self.off.iter().map(|name| Place {
                name: name.clone(),
                at: (0, 0),
                look: Look::Off,
            }))
            .collect();
        let primary = self.back.iter().find(|b| b.primary).map(|b| b.name.as_str());
        arrange_args(&places, primary)
    }

    /// Short of that: each output on at the mode it likes (off, should it have gone meanwhile),
    /// the primary primary, ours off all the same.
    pub fn fallback_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for b in &self.back {
            args.extend(["--output".to_owned(), b.name.clone(), "--auto".to_owned()]);
            if b.primary {
                args.push("--primary".to_owned());
            }
        }
        for name in &self.off {
            args.extend(["--output".to_owned(), name.clone(), "--off".to_owned()]);
        }
        args
    }
}

/// How a display is made, best first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// The connector of the vkms driver, the kernel's virtual display card: any mode.
    Vkms,
    /// A connector without a monitor, given an EDID of ours: real modes, up to what its port
    /// carries.
    Connector,
    /// One that carries a slow mode only, scaled up to the size asked for: Intel's DisplayPort,
    /// which without a monitor trains its link at the lowest rate.
    Scaled,
}

impl Kind {
    pub fn name(&self) -> &'static str {
        match self {
            Kind::Vkms => "vkms",
            Kind::Connector => "connector",
            Kind::Scaled => "scaled",
        }
    }
}

/// A connector put back.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Gone {
    pub connector: String,
    #[serde(default)]
    pub connector_id: Option<u32>,
    pub kind: Kind,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The number of the request answered.
    #[serde(default)]
    pub seq: u64,
    /// Empty when the request succeeded.
    #[serde(default)]
    pub error: String,
    /// With `Status`: displays that could still be given out.
    #[serde(default)]
    pub free: usize,
    /// With `Reserve`: the kind of the display; with `Status`, the kind the next one would be.
    #[serde(default)]
    pub kind: Option<Kind>,
    #[serde(default)]
    pub connector: String,
    #[serde(default)]
    pub connector_id: Option<u32>,
    /// With `Reserve`: the fastest pixel clock the connector carries, in kHz, where it is known.
    #[serde(default)]
    pub max_khz: Option<u32>,
    #[serde(default)]
    pub modes: Vec<String>,
    /// With `Status`: the connectors last put back, most recent last. An output a `--server` that
    /// went away left on, on one of them, is a leftover to turn off; on any other, it is not ours.
    #[serde(default)]
    pub released: Vec<Gone>,
    /// With `Status`: why there are no vkms displays, when there are none.
    #[serde(default)]
    pub note: String,
    /// With `Status`: the person at the device took back the desktop this client took over
    /// (Ctrl+P), which it is to give back, and may not take over again until it did.
    #[serde(default)]
    pub taken_back: bool,
}

impl Response {
    pub fn failed(error: impl ToString) -> Self {
        Self {
            error: error.to_string(),
            ..Default::default()
        }
    }
}

/// The longest line either side reads; requests are tiny.
const MAX_LINE: u64 = 4096;

/// Answers requests from `reader` on `writer` until the other side goes. Each answer carries the
/// number of its request ("seq"), so that one that comes after its caller stopped waiting is not
/// taken for the answer to the next request: a caller that waits too long goes on, it does not
/// hang up, which would let go of whatever it holds.
pub fn serve(
    mut reader: impl BufRead,
    mut writer: impl Write,
    mut handle: impl FnMut(Request) -> Response,
) {
    loop {
        let line = match read_line(&mut reader) {
            Ok(Some(line)) => line,
            Ok(None) | Err(_) => return,
        };
        let (seq, request) = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(mut value) => {
                let seq = value
                    .as_object_mut()
                    .and_then(|fields| fields.remove("seq"))
                    .and_then(|seq| seq.as_u64())
                    .unwrap_or(0);
                (seq, serde_json::from_value::<Request>(value))
            }
            Err(e) => (0, Err(e)),
        };
        let mut response = match request {
            Ok(request) => handle(request),
            Err(e) => Response::failed(format!("bad request: {}", e)),
        };
        response.seq = seq;
        if write_line(&mut writer, &response).is_err() {
            return;
        }
    }
}

/// Sends `request`, numbered `seq`, and reads its answer, passing over those to earlier requests.
pub fn call(
    reader: &mut impl BufRead,
    writer: &mut impl Write,
    request: &Request,
    seq: u64,
) -> std::io::Result<Response> {
    let invalid = |e| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
    let mut value = serde_json::to_value(request)?;
    if let Some(fields) = value.as_object_mut() {
        fields.insert("seq".to_owned(), seq.into());
    }
    write_line(writer, &value)?;
    loop {
        let Some(line) = read_line(reader)? else {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        };
        let response: Response = serde_json::from_str(&line).map_err(invalid)?;
        match response.seq.cmp(&seq) {
            std::cmp::Ordering::Equal => return Ok(response),
            std::cmp::Ordering::Less => continue,
            std::cmp::Ordering::Greater => {
                return Err(invalid(serde::de::Error::custom("an answer to no request")))
            }
        }
    }
}

fn write_line(writer: &mut impl Write, value: &impl serde::Serialize) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

// None at the end of the stream; an error for a line longer than `MAX_LINE`.
fn read_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let n = std::io::Read::take(&mut *reader, MAX_LINE).read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn requests_and_answers_round_trip() {
        let requests = [
            Request::Status,
            Request::Reserve { skip_vkms: false },
            Request::Reserve { skip_vkms: true },
            Request::Activate {
                connector: "DP-1".to_owned(),
            },
            Request::Release {
                connector: "DP-1".to_owned(),
            },
            Request::Hold { on: true },
            Request::LockInput,
            Request::TakenBack,
            Request::Restored,
        ];
        let mut input = Vec::new();
        for r in &requests {
            // Requests of an older client, unnumbered, are answered all the same.
            write_line(&mut input, r).unwrap();
        }
        let mut output = Vec::new();
        let mut seen = Vec::new();
        serve(Cursor::new(input), &mut output, |r| {
            seen.push(r.clone());
            Response {
                connector: "Virtual-1".to_owned(),
                kind: Some(Kind::Vkms),
                released: vec![Gone {
                    connector: "DP-1".to_owned(),
                    connector_id: Some(95),
                    kind: Kind::Scaled,
                }],
                ..Default::default()
            }
        });
        assert_eq!(seen, requests);
        let mut answers = Cursor::new(output);
        for _ in 0..requests.len() {
            let a: Response =
                serde_json::from_str(&read_line(&mut answers).unwrap().unwrap()).unwrap();
            assert_eq!(a.connector, "Virtual-1");
            assert_eq!(a.kind, Some(Kind::Vkms));
            assert_eq!(a.released[0].kind, Kind::Scaled);
        }
        assert_eq!(read_line(&mut answers).unwrap(), None);
    }

    #[test]
    fn a_late_answer_is_passed_over() {
        // The answers to requests 1 and 2, the first after its caller gave up on it.
        let mut answers = Vec::new();
        for (seq, connector) in [(1, "late"), (2, "ours")] {
            write_line(
                &mut answers,
                &Response {
                    seq,
                    connector: connector.to_owned(),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let mut sent = Vec::new();
        let answer = call(&mut Cursor::new(answers), &mut sent, &Request::Status, 2).unwrap();
        assert_eq!(answer.connector, "ours");
        assert_eq!(sent, b"{\"op\":\"status\",\"seq\":2}\n");
        // The service numbers its answers as their requests.
        let mut output = Vec::new();
        serve(Cursor::new(sent), &mut output, |request| {
            assert_eq!(request, Request::Status);
            Response::default()
        });
        let a: Response = serde_json::from_slice(&output[..output.len() - 1]).unwrap();
        assert_eq!(a.seq, 2);
        // One from beyond: something is wrong with the stream.
        let mut ahead = Vec::new();
        write_line(&mut ahead, &Response { seq: 9, ..Default::default() }).unwrap();
        assert!(call(&mut Cursor::new(ahead), &mut Vec::new(), &Request::Status, 3).is_err());
    }

    // As the test stand-in for the service (a script outside this repository) reads them.
    #[test]
    fn a_session_taking_its_desktop_over_says_so_plainly() {
        for (request, line) in [
            (Request::LockInput, "{\"op\":\"lock_input\"}\n"),
            (Request::TakenBack, "{\"op\":\"taken_back\"}\n"),
        ] {
            let mut sent = Vec::new();
            write_line(&mut sent, &request).unwrap();
            assert_eq!(String::from_utf8(sent).unwrap(), line);
        }
    }

    #[test]
    fn the_wire_format_is_plain() {
        let mut line = Vec::new();
        write_line(
            &mut line,
            &Request::Activate {
                connector: "DP-1".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(line, b"{\"op\":\"activate\",\"connector\":\"DP-1\"}\n");
        // A reservation without a word on vkms takes one.
        assert_eq!(
            serde_json::from_str::<Request>("{\"op\":\"reserve\"}").unwrap(),
            Request::Reserve { skip_vkms: false }
        );
        let mut line = Vec::new();
        write_line(&mut line, &Kind::Connector).unwrap();
        assert_eq!(line, b"\"connector\"\n");
    }

    fn office_restore() -> Restore {
        Restore {
            back: vec![Back {
                name: "HDMI-1".to_owned(),
                mode: "1920x1080".to_owned(),
                x: 0,
                y: 0,
                rotation: None,
                rate: Some(60.0),
                primary: true,
            }],
            off: vec!["Virtual-1-1".to_owned()],
        }
    }

    #[test]
    fn a_desktop_is_given_back_in_one_call() {
        let restore = office_restore();
        assert!(restore.valid());
        assert_eq!(
            restore.args().join(" "),
            "--output HDMI-1 --mode 1920x1080 --rate 60.00 --rotate normal --pos 0x0 --primary \
             --output Virtual-1-1 --off"
        );
        assert_eq!(
            restore.fallback_args().join(" "),
            "--output HDMI-1 --auto --primary --output Virtual-1-1 --off"
        );
        // It travels as it is.
        let request = Request::Takeover {
            restore: restore.clone(),
        };
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        assert_eq!(
            serde_json::from_str::<Request>("{\"op\":\"restored\"}").unwrap(),
            Request::Restored
        );
    }

    #[test]
    fn a_hold_travels_as_it_is() {
        assert_eq!(
            serde_json::to_string(&Request::Hold { on: true }).unwrap(),
            "{\"op\":\"hold\",\"on\":true}"
        );
        assert_eq!(
            serde_json::from_str::<Request>("{\"op\":\"hold\",\"on\":false}").unwrap(),
            Request::Hold { on: false }
        );
    }

    #[test]
    fn only_a_plain_restore_is_run() {
        let with = |f: &dyn Fn(&mut Restore)| {
            let mut r = office_restore();
            f(&mut r);
            r.valid()
        };
        assert!(!with(&|r| r.back[0].name = "HDMI-1; reboot".to_owned()));
        assert!(!with(&|r| r.back[0].mode = "--fb".to_owned()));
        assert!(!with(&|r| r.back[0].mode = String::new()));
        assert!(!with(&|r| r.back[0].rotation = Some("sideways".to_owned())));
        assert!(!with(&|r| r.back[0].rate = Some(f64::NAN)));
        assert!(!with(&|r| r.back[0].rate = Some(0.0)));
        assert!(!with(&|r| r.back[0].x = -1));
        assert!(!with(&|r| r.back[0].y = 40000));
        assert!(!with(&|r| r.off.push("HDMI-1".to_owned())));
        assert!(!with(&|r| r.back.clear()));
        assert!(!with(&|r| {
            let mut second = r.back[0].clone();
            second.name = "DP-1".to_owned();
            r.back.push(second);
        }));
        assert!(!with(&|r| r.off = (0..20).map(|i| format!("Virtual-1-{}", i)).collect()));
        assert!(with(&|r| r.back[0].rotation = Some("left".to_owned())));
        assert!(with(&|r| r.back[0].mode = "2560x1440_60.00_rustdesk".to_owned()));
    }

    #[test]
    fn garbage_is_answered_and_a_flood_ends_the_conversation() {
        let mut output = Vec::new();
        serve(
            Cursor::new(b"{\"op\":\"format_disk\"}\n".to_vec()),
            &mut output,
            |_| panic!("not a request"),
        );
        let a: Response = serde_json::from_slice(&output[..output.len() - 1]).unwrap();
        assert!(a.error.starts_with("bad request"));
        // A line past the limit is not read on.
        let flood = vec![b'x'; MAX_LINE as usize * 2];
        let mut output = Vec::new();
        serve(Cursor::new(flood), &mut output, |_| panic!("not a request"));
        assert!(output.is_empty());
    }
}
