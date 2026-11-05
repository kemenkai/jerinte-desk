//! Wire format of the permanent password report and of the admin's password commands, shared byte
//! for byte with the Java backend: the backend's protocol specification,
//! whose test vectors (src/test/resources/pwreport/vectors.json there) the tests below reproduce.
//!
//! Nothing here touches the config or the network, so every random value can be passed in.

use hbb_common::sodiumoxide::{
    crypto::{
        aead::chacha20poly1305_ietf as aead,
        hash::sha256,
        scalarmult::curve25519 as x25519,
        sign::{self, ed25519},
    },
    randombytes::randombytes_into,
};

pub const VERSION: u32 = 1;
pub const ACTION_SET_PASSWORD: &str = "set-password";
pub const POLL_METHOD: &str = "POST";
pub const POLL_PATH: &str = "/api/devices/password-command";
/// How far the device's clock may be from the backend's when it judges a command's expiry.
pub const CLOCK_SKEW_MS: u64 = 5 * 60 * 1000;

const REPORT_KDF: &str = "jerinte-pw-report-v1/kdf";
const REPORT_AAD: &str = "jerinte-pw-report-v1/aad";
const REPORT_SIG: &str = "jerinte-pw-report-v1/sig";
const POLL_SIG: &str = "jerinte-pw-poll-v1/sig";
const CMD_KDF: &str = "jerinte-pw-cmd-v1/kdf";
const CMD_AAD: &str = "jerinte-pw-cmd-v1/aad";
const CMD_SIG: &str = "jerinte-pw-cmd-v1/sig";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A key or signature of the wrong length.
    BadKey,
    /// The peer's public key gave an all-zero shared secret (a small-order point).
    WeakKey,
    Decrypt,
    BadSignature,
    /// A version, action or key id this client does not know.
    Unsupported,
    /// A command for another device, uuid or device key.
    NotForThisDevice,
    /// A command whose sequence number is not above the last one applied.
    Stale,
    Expired,
    BadPassword,
}

/// Messages that are signed, hashed or authenticated: each field is a 4-byte big-endian length
/// followed by its bytes, the first field being the message's label.
struct Msg(Vec<u8>);

impl Msg {
    fn new(label: &str) -> Self {
        let mut m = Msg(Vec::new());
        m.bytes(label.as_bytes());
        m
    }

    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(&(b.len() as u32).to_be_bytes());
        self.0.extend_from_slice(b);
        self
    }

    fn u32(&mut self, n: u32) -> &mut Self {
        self.bytes(&n.to_be_bytes())
    }

    fn u64(&mut self, n: u64) -> &mut Self {
        self.bytes(&n.to_be_bytes())
    }
}

/// A plaintext encrypted for one X25519 public key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub epk: [u8; 32],
    pub nonce: [u8; 12],
    /// Ciphertext followed by the 16-byte tag.
    pub ct: Vec<u8>,
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    randombytes_into(&mut b);
    b
}

fn derive_key(label: &str, shared: &[u8], epk: &[u8], recipient_pk: &[u8]) -> aead::Key {
    let mut m = Msg::new(label);
    m.bytes(shared).bytes(epk).bytes(recipient_pk);
    aead::Key(sha256::hash(&m.0).0)
}

fn shared_secret(sk: &[u8; 32], pk: &[u8; 32]) -> Result<[u8; 32], Error> {
    // libsodium refuses a result of all zeros, which a small-order public key would produce.
    x25519::scalarmult(&x25519::Scalar(*sk), &x25519::GroupElement(*pk))
        .map(|g| g.0)
        .map_err(|_| Error::WeakKey)
}

fn seal_with(
    kdf_label: &str,
    plaintext: &[u8],
    recipient_pk: &[u8; 32],
    esk: &[u8; 32],
    nonce: &[u8; 12],
    aad: &[u8],
) -> Result<Sealed, Error> {
    let epk = x25519::scalarmult_base(&x25519::Scalar(*esk)).0;
    let shared = shared_secret(esk, recipient_pk)?;
    let key = derive_key(kdf_label, &shared, &epk, recipient_pk);
    let ct = aead::seal(plaintext, Some(aad), &aead::Nonce(*nonce), &key);
    Ok(Sealed {
        epk,
        nonce: *nonce,
        ct,
    })
}

fn open_with(
    kdf_label: &str,
    sealed: &Sealed,
    my_sk: &[u8; 32],
    my_pk: &[u8; 32],
    aad: &[u8],
) -> Result<Vec<u8>, Error> {
    let shared = shared_secret(my_sk, &sealed.epk)?;
    let key = derive_key(kdf_label, &shared, &sealed.epk, my_pk);
    aead::open(&sealed.ct, Some(aad), &aead::Nonce(sealed.nonce), &key).map_err(|_| Error::Decrypt)
}

/// `SHA256(h1)`: tells two passwords apart without revealing `h1`, which alone authenticates.
pub fn fingerprint(h1: &[u8; 32]) -> [u8; 32] {
    sha256::hash(h1).0
}

pub fn device_pk_fingerprint(device_pk: &[u8]) -> [u8; 32] {
    sha256::hash(device_pk).0
}

pub fn sign(device_sk: &[u8], msg: &[u8]) -> Result<[u8; 64], Error> {
    let sk = sign::SecretKey::from_slice(device_sk).ok_or(Error::BadKey)?;
    Ok(sign::sign_detached(msg, &sk).to_bytes())
}

fn verify(pk: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<(), Error> {
    let pk = sign::PublicKey(*pk);
    let sig = ed25519::Signature::from_bytes(sig).map_err(|_| Error::BadSignature)?;
    if sign::verify_detached(&sig, msg, &pk) {
        Ok(())
    } else {
        Err(Error::BadSignature)
    }
}

// ---- Report (device -> backend) ----

/// What a report's ciphertext is bound to.
pub struct ReportMeta<'a> {
    pub uuid: &'a [u8],
    pub report_id: &'a [u8],
    pub kid: &'a str,
    /// The command this report answers, empty otherwise.
    pub cmd_id: &'a [u8],
    pub cmd_seq: u64,
}

fn report_aad(m: &ReportMeta) -> Vec<u8> {
    let mut a = Msg::new(REPORT_AAD);
    a.u32(VERSION)
        .bytes(m.uuid)
        .bytes(m.report_id)
        .bytes(m.kid.as_bytes())
        .bytes(m.cmd_id)
        .u64(m.cmd_seq);
    a.0
}

pub fn seal_report(password: &str, backend_pk: &[u8; 32], meta: &ReportMeta) -> Result<Sealed, Error> {
    seal_report_with(password, backend_pk, meta, &random_bytes(), &random_bytes())
}

pub fn seal_report_with(
    password: &str,
    backend_pk: &[u8; 32],
    meta: &ReportMeta,
    esk: &[u8; 32],
    nonce: &[u8; 12],
) -> Result<Sealed, Error> {
    seal_with(REPORT_KDF, password.as_bytes(), backend_pk, esk, nonce, &report_aad(meta))
}

/// The backend's side, here for the tests.
#[cfg(test)]
pub fn open_report(
    sealed: &Sealed,
    backend_sk: &[u8; 32],
    backend_pk: &[u8; 32],
    meta: &ReportMeta,
) -> Result<Vec<u8>, Error> {
    open_with(REPORT_KDF, sealed, backend_sk, backend_pk, &report_aad(meta))
}

pub struct ReportFields<'a> {
    pub id: &'a str,
    pub meta: ReportMeta<'a>,
    pub ts: u64,
    pub sealed: &'a Sealed,
    pub fp: &'a [u8; 32],
}

pub fn report_sign_message(f: &ReportFields) -> Vec<u8> {
    let mut m = Msg::new(REPORT_SIG);
    m.u32(VERSION)
        .bytes(f.id.as_bytes())
        .bytes(f.meta.uuid)
        .bytes(f.meta.report_id)
        .bytes(f.meta.kid.as_bytes())
        .u64(f.ts)
        .bytes(&f.sealed.epk)
        .bytes(&f.sealed.nonce)
        .bytes(&f.sealed.ct)
        .bytes(f.fp)
        .bytes(f.meta.cmd_id)
        .u64(f.meta.cmd_seq);
    m.0
}

// ---- Poll (device -> backend) ----

pub fn poll_sign_message(
    id: &str,
    uuid: &[u8],
    device_pk_fp: &[u8; 32],
    ts: u64,
    nonce: &[u8; 16],
    last_seq: u64,
) -> Vec<u8> {
    let mut m = Msg::new(POLL_SIG);
    m.u32(VERSION)
        .bytes(POLL_METHOD.as_bytes())
        .bytes(POLL_PATH.as_bytes())
        .bytes(id.as_bytes())
        .bytes(uuid)
        .bytes(device_pk_fp)
        .u64(ts)
        .bytes(nonce)
        .u64(last_seq);
    m.0
}

// ---- Command (backend -> device) ----

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHeader {
    pub version: u32,
    pub action: String,
    pub kid: String,
    pub id: String,
    pub uuid: Vec<u8>,
    pub dpk_fp: Vec<u8>,
    pub cmd_id: Vec<u8>,
    pub seq: u64,
    pub issued: u64,
    pub expires: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub header: CommandHeader,
    pub sealed: Sealed,
    pub sig: Vec<u8>,
}

fn header_fields(m: &mut Msg, h: &CommandHeader) {
    m.u32(h.version)
        .bytes(h.action.as_bytes())
        .bytes(h.kid.as_bytes())
        .bytes(h.id.as_bytes())
        .bytes(&h.uuid)
        .bytes(&h.dpk_fp)
        .bytes(&h.cmd_id)
        .u64(h.seq)
        .u64(h.issued)
        .u64(h.expires);
}

fn command_aad(h: &CommandHeader) -> Vec<u8> {
    let mut m = Msg::new(CMD_AAD);
    header_fields(&mut m, h);
    m.0
}

pub fn command_sign_message(h: &CommandHeader, sealed: &Sealed) -> Vec<u8> {
    let mut m = Msg::new(CMD_SIG);
    header_fields(&mut m, h);
    m.bytes(&sealed.epk).bytes(&sealed.nonce).bytes(&sealed.ct);
    m.0
}

/// The X25519 key pair that a device's Ed25519 key converts to, as libsodium converts it.
pub fn device_x25519(device_sk: &[u8], device_pk: &[u8; 32]) -> Result<([u8; 32], [u8; 32]), Error> {
    let sk = sign::SecretKey::from_slice(device_sk).ok_or(Error::BadKey)?;
    let xsk = ed25519::to_curve25519_sk(&sk).map_err(|_| Error::BadKey)?;
    let xpk = device_x25519_pk(device_pk)?;
    Ok((xsk.0, xpk))
}

pub fn device_x25519_pk(device_pk: &[u8; 32]) -> Result<[u8; 32], Error> {
    ed25519::to_curve25519_pk(&sign::PublicKey(*device_pk))
        .map(|pk| pk.0)
        .map_err(|_| Error::WeakKey)
}

/// The backend's side, here for the tests and the vectors.
#[cfg(test)]
pub fn seal_command_with(
    header: CommandHeader,
    password: &str,
    device_pk: &[u8; 32],
    backend_cmd_sk: &[u8],
    esk: &[u8; 32],
    nonce: &[u8; 12],
) -> Result<Command, Error> {
    let dxpk = device_x25519_pk(device_pk)?;
    let sealed = seal_with(CMD_KDF, password.as_bytes(), &dxpk, esk, nonce, &command_aad(&header))?;
    let sig = sign(backend_cmd_sk, &command_sign_message(&header, &sealed))?.to_vec();
    Ok(Command {
        header,
        sealed,
        sig,
    })
}

/// What a command must match on this device.
pub struct Expected<'a> {
    pub cmd_kid: &'a str,
    pub cmd_pk: &'a [u8; 32],
    pub id: &'a str,
    pub uuid: &'a [u8],
    pub device_sk: &'a [u8],
    pub device_pk: &'a [u8; 32],
    pub last_seq: u64,
    pub now_ms: u64,
}

pub fn is_acceptable_command_password(p: &str) -> bool {
    (8..=64).contains(&p.len()) && p.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// Verifies a command and returns the password it sets; any failed check rejects it whole.
pub fn open_command(c: &Command, e: &Expected) -> Result<String, Error> {
    let h = &c.header;
    if h.version != VERSION || h.action != ACTION_SET_PASSWORD || h.kid != e.cmd_kid {
        return Err(Error::Unsupported);
    }
    verify(e.cmd_pk, &command_sign_message(h, &c.sealed), &c.sig)?;
    if h.id != e.id || h.uuid != e.uuid || h.dpk_fp != device_pk_fingerprint(e.device_pk) {
        return Err(Error::NotForThisDevice);
    }
    if h.seq <= e.last_seq {
        return Err(Error::Stale);
    }
    if h.expires.saturating_add(CLOCK_SKEW_MS) <= e.now_ms {
        return Err(Error::Expired);
    }
    let (xsk, xpk) = device_x25519(e.device_sk, e.device_pk)?;
    let password = open_with(CMD_KDF, &c.sealed, &xsk, &xpk, &command_aad(h))?;
    let password = String::from_utf8(password).map_err(|_| Error::BadPassword)?;
    if !is_acceptable_command_password(&password) {
        return Err(Error::BadPassword);
    }
    Ok(password)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> (sign::PublicKey, sign::SecretKey) {
        sign::keypair_from_seed(&sign::Seed([1; 32]))
    }

    fn backend_report() -> ([u8; 32], [u8; 32]) {
        let sk = [2; 32];
        (sk, x25519::scalarmult_base(&x25519::Scalar(sk)).0)
    }

    fn header(seq: u64) -> CommandHeader {
        let (pk, _) = device();
        CommandHeader {
            version: VERSION,
            action: ACTION_SET_PASSWORD.to_owned(),
            kid: "c1".to_owned(),
            id: "11631266".to_owned(),
            uuid: b"device-uuid".to_vec(),
            dpk_fp: device_pk_fingerprint(&pk.0).to_vec(),
            cmd_id: vec![7; 16],
            seq,
            issued: 1_700_000_000_000,
            expires: 1_700_604_800_000,
        }
    }

    #[test]
    fn report_round_trip_and_tamper() {
        let (bsk, bpk) = backend_report();
        let meta = ReportMeta {
            uuid: b"device-uuid",
            report_id: &[6; 16],
            kid: "r1",
            cmd_id: &[],
            cmd_seq: 0,
        };
        let sealed = seal_report("kq7mzr2xw9tbn4hp", &bpk, &meta).unwrap();
        assert_eq!(open_report(&sealed, &bsk, &bpk, &meta).unwrap(), b"kq7mzr2xw9tbn4hp");
        let other = ReportMeta { kid: "r2", ..meta };
        assert_eq!(open_report(&sealed, &bsk, &bpk, &other), Err(Error::Decrypt));
    }

    #[test]
    fn small_order_peer_is_refused() {
        let meta = ReportMeta {
            uuid: b"u",
            report_id: &[1; 16],
            kid: "r1",
            cmd_id: &[],
            cmd_seq: 0,
        };
        // The neutral element: every scalar multiplies it to zero.
        assert_eq!(seal_report("x", &[0; 32], &meta), Err(Error::WeakKey));
    }

    #[test]
    fn command_checks() {
        let (dpk, dsk) = device();
        let (cpk, csk) = sign::keypair_from_seed(&sign::Seed([3; 32]));
        let cmd = seal_command_with(header(3), "Adm1n-Set-P@ss", &dpk.0, &csk.0, &[9; 32], &[10; 12])
            .unwrap();
        let ok = Expected {
            cmd_kid: "c1",
            cmd_pk: &cpk.0,
            id: "11631266",
            uuid: b"device-uuid",
            device_sk: &dsk.0,
            device_pk: &dpk.0,
            last_seq: 2,
            now_ms: 1_700_000_100_000,
        };
        assert_eq!(open_command(&cmd, &ok).unwrap(), "Adm1n-Set-P@ss");
        assert_eq!(open_command(&cmd, &Expected { last_seq: 3, ..ok }), Err(Error::Stale));
        assert_eq!(open_command(&cmd, &Expected { cmd_kid: "c2", ..ok }), Err(Error::Unsupported));
        assert_eq!(open_command(&cmd, &Expected { id: "1", ..ok }), Err(Error::NotForThisDevice));
        assert_eq!(
            open_command(&cmd, &Expected { now_ms: 1_700_604_800_000 + CLOCK_SKEW_MS, ..ok }),
            Err(Error::Expired)
        );
        let mut forged = cmd.clone();
        forged.header.seq = 4;
        assert_eq!(open_command(&forged, &ok), Err(Error::BadSignature));
        let mut bad_sig = cmd.clone();
        bad_sig.sig[0] ^= 1;
        assert_eq!(open_command(&bad_sig, &ok), Err(Error::BadSignature));
        let (other_pk, _) = sign::keypair_from_seed(&sign::Seed([4; 32]));
        assert_eq!(
            open_command(&cmd, &Expected { cmd_pk: &other_pk.0, ..ok }),
            Err(Error::BadSignature)
        );
        let weak = seal_command_with(header(5), "short", &dpk.0, &csk.0, &[9; 32], &[10; 12]).unwrap();
        assert_eq!(open_command(&weak, &ok), Err(Error::BadPassword));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Writes the vectors the Java backend checks itself against:
    /// `PWREPORT_VECTORS_OUT=path cargo test write_vectors -- --ignored`.
    #[test]
    #[ignore]
    fn write_vectors() {
        let (dpk, dsk) = device();
        let (dxsk, dxpk) = device_x25519(&dsk.0, &dpk.0).unwrap();
        let (bsk, bpk) = backend_report();
        let (cpk, csk) = sign::keypair_from_seed(&sign::Seed([3; 32]));
        let uuid = b"device-uuid";
        let salt = "abcdefghijklmnopqrstuvwxyz012345";

        let report = |password: &str, report_id: [u8; 16], esk: [u8; 32], nonce: [u8; 12], cmd_id: &[u8], cmd_seq: u64| {
            let meta = ReportMeta { uuid, report_id: &report_id, kid: "r1", cmd_id, cmd_seq };
            let sealed = seal_report_with(password, &bpk, &meta, &esk, &nonce).unwrap();
            let shared = shared_secret(&esk, &bpk).unwrap();
            let key = derive_key(REPORT_KDF, &shared, &sealed.epk, &bpk);
            let h1 = {
                let mut m = Vec::from(password.as_bytes());
                m.extend_from_slice(salt.as_bytes());
                sha256::hash(&m).0
            };
            let fp = fingerprint(&h1);
            let ts = 1_700_000_000_000u64;
            let msg = report_sign_message(&ReportFields { id: "11631266", meta: ReportMeta { uuid, report_id: &report_id, kid: "r1", cmd_id, cmd_seq }, ts, sealed: &sealed, fp: &fp });
            let sig = sign(&dsk.0, &msg).unwrap();
            format!(
                r#"{{"id":"11631266","uuid":"{}","report_id":"{}","kid":"r1","ts":{ts},"esk":"{}","nonce":"{}","password":"{password}","salt":"{salt}","h1":"{}","fp":"{}","cmd_id":"{}","cmd_seq":{cmd_seq},"epk":"{}","shared":"{}","key":"{}","aad":"{}","ct":"{}","sign_msg":"{}","sig":"{}"}}"#,
                hex(uuid), hex(&report_id), hex(&esk), hex(&nonce), hex(&h1), hex(&fp), hex(cmd_id),
                hex(&sealed.epk), hex(&shared), hex(&key.0), hex(&report_aad(&meta)), hex(&sealed.ct), hex(&msg), hex(&sig)
            )
        };
        let plain_report = report("kq7mzr2xw9tbn4hp", [6; 16], [4; 32], [5; 12], &[], 0);
        let command_report = report("Adm1n-Set-P@ss", [11; 16], [12; 32], [13; 12], &[7; 16], 3);

        let poll_nonce = [8u8; 16];
        let pk_fp = device_pk_fingerprint(&dpk.0);
        let poll_msg = poll_sign_message("11631266", uuid, &pk_fp, 1_700_000_000_000, &poll_nonce, 2);
        let poll_sig = sign(&dsk.0, &poll_msg).unwrap();

        let h = header(3);
        let cmd = seal_command_with(h.clone(), "Adm1n-Set-P@ss", &dpk.0, &csk.0, &[9; 32], &[10; 12]).unwrap();
        let cmd_shared = shared_secret(&[9; 32], &dxpk).unwrap();
        let cmd_key = derive_key(CMD_KDF, &cmd_shared, &cmd.sealed.epk, &dxpk);

        // Ed25519 public keys the conversion must refuse: the neutral element, the point of order 2
        // (y = p - 1), and a y that is not reduced (y = p).
        let mut order2 = [0xffu8; 32];
        order2[0] = 0xec;
        order2[31] = 0x7f;
        let mut y_is_p = order2;
        y_is_p[0] = 0xed;
        let mut neutral = [0u8; 32];
        neutral[0] = 1;
        for bad in [neutral, order2, y_is_p] {
            assert_eq!(device_x25519_pk(&bad), Err(Error::WeakKey), "{}", hex(&bad));
        }

        let json = format!(
            r#"{{"version":{VERSION},
"device":{{"seed":"{}","ed_pk":"{}","ed_sk":"{}","x_pk":"{}","x_sk":"{}","pk_fp":"{}"}},
"backend_report":{{"sk":"{}","pk":"{}"}},
"backend_cmd":{{"seed":"{}","pk":"{}","sk":"{}","kid":"c1"}},
"report":{plain_report},
"report_for_command":{command_report},
"poll":{{"id":"11631266","uuid":"{}","ts":1700000000000,"nonce":"{}","last_seq":2,"sign_msg":"{}","sig":"{}"}},
"command":{{"version":{},"action":"{}","kid":"{}","id":"{}","uuid":"{}","dpk_fp":"{}","cmd_id":"{}","seq":{},"issued":{},"expires":{},"password":"Adm1n-Set-P@ss","esk":"{}","nonce":"{}","dxpk":"{}","epk":"{}","shared":"{}","key":"{}","aad":"{}","ct":"{}","sign_msg":"{}","sig":"{}"}},
"reject_ed_pk":["{}","{}","{}"]}}
"#,
            hex(&[1; 32]), hex(&dpk.0), hex(&dsk.0), hex(&dxpk), hex(&dxsk), hex(&pk_fp),
            hex(&bsk), hex(&bpk),
            hex(&[3; 32]), hex(&cpk.0), hex(&csk.0),
            hex(uuid), hex(&poll_nonce), hex(&poll_msg), hex(&poll_sig),
            h.version, h.action, h.kid, h.id, hex(&h.uuid), hex(&h.dpk_fp), hex(&h.cmd_id), h.seq, h.issued, h.expires,
            hex(&[9; 32]), hex(&[10; 12]), hex(&dxpk), hex(&cmd.sealed.epk), hex(&cmd_shared), hex(&cmd_key.0),
            hex(&command_aad(&h)), hex(&cmd.sealed.ct), hex(&command_sign_message(&h, &cmd.sealed)), hex(&cmd.sig),
            hex(&neutral), hex(&order2), hex(&y_is_p),
        );
        let out = std::env::var("PWREPORT_VECTORS_OUT").expect("PWREPORT_VECTORS_OUT");
        std::fs::write(out, json).unwrap();
    }

    #[test]
    fn fields_are_length_prefixed() {
        let mut m = Msg::new("L");
        m.bytes(b"ab").u32(1).u64(2);
        assert_eq!(
            m.0,
            [
                &[0, 0, 0, 1][..],
                b"L",
                &[0, 0, 0, 2],
                b"ab",
                &[0, 0, 0, 4, 0, 0, 0, 1],
                &[0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 2]
            ]
            .concat()
        );
    }
}
