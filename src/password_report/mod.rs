//! Permanent password reporting for the customer edition: the device sets its own random permanent
//! password, seals it for the backend and signs it with its device key; admins read it on the
//! backend and may set a specific one, which the device fetches, verifies and applies.
//!
//! Design: the backend's design notes; wire format: protocol.rs.
//!
//! Runs only in the `--server` process of a build whose custom-client.json carries
//! `password-report-pk` and `password-report-kid` (top-level keys, so they land in `HARD_SETTINGS`
//! and cannot be changed at run time). Commands are fetched only when `password-command-pk` and
//! `password-command-kid` are there too.
//!
//! The local state is a cache for retries; the backend is the authority. It answers every report
//! with the fingerprint it holds, so a lost or tampered state only costs a new password.

mod protocol;

use hbb_common::{
    config::{self, compute_permanent_password_h1, decode_permanent_password_h1_from_storage, Config},
    log,
    password_security::symmetric_crypt,
    sodiumoxide::base64,
    tokio::{self, time::Instant},
};
use protocol::{Command, CommandHeader, Expected, ReportFields, ReportMeta, Sealed};
use serde_derive::{Deserialize, Serialize};
use std::{
    sync::{
        atomic::{AtomicI64, Ordering},
        Mutex,
    },
    time::Duration,
};

const KEY_REPORT_PK: &str = "password-report-pk";
const KEY_REPORT_KID: &str = "password-report-kid";
const KEY_COMMAND_PK: &str = "password-command-pk";
const KEY_COMMAND_KID: &str = "password-command-kid";
/// Kept in `Config2.options`, which `--server` syncs with root on macOS and Linux.
const STATE_OPTION: &str = "pw-report-state";

const PASSWORD_LEN: usize = 16;
const CHECK_INTERVAL: Duration = Duration::from_secs(5 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
const RESEND_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(60);
/// The longest wait before trying the backend again after it failed. Never giving up, so that a
/// report still goes out and commands still come in once the backend is back.
const RETRY_MAX: Duration = Duration::from_secs(15 * 60);
/// Failures the backend or the network can repeat every minute are logged at most this often.
const LOG_INTERVAL: Duration = Duration::from_secs(60 * 60);
const REPORT_PATH: &str = "/api/devices/password-report";

/// Serializes writing the password with updating the state, so the periodic check never sees a
/// password whose report is not stored yet (it would take it for a local change and replace it).
static LOCK: Mutex<()> = Mutex::new(());
/// Backend time minus local time, learned from replies; signatures use backend time.
static CLOCK_OFFSET_MS: AtomicI64 = AtomicI64::new(0);

struct Keys {
    report_pk: [u8; 32],
    report_kid: String,
    command: Option<([u8; 32], String)>,
}

/// The latest report, binary fields in base64. `uuid` is the one its ciphertext is bound to;
/// `confirmed_id` is the device ID the backend last acknowledged this report under, empty until then.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct State {
    report_id: String,
    kid: String,
    uuid: String,
    fp: String,
    epk: String,
    nonce: String,
    ct: String,
    cmd_id: String,
    cmd_seq: u64,
    confirmed_id: String,
    /// The highest command sequence number applied; commands at or below it are refused.
    last_seq: u64,
}

impl State {
    /// Whether this is a report that can be sent as it is; anything else is rebuilt.
    fn is_sendable(&self) -> bool {
        let len = |s: &str| unb64(s).map(|v| v.len());
        !self.kid.is_empty()
            && len(&self.report_id) == Some(16)
            && matches!(len(&self.uuid), Some(1..=128))
            && len(&self.fp) == Some(32)
            && len(&self.epk) == Some(32)
            && len(&self.nonce) == Some(12)
            && matches!(len(&self.ct), Some(24..=80))
            && match len(&self.cmd_id) {
                Some(0) => self.cmd_seq == 0,
                Some(16) => self.cmd_seq > 0,
                _ => false,
            }
    }
}

#[derive(Debug, PartialEq)]
enum Action {
    Nothing,
    /// Set a new random password and report it.
    Rotate,
    /// Send the stored report now, e.g. re-signed under a new device ID.
    SendNow,
}

/// What the periodic check should do, given the stored state, the fingerprint of the current
/// permanent password (`None` when there is no usable one) and the device's current identity.
fn decide(
    state: Option<&State>,
    current_fp: Option<&str>,
    report_kid: &str,
    id: &str,
    uuid: &str,
) -> Action {
    let Some(s) = state else {
        return Action::Rotate;
    };
    // A damaged state, a new backend key (a new build) or a new machine uuid (the ciphertext is
    // bound to it) all need a fresh report, and without a usable password there is nothing to
    // protect by waiting.
    if !s.is_sendable() || s.kid != report_kid || s.uuid != uuid || current_fp.is_none() {
        return Action::Rotate;
    }
    if current_fp == Some(s.fp.as_str()) {
        if !s.confirmed_id.is_empty() && s.confirmed_id != id {
            return Action::SendNow;
        }
        return Action::Nothing;
    }
    // Changed locally, e.g. through IPC. Replace it only once the backend has the previous
    // report, so an unreachable backend never leaves admins without a working password.
    if s.confirmed_id.is_empty() {
        Action::Nothing
    } else {
        Action::Rotate
    }
}

pub fn start() {
    if !crate::is_server() {
        return;
    }
    let Some(keys) = keys() else {
        return;
    };
    log::info!(
        "password report enabled (kid {}, commands {})",
        keys.report_kid,
        if keys.command.is_some() { "on" } else { "off" }
    );
    tokio::spawn(run(keys));
}

fn keys() -> Option<Keys> {
    let hard = config::HARD_SETTINGS.read().unwrap();
    let key32 = |name: &str| -> Option<[u8; 32]> {
        let v = base64::decode(hard.get(name)?.trim(), base64::Variant::Original).ok()?;
        v.try_into().ok()
    };
    let text = |name: &str| hard.get(name).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    let report_pk = key32(KEY_REPORT_PK);
    let report_kid = text(KEY_REPORT_KID);
    if report_pk.is_none() && report_kid.is_none() {
        return None;
    }
    let (Some(report_pk), Some(report_kid)) = (report_pk, report_kid) else {
        log::error!("password report disabled: {KEY_REPORT_PK} or {KEY_REPORT_KID} is missing or malformed");
        return None;
    };
    let command = match (key32(KEY_COMMAND_PK), text(KEY_COMMAND_KID)) {
        (Some(pk), Some(kid)) => Some((pk, kid)),
        (None, None) => None,
        _ => {
            log::error!("password commands disabled: {KEY_COMMAND_PK} or {KEY_COMMAND_KID} is missing or malformed");
            None
        }
    };
    Some(Keys {
        report_pk,
        report_kid,
        command,
    })
}

async fn run(keys: Keys) {
    // A non-root `--server` on macOS and Linux holds a copy of root's config; acting on it before
    // the first sync from root lands would let that sync undo the new password. If the sync gives
    // up for good, nothing will overwrite the local copy any more, so go on with it.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if !crate::platform::is_root() {
        while !crate::server::initial_config_sync_settled() {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    let now = Instant::now();
    let (mut next_check, mut next_send, mut next_poll) = (now, now, now);
    let (mut send_failures, mut poll_failures) = (0, 0);
    loop {
        let now = Instant::now();
        if now >= next_check {
            // Not while backing off: a check that keeps asking for the same report, as after the
            // ID changed, would otherwise start the backoff over every five minutes. The retry
            // already due sends whatever is to be sent then.
            if check(&keys) && send_failures == 0 {
                next_send = now;
            }
            next_check = now + CHECK_INTERVAL;
        }
        if Instant::now() >= next_send {
            let wait = if send(&keys).await {
                send_failures = 0;
                RESEND_INTERVAL
            } else {
                send_failures += 1;
                retry_after(RETRY_INTERVAL, send_failures)
            };
            next_send = Instant::now() + wait;
        }
        if keys.command.is_some() && Instant::now() >= next_poll {
            let wait = match poll(&keys).await {
                Some(set) => {
                    poll_failures = 0;
                    if set {
                        next_send = Instant::now();
                    }
                    POLL_INTERVAL
                }
                None => {
                    poll_failures += 1;
                    retry_after(POLL_INTERVAL, poll_failures)
                }
            };
            next_poll = Instant::now() + wait;
        }
        let mut wake = next_check.min(next_send);
        if keys.command.is_some() {
            wake = wake.min(next_poll);
        }
        tokio::time::sleep_until(wake).await;
    }
}

/// The wait before trying again after `failures` failures in a row: `base` doubling, 15 minutes at
/// most.
fn retry_after(base: Duration, failures: u32) -> Duration {
    (base * 2u32.pow(failures.saturating_sub(1).min(4))).min(RETRY_MAX)
}

/// The periodic check; returns whether a report should go out now.
fn check(keys: &Keys) -> bool {
    let _lock = LOCK.lock().unwrap();
    let state = load_state();
    let current = current_fingerprint().map(|fp| b64(&fp));
    let uuid = b64(&hbb_common::get_uuid());
    match decide(state.as_ref(), current.as_deref(), &keys.report_kid, &Config::get_id(), &uuid) {
        Action::Nothing => false,
        Action::SendNow => true,
        Action::Rotate => {
            let last_seq = state.map(|s| s.last_seq).unwrap_or(0);
            let password = Config::get_auto_password(PASSWORD_LEN);
            match apply(keys, &password, &[], 0, last_seq) {
                Ok(()) => {
                    log::info!("password report: set a new permanent password, report pending");
                    true
                }
                Err(e) => {
                    log::error!("password report: failed to set a new permanent password: {e}");
                    false
                }
            }
        }
    }
}

/// Writes the password and the report for it; the caller holds `LOCK`.
fn apply(keys: &Keys, password: &str, cmd_id: &[u8], cmd_seq: u64, last_seq: u64) -> Result<(), String> {
    let salt = Config::get_salt();
    let h1 = compute_permanent_password_h1(password, &salt);
    let storage = encode_storage(&h1)?;
    // What persisted decides: the trusted devices are revoked before the write, so their
    // bookkeeping failing after it leaves them revoked, and the password still wants reporting.
    let written = crate::trusted_devices::config::set_password_storage(&storage, &salt);
    let (stored, stored_salt) = Config::get_local_permanent_password_storage_and_salt();
    if stored != storage || stored_salt != salt {
        return Err(match written {
            Err(e) => e.to_string(),
            Ok(_) => "the permanent password did not persist".to_owned(),
        });
    }
    if let Err(e) = written {
        log::warn!("password report: trusted devices after the new password: {e}");
    }
    let report_id = protocol::random_bytes::<16>();
    let uuid = hbb_common::get_uuid();
    let meta = ReportMeta {
        uuid: &uuid,
        report_id: &report_id,
        kid: &keys.report_kid,
        cmd_id,
        cmd_seq,
    };
    let sealed = protocol::seal_report(password, &keys.report_pk, &meta).map_err(|e| format!("{e:?}"))?;
    save_state(&State {
        report_id: b64(&report_id),
        kid: keys.report_kid.clone(),
        uuid: b64(&uuid),
        fp: b64(&protocol::fingerprint(&h1)),
        epk: b64(&sealed.epk),
        nonce: b64(&sealed.nonce),
        ct: b64(&sealed.ct),
        cmd_id: b64(cmd_id),
        cmd_seq,
        confirmed_id: String::new(),
        last_seq: last_seq.max(cmd_seq),
    });
    Ok(())
}

/// The storage format `Config` keeps the permanent password in, built from public functions only
/// and checked by decoding it back, so a change of format upstream refuses to write instead of
/// writing something the server cannot authenticate with.
fn encode_storage(h1: &[u8; 32]) -> Result<String, String> {
    let hashed = format!("00{}", base64::encode(h1, base64::Variant::Original));
    let encrypted =
        symmetric_crypt(hashed.as_bytes(), true).map_err(|_| "cannot encrypt the password storage")?;
    let storage = format!("01{}", base64::encode(encrypted, base64::Variant::Original));
    if decode_permanent_password_h1_from_storage(&storage) != Some(*h1) {
        return Err("the permanent password storage format has changed".to_owned());
    }
    Ok(storage)
}

fn current_fingerprint() -> Option<[u8; 32]> {
    let (storage, salt) = Config::get_local_permanent_password_storage_and_salt();
    if salt.is_empty() {
        return None;
    }
    decode_permanent_password_h1_from_storage(&storage).map(|h1| protocol::fingerprint(&h1))
}

fn load_state() -> Option<State> {
    let v = Config::get_option(STATE_OPTION);
    if v.is_empty() {
        return None;
    }
    serde_json::from_str(&v).ok()
}

fn save_state(s: &State) {
    Config::set_option(
        STATE_OPTION.to_owned(),
        serde_json::to_string(s).unwrap_or_default(),
    );
}

// ---- Network ----

fn api_url(path: &str) -> Option<String> {
    let api = crate::common::get_api_server(
        Config::get_option("api-server"),
        Config::get_option("custom-rendezvous-server"),
    );
    if api.is_empty() || crate::is_public(&api) {
        return None;
    }
    Some(format!("{api}{path}"))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn backend_now_ms() -> u64 {
    (now_ms() as i64 + CLOCK_OFFSET_MS.load(Ordering::Relaxed)).max(0) as u64
}

/// POSTs and returns the status and the parsed body; learns the backend's clock from the reply.
async fn post(path: &str, body: serde_json::Value) -> Option<(u16, serde_json::Value)> {
    let url = api_url(path)?;
    let sent = now_ms();
    match crate::common::post_request_with_status(url, body.to_string(), "").await {
        Ok((status, text)) => {
            let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            if let Some(server) = v.get("server_time").and_then(|t| t.as_u64()) {
                let local = sent / 2 + now_ms() / 2;
                CLOCK_OFFSET_MS.store(server as i64 - local as i64, Ordering::Relaxed);
            }
            Some((status, v))
        }
        Err(e) => {
            log::debug!("password report: {path} failed: {e}");
            None
        }
    }
}

/// Sends the stored report; returns whether the backend now holds exactly this report.
async fn send(keys: &Keys) -> bool {
    let Some(state) = load_state() else {
        return false;
    };
    // Anything else is for the periodic check to replace, not to send.
    if !state.is_sendable()
        || state.kid != keys.report_kid
        || state.uuid != b64(&hbb_common::get_uuid())
    {
        return false;
    }
    for attempt in 0..2 {
        let id = Config::get_id();
        let Some(body) = report_body(&state, &id) else {
            return false;
        };
        let Some((status, reply)) = post(REPORT_PATH, body).await else {
            return false;
        };
        let result = reply.get("result").and_then(|r| r.as_str()).unwrap_or_default();
        match (status, result) {
            (200, "ok") => {
                let fp = reply.get("fp").and_then(|v| v.as_str()).unwrap_or_default();
                let report_id = reply.get("report_id").and_then(|v| v.as_str()).unwrap_or_default();
                if fp != state.fp || report_id != state.report_id {
                    hbb_common::throttled_log!(
                        LOG_INTERVAL,
                        warn,
                        "password report: the backend holds another report, resending"
                    );
                    return false;
                }
                let _lock = LOCK.lock().unwrap();
                if let Some(mut s) = load_state() {
                    if s.report_id == state.report_id && s.confirmed_id != id {
                        s.confirmed_id = id;
                        save_state(&s);
                        log::info!("password report: confirmed by the backend");
                    }
                }
                return true;
            }
            // The clock offset was just learned from this reply: sign once more.
            (401, "skew") if attempt == 0 => continue,
            (409, _) => return false,
            _ => {
                hbb_common::throttled_log!(
                    LOG_INTERVAL,
                    warn,
                    "password report: rejected by the backend ({status} {result})"
                );
                return false;
            }
        }
    }
    false
}

fn report_body(s: &State, id: &str) -> Option<serde_json::Value> {
    let (sk, _) = Config::get_key_pair();
    // The uuid the ciphertext is bound to; `send` only gets here while it is still this machine's.
    let uuid = unb64(&s.uuid)?;
    let report_id = unb64(&s.report_id)?;
    let cmd_id = unb64(&s.cmd_id)?;
    let fp: [u8; 32] = unb64(&s.fp)?.try_into().ok()?;
    let sealed = Sealed {
        epk: unb64(&s.epk)?.try_into().ok()?,
        nonce: unb64(&s.nonce)?.try_into().ok()?,
        ct: unb64(&s.ct)?,
    };
    let ts = backend_now_ms();
    let msg = protocol::report_sign_message(&ReportFields {
        id,
        meta: ReportMeta {
            uuid: &uuid,
            report_id: &report_id,
            kid: &s.kid,
            cmd_id: &cmd_id,
            cmd_seq: s.cmd_seq,
        },
        ts,
        sealed: &sealed,
        fp: &fp,
    });
    let sig = protocol::sign(&sk, &msg).ok()?;
    Some(serde_json::json!({
        "version": protocol::VERSION,
        "id": id,
        "uuid": b64(&uuid),
        "report_id": s.report_id,
        "kid": s.kid,
        "ts": ts,
        "epk": s.epk,
        "nonce": s.nonce,
        "ct": s.ct,
        "fp": s.fp,
        "cmd_id": s.cmd_id,
        "cmd_seq": s.cmd_seq,
        "sig": b64(&sig),
    }))
}

/// Fetches the pending command, if any, and applies it; returns whether a password was set,
/// or `None` when the backend did not answer as it should, which the polling backs off from.
async fn poll(keys: &Keys) -> Option<bool> {
    let Some((cmd_pk, cmd_kid)) = keys.command.as_ref() else {
        return Some(false);
    };
    for attempt in 0..2 {
        let id = Config::get_id();
        let (sk, pk) = Config::get_key_pair();
        let Ok(pk) = <[u8; 32]>::try_from(pk.as_slice()) else {
            return Some(false);
        };
        let uuid = hbb_common::get_uuid();
        let last_seq = load_state().map(|s| s.last_seq).unwrap_or(0);
        let nonce = protocol::random_bytes::<16>();
        let ts = backend_now_ms();
        let msg = protocol::poll_sign_message(
            &id,
            &uuid,
            &protocol::device_pk_fingerprint(&pk),
            ts,
            &nonce,
            last_seq,
        );
        let Ok(sig) = protocol::sign(&sk, &msg) else {
            return Some(false);
        };
        let body = serde_json::json!({
            "version": protocol::VERSION,
            "id": id,
            "uuid": b64(&uuid),
            "ts": ts,
            "nonce": b64(&nonce),
            "last_seq": last_seq,
            "sig": b64(&sig),
        });
        let Some((status, reply)) = post(protocol::POLL_PATH, body).await else {
            return None;
        };
        let result = reply.get("result").and_then(|r| r.as_str()).unwrap_or_default();
        match (status, result) {
            (200, "ok") => {}
            (401, "skew") if attempt == 0 => continue,
            _ => return None,
        }
        if reply.get("nonce").and_then(|n| n.as_str()) != Some(b64(&nonce).as_str()) {
            return None;
        }
        let Some(c) = reply.get("command").filter(|c| !c.is_null()) else {
            return Some(false);
        };
        let Some(command) = parse_command(c) else {
            hbb_common::throttled_log!(LOG_INTERVAL, warn, "password report: ignoring a malformed command");
            return Some(false);
        };
        let expected = Expected {
            cmd_kid,
            cmd_pk,
            id: &id,
            uuid: &uuid,
            device_sk: &sk,
            device_pk: &pk,
            last_seq,
            // The backend's clock, as for signing: a device clock that is days off must not
            // refuse every command. The API is reached over verified HTTPS in this deployment.
            now_ms: backend_now_ms(),
        };
        let password = match protocol::open_command(&command, &expected) {
            Ok(p) => p,
            Err(e) => {
                hbb_common::throttled_log!(
                    LOG_INTERVAL,
                    warn,
                    "password report: refused command {}: {e:?}",
                    command.header.seq
                );
                return Some(false);
            }
        };
        let _lock = LOCK.lock().unwrap();
        // Re-read under the lock: a command applied meanwhile must not be applied twice.
        let last_seq = load_state().map(|s| s.last_seq).unwrap_or(0);
        if command.header.seq <= last_seq {
            return Some(false);
        }
        return Some(match apply(keys, &password, &command.header.cmd_id, command.header.seq, last_seq) {
            Ok(()) => {
                log::info!("password report: applied command {}", command.header.seq);
                true
            }
            Err(e) => {
                log::error!("password report: failed to apply command {}: {e}", command.header.seq);
                false
            }
        });
    }
    None
}

/// Parses a command; fields of an impossible size are refused before decoding.
fn parse_command(c: &serde_json::Value) -> Option<Command> {
    let text = |k: &str| {
        c.get(k)
            .and_then(|v| v.as_str())
            .filter(|s| s.len() <= 128)
            .map(str::to_owned)
    };
    let num = |k: &str| c.get(k).and_then(|v| v.as_u64());
    let bin = |k: &str| unb64(c.get(k)?.as_str().filter(|s| s.len() <= 256)?);
    Some(Command {
        header: CommandHeader {
            version: u32::try_from(num("version")?).ok()?,
            action: text("action")?,
            kid: text("kid")?,
            id: text("id")?,
            uuid: bin("uuid")?,
            dpk_fp: bin("dpk_fp")?,
            cmd_id: bin("cmd_id")?,
            seq: num("seq")?,
            issued: num("issued")?,
            expires: num("expires")?,
        },
        sealed: Sealed {
            epk: bin("epk")?.try_into().ok()?,
            nonce: bin("nonce")?.try_into().ok()?,
            ct: bin("ct")?,
        },
        sig: bin("sig")?,
    })
}

fn b64(b: &[u8]) -> String {
    base64::encode(b, base64::Variant::Original)
}

fn unb64(s: &str) -> Option<Vec<u8>> {
    base64::decode(s, base64::Variant::Original).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_backend_is_tried_less_often_but_never_given_up() {
        let minutes: Vec<u64> = (1..=7)
            .map(|n| retry_after(RETRY_INTERVAL, n).as_secs() / 60)
            .collect();
        assert_eq!(minutes, [1, 2, 4, 8, 15, 15, 15]);
        assert_eq!(retry_after(POLL_INTERVAL, 1), POLL_INTERVAL);
    }

    const UUID: &str = "dXVpZA==";

    fn fp(c: u8) -> String {
        b64(&[c; 32])
    }

    fn state(fp: &str, confirmed_id: &str) -> State {
        State {
            report_id: b64(&[6; 16]),
            kid: "r1".into(),
            uuid: UUID.into(),
            fp: fp.into(),
            epk: b64(&[4; 32]),
            nonce: b64(&[5; 12]),
            ct: b64(&[9; 32]),
            confirmed_id: confirmed_id.into(),
            ..Default::default()
        }
    }

    #[test]
    fn decide_follows_the_design() {
        let (a, b) = (fp(1), fp(2));
        let (a, b) = (a.as_str(), b.as_str());
        // First run: no report yet.
        assert_eq!(decide(None, Some(a), "r1", "1", UUID), Action::Rotate);
        assert_eq!(decide(None, None, "r1", "1", UUID), Action::Rotate);
        // Unchanged and confirmed under this ID.
        assert_eq!(decide(Some(&state(a, "1")), Some(a), "r1", "1", UUID), Action::Nothing);
        // Unchanged, not confirmed yet: the send timer handles it.
        assert_eq!(decide(Some(&state(a, "")), Some(a), "r1", "1", UUID), Action::Nothing);
        // The device ID changed: re-sign and send the same report now.
        assert_eq!(decide(Some(&state(a, "1")), Some(a), "r1", "2", UUID), Action::SendNow);
        // Changed locally after the backend confirmed: replace it.
        assert_eq!(decide(Some(&state(a, "1")), Some(b), "r1", "1", UUID), Action::Rotate);
        // Changed locally before the backend confirmed: deliver the stored report first.
        assert_eq!(decide(Some(&state(a, "")), Some(b), "r1", "1", UUID), Action::Nothing);
        // No usable password at all: nothing to protect by waiting.
        assert_eq!(decide(Some(&state(a, "")), None, "r1", "1", UUID), Action::Rotate);
        assert_eq!(decide(Some(&state(a, "1")), None, "r1", "1", UUID), Action::Rotate);
        // A new backend key.
        assert_eq!(decide(Some(&state(a, "1")), Some(a), "r2", "1", UUID), Action::Rotate);
        // A new machine uuid: the stored ciphertext is bound to the old one.
        assert_eq!(decide(Some(&state(a, "")), Some(a), "r1", "1", "b3RoZXI="), Action::Rotate);
        // A damaged state, e.g. a field lost in a partial overwrite.
        let mut damaged = state(a, "1");
        damaged.epk.clear();
        assert_eq!(decide(Some(&damaged), Some(a), "r1", "1", UUID), Action::Rotate);
        let mut half_command = state(a, "1");
        half_command.cmd_seq = 3;
        assert_eq!(decide(Some(&half_command), Some(a), "r1", "1", UUID), Action::Rotate);
    }

    #[test]
    fn state_survives_unknown_and_missing_fields() {
        let s: State = serde_json::from_str(r#"{"fp":"x","last_seq":3,"later":"field"}"#).unwrap();
        assert_eq!(s.fp, "x");
        assert_eq!(s.last_seq, 3);
        assert_eq!(s.confirmed_id, "");
        assert!(!s.is_sendable());
        assert!(state(&fp(1), "").is_sendable());
    }

    #[test]
    fn storage_round_trips() {
        let h1 = compute_permanent_password_h1("kq7mzr2xw9tbn4hp", "abcdefghijklmnopqrstuvwxyz012345");
        let storage = encode_storage(&h1).unwrap();
        assert!(storage.starts_with("01"));
        assert_eq!(decode_permanent_password_h1_from_storage(&storage), Some(h1));
    }

    #[test]
    fn parses_the_backend_command_json() {
        // The command of vectors.json, as the Java backend serializes it.
        let hex = |s: &str| -> Vec<u8> {
            (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
        };
        let c = serde_json::json!({
            "version": 1, "action": "set-password", "kid": "c1", "id": "11631266",
            "uuid": b64(b"device-uuid"),
            "dpk_fp": b64(&hex("34750f98bd59fcfc946da45aaabe933be154a4b5094e1c4abf42866505f3c97e")),
            "cmd_id": b64(&[7; 16]), "seq": 3, "issued": 1700000000000u64, "expires": 1700604800000u64,
            "epk": b64(&hex("57db4b359f23ae5e146e4e2512056704722506348c150c14753d0c933d04d421")),
            "nonce": b64(&[10; 12]),
            "ct": b64(&hex("6fbff8e6635e3e13ac2e7f0e091ab9c8edd15a75c579fe9aa2fc339605c4")),
            "sig": b64(&hex("bde5d37404e1412ea98555784f029f9a48cf9d1df4373ef368340a8905418c34191a14e0e05a5bd17dc1c07f831ca58d437c5a5fbf03bdf00f8393dca15ce404")),
        });
        let command = parse_command(&c).unwrap();
        let (dpk, dsk) = hbb_common::sodiumoxide::crypto::sign::keypair_from_seed(
            &hbb_common::sodiumoxide::crypto::sign::Seed([1; 32]),
        );
        let (cpk, _) = hbb_common::sodiumoxide::crypto::sign::keypair_from_seed(
            &hbb_common::sodiumoxide::crypto::sign::Seed([3; 32]),
        );
        let expected = Expected {
            cmd_kid: "c1",
            cmd_pk: &cpk.0,
            id: "11631266",
            uuid: b"device-uuid",
            device_sk: &dsk.0,
            device_pk: &dpk.0,
            last_seq: 2,
            now_ms: 1_700_000_100_000,
        };
        assert_eq!(protocol::open_command(&command, &expected).unwrap(), "Adm1n-Set-P@ss");
        let mut missing = c.clone();
        missing.as_object_mut().unwrap().remove("sig");
        assert!(parse_command(&missing).is_none());
    }
}
