//! Devices, pairing secrets, hub config and the audit log.
use octet_store::{create_private, create_private_dir, sync_directory};
use sha2::{Digest, Sha256};
use std::{
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
};

/// The loopback port when `hub.json` names none.
const DEFAULT_PORT: u16 = 47823;
/// How long a pairing secret works.
const SECRET_LIFETIME: Duration = Duration::from_mins(10);
/// Pairing secrets that can wait at once; the oldest give way.
const SECRETS_KEPT: usize = 16;
/// Longest device name or platform, in characters.
const NAME_LIMIT: usize = 64;
/// How stale `last_seen` may get on disk before a connection rewrites it.
const SEEN_WRITE_GAP: u64 = 60;
/// Shown when a pairing secret is wrong, used or old.
pub(crate) const SECRET_REFUSED: &str = "Pairing code already used or expired";

/// A paired phone.
#[derive(Clone, PartialEq, Eq)]
pub struct Device {
    /// The name the phone gave at pairing.
    pub name: String,
    /// The phone's platform.
    pub platform: String,
    /// The tailnet login seen at pairing.
    pub login: String,
    /// When it paired, in seconds since the epoch.
    pub created: u64,
    /// When it last connected, in seconds since the epoch.
    pub last_seen: u64,
    /// The hex SHA-256 of its token; the token itself is never stored.
    pub token_sha256: String,
    /// The Expo push token notifications go to, if the phone turned them on.
    pub push_token: Option<String>,
}

/// Shows the device without its token hash or push token.
impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device")
            .field("name", &self.name)
            .field("platform", &self.platform)
            .field("login", &self.login)
            .field("created", &self.created)
            .field("last_seen", &self.last_seen)
            .field("token_sha256", &"<redacted>")
            .field(
                "push_token",
                &self.push_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The hub's settings from `hub.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubConfig {
    /// The loopback port.
    pub port: u16,
    /// The workspaces a phone may start sessions in.
    pub workspaces: Vec<PathBuf>,
}

/// A file's mtime and size.
type Stamp = (SystemTime, u64);

/// A pairing secret as the hub remembers it: its hash and when it was issued.
#[derive(Debug)]
struct Secret {
    /// The hex SHA-256 of the secret; the secret itself is not kept.
    sha256: String,
    /// When the secret was issued.
    issued: SystemTime,
}

/// The hub's files in the data directory: `devices.json`, `hub.json` and
/// `hub-audit.jsonl`, all owner-only.
#[derive(Debug)]
pub struct Store {
    /// The data directory holding the hub's files.
    directory: PathBuf,
    /// The paired devices, as last read or written.
    devices: Vec<Device>,
    /// The settings read from `hub.json` when the store opened.
    config: HubConfig,
    /// Pairing secrets waiting to be used.
    secrets: Vec<Secret>,
    /// The mtime and size of `devices.json` as this store last wrote or read
    /// it; the size catches a change within the clock's granularity.
    stamp: Option<Stamp>,
}

impl Store {
    /// Opens the store in `directory`, creating it owner-only if missing.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created or tightened to 0700, or
    /// `devices.json` or `hub.json` cannot be read or are malformed.
    pub async fn open(directory: &Path) -> io::Result<Store> {
        create_private_dir(directory).await?;
        if fs::metadata(directory).await?.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let (devices, stamp) = read_devices(directory).await?;
        Ok(Store {
            directory: directory.to_owned(),
            devices,
            config: read_config(directory).await?,
            secrets: Vec::new(),
            stamp,
        })
    }

    /// A new pairing secret: 32 random bytes in hex, good for 10 minutes and
    /// one pairing. Only its hash is kept.
    ///
    /// # Panics
    ///
    /// Panics if the operating system has no random bytes to give.
    pub fn pairing_secret(&mut self, now: SystemTime) -> String {
        let secret = random_hex();
        self.secrets.retain(|kept| !expired(kept.issued, now));
        if self.secrets.len() >= SECRETS_KEPT {
            self.secrets.remove(0);
        }
        self.secrets.push(Secret {
            sha256: sha256_hex(&secret),
            issued: now,
        });
        secret
    }

    /// Pairs a phone: spends `secret` and returns the new device token, which
    /// is shown once and not stored, and the device. A refusal that is the
    /// phone's to fix leaves the secret unspent.
    ///
    /// # Errors
    ///
    /// Fails with the reason to show the phone when the secret is unknown,
    /// used or older than 10 minutes, a field is empty or too long, the name
    /// is taken, or the device cannot be saved.
    pub async fn pair(
        &mut self,
        secret: &str,
        name: &str,
        platform: &str,
        login: &str,
        now: SystemTime,
    ) -> Result<(String, Device), &'static str> {
        let hash = sha256_hex(secret);
        let position = self
            .secrets
            .iter()
            .position(|kept| same(&kept.sha256, &hash) && !expired(kept.issued, now))
            .ok_or(SECRET_REFUSED)?;
        let name = checked_text(
            name,
            "Device name must be 1 to 64 characters",
            "Device name can't contain control characters",
        )?;
        let platform = checked_text(
            platform,
            "Device platform must be 1 to 64 characters",
            "Device platform can't contain control characters",
        )?;
        if login.is_empty() {
            return Err("Tailnet login is missing");
        }
        let token = random_hex();
        let seconds = seconds(now);
        let device = Device {
            name: name.to_owned(),
            platform: platform.to_owned(),
            login: login.to_owned(),
            created: seconds,
            last_seen: seconds,
            token_sha256: sha256_hex(&token),
            push_token: None,
        };
        self.update(|devices| {
            if devices.iter().any(|kept| kept.name == device.name) {
                return (Err("A device with that name is already paired"), false);
            }
            devices.push(device.clone());
            (Ok(()), true)
        })
        .await
        .map_err(|_| "Could not save the device")??;
        self.secrets.swap_remove(position);
        Ok((token, device))
    }

    /// The device whose token this is, if it also connects from the login it
    /// paired with. Records the time it was seen.
    pub async fn authenticate(
        &mut self,
        token: &str,
        login: &str,
        now: SystemTime,
    ) -> Option<Device> {
        let hash = sha256_hex(token);
        // Every device is compared, so the time taken does not say which matched.
        let mut found = None;
        for (index, device) in self.devices.iter().enumerate() {
            if same(&device.token_sha256, &hash) {
                found = Some(index);
            }
        }
        let device = self.devices.get_mut(found?)?;
        if device.login != login {
            return None;
        }
        let seen = seconds(now).max(device.last_seen);
        let stale = seen >= device.last_seen.saturating_add(SEEN_WRITE_GAP);
        device.last_seen = seen;
        let device = device.clone();
        if stale {
            // The write re-reads the file, so a device revoked elsewhere is
            // refused here; a failed write only leaves last_seen older on disk.
            let written = self
                .update(|devices| {
                    let Some(kept) = devices
                        .iter_mut()
                        .find(|kept| kept.token_sha256 == device.token_sha256)
                    else {
                        return (false, false);
                    };
                    kept.last_seen = kept.last_seen.max(seen);
                    (true, true)
                })
                .await;
            if matches!(written, Ok(false)) {
                return None;
            }
        }
        Some(device)
    }

    /// Deletes the device called `name`; false if there is none or the file
    /// cannot be written.
    pub async fn revoke(&mut self, name: &str) -> bool {
        self.update(|devices| {
            let before = devices.len();
            devices.retain(|device| device.name != name);
            let removed = devices.len() != before;
            (removed, removed)
        })
        .await
        .unwrap_or(false)
    }

    /// Sets, or with `None` clears, the push token of the device whose token hash is
    /// `token_sha256`. False if there is no such device or the file cannot be written.
    pub async fn set_push_token(&mut self, token_sha256: &str, token: Option<String>) -> bool {
        self.update(|devices| {
            let Some(device) = devices
                .iter_mut()
                .find(|device| device.token_sha256 == token_sha256)
            else {
                return (false, false);
            };
            let changed = device.push_token != token;
            device.push_token = token;
            (true, changed)
        })
        .await
        .unwrap_or(false)
    }

    /// The paired devices.
    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    /// Re-reads `devices.json` if another process (`octet hub revoke`)
    /// changed it; true if the devices were replaced.
    pub async fn reload_if_changed(&mut self) -> bool {
        let stamp = file_stamp(&self.directory.join("devices.json")).await;
        if stamp == self.stamp {
            return false;
        }
        match read_devices(&self.directory).await {
            Ok((devices, stamp)) => {
                self.devices = devices;
                self.stamp = stamp;
                true
            }
            Err(_) => false,
        }
    }

    /// The hub's settings.
    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    /// Appends one `{time, device, session, what}` line to `hub-audit.jsonl`.
    /// `what` names the action, never its content: no prompt text, tokens or
    /// secrets. A line that cannot be written is dropped.
    pub async fn audit(&self, device: &str, session: &str, what: &str) {
        let line = serde_json::json!({
            "time": seconds(SystemTime::now()),
            "device": device,
            "session": session,
            "what": what,
        });
        let Ok(mut bytes) = serde_json::to_vec(&line) else {
            return;
        };
        bytes.push(b'\n');
        let Ok(mut file) = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(self.directory.join("hub-audit.jsonl"))
            .await
        else {
            return;
        };
        // One write per line keeps lines whole when the hub and a CLI append.
        if file.write_all(&bytes).await.is_ok() {
            let _ = file.flush().await;
        }
    }

    /// Changes `devices.json` under an exclusive lock on `devices.lock`:
    /// re-reads the file, applies `change` to what is there, and writes the
    /// result if `change` says (the second value) it changed something. The
    /// list in memory becomes what is on disk, so another process's changes
    /// are never written back over.
    async fn update<R>(
        &mut self,
        change: impl FnOnce(&mut Vec<Device>) -> (R, bool),
    ) -> io::Result<R> {
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.directory.join("devices.lock"))
            .await?
            .into_std()
            .await;
        // The lock is released when `lock` drops, even if this future does.
        let lock = tokio::task::spawn_blocking(move || lock.lock().map(|()| lock))
            .await
            .map_err(io::Error::other)??;
        let (mut devices, stamp) = read_devices(&self.directory).await?;
        let (result, write) = change(&mut devices);
        if write {
            let devices: Vec<_> = devices
                .iter()
                .map(|device| {
                    let mut entry = serde_json::json!({
                        "name": device.name,
                        "platform": device.platform,
                        "login": device.login,
                        "created": device.created,
                        "last_seen": device.last_seen,
                        "token_sha256": device.token_sha256,
                    });
                    if let Some(push) = &device.push_token {
                        entry["push_token"] = serde_json::json!(push);
                    }
                    entry
                })
                .collect::<Vec<_>>();
            let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({ "devices": devices }))?;
            bytes.push(b'\n');
            write_private(&self.directory, "devices.json", &bytes).await?;
            self.stamp = file_stamp(&self.directory.join("devices.json")).await;
        } else {
            self.stamp = stamp;
        }
        self.devices = devices;
        drop(lock);
        Ok(result)
    }
}

/// Writes `name` in `directory` owner-only: a temp file, synced, then renamed.
async fn write_private(directory: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!("{name}.{}.{count}.tmp", std::process::id()));
    match fs::remove_file(&temporary).await {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let mut file = create_private(&temporary).await?;
    let written = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await
    }
    .await;
    drop(file);
    if let Err(error) = written {
        let _ = fs::remove_file(&temporary).await;
        return Err(error);
    }
    if let Err(error) = fs::rename(&temporary, directory.join(name)).await {
        let _ = fs::remove_file(&temporary).await;
        return Err(error);
    }
    sync_directory(directory).await
}

/// The mtime and size of `path`, if it exists.
async fn file_stamp(path: &Path) -> Option<Stamp> {
    let metadata = fs::metadata(path).await.ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// An unreadable-data error that says what was wrong.
fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

/// The devices in `devices.json` (none if it is missing) and its stamp.
async fn read_devices(directory: &Path) -> io::Result<(Vec<Device>, Option<Stamp>)> {
    let path = directory.join("devices.json");
    let bytes = match fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(error) => return Err(error),
    };
    let stamp = file_stamp(&path).await;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("devices.json is not valid JSON"))?;
    let list = value["devices"]
        .as_array()
        .ok_or_else(|| invalid("devices.json has no devices list"))?;
    let devices = list
        .iter()
        .map(|entry| {
            let text = |key: &str| entry[key].as_str().map(str::to_owned);
            Some(Device {
                name: text("name")?,
                platform: text("platform")?,
                login: text("login")?,
                created: entry["created"].as_u64()?,
                last_seen: entry["last_seen"].as_u64()?,
                token_sha256: text("token_sha256")?,
                push_token: text("push_token"),
            })
            .filter(|device| {
                !device.login.is_empty()
                    && device.token_sha256.len() == 64
                    && device
                        .token_sha256
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid("devices.json has a malformed device"))?;
    Ok((devices, stamp))
}

/// The settings in `hub.json`; defaults if it is missing.
async fn read_config(directory: &Path) -> io::Result<HubConfig> {
    let mut config = HubConfig {
        port: DEFAULT_PORT,
        workspaces: Vec::new(),
    };
    let bytes = match fs::read(directory.join("hub.json")).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(config),
        Err(error) => return Err(error),
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| invalid("hub.json is not valid JSON"))?;
    if !value["port"].is_null() {
        config.port = value["port"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or_else(|| invalid("hub.json port must be 1 to 65535"))?;
    }
    if !value["workspaces"].is_null() {
        config.workspaces = value["workspaces"]
            .as_array()
            .and_then(|list| {
                list.iter()
                    .map(|path| path.as_str().map(PathBuf::from))
                    .collect()
            })
            .ok_or_else(|| invalid("hub.json workspaces must be a list of paths"))?;
    }
    Ok(config)
}

/// Whether a secret issued at `issued` is past its 10 minutes at `now`; a
/// `now` before `issued` (the clock moved back) counts as expired.
fn expired(issued: SystemTime, now: SystemTime) -> bool {
    !now.duration_since(issued)
        .is_ok_and(|age| age <= SECRET_LIFETIME)
}

/// `text` trimmed, or why it cannot name a device: empty, over 64 characters
/// (`length`) or holding a control character (`control`).
fn checked_text<'a>(
    text: &'a str,
    length: &'static str,
    control: &'static str,
) -> Result<&'a str, &'static str> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > NAME_LIMIT {
        Err(length)
    } else if text.chars().any(char::is_control) {
        Err(control)
    } else {
        Ok(text)
    }
}

/// Whole seconds since the epoch (0 before it).
fn seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// 32 random bytes as 64 hex digits.
fn random_hex() -> String {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).expect("the operating system gave no random bytes");
    hex(&bytes)
}

/// Lowercase hex digits of `bytes`.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The hex SHA-256 of `text`.
fn sha256_hex(text: &str) -> String {
    hex(Sha256::digest(text.as_bytes()).as_slice())
}

/// Equality that takes the same time wherever the texts differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0_u8, |difference, (x, y)| difference | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use octet_testkit::TempDir;
    use std::{os::unix::fs::PermissionsExt, time::Duration};

    const LOGIN: &str = "me@example.com";

    async fn store(dir: &TempDir) -> Store {
        Store::open(dir.path()).await.unwrap()
    }

    fn epoch(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    async fn paired(store: &mut Store, name: &str, now: SystemTime) -> String {
        let secret = store.pairing_secret(now);
        store
            .pair(&secret, name, "ios", LOGIN, now)
            .await
            .unwrap()
            .0
    }

    #[tokio::test]
    async fn a_pairing_secret_works_once_and_expires() {
        let dir = TempDir::new("hub-pair");
        let mut store = store(&dir).await;
        let now = epoch(1_000_000);
        let secret = store.pairing_secret(now);
        assert_eq!(secret.len(), 64);
        let (token, device) = store
            .pair(&secret, "Gemini 10", "ios", LOGIN, now)
            .await
            .unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(
            (
                device.name.as_str(),
                device.platform.as_str(),
                device.login.as_str()
            ),
            ("Gemini 10", "ios", LOGIN)
        );
        assert_eq!((device.created, device.last_seen), (1_000_000, 1_000_000));
        let refusal = Err("Pairing code already used or expired");
        assert_eq!(
            store
                .pair(&secret, "Again", "ios", LOGIN, now)
                .await
                .map(|_| ()),
            refusal
        );
        let stale = store.pairing_secret(now);
        let late = now + Duration::from_mins(11);
        assert_eq!(
            store
                .pair(&stale, "Late", "ios", LOGIN, late)
                .await
                .map(|_| ()),
            refusal
        );
        let nearly = store.pairing_secret(now);
        let edge = now + Duration::from_mins(10);
        assert!(
            store
                .pair(&nearly, "Edge", "ios", LOGIN, edge)
                .await
                .is_ok()
        );
        assert_eq!(
            store
                .pair("0", "Guess", "ios", LOGIN, now)
                .await
                .map(|_| ()),
            refusal
        );
        assert_eq!(store.devices().len(), 2);
    }

    #[tokio::test]
    async fn a_pairing_refusal_keeps_the_secret_for_a_retry() {
        let dir = TempDir::new("hub-retry");
        let mut store = store(&dir).await;
        let now = epoch(5);
        let first = paired(&mut store, "Phone", now).await;
        assert_eq!(first.len(), 64);
        let secret = store.pairing_secret(now);
        assert!(
            store
                .pair(&secret, "Phone", "ios", LOGIN, now)
                .await
                .is_err()
        );
        assert!(store.pair(&secret, "", "ios", LOGIN, now).await.is_err());
        assert!(store.pair(&secret, "Other", "ios", "", now).await.is_err());
        assert!(
            store
                .pair(&secret, "Other", "ios", LOGIN, now)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn a_token_needs_its_pairing_login() {
        let dir = TempDir::new("hub-login");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(100)).await;
        let device = store.authenticate(&token, LOGIN, epoch(200)).await.unwrap();
        assert_eq!(device.name, "Gemini 10");
        assert_eq!(device.last_seen, 200);
        assert_eq!(store.devices()[0].last_seen, 200);
        assert!(
            store
                .authenticate(&token, "other@example.com", epoch(300))
                .await
                .is_none()
        );
        assert!(
            store
                .authenticate(&"0".repeat(64), LOGIN, epoch(300))
                .await
                .is_none()
        );
        assert!(store.authenticate("", LOGIN, epoch(300)).await.is_none());
        assert_eq!(store.devices()[0].last_seen, 200);
    }

    #[tokio::test]
    async fn revoking_removes_a_device_and_the_file_says_so() {
        let dir = TempDir::new("hub-revoke");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(1)).await;
        let file = dir.path().join("devices.json");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("Gemini 10") && !text.contains(&token));
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(Store::open(dir.path()).await.unwrap().devices().len(), 1);
        assert!(!store.revoke("Nobody").await);
        assert!(store.revoke("Gemini 10").await);
        assert!(store.authenticate(&token, LOGIN, epoch(2)).await.is_none());
        let reopened = Store::open(dir.path()).await.unwrap();
        assert!(reopened.devices().is_empty());
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!std::fs::read_to_string(&file).unwrap().contains(&token));
    }

    const PUSH: &str = "ExpoPushToken[Zk3_secretvalue-77]";

    #[tokio::test]
    async fn push_tokens_are_kept_private() {
        let dir = TempDir::new("hub-push");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(1)).await;
        let device = store.authenticate(&token, LOGIN, epoch(2)).await.unwrap();
        assert_eq!(device.push_token, None);

        // Setting stores it in the 0600 file as `push_token`, and it reads back.
        assert!(
            store
                .set_push_token(&device.token_sha256, Some(PUSH.to_owned()))
                .await
        );
        let file = dir.path().join("devices.json");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"push_token\""), "{text}");
        assert!(text.contains(PUSH), "{text}");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(reopened.devices()[0].push_token.as_deref(), Some(PUSH));
        let seen = store.authenticate(&token, LOGIN, epoch(3)).await.unwrap();
        assert_eq!(seen.push_token.as_deref(), Some(PUSH));

        // Debug shows neither the push token nor the token hash.
        let shown = format!("{:?}", reopened.devices()[0]);
        assert!(!shown.contains("secretvalue"), "{shown}");
        assert!(!shown.contains(&device.token_sha256), "{shown}");
        assert!(
            shown.contains("Gemini 10"),
            "the name is not a secret: {shown}"
        );
        assert!(shown.contains("redacted"), "{shown}");

        // Clearing drops the key; another device is not touched.
        let other = paired(&mut store, "Other", epoch(4)).await;
        assert!(store.set_push_token(&device.token_sha256, None).await);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains("push_token") && !text.contains(PUSH),
            "{text}"
        );
        assert!(store.authenticate(&other, LOGIN, epoch(5)).await.is_some());
        assert!(
            store
                .set_push_token(&device.token_sha256, Some(PUSH.to_owned()))
                .await
        );

        // An unknown device is reported, and a revoke drops the token with the device.
        assert!(
            !store
                .set_push_token(&"0".repeat(64), Some(PUSH.to_owned()))
                .await
        );
        assert!(store.revoke("Gemini 10").await);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains(PUSH) && !text.contains("push_token"),
            "{text}"
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn a_devices_file_from_before_push_tokens_still_loads() {
        let dir = TempDir::new("hub-legacy");
        let hash = "ab".repeat(32);
        let legacy = format!(
            r#"{{"devices":[{{"name":"Old","platform":"ios","login":"{LOGIN}","created":1,"last_seen":2,"token_sha256":"{hash}"}}]}}"#
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("devices.json"), legacy).unwrap();
        let store = store(&dir).await;
        assert_eq!(store.devices().len(), 1);
        assert_eq!(store.devices()[0].push_token, None);
    }

    #[tokio::test]
    async fn a_revoke_from_another_process_is_seen_on_reload() {
        let dir = TempDir::new("hub-reload");
        let mut hub = store(&dir).await;
        let token = paired(&mut hub, "Gemini 10", epoch(1)).await;
        assert!(!hub.reload_if_changed().await);
        let mut cli = Store::open(dir.path()).await.unwrap();
        assert!(cli.revoke("Gemini 10").await);
        assert!(hub.reload_if_changed().await);
        assert!(hub.devices().is_empty());
        assert!(hub.authenticate(&token, LOGIN, epoch(2)).await.is_none());
        assert!(!hub.reload_if_changed().await);
    }

    #[tokio::test]
    async fn the_config_defaults_and_reads_hub_json() {
        let dir = TempDir::new("hub-config");
        let store = store(&dir).await;
        assert_eq!(
            store.config(),
            &HubConfig {
                port: 47823,
                workspaces: Vec::new()
            }
        );
        std::fs::write(
            dir.path().join("hub.json"),
            r#"{"port":50000,"workspaces":["/a","/b"]}"#,
        )
        .unwrap();
        let store = Store::open(dir.path()).await.unwrap();
        assert_eq!(store.config().port, 50000);
        assert_eq!(
            store.config().workspaces,
            [PathBuf::from("/a"), PathBuf::from("/b")]
        );
        std::fs::write(dir.path().join("hub.json"), r#"{"port":70000}"#).unwrap();
        assert!(Store::open(dir.path()).await.is_err());
    }

    #[tokio::test]
    async fn audit_lines_never_hold_prompt_text() {
        let dir = TempDir::new("hub-audit");
        let store = store(&dir).await;
        store.audit("Gemini 10", "s", "prompt").await;
        store.audit("Gemini 10", "s", "answer").await;
        let file = dir.path().join("hub-audit.jsonl");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        let object = lines[0].as_object().unwrap();
        let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["device", "session", "time", "what"]);
        assert_eq!(
            (
                object["device"].as_str(),
                object["session"].as_str(),
                object["what"].as_str()
            ),
            (Some("Gemini 10"), Some("s"), Some("prompt"))
        );
        assert!(object["time"].is_u64());
    }

    #[tokio::test]
    async fn secrets_and_tokens_never_reach_the_audit_file() {
        let dir = TempDir::new("hub-nolog");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(1)).await;
        store.audit("Gemini 10", "s", "paired").await;
        let text = std::fs::read_to_string(dir.path().join("hub-audit.jsonl")).unwrap();
        assert!(!text.contains(&token));
        assert!(!format!("{store:?}").contains(&token));
    }

    #[tokio::test]
    async fn a_stale_store_cannot_bring_back_a_revoked_device() {
        let dir = TempDir::new("hub-lost");
        let mut x = store(&dir).await;
        let _a = paired(&mut x, "A", epoch(100)).await;
        let b = paired(&mut x, "B", epoch(100)).await;
        let mut y = Store::open(dir.path()).await.unwrap();
        assert!(y.revoke("A").await);
        let device = x.authenticate(&b, LOGIN, epoch(1000)).await.unwrap();
        assert_eq!(device.last_seen, 1000);
        let names = |store: &Store| {
            store
                .devices()
                .iter()
                .map(|d| d.name.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&x), ["B"]);
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(names(&reopened), ["B"]);
        assert_eq!(reopened.devices()[0].last_seen, 1000);
        // A pairing from the stale store keeps the revoke too.
        let secret = x.pairing_secret(epoch(1000));
        x.pair(&secret, "C", "ios", LOGIN, epoch(1000))
            .await
            .unwrap();
        assert_eq!(names(&Store::open(dir.path()).await.unwrap()), ["B", "C"]);
        // And a device revoked elsewhere cannot authenticate when it would write.
        assert!(y.reload_if_changed().await);
        assert!(x.revoke("B").await);
        assert!(y.authenticate(&b, LOGIN, epoch(5000)).await.is_none());
        assert_eq!(names(&Store::open(dir.path()).await.unwrap()), ["C"]);
        let lock = dir.path().join("devices.lock");
        assert_eq!(
            std::fs::metadata(lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn a_pairing_secret_from_the_future_is_expired() {
        let dir = TempDir::new("hub-clock");
        let mut store = store(&dir).await;
        let secret = store.pairing_secret(epoch(10_000));
        let earlier = store.pair(&secret, "Phone", "ios", LOGIN, epoch(100)).await;
        assert_eq!(
            earlier.map(|_| ()),
            Err("Pairing code already used or expired")
        );
    }

    #[tokio::test]
    async fn names_are_trimmed_and_free_of_control_characters() {
        let dir = TempDir::new("hub-names");
        let mut store = store(&dir).await;
        let now = epoch(1);
        let secret = store.pairing_secret(now);
        for (name, platform) in [
            ("a\nb", "ios"),
            ("a\u{1b}[2J", "ios"),
            ("ok", "i\tos"),
            ("  ", "ios"),
        ] {
            assert!(
                store
                    .pair(&secret, name, platform, LOGIN, now)
                    .await
                    .is_err(),
                "{name:?}"
            );
        }
        let long = "x".repeat(65);
        assert!(store.pair(&secret, &long, "ios", LOGIN, now).await.is_err());
        let (_, device) = store
            .pair(&secret, "  Gemini 10 ", " ios ", LOGIN, now)
            .await
            .unwrap();
        assert_eq!(
            (device.name.as_str(), device.platform.as_str()),
            ("Gemini 10", "ios")
        );
    }

    #[tokio::test]
    async fn an_open_directory_is_tightened_to_owner_only() {
        let dir = TempDir::new("hub-mode");
        std::fs::create_dir(dir.path()).unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        store(&dir).await;
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[tokio::test]
    async fn a_devices_file_that_cannot_be_trusted_is_refused() {
        let dir = TempDir::new("hub-bad");
        std::fs::create_dir(dir.path()).unwrap();
        let hash = "ab".repeat(32);
        let device = |login: &str, hash: &str| {
            format!(
                r#"{{"devices":[{{"name":"n","platform":"p","login":"{login}","created":1,"last_seen":1,"token_sha256":"{hash}"}}]}}"#
            )
        };
        let file = dir.path().join("devices.json");
        std::fs::write(&file, device(LOGIN, &hash)).unwrap();
        assert!(Store::open(dir.path()).await.is_ok());
        for bad in [
            device("", &hash),
            device(LOGIN, "abc"),
            device(LOGIN, &hash.to_uppercase()),
            device(LOGIN, &"g".repeat(64)),
        ] {
            std::fs::write(&file, bad).unwrap();
            let error = Store::open(dir.path()).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }
}
