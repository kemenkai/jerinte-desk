//! What the service is connected to on our servers, for the status bar: the ID server it registers
//! with, and the API's last heartbeat. The registration loop and the heartbeat say what they do
//! here, one call where each thing happens; the UI asks for a snapshot every second
//! (`Data::Config((IPC_NAME, None))`), which a service from before this answers with `None`.
//!
//! Only memory: nothing written to the configuration, no lookup or connection made. Addresses go
//! out as host and port, never with a URL's path, which may be a prefix the servers' proxy keeps
//! unlisted.

use hbb_common::{
    config::{Config, ServerEntry, RENDEZVOUS_PORT},
    socket_client::check_port,
    websocket::{check_ws, is_ws_endpoint},
};
use serde_json::{json, Value};
use std::{
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

/// The name the UI asks the service for the snapshot by.
pub const IPC_NAME: &str = "service-status";

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Stopped,
    Picking,
    Connecting,
    Registered,
    Waiting,
}

/// The ID server a round registers with, as it is dialed.
#[derive(Clone, Debug, PartialEq)]
struct Target {
    via: &'static str,
    host: String,
    port: u16,
    proxy: Option<String>,
    entry: Option<String>,
}

#[derive(Debug)]
struct Failure {
    target: Option<Target>,
    text: String,
    at: Instant,
}

#[derive(Debug)]
struct Heartbeat {
    url: String,
    error: Option<String>,
    at: Instant,
}

#[derive(Debug, Default)]
struct Record {
    state: Option<State>,
    /// A round registering with several servers, the public ones' fallback list, goes unrecorded:
    /// the online state it would be told by is the best of them all.
    several: bool,
    target: Option<Target>,
    registered_at: Option<Instant>,
    retry_at: Option<Instant>,
    /// The last error a round ended with, until a registration succeeds.
    failure: Option<Failure>,
    /// The round ended after a registration: the wait after it is for a connection that dropped,
    /// not for a server that could not be reached.
    dropped: bool,
    heartbeat: Option<Heartbeat>,
}

impl Record {
    fn round(&mut self, picking: bool) {
        self.state = Some(if picking {
            State::Picking
        } else {
            State::Connecting
        });
        self.several = false;
        self.target = None;
        self.registered_at = None;
        self.retry_at = None;
        self.dropped = false;
    }

    fn stopped(&mut self) {
        *self = Record {
            state: Some(State::Stopped),
            ..Default::default()
        };
    }

    fn connecting(&mut self, target: Option<Target>) {
        if self.several {
            return;
        }
        self.state = Some(State::Connecting);
        self.target = target;
        self.registered_at = None;
    }

    /// Over UDP every answer registers again; the first one is when.
    fn registered(&mut self, now: Instant) {
        if self.several || self.state != Some(State::Connecting) {
            return;
        }
        self.state = Some(State::Registered);
        self.registered_at = Some(now);
        self.failure = None;
    }

    fn ended(&mut self, error: Option<&str>, now: Instant) {
        if self.several || self.state == Some(State::Stopped) {
            return;
        }
        self.state = Some(State::Connecting);
        self.dropped = self.registered_at.take().is_some();
        if let Some(error) = error {
            self.failure = Some(Failure {
                target: self.target.clone(),
                text: without_paths(error),
                at: now,
            });
        }
    }

    fn waiting(&mut self, wait: Duration, now: Instant) {
        self.state = Some(State::Waiting);
        self.retry_at = Some(now + wait);
    }

    fn heartbeat(&mut self, url: &str, error: Option<&str>, now: Instant) {
        self.heartbeat = Some(Heartbeat {
            url: url.to_owned(),
            error: error.map(without_paths),
            at: now,
        });
    }

    /// `online` is `get_online_state()`, which is this round's single server's: registered once
    /// the server took the registration and it still answers. `heartbeat_url` is the one the
    /// heartbeat posts to now, a record for another or none at all being no record.
    fn snapshot(&self, online: i64, heartbeat_url: &str, now: Instant) -> Option<Value> {
        let state = self.state?;
        if self.several {
            return None;
        }
        let name = match state {
            State::Stopped => return Some(json!({ "state": "stopped" })),
            State::Picking => "picking",
            State::Waiting => "waiting",
            // UDP that goes unanswered never ends the round: it says so with -1.
            State::Registered | State::Connecting if online < 0 => "unanswered",
            State::Registered if online > 0 => "registered",
            State::Registered | State::Connecting => "connecting",
        };
        let mut v = json!({ "state": name });
        if let Some(target) = &self.target {
            let mut id = target.json();
            if name == "registered" {
                id["for_s"] = json!(since(self.registered_at, now));
                id["rtt_us"] = json!(online);
                id["rtt_kind"] = json!(if target.via == "udp" {
                    "average"
                } else {
                    "at_register"
                });
            }
            v["id"] = id;
        }
        if let (State::Waiting, Some(at)) = (state, self.retry_at) {
            let left = at.saturating_duration_since(now);
            v["retry_in_s"] = json!((left.as_millis() as u64 + 999) / 1000);
            if self.dropped {
                v["dropped"] = json!(true);
            }
        }
        if let Some(failure) = &self.failure {
            v["error"] = json!({
                "text": failure.text,
                "ago_s": since(Some(failure.at), now),
                "target": failure.target.as_ref().map(Target::json),
            });
        }
        let heartbeat = self
            .heartbeat
            .as_ref()
            .filter(|h| !heartbeat_url.is_empty() && h.url == heartbeat_url);
        if let Some(h) = heartbeat {
            if let Some((host, port)) = url_host_port(&h.url) {
                v["api"] = json!({
                    "host": host,
                    "port": port,
                    "ok": h.error.is_none(),
                    "error": h.error,
                    "ago_s": since(Some(h.at), now),
                });
            }
        }
        Some(v)
    }
}

impl Target {
    fn json(&self) -> Value {
        json!({
            "via": self.via,
            "host": self.host,
            "port": self.port,
            "proxy": self.proxy,
            "entry": self.entry,
        })
    }
}

fn since(at: Option<Instant>, now: Instant) -> u64 {
    at.map_or(0, |at| now.saturating_duration_since(at).as_secs())
}

lazy_static::lazy_static! {
    static ref RECORD: Mutex<Record> = Default::default();
    static ref URL: hbb_common::regex::Regex = hbb_common::regex::Regex::new(r#"[A-Za-z][A-Za-z0-9+.-]*://[^\s"'<>(){}]+"#).unwrap();
}

fn record() -> MutexGuard<'static, Record> {
    RECORD.lock().unwrap_or_else(|e| e.into_inner())
}

/// A round of the registration loop begins, picking the entry first when there are entries.
pub fn round(picking: bool) {
    record().round(picking);
}

/// The servers the round registers with, `Self::start` for each.
pub fn servers(count: usize) {
    record().several = count > 1;
}

/// The service is stopped: nothing is connected, the API not either.
pub fn stopped() {
    record().stopped();
}

/// `start_udp` or `start_tcp` begins for `host`, its port added.
pub fn connecting(host: &str, udp: bool) {
    let (dial, proxy) = if udp {
        (host.to_owned(), None)
    } else {
        (check_ws(host), Config::get_socks().map(|s| s.proxy))
    };
    let entry = Config::get_server_entry();
    record().connecting(target(host, udp, &dial, proxy.as_deref(), entry.as_ref()));
}

/// The ID server took the registration.
pub fn registered() {
    record().registered(Instant::now());
}

/// `Self::start` returned: with an error, a failure; `Ok`, a restart or another entry's turn.
pub fn ended(error: Option<&str>) {
    record().ended(error, Instant::now());
}

/// The loop backs off for `wait` before the next round.
pub fn waiting(wait: Duration) {
    record().waiting(wait, Instant::now());
}

/// A heartbeat to `url` came back, `error` saying why it failed.
pub fn heartbeat(url: &str, error: Option<&str>) {
    record().heartbeat(url, error, Instant::now());
}

/// The snapshot as JSON: none before the first round nor for a round with several servers, the UI
/// then showing what upstream does.
pub fn snapshot() -> Option<String> {
    let online = hbb_common::config::get_online_state();
    let url = crate::hbbs_http::sync::heartbeat_url();
    record()
        .snapshot(online, &url, Instant::now())
        .map(|v| v.to_string())
}

/// What a round dials for `host`: over ws the URL `check_ws` makes of it, `dial`, as
/// `connect_tcp` does, of which only the host and port; the name of the entry `host` is the ID
/// server of, both with their port added first. A proxy only on the TCP path, ws included.
fn target(
    host: &str,
    udp: bool,
    dial: &str,
    proxy: Option<&str>,
    entry: Option<&ServerEntry>,
) -> Option<Target> {
    let (via, host_port) = if udp {
        ("udp", host.to_owned())
    } else if is_ws_endpoint(dial) {
        let via = if dial.starts_with("wss://") { "wss" } else { "ws" };
        (via, crate::common::ws_url_host_port(dial).ok()?)
    } else {
        ("tcp", dial.to_owned())
    };
    let (name, port) = host_port.rsplit_once(':')?;
    Some(Target {
        via,
        host: name.to_owned(),
        port: port.parse().ok()?,
        proxy: proxy.filter(|_| !udp).and_then(proxy_host_port),
        entry: entry
            .filter(|e| check_port(&e.id, RENDEZVOUS_PORT) == check_port(host, RENDEZVOUS_PORT))
            .map(|e| e.name.clone()),
    })
}

/// A proxy's address without its scheme, credentials or path.
fn proxy_host_port(proxy: &str) -> Option<String> {
    let rest = proxy.split_once("://").map_or(proxy, |(_, rest)| rest);
    let rest = rest.split('/').next().unwrap_or_default();
    let rest = rest.rsplit_once('@').map_or(rest, |(_, rest)| rest).trim();
    (!rest.is_empty()).then(|| rest.to_owned())
}

fn url_host_port(url: &str) -> Option<(String, u16)> {
    let url = url::Url::parse(url).ok()?;
    Some((url.host_str()?.to_owned(), url.port_or_known_default()?))
}

/// An error's text with every URL in it cut to its scheme, host and port.
fn without_paths(text: &str) -> String {
    URL.replace_all(text, |c: &hbb_common::regex::Captures| {
        let url = &c[0];
        match url_host_port(url) {
            Some((host, port)) => {
                let scheme = url.split_once("://").map_or("", |(scheme, _)| scheme);
                format!("{scheme}://{host}:{port}")
            }
            None => "<url>".to_owned(),
        }
    })
    .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAN: &str = "192.168.1.10:21116";
    const WAN: &str = "wss://app.example.com:4443/p1/id";
    const API: &str = "https://app.example.com:4443/p2/api/heartbeat";

    fn entry(name: &str, id: &str) -> ServerEntry {
        ServerEntry {
            name: name.to_owned(),
            id: id.to_owned(),
            relay: "".to_owned(),
        }
    }

    fn tcp(host: &str) -> Option<Target> {
        target(host, false, host, None, None)
    }

    fn state(r: &Record, online: i64, now: Instant) -> Value {
        r.snapshot(online, "", now).unwrap()["state"].clone()
    }

    #[test]
    fn a_round_goes_from_picking_to_registered_and_back_when_it_ends() {
        let now = Instant::now();
        let mut r = Record::default();
        assert_eq!(r.snapshot(0, "", now), None);
        r.round(true);
        assert_eq!(state(&r, 0, now), "picking");
        r.connecting(tcp(LAN));
        assert_eq!(state(&r, 0, now), "connecting");
        r.registered(now);
        // Registered, but the latency not written yet: not ready as upstream says.
        assert_eq!(state(&r, 0, now), "connecting");
        let s = r.snapshot(812, "", now + Duration::from_secs(194)).unwrap();
        assert_eq!(s["state"], "registered");
        assert_eq!(s["id"]["for_s"], 194);
        assert_eq!(s["id"]["rtt_us"], 812);
        assert_eq!(s["id"]["rtt_kind"], "at_register");
        // A connection that drops does not stay registered.
        r.ended(Some("Rendezvous connection is reset by the peer"), now);
        assert_eq!(state(&r, 812, now), "connecting");
        assert_eq!(
            r.snapshot(0, "", now).unwrap()["error"]["text"],
            "Rendezvous connection is reset by the peer"
        );
    }

    #[test]
    fn a_failure_shows_while_waiting_and_until_a_registration() {
        let now = Instant::now();
        let mut r = Record::default();
        r.round(false);
        r.connecting(tcp(LAN));
        r.ended(Some("Connection refused (os error 61)"), now);
        r.waiting(Duration::from_secs(20), now);
        let s = r.snapshot(0, "", now + Duration::from_millis(2500)).unwrap();
        assert_eq!(s["state"], "waiting");
        assert_eq!(s["retry_in_s"], 18);
        assert_eq!(s["id"]["port"], 21116);
        assert_eq!(s["error"]["ago_s"], 2);
        assert_eq!(s["error"]["target"]["host"], "192.168.1.10");
        // Woken early, the next round begins: no wait left over, the failure still told.
        r.round(true);
        let s = r.snapshot(0, "", now).unwrap();
        assert_eq!(s["state"], "picking");
        assert!(s.get("retry_in_s").is_none() && s.get("id").is_none());
        assert_eq!(s["error"]["text"], "Connection refused (os error 61)");
        r.connecting(tcp(WAN));
        r.registered(now);
        assert!(r.snapshot(1, "", now).unwrap().get("error").is_none());
        // Registered, then dropped: the wait is not for a server that cannot be reached.
        r.ended(Some("Rendezvous connection is reset by the peer"), now);
        r.waiting(Duration::from_secs(10), now);
        assert_eq!(r.snapshot(0, "", now).unwrap()["dropped"], true);
        r.round(false);
        r.connecting(tcp(WAN));
        r.ended(Some("refused"), now);
        r.waiting(Duration::from_secs(10), now);
        assert!(r.snapshot(0, "", now).unwrap().get("dropped").is_none());
    }

    #[test]
    fn ending_well_is_no_failure() {
        let now = Instant::now();
        let mut r = Record::default();
        r.round(false);
        r.connecting(tcp(LAN));
        r.registered(now);
        r.ended(None, now);
        let s = r.snapshot(1, "", now).unwrap();
        assert_eq!(s["state"], "connecting");
        assert!(s.get("error").is_none());
    }

    #[test]
    fn udp_registers_once_and_goes_unanswered() {
        let now = Instant::now();
        let mut r = Record::default();
        r.round(false);
        r.connecting(target(LAN, true, LAN, Some("1.2.3.4:1080"), None));
        // Never answered: the round goes on, saying -1.
        assert_eq!(state(&r, -1, now), "unanswered");
        r.registered(now);
        r.registered(now + Duration::from_secs(30));
        let s = r.snapshot(900, "", now + Duration::from_secs(60)).unwrap();
        assert_eq!(s["id"]["for_s"], 60);
        assert_eq!(s["id"]["rtt_kind"], "average");
        assert_eq!(s["id"]["via"], "udp");
        assert_eq!(s["id"]["proxy"], Value::Null);
        assert_eq!(state(&r, 0, now), "connecting");
        assert_eq!(state(&r, -1, now), "unanswered");
    }

    #[test]
    fn several_servers_and_the_service_stopped() {
        let now = Instant::now();
        let mut r = Record::default();
        r.heartbeat(API, None, now);
        r.round(false);
        r.several = true;
        r.connecting(tcp(LAN));
        r.registered(now);
        r.ended(Some("refused"), now);
        assert_eq!(r.snapshot(1, API, now), None);
        r.round(false);
        assert!(r.failure.is_none());
        r.stopped();
        assert_eq!(r.snapshot(1, API, now), Some(json!({ "state": "stopped" })));
        // A heartbeat in flight while stopping does not show either.
        r.heartbeat(API, None, now);
        r.ended(Some("closed"), now);
        assert_eq!(r.snapshot(1, API, now), Some(json!({ "state": "stopped" })));
    }

    #[test]
    fn a_heartbeat_shows_for_the_url_it_went_to_only() {
        let now = Instant::now();
        let mut r = Record::default();
        r.round(false);
        r.heartbeat(
            API,
            Some("error sending request for url (https://app.example.com:4443/p2/api/heartbeat)"),
            now,
        );
        let s = r.snapshot(0, API, now + Duration::from_secs(12)).unwrap();
        assert_eq!(
            s["api"],
            json!({
                "host": "app.example.com", "port": 4443, "ok": false, "ago_s": 12,
                "error": "error sending request for url (https://app.example.com:4443)",
            })
        );
        assert!(r.snapshot(0, "", now).unwrap().get("api").is_none());
        let other = "https://api.example.org/api/heartbeat";
        assert!(r.snapshot(0, other, now).unwrap().get("api").is_none());
        r.heartbeat(other, None, now);
        assert_eq!(r.snapshot(0, other, now).unwrap()["api"]["port"], 443);
    }

    #[test]
    fn what_is_dialed_shows_as_host_and_port() {
        let wan = target(WAN, false, WAN, Some("socks5://u:p@10.0.0.1:1080"), None).unwrap();
        assert_eq!(
            (wan.via, wan.host.as_str(), wan.port, wan.proxy.as_deref()),
            ("wss", "app.example.com", 4443, Some("10.0.0.1:1080"))
        );
        // Only ws on: the address check_ws makes of the ID server is dialed.
        let ws = target(LAN, false, "ws://192.168.1.10:21118", None, None).unwrap();
        assert_eq!((ws.via, ws.port), ("ws", 21118));
        let tls = target(
            "id.example.com:21116",
            false,
            "wss://id.example.com/ws/id",
            None,
            None,
        )
        .unwrap();
        assert_eq!((tls.via, tls.host.as_str(), tls.port), ("wss", "id.example.com", 443));
        let v6 = target("[fd00::1]:21116", true, "[fd00::1]:21116", None, None).unwrap();
        assert_eq!((v6.host.as_str(), v6.port), ("[fd00::1]", 21116));
        assert_eq!(target("wss:///id", false, "wss:///id", None, None), None);
    }

    #[test]
    fn the_entry_is_named_by_its_id_server_with_the_port_added() {
        let lan = entry("lan", "192.168.1.10");
        let wan = entry("wan", WAN);
        let at = |host: &str, e: &ServerEntry| target(host, false, host, None, Some(e)).unwrap().entry;
        assert_eq!(at(LAN, &lan).as_deref(), Some("lan"));
        assert_eq!(at(WAN, &wan).as_deref(), Some("wan"));
        assert_eq!(at(LAN, &wan), None);
    }

    #[test]
    fn urls_lose_their_paths() {
        assert_eq!(
            without_paths("URL error: wss://app.example.com/p1/id refused, ws://[fd00::1]:21118/x."),
            "URL error: wss://app.example.com:443 refused, ws://[fd00::1]:21118"
        );
        assert_eq!(without_paths("deadline has elapsed"), "deadline has elapsed");
        assert_eq!(proxy_host_port("1.2.3.4:1080").as_deref(), Some("1.2.3.4:1080"));
        assert_eq!(proxy_host_port("http://p.example.com:8080/").as_deref(), Some("p.example.com:8080"));
        assert_eq!(proxy_host_port(""), None);
    }
}
