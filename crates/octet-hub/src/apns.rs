//! APNs for the Live Activity: the `apns` block of `hub.json` ([`ApnsConfig`]), the token
//! signing key and the JWT it signs ([`Signer`]), and one send through `curl` ([`send`]).
//!
//! Nothing here decides when to send; the activity driver does.
//!
//! The key is read once, through a descriptor opened with `O_NOFOLLOW`, and only if it is a
//! regular file of mode 0600 owned by the hub's user and a PKCS#8 PEM P-256 key; anything else
//! leaves APNs off with one log line that names no path or value. The JWT is ES256 over a fixed
//! header and claims, kept for [`JWT_LIFETIME`] and then made again.
//!
//! [`send`] runs `curl` with a fixed argv ([`CURL_ARGS`]) and gives it everything else (the URL
//! with the device token, the headers with the JWT, and the body) as a curl config on stdin, so
//! no secret is in `ps`.
use octet_core::remote::protocol::is_activity_token;
use p256::{
    ecdsa::{Signature, SigningKey, signature::Signer as _},
    pkcs8::DecodePrivateKey,
};
use rustix::{
    fs::{FileType, Mode, OFlags},
    io::Errno,
};
use serde_json::Value;
use std::{
    fmt,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

/// The production APNs host.
pub const HOST: &str = "api.push.apple.com";
/// The sandbox APNs host, for development builds of the app.
pub const SANDBOX_HOST: &str = "api.sandbox.push.apple.com";
/// The topic of a Live Activity push for the app.
pub const TOPIC: &str = "dev.octet.mobile.push-type.liveactivity";
/// How long one JWT is used before another is made. Apple refuses one older than an hour.
pub const JWT_LIFETIME: Duration = Duration::from_mins(50);
/// `curl`'s whole argv, after the program. `-q` must come first: it stops curl reading
/// `~/.curlrc`. `-K -` reads the rest, which holds every secret, from stdin.
pub const CURL_ARGS: [&str; 12] = [
    "-q",
    "--http2",
    "-s",
    "-S",
    "--max-time",
    "10",
    "-o",
    "/dev/null",
    "-w",
    "%{http_code}",
    "-K",
    "-",
];
/// The environment variable that points APNs at a local fake, in debug builds only.
const SEAM: &str = "OCTET_HUB_APNS_URL";
/// Whether this build honours [`SEAM`]: debug builds only, so nothing that sets a release hub's
/// environment can redirect its pushes.
const SEAM_HONOURED: bool = cfg!(debug_assertions);
/// The only prefix the seam is honoured for; a port follows, and nothing else.
const LOOPBACK: &str = "http://127.0.0.1:";
/// How long one send may take in all; `curl` itself is told 10 seconds.
const SEND_WITHIN: Duration = Duration::from_secs(15);
/// The length of a team id and of a key id.
const ID_CHARS: usize = 10;
/// The most of a key file that is read; a `.p8` is about 250 bytes.
const KEY_BYTES: u64 = 16 * 1024;

/// A team or key id is not 10 upper-case letters and digits.
const IDS_MALFORMED: &str = "APNs is off: the team or key id is malformed";
/// The key file is missing.
const KEY_MISSING: &str = "APNs key is missing";
/// The key path names a symlink.
const KEY_SYMLINK: &str = "APNs key must not be a symlink";
/// The key file could not be opened or read.
const KEY_UNREADABLE: &str = "APNs key could not be read";
/// The key path is not a regular file.
const KEY_NOT_FILE: &str = "APNs key must be a regular file";
/// The key file's mode is not 0600.
const KEY_MODE: &str = "APNs key must be 0600";
/// The key file belongs to another user.
const KEY_OWNER: &str = "APNs key must be owned by the hub's user";
/// The key file does not hold a PKCS#8 PEM P-256 key.
const KEY_NOT_P256: &str = "APNs key is not a PKCS#8 P-256 key";

/// The `apns` block of `hub.json`: who signs, with which key, to which host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApnsConfig {
    /// The Apple developer team id: 10 upper-case letters and digits.
    pub team_id: String,
    /// The APNs key's id: 10 upper-case letters and digits.
    pub key_id: String,
    /// The absolute path of the `.p8` key.
    pub key_path: PathBuf,
    /// Whether to send to the sandbox host, for a development build of the app.
    pub sandbox: bool,
}

impl ApnsConfig {
    /// Reads `hub.json`'s `apns` value: `Ok(None)` when there is none, or `Err` with the one
    /// log line for a malformed one, which names the field and never its value.
    ///
    /// # Errors
    ///
    /// When `apns` is not an object, a team or key id is not 10 upper-case letters and digits,
    /// `key_path` is not an absolute path, or `sandbox` is not a boolean.
    pub fn from_hub_json(apns: &Value) -> Result<Option<Self>, String> {
        if apns.is_null() {
            return Ok(None);
        }
        let off = |field: &str| format!("APNs is off: hub.json apns.{field} is malformed");
        if !apns.is_object() {
            return Err("APNs is off: hub.json apns must be an object".to_owned());
        }
        let id = |field: &str| {
            apns[field]
                .as_str()
                .filter(|id| is_apple_id(id))
                .map(str::to_owned)
                .ok_or_else(|| off(field))
        };
        let team_id = id("team_id")?;
        let key_id = id("key_id")?;
        let key_path = apns["key_path"]
            .as_str()
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| off("key_path"))?;
        let sandbox = apns["sandbox"].as_bool().ok_or_else(|| off("sandbox"))?;
        Ok(Some(Self {
            team_id,
            key_id,
            key_path,
            sandbox,
        }))
    }

    /// The host pushes go to: [`SANDBOX_HOST`] or [`HOST`].
    pub fn host(&self) -> &'static str {
        if self.sandbox { SANDBOX_HOST } else { HOST }
    }

    /// Where requests go, before `/3/device/<token>`: `https://<host>`, or, in a debug build,
    /// `OCTET_HUB_APNS_URL` when it is `http://127.0.0.1:<port>`.
    pub fn url_base(&self) -> String {
        let seam = SEAM_HONOURED.then(|| std::env::var(SEAM).ok()).flatten();
        self.url_base_where(SEAM_HONOURED, seam.as_deref())
    }

    /// [`url_base`](Self::url_base) for a seam value, in a build that does or does not honour it.
    fn url_base_where(&self, honoured: bool, seam: Option<&str>) -> String {
        match seam.filter(|seam| honoured && is_loopback(seam)) {
            Some(local) => local.to_owned(),
            None => format!("https://{}", self.host()),
        }
    }
}

/// Whether `id` is a team or key id: 10 upper-case ASCII letters and digits.
fn is_apple_id(id: &str) -> bool {
    id.len() == ID_CHARS
        && id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// Whether `url` is exactly `http://127.0.0.1:<port>`, with a port of 1 to 5 digits.
fn is_loopback(url: &str) -> bool {
    url.strip_prefix(LOOPBACK).is_some_and(|port| {
        (1..=5).contains(&port.len()) && port.bytes().all(|b| b.is_ascii_digit())
    })
}

/// The signing key and the JWT it last made. Neither is ever printed.
pub struct Signer {
    /// The P-256 key from the `.p8` file.
    key: SigningKey,
    /// The JWT's `iss`.
    team_id: String,
    /// The JWT's `kid`.
    key_id: String,
    /// The JWT in use and when it was made.
    token: Option<(SystemTime, String)>,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer")
            .field("team_id", &self.team_id)
            .field("key_id", &self.key_id)
            .field("key", &"<redacted>")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Signer {
    /// Reads the key `config` names. If it cannot be used, APNs is off: this logs one line, which
    /// names no path or value ("APNs key must be 0600" for the wrong mode), and is `None`.
    pub fn load(config: &ApnsConfig) -> Option<Self> {
        match Self::try_load(config) {
            Ok(signer) => Some(signer),
            Err(line) => {
                eprintln!("{line}");
                None
            }
        }
    }

    /// [`load`](Self::load), with the log line as the error.
    fn try_load(config: &ApnsConfig) -> Result<Self, &'static str> {
        // The ids go into the JWT unescaped, so a config built by hand is checked as hub.json's is.
        if !is_apple_id(&config.team_id) || !is_apple_id(&config.key_id) {
            return Err(IDS_MALFORMED);
        }
        Ok(Self {
            key: read_key(&config.key_path)?,
            team_id: config.team_id.clone(),
            key_id: config.key_id.clone(),
            token: None,
        })
    }

    /// The JWT to send at `now`: the one in use while it is younger than [`JWT_LIFETIME`], or a
    /// new one, issued at `now`, once it is that old (or the clock moved back past it).
    pub fn jwt(&mut self, now: SystemTime) -> &str {
        let fresh = self.token.as_ref().is_some_and(|(made, _)| {
            now.duration_since(*made)
                .is_ok_and(|age| age < JWT_LIFETIME)
        });
        if !fresh {
            self.token = None;
        }
        let (key, team_id, key_id) = (&self.key, &self.team_id, &self.key_id);
        &self
            .token
            .get_or_insert_with(|| (now, make_jwt(key, team_id, key_id, now)))
            .1
    }
}

/// An ES256 JWT for APNs: `{"alg":"ES256","kid":<key_id>}` and `{"iss":<team_id>,"iat":<secs>}`,
/// in that key order, and the raw 64-byte `r‖s` signature, each base64url without padding. The
/// ids are checked letters and digits, so they need no escaping.
fn make_jwt(key: &SigningKey, team_id: &str, key_id: &str, now: SystemTime) -> String {
    let iat = now
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs());
    let header = format!(r#"{{"alg":"ES256","kid":"{key_id}"}}"#);
    let claims = format!(r#"{{"iss":"{team_id}","iat":{iat}}}"#);
    let signed = format!(
        "{}.{}",
        base64url(header.as_bytes()),
        base64url(claims.as_bytes())
    );
    // RFC 6979: the same key and input always give the same signature.
    let signature: Signature = key.sign(signed.as_bytes());
    format!("{signed}.{}", base64url(&signature.to_bytes()))
}

/// Reads a PKCS#8 PEM P-256 key from `path`, which must be a regular file (not a symlink), of mode
/// 0600, owned by the hub's effective user; or the log line for why not.
fn read_key(path: &Path) -> Result<SigningKey, &'static str> {
    // `NONBLOCK`: a FIFO at the path must not hang the hub; it is refused below.
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
    let fd = rustix::fs::open(path, flags, Mode::empty()).map_err(|error| match error {
        Errno::NOENT => KEY_MISSING,
        Errno::LOOP => KEY_SYMLINK,
        _ => KEY_UNREADABLE,
    })?;
    let stat = rustix::fs::fstat(&fd).map_err(|_| KEY_UNREADABLE)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(KEY_NOT_FILE);
    }
    if Mode::from_raw_mode(stat.st_mode) != Mode::RUSR | Mode::WUSR {
        return Err(KEY_MODE);
    }
    if stat.st_uid != rustix::process::geteuid().as_raw() {
        return Err(KEY_OWNER);
    }
    let mut text = String::new();
    std::fs::File::from(fd)
        .take(KEY_BYTES)
        .read_to_string(&mut text)
        .map_err(|_| KEY_NOT_P256)?;
    let key = SigningKey::from_pkcs8_pem(&text).map_err(|_| KEY_NOT_P256);
    // The PEM is not kept: what can be wiped is.
    let mut bytes = text.into_bytes();
    bytes.fill(0);
    std::hint::black_box(&bytes);
    key
}

/// Base64url without padding (RFC 4648 §5).
pub(crate) fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let (a, b, c) = (
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        );
        let sextets = [
            a >> 2,
            (a & 0x03) << 4 | b >> 4,
            (b & 0x0f) << 2 | c >> 6,
            c & 0x3f,
        ];
        // One byte gives two characters, two give three, three give four.
        for sextet in &sextets[..=chunk.len()] {
            out.push(char::from(ALPHABET[usize::from(*sextet)]));
        }
    }
    out
}

/// How soon APNs should deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// `apns-priority: 10`: now (a start, or an approval).
    Immediate,
    /// `apns-priority: 5`: when it suits the phone's power.
    Normal,
}

impl Priority {
    /// The `apns-priority` header's value.
    fn header(self) -> &'static str {
        match self {
            Self::Immediate => "10",
            Self::Normal => "5",
        }
    }
}

/// What APNs said to one send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 200: accepted.
    Sent,
    /// 410: the token is no longer valid; drop it.
    Gone,
    /// Any other HTTP status, or 0 when there was none (curl failed, timed out or never ran, or
    /// the request could not be written).
    Failed(u16),
}

/// The curl config for one request: the URL with `token`, the four headers and `payload` as
/// compact JSON, each a quoted value. `None` when `token` is not lowercase hex or a value holds
/// a control character such as a newline.
fn curl_config(
    url_base: &str,
    jwt: &str,
    token: &str,
    payload: &Value,
    priority: Priority,
) -> Option<String> {
    if !is_activity_token(token) {
        return None;
    }
    let body = serde_json::to_string(payload).ok()?;
    let lines = [
        ("url", format!("{url_base}/3/device/{token}")),
        ("header", format!("apns-topic: {TOPIC}")),
        ("header", "apns-push-type: liveactivity".to_owned()),
        ("header", format!("apns-priority: {}", priority.header())),
        ("header", format!("authorization: bearer {jwt}")),
        ("data", body),
    ];
    let mut config = String::new();
    for (key, value) in lines {
        config.push_str(key);
        config.push_str(" = \"");
        config.push_str(&quoted(&value)?);
        config.push_str("\"\n");
    }
    Some(config)
}

/// `value` escaped for a double-quoted curl config value (`\` and `"`), or `None` if it holds a
/// control character, which could end the line or the value.
fn quoted(value: &str) -> Option<String> {
    if value.chars().any(char::is_control) {
        return None;
    }
    Some(value.replace('\\', r"\\").replace('"', r#"\""#))
}

/// Sends `payload` to the device `token` at `url_base` (see [`ApnsConfig::url_base`]) with
/// `program` (`curl`), signed with `jwt`, and says what APNs answered. argv is
/// [`CURL_ARGS`]; the URL, the headers and the body go on stdin.
pub async fn send(
    program: &str,
    url_base: &str,
    jwt: &str,
    token: &str,
    payload: &Value,
    priority: Priority,
) -> Outcome {
    let Some(config) = curl_config(url_base, jwt, token, payload, priority) else {
        return Outcome::Failed(0);
    };
    let post = async {
        let mut child = Command::new(program)
            .args(CURL_ARGS)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut stdin = child.stdin.take()?;
        stdin.write_all(config.as_bytes()).await.ok()?;
        drop(stdin);
        let output = child.wait_with_output().await.ok()?;
        if !output.status.success() {
            return None;
        }
        std::str::from_utf8(&output.stdout)
            .ok()?
            .parse::<u16>()
            .ok()
    };
    match timeout(SEND_WITHIN, post).await {
        Ok(Some(200)) => Outcome::Sent,
        Ok(Some(410)) => Outcome::Gone,
        Ok(Some(status)) => Outcome::Failed(status),
        _ => Outcome::Failed(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::TempDir;
    use p256::{
        ecdsa::{VerifyingKey, signature::Verifier},
        pkcs8::DecodePublicKey,
    };
    use serde_json::json;
    use std::{os::unix::fs::PermissionsExt, process::Command as StdCommand};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, BufReader},
        net::TcpListener,
    };

    const TEAM: &str = "TEAM123456";
    const KID: &str = "KEY1234567";
    /// The known-answer test's `iat`.
    const IAT: u64 = 1_760_000_000;
    /// base64url of `{"alg":"ES256","kid":"KEY1234567"}`, made apart from the hub (Python's
    /// `base64.urlsafe_b64encode`, padding stripped).
    const HEADER: &str = "eyJhbGciOiJFUzI1NiIsImtpZCI6IktFWTEyMzQ1NjcifQ";
    /// base64url of `{"iss":"TEAM123456","iat":1760000000}`, made the same way.
    const CLAIMS: &str = "eyJpc3MiOiJURUFNMTIzNDU2IiwiaWF0IjoxNzYwMDAwMDAwfQ";
    /// The RFC 6979 signature over `HEADER.CLAIMS` with `tests/data/apns-test.p8`, made apart
    /// from the hub too (Python's `cryptography`, `deterministic_signing=True`) and checked with
    /// `openssl dgst -verify` against `tests/data/apns-test.pub.pem`.
    const SIGNATURE: &str =
        "FUOoVhQ93rdTD5YMXCXIvR6rMS0nvu5uDcL9l1xAPltHZv11yYZLibqdygziwTK1rKrXGHtGqGecg0HgIf0t6Q";
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    /// A file under `tests/data`, as text.
    fn data(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data")
            .join(name);
        std::fs::read_to_string(path).expect("test data")
    }

    /// The PEM block of a test data file, without the note above it.
    fn pem(name: &str) -> String {
        let text = data(name);
        let start = text.find("-----BEGIN").expect("a PEM block");
        text[start..].to_owned()
    }

    /// Writes `text` to `dir/name` with `mode`, and returns its path.
    fn file(dir: &TempDir, name: &str, text: &str, mode: u32) -> PathBuf {
        std::fs::create_dir_all(dir.path()).expect("dir");
        let path = dir.path().join(name);
        std::fs::write(&path, text).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("mode");
        path
    }

    fn config(key_path: PathBuf, sandbox: bool) -> ApnsConfig {
        ApnsConfig {
            team_id: TEAM.into(),
            key_id: KID.into(),
            key_path,
            sandbox,
        }
    }

    /// A signer over the test key, copied into a private file.
    fn test_signer(dir: &TempDir) -> Signer {
        let path = file(dir, "key.p8", &pem("apns-test.p8"), 0o600);
        Signer::try_load(&config(path, false)).expect("the test key loads")
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// Writes an executable stand-in `curl` with `body` after `#!/bin/sh`. It is written by a
    /// child `sh`, so no thread of this test binary holds it open for writing when it runs (a
    /// fork elsewhere in the binary would otherwise make running it fail with `ETXTBSY`).
    fn stand_in(dir: &TempDir, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir.path()).expect("dir");
        let path = dir.path().join("curl");
        let status = StdCommand::new("sh")
            .args([
                "-c",
                r#"printf '#!/bin/sh\n%s\n' "$1" > "$2" && chmod 755 "$2""#,
            ])
            .args(["sh", body])
            .arg(&path)
            .status()
            .expect("sh");
        assert!(status.success());
        path
    }

    /// A stand-in `curl` that records its argv (a line each) and its stdin in `dir`, then prints
    /// `status` and exits with `exit`.
    fn recording_curl(dir: &TempDir, status: &str, exit: u8) -> PathBuf {
        let records = dir.path().display().to_string();
        stand_in(
            dir,
            &format!(
                "printf '%s\\n' \"$@\" > '{records}/argv'\ncat > '{records}/stdin'\n\
                 printf '%s' '{status}'\nexit {exit}"
            ),
        )
    }

    fn record(dir: &TempDir, name: &str) -> Option<String> {
        std::fs::read_to_string(dir.path().join(name)).ok()
    }

    #[test]
    fn base64url_matches_rfc_4648() {
        // RFC 4648 §10, without padding.
        for (input, output) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64url(input.as_bytes()), output, "{input:?}");
        }
        // The URL-safe alphabet: `-` and `_` for 62 and 63, where standard base64 has `+` and `/`.
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url(&[0xfb, 0xef, 0xbe]), "----");
        assert_eq!(base64url(&[0xff, 0xff, 0xff]), "____");
        assert_eq!(base64url(&[0, 0, 0]), "AAAA");
    }

    #[test]
    fn the_jwt_is_a_known_answer() {
        let dir = TempDir::new("apns-kat");
        let mut signer = test_signer(&dir);
        let jwt = signer.jwt(at(IAT)).to_owned();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "{jwt}");
        assert_eq!(parts[0], HEADER);
        assert_eq!(parts[1], CLAIMS);
        // A raw r‖s signature, not DER: 64 bytes are 86 characters.
        assert_eq!(parts[2].len(), 86);
        let public = VerifyingKey::from_public_key_pem(&pem("apns-test.pub.pem")).expect("public");
        let raw = decode(parts[2]);
        let signature = Signature::from_slice(&raw).expect("64 bytes");
        public
            .verify(format!("{HEADER}.{CLAIMS}").as_bytes(), &signature)
            .expect("the signature verifies");
        assert_eq!(parts[2], SIGNATURE);
    }

    /// Decodes unpadded base64url, for the test only.
    fn decode(text: &str) -> Vec<u8> {
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let values: Vec<u32> = text
            .bytes()
            .map(|b| {
                let at = ALPHABET.iter().position(|a| *a == b).expect("base64url");
                u32::try_from(at).expect("small")
            })
            .collect();
        let mut out = Vec::new();
        for chunk in values.chunks(4) {
            let n = chunk
                .iter()
                .chain(std::iter::repeat(&0))
                .take(4)
                .fold(0, |n, v| n << 6 | v);
            let bytes = n.to_be_bytes();
            out.extend_from_slice(&bytes[1..chunk.len()]);
        }
        out
    }

    #[test]
    fn the_jwt_is_reused_under_50_minutes_and_remade_after() {
        let dir = TempDir::new("apns-jwt-age");
        let mut signer = test_signer(&dir);
        let first = signer.jwt(at(IAT)).to_owned();
        let almost = IAT + 50 * 60 - 1;
        assert_eq!(signer.jwt(at(almost)), first);
        let second = signer.jwt(at(IAT + 50 * 60)).to_owned();
        assert_ne!(second, first);
        assert!(second.contains(&base64url(
            format!(r#"{{"iss":"{TEAM}","iat":{}}}"#, IAT + 50 * 60).as_bytes()
        )));
        // Kept again from its own making.
        assert_eq!(signer.jwt(at(IAT + 99 * 60)), second);
        // A clock that moved back past it makes a new one.
        let back = signer.jwt(at(IAT)).to_owned();
        assert_eq!(back, first);
        assert_ne!(back, second);
    }

    #[test]
    fn bad_keys_turn_apns_off() {
        let dir = TempDir::new("apns-bad-keys");
        let good = pem("apns-test.p8");
        let real = file(&dir, "real.p8", &good, 0o600);
        let cases: Vec<(&str, PathBuf, &str)> = vec![
            (
                "world-readable",
                file(&dir, "open.p8", &good, 0o644),
                KEY_MODE,
            ),
            (
                "owner read only",
                file(&dir, "ro.p8", &good, 0o400),
                KEY_MODE,
            ),
            (
                "group readable",
                file(&dir, "group.p8", &good, 0o640),
                KEY_MODE,
            ),
            ("symlink", symlink(&dir, &real), KEY_SYMLINK),
            ("missing", dir.path().join("nothing.p8"), KEY_MISSING),
            ("directory", dir.path().to_path_buf(), KEY_NOT_FILE),
            ("fifo", fifo(&dir), KEY_NOT_FILE),
            (
                "P-384",
                file(&dir, "p384.p8", &pem("apns-p384-test.p8"), 0o600),
                KEY_NOT_P256,
            ),
            (
                "SEC1, not PKCS#8",
                file(&dir, "sec1.p8", &pem("apns-sec1-test.pem"), 0o600),
                KEY_NOT_P256,
            ),
            (
                "garbage",
                file(&dir, "garbage.p8", "not a key\n", 0o600),
                KEY_NOT_P256,
            ),
            ("empty", file(&dir, "empty.p8", "", 0o600), KEY_NOT_P256),
        ];
        for (case, path, line) in cases {
            let config = config(path.clone(), false);
            let error = Signer::try_load(&config).expect_err(case);
            assert_eq!(error, line, "{case}");
            assert!(!error.contains(&*dir.path().to_string_lossy()), "{case}");
            assert!(Signer::load(&config).is_none(), "{case}");
        }
        // The mode line is exactly what the plan says.
        assert_eq!(KEY_MODE, "APNs key must be 0600");
        // A config built by hand with an id that could break the JWT's JSON is refused too.
        let mut forged = config(real.clone(), false);
        forged.key_id = r#"K","x":"1"#.into();
        assert_eq!(Signer::try_load(&forged).expect_err("id"), IDS_MALFORMED);
        // And the good key, through the same path, loads.
        assert!(Signer::load(&config(real, true)).is_some());
    }

    fn symlink(dir: &TempDir, target: &Path) -> PathBuf {
        let path = dir.path().join("link.p8");
        std::os::unix::fs::symlink(target, &path).expect("symlink");
        path
    }

    fn fifo(dir: &TempDir) -> PathBuf {
        let path = dir.path().join("fifo.p8");
        let status = StdCommand::new("mkfifo")
            .arg("-m")
            .arg("600")
            .arg(&path)
            .status()
            .expect("mkfifo");
        assert!(status.success());
        path
    }

    #[test]
    fn the_signer_never_prints_its_key_or_jwt() {
        let dir = TempDir::new("apns-debug");
        let mut signer = test_signer(&dir);
        let jwt = signer.jwt(at(IAT)).to_owned();
        let printed = format!("{signer:?} {signer:#?}");
        let signature = jwt.rsplit('.').next().expect("signature");
        assert!(!printed.contains(signature), "{printed}");
        assert!(!printed.contains(&jwt), "{printed}");
        let scalar = signer
            .key
            .to_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .concat();
        assert!(!printed.contains(&scalar), "{printed}");
        assert!(printed.contains("<redacted>"), "{printed}");
        assert!(printed.contains(TEAM) && printed.contains(KID), "{printed}");
    }

    #[test]
    fn the_config_is_read_strictly_and_names_no_value() {
        let good = json!({"team_id": TEAM, "key_id": KID, "key_path": "/k/AuthKey.p8",
                          "sandbox": true});
        assert_eq!(
            ApnsConfig::from_hub_json(&good),
            Ok(Some(config("/k/AuthKey.p8".into(), true)))
        );
        assert_eq!(ApnsConfig::from_hub_json(&Value::Null), Ok(None));
        let with = |field: &str, value: Value| {
            let mut apns = good.clone();
            apns[field] = value;
            apns
        };
        let without = |field: &str| {
            let mut apns = good.clone();
            apns.as_object_mut().expect("object").remove(field);
            apns
        };
        let cases = [
            ("team_id", with("team_id", json!("team123456"))),
            ("team_id", with("team_id", json!("TEAM12345"))),
            ("team_id", with("team_id", json!("TEAM1234567"))),
            ("team_id", with("team_id", json!("TEAM-23456"))),
            ("team_id", with("team_id", json!(1_234_567_890))),
            ("team_id", without("team_id")),
            ("key_id", with("key_id", json!("KEY123456É"))),
            ("key_id", with("key_id", json!("KEY 234567"))),
            ("key_id", without("key_id")),
            ("key_path", with("key_path", json!("keys/AuthKey.p8"))),
            ("key_path", with("key_path", json!(""))),
            ("key_path", with("key_path", json!(["/k"]))),
            ("key_path", without("key_path")),
            ("sandbox", with("sandbox", json!("yes"))),
            ("sandbox", without("sandbox")),
        ];
        for (field, apns) in cases {
            let line = ApnsConfig::from_hub_json(&apns).expect_err(field);
            assert_eq!(
                line,
                format!("APNs is off: hub.json apns.{field} is malformed")
            );
            for value in apns.as_object().expect("object").values() {
                if let Some(text) = value.as_str().filter(|text| !text.is_empty()) {
                    assert!(!line.contains(text), "{line} names {text}");
                }
            }
        }
        for apns in [json!("x"), json!([]), json!(true)] {
            assert_eq!(
                ApnsConfig::from_hub_json(&apns),
                Err("APNs is off: hub.json apns must be an object".to_owned())
            );
        }
    }

    #[test]
    fn the_sandbox_has_its_own_host() {
        let production = config("/k".into(), false);
        let sandbox = config("/k".into(), true);
        assert_eq!(production.host(), "api.push.apple.com");
        assert_eq!(sandbox.host(), "api.sandbox.push.apple.com");
        assert_eq!(
            production.url_base_where(false, None),
            "https://api.push.apple.com"
        );
        assert_eq!(
            sandbox.url_base_where(false, None),
            "https://api.sandbox.push.apple.com"
        );
        let request = curl_config(
            &sandbox.url_base_where(false, None),
            "j",
            TOKEN,
            &json!({}),
            Priority::Normal,
        )
        .expect("config");
        assert!(request.starts_with(&format!(
            "url = \"https://api.sandbox.push.apple.com/3/device/{TOKEN}\"\n"
        )));
    }

    #[test]
    fn the_seam_is_loopback_only_and_debug_only() {
        let apns = config("/k".into(), false);
        let real = "https://api.push.apple.com";
        assert_eq!(apns.url_base_where(true, None), real);
        assert_eq!(
            apns.url_base_where(true, Some("http://127.0.0.1:8080")),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            apns.url_base_where(true, Some("http://127.0.0.1:1")),
            "http://127.0.0.1:1"
        );
        for seam in [
            "",
            "http://127.0.0.1",
            "http://127.0.0.1:",
            "http://127.0.0.1:123456",
            "http://127.0.0.1:8080/",
            "http://127.0.0.1:8080/3",
            "http://127.0.0.1:80@evil.test",
            "http://127.0.0.1:80 -o /tmp/x",
            "http://127.0.0.1:80\n",
            "https://127.0.0.1:8080",
            "http://localhost:8080",
            "http://127.0.0.2:8080",
            "https://example.com",
        ] {
            assert_eq!(apns.url_base_where(true, Some(seam)), real, "{seam:?}");
        }
        // A release build never honours it.
        assert_eq!(
            apns.url_base_where(false, Some("http://127.0.0.1:8080")),
            real
        );
        assert_eq!(SEAM_HONOURED, cfg!(debug_assertions));
    }

    #[tokio::test]
    async fn secrets_only_on_stdin() {
        let dir = TempDir::new("apns-stdin");
        let curl = recording_curl(&dir, "200", 0);
        let jwt = test_signer(&dir).jwt(at(IAT)).to_owned();
        // A body with a quote, a backslash and an escaped newline in a string.
        let payload = json!({"aps": {"event": "update", "note": "a\"b\\c\nd"}});
        let outcome = send(
            curl.to_str().expect("utf-8"),
            "https://api.push.apple.com",
            &jwt,
            TOKEN,
            &payload,
            Priority::Immediate,
        )
        .await;
        assert_eq!(outcome, Outcome::Sent);
        let argv = record(&dir, "argv").expect("curl ran");
        assert_eq!(argv.lines().collect::<Vec<_>>(), CURL_ARGS);
        assert!(!argv.contains(TOKEN) && !argv.contains(&jwt), "{argv}");
        assert!(!argv.contains("apple.com"), "{argv}");
        let stdin = record(&dir, "stdin").expect("stdin");
        let expected = format!(
            "url = \"https://api.push.apple.com/3/device/{TOKEN}\"\n\
             header = \"apns-topic: dev.octet.mobile.push-type.liveactivity\"\n\
             header = \"apns-push-type: liveactivity\"\n\
             header = \"apns-priority: 10\"\n\
             header = \"authorization: bearer {jwt}\"\n\
             data = \"{{\\\"aps\\\":{{\\\"event\\\":\\\"update\\\",\
             \\\"note\\\":\\\"a\\\\\\\"b\\\\\\\\c\\\\nd\\\"}}}}\"\n"
        );
        assert_eq!(stdin, expected);
        // Every line is one `key = "value"`: no value holds a newline.
        for line in stdin.lines() {
            let (key, value) = line.split_once(" = ").expect("key = value");
            assert!(["url", "header", "data"].contains(&key), "{line}");
            assert!(value.starts_with('"') && value.ends_with('"'), "{line}");
        }
        assert_eq!(stdin.lines().count(), 6);
        assert_eq!(stdin.matches('\n').count(), 6);
        // The other priority.
        let dir = TempDir::new("apns-stdin-normal");
        let curl = recording_curl(&dir, "200", 0);
        let program = curl.to_str().expect("utf-8");
        send(
            program,
            "https://h",
            "j",
            TOKEN,
            &json!({}),
            Priority::Normal,
        )
        .await;
        let stdin = record(&dir, "stdin").expect("stdin");
        assert!(stdin.contains("header = \"apns-priority: 5\"\n"), "{stdin}");
    }

    #[tokio::test]
    async fn a_value_that_could_break_a_line_or_a_bad_token_is_never_sent() {
        let dir = TempDir::new("apns-refused");
        let curl = recording_curl(&dir, "200", 0);
        let program = curl.to_str().expect("utf-8");
        let body = json!({});
        for (base, jwt, token) in [
            ("https://h", "a\nb", TOKEN),
            ("https://h", "a\rb", TOKEN),
            ("https://h", "a\0b", TOKEN),
            ("https://h\n", "j", TOKEN),
            ("https://h", "j", "ABCDEF"),
            ("https://h", "j", "abc\n"),
            ("https://h", "j", ""),
            ("https://h", "j", "../x"),
        ] {
            let outcome = send(program, base, jwt, token, &body, Priority::Normal).await;
            assert_eq!(outcome, Outcome::Failed(0), "{jwt:?} {token:?}");
        }
        assert_eq!(record(&dir, "argv"), None, "curl never ran");
    }

    #[tokio::test]
    async fn apns_status_handling() {
        let body = json!({"aps": {}});
        // One stand-in for every case (a new executable is slow to start the first time on
        // macOS): it answers what the files beside it say.
        let dir = TempDir::new("apns-status");
        let answers = dir.path().display().to_string();
        let curl = stand_in(
            &dir,
            &format!("cat > /dev/null\ncat '{answers}/status'\nexit \"$(cat '{answers}/exit')\""),
        );
        let program = curl.to_str().expect("utf-8");
        for (status, exit, outcome) in [
            ("200", 0, Outcome::Sent),
            ("410", 0, Outcome::Gone),
            ("400", 0, Outcome::Failed(400)),
            ("403", 0, Outcome::Failed(403)),
            ("429", 0, Outcome::Failed(429)),
            ("500", 0, Outcome::Failed(500)),
            // curl could not connect, or timed out.
            ("000", 7, Outcome::Failed(0)),
            ("000", 28, Outcome::Failed(0)),
            // A 410 from a curl that then failed is not trusted.
            ("410", 56, Outcome::Failed(0)),
            ("", 0, Outcome::Failed(0)),
            ("nope", 0, Outcome::Failed(0)),
        ] {
            std::fs::write(dir.path().join("status"), status).expect("status");
            std::fs::write(dir.path().join("exit"), exit.to_string()).expect("exit");
            let answer = send(program, "https://h", "j", TOKEN, &body, Priority::Normal).await;
            assert_eq!(answer, outcome, "{status} exit {exit}");
        }
        // No curl at all, and a real curl with nothing listening.
        let missing = send(
            "/nonexistent/curl",
            "https://h",
            "j",
            TOKEN,
            &body,
            Priority::Normal,
        );
        assert_eq!(missing.await, Outcome::Failed(0));
        let real = send(
            "curl",
            "http://127.0.0.1:1",
            "j",
            TOKEN,
            &body,
            Priority::Normal,
        );
        assert_eq!(real.await, Outcome::Failed(0));
    }

    /// One request as a loopback HTTP/1.1 server saw it: the head's lines and the body.
    async fn serve_one(listener: TcpListener, status: &str) -> (Vec<String>, String) {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut reader = BufReader::new(stream);
        let mut head = Vec::new();
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.expect("line");
            let line = line.trim_end().to_owned();
            if line.is_empty() {
                break;
            }
            head.push(line);
        }
        let length: usize = head
            .iter()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().ok())?
            })
            .expect("a content-length");
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await.expect("body");
        let answer = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        reader
            .into_inner()
            .write_all(answer.as_bytes())
            .await
            .expect("answer");
        (head, String::from_utf8(body).expect("utf-8"))
    }

    #[tokio::test]
    async fn the_real_curl_falls_back_to_http1_on_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let server = tokio::spawn(serve_one(listener, "410 Gone"));
        let payload = json!({"aps": {"event": "end", "note": "a\"b\\c\nd"}});
        let outcome = send("curl", &base, "h.c.s", TOKEN, &payload, Priority::Normal).await;
        assert_eq!(outcome, Outcome::Gone);
        let (head, body) = server.await.expect("server");
        assert_eq!(head[0], format!("POST /3/device/{TOKEN} HTTP/1.1"));
        for header in [
            "apns-topic: dev.octet.mobile.push-type.liveactivity",
            "apns-push-type: liveactivity",
            "apns-priority: 5",
            "authorization: bearer h.c.s",
        ] {
            assert!(head.iter().any(|line| line == header), "{header}: {head:?}");
        }
        assert_eq!(body, serde_json::to_string(&payload).expect("json"));
    }
}
