//! Several ways to the same servers (`server-entries`): the intranet's native ports, the public
//! WebSocket entry and the like. The service registers through one at a time, the first in order
//! that answers, else the quickest. It keeps to it until registering through it fails or the
//! network changes, looks again now and then for one earlier in order, and backs off while none
//! answers. The UI takes up the service's pick (`Config::follow_server_entry`).

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant, SystemTime},
};

use hbb_common::{
    anyhow::{anyhow, bail},
    config::{self, Config, ServerEntry, RENDEZVOUS_PORT},
    futures::{stream::FuturesUnordered, FutureExt, StreamExt},
    log,
    protobuf::Message as _,
    rand::{self, Rng},
    rendezvous_proto::*,
    socket_client,
    sodiumoxide::crypto::sign,
    tokio::{
        self,
        sync::Notify,
        time::{sleep, timeout},
    },
    ResultType,
};

/// How long an entry on the local network may take to answer, and one farther away.
const NEAR_TIMEOUT: Duration = Duration::from_millis(1_500);
const FAR_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an entry that registering through failed stays out of the picking.
const COOL_DOWN: Duration = Duration::from_secs(60);
/// A round of registration that lasted this long got somewhere: the next starts at once.
pub const UP_FOR: Duration = Duration::from_secs(60);
/// The waits between rounds that did not, the last repeated; each within a fifth either way.
const BACKOFF: [u64; 5] = [5, 10, 20, 40, 60];
const WATCH_EVERY: Duration = Duration::from_secs(5);
/// After the network changes, how long to let it settle before picking, and before trying once
/// more for the first entry (DHCP, 802.1X and VPNs take a few seconds).
const SETTLE: Duration = Duration::from_secs(2);
const ONCE_MORE: Duration = Duration::from_secs(8);
/// Picking again for the network is done at most this often, so a flapping network cannot keep
/// resetting the backoff.
const REPICK_AT_MOST_EVERY: Duration = Duration::from_secs(60);
/// While an entry after the first is in use, how often those before it are probed, and how many
/// probes in a row one of them must answer to be gone back to.
const UPGRADE_EVERY: Duration = Duration::from_secs(300);
const UPGRADE_PASSES: u32 = 2;
/// Wall time running ahead of the monotonic clock by this much between two checks means the
/// machine slept: the monotonic clock stops then on macOS and Linux.
const SLEPT: Duration = Duration::from_secs(10);

lazy_static::lazy_static! {
    // The ID servers of entries that registering through failed, and when.
    static ref FAILED: Mutex<HashMap<String, Instant>> = Default::default();
    // Ends a wait between rounds of registration early.
    static ref WAKE: Notify = Notify::new();
}
// Set when the wait was ended for a new network: the backoff starts over.
static FRESH: AtomicBool = AtomicBool::new(false);

/// Whether the build or the user set entries to pick from.
pub fn active() -> bool {
    !Config::get_server_entries().is_empty()
}

/// Picks the entry to register through next and sets it as the one in use: the first in order
/// that answers, else the quickest. Entries that failed lately are tried only when no other
/// answers. `None` while none answers.
pub async fn pick() -> Option<ServerEntry> {
    let entries = Config::get_server_entries();
    if let Some(in_use) = Config::get_server_entry() {
        // Taken off the list meanwhile: no longer to be used, picked or not.
        if !entries.contains(&in_use) {
            Config::set_server_entry(None);
        }
    }
    let now = Instant::now();
    let fresh = candidates(&entries, &FAILED.lock().unwrap(), now);
    let cooling: Vec<ServerEntry> = entries
        .iter()
        .filter(|e| !fresh.contains(e))
        .cloned()
        .collect();
    let key = probe_key().await;
    let mut report = Vec::new();
    let mut picked = first_or_quickest(&fresh, key.as_deref(), &mut report).await;
    if picked.is_none() && !cooling.is_empty() {
        // Nothing else answers: one that failed lately is better than none.
        picked = first_or_quickest(&cooling, key.as_deref(), &mut report).await;
    }
    log::info!(
        "[ENTRY] picked={} {} (cooling down: {})",
        picked.as_ref().map_or("none", |e| e.name.as_str()),
        report.join(" "),
        cooling.len()
    );
    if let Some(entry) = &picked {
        use_entry(entry);
    }
    picked
}

/// The entry to register through when none answered its probe in time: the one in use, else the
/// last listed. Registering waits the usual connect timeout, which a slow network may still meet.
pub fn fallback() -> Option<ServerEntry> {
    let entry = Config::get_server_entry().or_else(|| Config::get_server_entries().pop())?;
    use_entry(&entry);
    Some(entry)
}

/// Sets `entry` as the one in use.
fn use_entry(entry: &ServerEntry) {
    if Config::get_server_entry().as_ref() == Some(entry) {
        return;
    }
    Config::set_server_entry(Some(entry.clone()));
    // The NAT seen through it is another one, and none over WebSocket. Unknown until tested, or
    // the last entry's type would stand in for it and end the test early.
    if !entry.is_ws() {
        Config::set_nat_type(NatType::UNKNOWN_NAT as _);
    }
    crate::test_nat_type();
}

/// Probes `entries` together and settles on the first if it answers, else on the quickest;
/// adds how each went to `report`.
async fn first_or_quickest(
    entries: &[ServerEntry],
    key: Option<&str>,
    report: &mut Vec<String>,
) -> Option<ServerEntry> {
    let mut probes: FuturesUnordered<_> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| async move { (i, probe(entry, key).await) })
        .collect();
    let mut chooser = Chooser::default();
    let mut outcomes = vec![String::new(); entries.len()];
    let mut chosen = None;
    while let Some((i, result)) = probes.next().await {
        outcomes[i] = match &result {
            Ok(took) => format!("{}ms", took.as_millis()),
            Err(err) => err.to_string(),
        };
        if let Some(c) = chooser.take(i, result.is_ok()) {
            chosen = Some(c);
            break;
        }
    }
    let chosen = chosen.or_else(|| chooser.end());
    report.extend(
        entries
            .iter()
            .zip(&outcomes)
            .map(|(e, o)| format!("{}={}", e.name, if o.is_empty() { "-" } else { o })),
    );
    chosen.map(|i| entries[i].clone())
}

/// What follows a round of registration.
#[derive(Debug, PartialEq)]
pub enum Next {
    /// Restarted: at once, the backoff starting over.
    Restart,
    /// The service is stopped: look again after a while.
    Stopped,
    /// It was registered and up a while: at once, backing off only if that fails too.
    Again,
    /// It never registered, or did not stay up: after the backoff.
    BackOff,
}

/// `registered`: the server accepted the registration during the round. A round that only kept
/// a connection open, to a server that never answered it, did not get anywhere however long.
pub fn after_round(restarted: bool, stopped: bool, registered: bool, lasted: Duration) -> Next {
    if restarted {
        Next::Restart
    } else if stopped {
        Next::Stopped
    } else if registered && lasted >= UP_FOR {
        Next::Again
    } else {
        Next::BackOff
    }
}

/// Whether a failed round counts against its entry: one that never registered, and not for a
/// restart. One that registered and then dropped may well work again at once.
pub fn held_against(failed: bool, restarted: bool, registered: bool) -> bool {
    failed && !restarted && !registered
}

/// Registering through `entry` failed soon: leave it out of the picking for a while.
pub fn failed(entry: &ServerEntry) {
    FAILED
        .lock()
        .unwrap()
        .insert(entry.id.clone(), Instant::now());
}

/// Waits `wait` between rounds of registration, or less when the network changes or the
/// registration restarts. Says whether the backoff starts over.
pub async fn wait(wait: Duration) -> bool {
    tokio::select! {
        _ = sleep(wait) => {}
        _ = WAKE.notified() => {}
    }
    FRESH.swap(false, Ordering::SeqCst)
}

/// Ends a wait between rounds, as the restarted registration should start at once.
pub fn wake() {
    WAKE.notify_one();
}

/// A restart was dealt with without waiting: what it left for a wait is spent, so that it does
/// not end a later wait at once or start that one's backoff over.
pub fn woken() {
    FRESH.store(false, Ordering::SeqCst);
    // The permit `notify_one` keeps when nobody waits.
    let _ = WAKE.notified().now_or_never();
}

/// Restarts registration for a new network: the backoff starts over, and what failed on the old
/// network may work on this one.
fn repick() {
    FAILED.lock().unwrap().clear();
    FRESH.store(true, Ordering::SeqCst);
    crate::RendezvousMediator::restart();
}

/// Watches for what should make the service register again: the way out to the servers changing
/// (another network, a VPN or a TUN going up or down) or the machine waking from sleep. With
/// entries, also an entry earlier in order than the one in use answering again; without them,
/// the single ID server is watched, so that registration need not wait out its backoff.
pub async fn watch() {
    let mut routes = Routes::default();
    let mut addrs: HashMap<String, SocketAddr> = HashMap::new();
    let mut wall = SystemTime::now();
    let mut mono = Instant::now();
    let mut last_repick: Option<Instant> = None;
    let mut pending = false;
    let mut once_more_at: Option<Instant> = None;
    let mut next_upgrade = Instant::now() + UPGRADE_EVERY;
    let mut passes = 0;
    loop {
        sleep(WATCH_EVERY).await;
        let entries = Config::get_server_entries();
        let ids: Vec<String> = if entries.is_empty() {
            vec![Config::get_rendezvous_server()]
        } else {
            entries.iter().map(|e| e.id.clone()).collect()
        };
        let now = Instant::now();
        let slept = slept(
            wall.elapsed().unwrap_or_default(),
            now.duration_since(mono),
        );
        (wall, mono) = (SystemTime::now(), now);
        let mut seen = Vec::with_capacity(ids.len());
        for id in &ids {
            let addr = resolve(id, &mut addrs).await;
            seen.push(addr.and_then(crate::common::local_ip_towards));
        }
        if routes.see(seen) || slept {
            log::info!(
                "[ENTRY] {}; registering again",
                if slept { "woke from sleep" } else { "the way out changed" }
            );
            pending = true;
            // Names may resolve otherwise now, a fake-IP DNS gone or come.
            addrs.clear();
        }
        if pending && last_repick.map_or(true, |t| t.elapsed() >= REPICK_AT_MOST_EVERY) {
            pending = false;
            sleep(SETTLE).await;
            last_repick = Some(Instant::now());
            once_more_at = (!entries.is_empty()).then(|| Instant::now() + ONCE_MORE);
            repick();
            continue;
        }
        if entries.is_empty() {
            continue;
        }
        if once_more_at.map_or(false, |t| Instant::now() >= t) {
            once_more_at = None;
            // Once more for an earlier entry slow to come up, but picking again only if one
            // answers: a later one that works by now is not to be dropped for nothing.
            let again = match earlier_than_in_use(&entries) {
                // None in use: none answered then, so try them all again.
                None => true,
                Some(earlier) => any_answers(&earlier).await,
            };
            if again {
                repick();
            }
        }
        if Instant::now() >= next_upgrade {
            next_upgrade = Instant::now() + UPGRADE_EVERY;
            let answered = match earlier_than_in_use(&entries) {
                Some(earlier) => any_answers(&earlier).await,
                None => false,
            };
            passes = if answered { passes + 1 } else { 0 };
            if passes >= UPGRADE_PASSES {
                passes = 0;
                log::info!("[ENTRY] an earlier entry answers again; picking again");
                crate::RendezvousMediator::restart();
            }
        }
    }
}

/// Whether any of `entries` answers as the ID server.
async fn any_answers(entries: &[ServerEntry]) -> bool {
    if entries.is_empty() {
        return false;
    }
    let key = probe_key().await;
    for entry in entries {
        if probe(entry, key.as_deref()).await.is_ok() {
            return true;
        }
    }
    false
}

/// The key to tell the ID server by: `None` when none is configured, the built-in public one
/// standing in, which our own server would not sign with.
async fn probe_key() -> Option<String> {
    let key = crate::get_key(true).await;
    (key != config::RS_PUB_KEY || !Config::get_option("key").is_empty()).then_some(key)
}

/// Whether the ID server answers through `entry` as itself, and how long that took. With a key,
/// its first message on a stream must be its key exchange signed with that key, which a TUN
/// proxy answering every connection on its own cannot produce. Without one, being reached is
/// all there is to go by.
async fn probe(entry: &ServerEntry, key: Option<&str>) -> ResultType<Duration> {
    let started = Instant::now();
    let budget = probe_timeout(&entry.id);
    // Over the whole of it: a WebSocket connection may try one TLS stack after another.
    match timeout(budget, answer(entry, key, budget)).await {
        Ok(result) => result.map(|()| started.elapsed()),
        Err(_) => bail!("no answer"),
    }
}

async fn answer(entry: &ServerEntry, key: Option<&str>, budget: Duration) -> ResultType<()> {
    let ms = budget.as_millis() as u64;
    let mut stream = if entry.is_ws() {
        // A WebSocket URL is dialled as one whatever `use_ws` says.
        socket_client::connect_tcp(entry.id.as_str(), ms).await?
    } else {
        // Not `connect_tcp`, which under a WebSocket entry would make a WebSocket URL of this.
        let target = socket_client::check_port(&entry.id, RENDEZVOUS_PORT);
        socket_client::connect_tcp_local(target.as_str(), None, ms).await?
    };
    let Some(key) = key else {
        return Ok(());
    };
    let bytes = match stream.next().await {
        Some(Ok(bytes)) => bytes,
        Some(Err(err)) => return Err(err.into()),
        None => bail!("closed"),
    };
    let Some(rendezvous_message::Union::KeyExchange(ex)) =
        RendezvousMessage::parse_from_bytes(&bytes)?.union
    else {
        bail!("not the ID server's key exchange");
    };
    let pk = crate::common::get_rs_pk(key).ok_or_else(|| anyhow!("invalid key"))?;
    if ex.keys.len() != 1 || sign::verify(&ex.keys[0], &pk).is_err() {
        bail!("not signed with the ID server's key");
    }
    Ok(())
}

/// An ID server's socket address, looked up again only when not known yet.
async fn resolve(id: &str, known: &mut HashMap<String, SocketAddr>) -> Option<SocketAddr> {
    if let Some(addr) = known.get(id) {
        return Some(*addr);
    }
    let (host, port) = host_port(id)?;
    // A resolver stuck without a network must not hold the watch up.
    let addr = timeout(FAR_TIMEOUT, tokio::net::lookup_host((host.as_str(), port)))
        .await
        .ok()?
        .ok()?
        .next()?;
    known.insert(id.to_owned(), addr);
    Some(addr)
}

fn earlier_than_in_use(entries: &[ServerEntry]) -> Option<Vec<ServerEntry>> {
    earlier(
        entries,
        Config::get_server_entry().as_ref(),
        &FAILED.lock().unwrap(),
        Instant::now(),
    )
}

/// The entries before `in_use` that are not cooling down; `None` with none in use, or one not
/// among them.
fn earlier(
    entries: &[ServerEntry],
    in_use: Option<&ServerEntry>,
    failed: &HashMap<String, Instant>,
    now: Instant,
) -> Option<Vec<ServerEntry>> {
    let at = entries.iter().position(|e| Some(e) == in_use)?;
    Some(candidates(&entries[..at], failed, now))
}

/// The entries, in order, that registering through has not failed for in the last while.
fn candidates(
    entries: &[ServerEntry],
    failed: &HashMap<String, Instant>,
    now: Instant,
) -> Vec<ServerEntry> {
    entries
        .iter()
        .filter(|e| {
            failed
                .get(&e.id)
                .map_or(true, |at| now.duration_since(*at) >= COOL_DOWN)
        })
        .cloned()
        .collect()
}

/// Settles which candidate to use as the probes end, in the order they end: the first
/// candidate if it answers, else whichever answers first.
#[derive(Default)]
struct Chooser {
    first_failed: bool,
    quickest: Option<usize>,
}

impl Chooser {
    /// Takes the end of candidate `i`'s probe; the one to use once that is settled.
    fn take(&mut self, i: usize, answered: bool) -> Option<usize> {
        match (i, answered) {
            (0, true) => Some(0),
            (0, false) => {
                self.first_failed = true;
                self.quickest
            }
            (i, true) if self.first_failed => Some(i),
            (i, true) => {
                self.quickest.get_or_insert(i);
                None
            }
            _ => None,
        }
    }

    /// Once every probe has ended.
    fn end(self) -> Option<usize> {
        self.quickest
    }
}

/// The waits between rounds of registration that did not get far.
#[derive(Default)]
pub struct Backoff {
    failures: usize,
}

impl Backoff {
    pub fn next(&mut self) -> Duration {
        let base = backoff_base(self.failures);
        self.failures += 1;
        base.mul_f64(1.0 + rand::thread_rng().gen_range(-0.2..=0.2))
    }

    pub fn reset(&mut self) {
        self.failures = 0;
    }
}

fn backoff_base(failures: usize) -> Duration {
    Duration::from_secs(BACKOFF[failures.min(BACKOFF.len() - 1)])
}

/// The ways out to the entries last settled on, and others seen once since.
#[derive(Default)]
struct Routes {
    settled: Option<Vec<Option<IpAddr>>>,
    seen: Option<Vec<Option<IpAddr>>>,
}

impl Routes {
    /// Takes the ways out seen now. True once they differ from the settled ones two checks in a
    /// row, so that one odd check is not taken for a new network.
    fn see(&mut self, now: Vec<Option<IpAddr>>) -> bool {
        let Some(settled) = &self.settled else {
            self.settled = Some(now);
            return false;
        };
        if *settled == now {
            self.seen = None;
            return false;
        }
        if self.seen.as_ref() == Some(&now) {
            self.settled = Some(now);
            self.seen = None;
            return true;
        }
        self.seen = Some(now);
        false
    }
}

fn slept(wall: Duration, mono: Duration) -> bool {
    wall > mono + SLEPT
}

/// How long an entry may take to answer: shorter on the local network.
fn probe_timeout(id: &str) -> Duration {
    let near = match host_port(id).and_then(|(host, _)| host.parse::<IpAddr>().ok()) {
        Some(IpAddr::V4(ip)) => ip.is_private() || ip.is_loopback() || ip.is_link_local(),
        Some(IpAddr::V6(ip)) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00,
        None => false,
    };
    if near {
        NEAR_TIMEOUT
    } else {
        FAR_TIMEOUT
    }
}

/// The brackets around an IPv6 address. A function, not a char array: the toolchain of the 32-bit
/// Windows 7 build cannot trim with an array pattern from both ends.
fn is_bracket(c: char) -> bool {
    c == '[' || c == ']'
}

/// The host and port of an ID server given as `host:port` or as a WebSocket URL.
fn host_port(id: &str) -> Option<(String, u16)> {
    if hbb_common::websocket::is_ws_endpoint(id) {
        let url = url::Url::parse(id).ok()?;
        let host = url.host_str()?.trim_matches(is_bracket).to_owned();
        return Some((host, url.port_or_known_default()?));
    }
    let (host, port) =
        socket_client::split_host_port(socket_client::check_port(id, RENDEZVOUS_PORT))?;
    Some((
        host.trim_matches(is_bracket).to_owned(),
        u16::try_from(port).ok()?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, id: &str) -> ServerEntry {
        ServerEntry {
            name: name.to_owned(),
            id: id.to_owned(),
            relay: String::new(),
        }
    }

    fn choose(ends: &[(usize, bool)]) -> Option<usize> {
        let mut chooser = Chooser::default();
        for &(i, answered) in ends {
            if let Some(c) = chooser.take(i, answered) {
                return Some(c);
            }
        }
        chooser.end()
    }

    #[test]
    fn the_first_entry_wins_if_it_answers_else_the_quickest() {
        assert_eq!(choose(&[(0, true)]), Some(0));
        // A later one answering first does not settle it while the first may still answer.
        assert_eq!(choose(&[(2, true), (0, true)]), Some(0));
        assert_eq!(choose(&[(2, true), (1, true), (0, false)]), Some(2));
        assert_eq!(choose(&[(0, false), (2, false), (1, true)]), Some(1));
        assert_eq!(choose(&[(1, false), (0, false), (2, false)]), None);
        assert_eq!(choose(&[(1, true)]), Some(1));
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn an_entry_registering_failed_through_sits_out_a_minute() {
        let entries = [entry("lan", "10.0.0.1:21116"), entry("wan", "wss://a.b/id")];
        let now = Instant::now();
        let mut failed = HashMap::new();
        let names = |failed: &HashMap<String, Instant>, now| -> Vec<String> {
            candidates(&entries, failed, now)
                .into_iter()
                .map(|e| e.name)
                .collect()
        };
        assert_eq!(names(&failed, now), ["lan", "wan"]);
        failed.insert("10.0.0.1:21116".to_owned(), now);
        assert_eq!(names(&failed, now + Duration::from_secs(59)), ["wan"]);
        assert_eq!(names(&failed, now + COOL_DOWN), ["lan", "wan"]);
    }

    #[test]
    fn the_backoff_doubles_to_a_minute_and_stays_there() {
        let secs: Vec<u64> = (0..7).map(|n| backoff_base(n).as_secs()).collect();
        assert_eq!(secs, [5, 10, 20, 40, 60, 60, 60]);
        let mut backoff = Backoff::default();
        for base in [5.0, 10.0, 20.0] {
            let wait = backoff.next().as_secs_f64();
            assert!(wait >= base * 0.8 && wait <= base * 1.2, "{wait} for {base}");
        }
        backoff.reset();
        assert!(backoff.next() <= Duration::from_secs(6));
    }

    #[test]
    fn a_new_way_out_counts_once_seen_twice_in_a_row() {
        let a: Option<IpAddr> = Some("192.168.1.5".parse().unwrap());
        let b: Option<IpAddr> = Some("198.18.0.1".parse().unwrap());
        let mut routes = Routes::default();
        assert!(!routes.see(vec![a, None]), "the first look settles");
        assert!(!routes.see(vec![a, None]));
        assert!(!routes.see(vec![b, None]), "once may be a glitch");
        assert!(!routes.see(vec![a, None]), "and it was");
        assert!(!routes.see(vec![b, None]));
        assert!(routes.see(vec![b, None]), "twice in a row is a change");
        assert!(!routes.see(vec![b, None]));
        assert!(!routes.see(vec![b, a]));
        assert!(routes.see(vec![b, a]), "an entry becoming reachable is one too");
    }

    #[test]
    fn sleep_shows_as_wall_time_running_ahead() {
        let s = Duration::from_secs;
        assert!(!slept(s(5), s(5)));
        assert!(!slept(s(14), s(5)));
        assert!(slept(s(600), s(5)));
    }

    #[test]
    fn entries_on_the_local_network_get_less_time_to_answer() {
        assert_eq!(probe_timeout("192.168.1.10:21116"), NEAR_TIMEOUT);
        assert_eq!(probe_timeout("ws://192.168.1.10:21118"), NEAR_TIMEOUT);
        assert_eq!(probe_timeout("10.1.2.3"), NEAR_TIMEOUT);
        assert_eq!(probe_timeout("[fd00::1]:21116"), NEAR_TIMEOUT);
        assert_eq!(probe_timeout("wss://example.com:4443/p1/id"), FAR_TIMEOUT);
        assert_eq!(probe_timeout("203.0.113.7:21116"), FAR_TIMEOUT);
        assert_eq!(probe_timeout("example.com"), FAR_TIMEOUT);
    }

    #[test]
    fn the_host_and_port_come_out_of_either_form() {
        let hp = |id: &str| host_port(id).map(|(h, p)| format!("{h} {p}"));
        assert_eq!(hp("192.168.1.10:21116").as_deref(), Some("192.168.1.10 21116"));
        assert_eq!(hp("192.168.1.10").as_deref(), Some("192.168.1.10 21116"));
        assert_eq!(hp("[fd00::1]:21116").as_deref(), Some("fd00::1 21116"));
        assert_eq!(
            hp("wss://example.com:4443/p1/id").as_deref(),
            Some("example.com 4443")
        );
        assert_eq!(hp("wss://example.com/id").as_deref(), Some("example.com 443"));
        assert_eq!(hp("ws://10.0.0.1:21118").as_deref(), Some("10.0.0.1 21118"));
    }

    #[test]
    fn the_entries_before_the_one_in_use_are_probed_unless_cooling_down() {
        let entries = [
            entry("lan", "10.0.0.1:21116"),
            entry("lan2", "10.0.0.2:21116"),
            entry("wan", "wss://a.b/id"),
        ];
        let now = Instant::now();
        let mut failed = HashMap::new();
        let names = |in_use: Option<&ServerEntry>, failed: &HashMap<String, Instant>| {
            earlier(&entries, in_use, failed, now)
                .map(|v| v.into_iter().map(|e| e.name).collect::<Vec<_>>())
        };
        assert_eq!(
            names(Some(&entries[2]), &failed),
            Some(vec!["lan".to_owned(), "lan2".to_owned()])
        );
        assert_eq!(names(Some(&entries[0]), &failed), Some(vec![]));
        assert_eq!(names(None, &failed), None);
        assert_eq!(names(Some(&entry("gone", "10.9.9.9:21116")), &failed), None);
        failed.insert("10.0.0.1:21116".to_owned(), now);
        assert_eq!(
            names(Some(&entries[2]), &failed),
            Some(vec!["lan2".to_owned()])
        );
    }

    use hbb_common::tokio;

    /// A server on loopback that sends `first`, if any, on each connection and holds it open.
    async fn server(first: Option<RendezvousMessage>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            while let Ok((socket, peer)) = listener.accept().await {
                let first = first.clone();
                tokio::spawn(async move {
                    let mut stream = hbb_common::tcp::FramedStream::from(socket, peer);
                    if let Some(msg) = first {
                        let _ = stream.send(&msg).await;
                    }
                    let _ = stream.next().await;
                });
            }
        });
        addr
    }

    fn key_exchange(sk: &sign::SecretKey) -> RendezvousMessage {
        let mut msg = RendezvousMessage::new();
        msg.set_key_exchange(KeyExchange {
            keys: vec![sign::sign(&[7u8; 32], sk).into()],
            ..Default::default()
        });
        msg
    }

    #[tokio::test]
    async fn only_the_id_servers_signed_key_exchange_counts_as_an_answer() {
        let (pk, sk) = sign::gen_keypair();
        let (_, other_sk) = sign::gen_keypair();
        let key = crate::encode64(pk);
        let key = Some(key.as_str());
        let signed = server(Some(key_exchange(&sk))).await;
        let forged = server(Some(key_exchange(&other_sk))).await;
        let mut other = RendezvousMessage::new();
        other.set_register_peer_response(RegisterPeerResponse::default());
        let other = server(Some(other)).await;
        // A TUN proxy takes the connection and says nothing.
        let silent = server(None).await;
        assert!(probe(&entry("signed", &signed), key).await.is_ok());
        assert!(probe(&entry("forged", &forged), key).await.is_err());
        assert!(probe(&entry("other", &other), key).await.is_err());
        let started = Instant::now();
        assert!(probe(&entry("silent", &silent), key).await.is_err());
        assert!(started.elapsed() < NEAR_TIMEOUT + Duration::from_millis(500));
        // With no key to tell it by, being reached is all there is.
        assert!(probe(&entry("silent", &silent), None).await.is_ok());
        assert!(probe(&entry("nobody", "127.0.0.1:1"), key).await.is_err());
    }

    #[test]
    fn a_round_ends_in_a_restart_a_stopped_wait_at_once_or_after_the_backoff() {
        let s = Duration::from_secs;
        assert_eq!(after_round(true, true, false, s(1)), Next::Restart);
        assert_eq!(after_round(false, true, false, s(1)), Next::Stopped);
        assert_eq!(after_round(false, false, true, UP_FOR), Next::Again);
        assert_eq!(after_round(false, false, true, s(59)), Next::BackOff, "registered, dropped soon");
        assert_eq!(
            after_round(false, false, false, s(90)),
            Next::BackOff,
            "a connection the server never answered, however long"
        );
        assert!(held_against(true, false, false));
        assert!(!held_against(true, false, true), "registered, then dropped");
        assert!(!held_against(true, true, false), "restarted");
        assert!(!held_against(false, false, false));
    }
}
