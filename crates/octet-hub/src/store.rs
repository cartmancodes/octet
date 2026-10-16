//! Devices, pairing secrets, hub config and the audit log.
use crate::apns::ApnsConfig;
use octet_core::remote::protocol::{PushKinds, is_activity_token, is_push_token};
use octet_store::{create_private, create_private_dir, sync_directory};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    fs::{self, OpenOptions},
    io::AsyncWriteExt,
    sync::mpsc,
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
    /// What it is pushed: the defaults for a new pairing, every kind for one from before kinds,
    /// until the phone says.
    pub push_kinds: PushKinds,
    /// The `ActivityKit` tokens its Live Activity pushes go to, if the phone registered them.
    pub activity: Option<ActivityTokens>,
}

/// A device's `ActivityKit` tokens, from its last `register_activity`, which replaced both. A
/// device keeps one only with at least one token; with neither its `activity` is `None`.
#[derive(Clone, PartialEq, Eq)]
pub struct ActivityTokens {
    /// The running activity's push token, lowercase hex; none while no activity runs.
    pub token: Option<String>,
    /// The push-to-start token, lowercase hex; none if the last registration left it out.
    pub start_token: Option<String>,
    /// When the phone registered them, in seconds since the epoch; none for a registration
    /// from before the hub kept the time, which is not known to be old.
    pub registered: Option<u64>,
}

impl ActivityTokens {
    /// Whether `token` is either of these.
    fn holds(&self, token: &str) -> bool {
        self.token.as_deref() == Some(token) || self.start_token.as_deref() == Some(token)
    }

    /// These tokens, or none when there are none.
    fn or_none(self) -> Option<Self> {
        (self.token.is_some() || self.start_token.is_some()).then_some(self)
    }

    /// These tokens without each one `gone` says, or none when none is left.
    fn without(&self, gone: impl Fn(&str) -> bool) -> Option<Self> {
        let kept = |token: &Option<String>| token.clone().filter(|token| !gone(token));
        Self {
            token: kept(&self.token),
            start_token: kept(&self.start_token),
            registered: self.registered,
        }
        .or_none()
    }
}

/// Shows whether each token is there, never the token.
impl std::fmt::Debug for ActivityTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityTokens")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field(
                "start_token",
                &self.start_token.as_ref().map(|_| "<redacted>"),
            )
            .field("registered", &self.registered)
            .finish()
    }
}

/// Shows the device without its token hash, push token or activity tokens.
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
            .field("push_kinds", &self.push_kinds)
            .field("activity", &self.activity)
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
    /// APNs for the Live Activity, when `hub.json` names a well-formed `apns` block; off
    /// otherwise.
    pub apns: Option<ApnsConfig>,
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

/// A change to `devices.json`: what it does to the devices on disk, its result, and whether it
/// changed them (only then is the file written).
pub(crate) type Change<R> = Box<dyn FnOnce(&mut Vec<Device>) -> (R, bool) + Send>;

/// `devices.json` as a write left it, for the store in memory to take ([`Store::adopt`]).
pub(crate) struct Disk {
    devices: Vec<Device>,
    stamp: Option<Stamp>,
}

// The devices hold push and activity tokens: a count only.
impl std::fmt::Debug for Disk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Disk")
            .field("devices", &self.devices.len())
            .finish_non_exhaustive()
    }
}

/// A write waiting for the [`Writer`]'s task: the change, and what is done with its result.
type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Changes `devices.json` the way [`Store`] does, in one task of its own, so the hub's loop never
/// waits on the lock, the read, the write or the syncs. The writes queue in the order they are
/// asked for ([`queue`](Self::queue) never waits), and each is made, and its result handed on,
/// before the next begins.
#[derive(Debug, Clone)]
pub(crate) struct Writer {
    directory: PathBuf,
    jobs: mpsc::UnboundedSender<Job>,
}

impl Writer {
    /// The writer of the `devices.json` in `directory`, and its task, which runs until the last
    /// clone of it goes and the writes queued by then are made.
    pub(crate) fn start(directory: PathBuf) -> Self {
        let (jobs, mut queued) = mpsc::unbounded_channel::<Job>();
        tokio::spawn(async move {
            while let Some(job) = queued.recv().await {
                job.await;
            }
        });
        Self { directory, jobs }
    }

    /// Queues `change`, made under the lock on `devices.lock` (see [`Store`]'s writes) after every
    /// write queued before it; `deliver` is given its result, and the file as it is after unless
    /// it failed, before the next write begins.
    pub(crate) fn queue<R, D>(
        &self,
        change: Change<R>,
        deliver: impl FnOnce(io::Result<(R, Disk)>) -> D + Send + 'static,
    ) where
        R: Send + 'static,
        D: Future<Output = ()> + Send + 'static,
    {
        let directory = self.directory.clone();
        let job: Job = Box::pin(async move {
            let written = locked_update(&directory, change).await;
            deliver(written).await;
        });
        // The task outlives every clone of the sender, so the queue is never closed early.
        let _ = self.jobs.send(job);
    }
}

/// A pairing under way: its device token (shown once), the device to save, and the secret it
/// spent, which goes back ([`Store::unspend`]) if the save fails.
pub(crate) struct Pairing {
    pub(crate) token: String,
    pub(crate) device: Device,
    secret: Secret,
}

// The token is the phone's credential: only the device's name shows.
impl std::fmt::Debug for Pairing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pairing")
            .field("device", &self.device.name)
            .finish_non_exhaustive()
    }
}

impl Store {
    /// Opens the store in `directory`, creating it owner-only if missing.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created or tightened to 0700, or
    /// `devices.json` or `hub.json` cannot be read or are malformed.
    pub async fn open(directory: &Path) -> io::Result<Store> {
        let mut store = Self::open_devices(directory).await?;
        store.config = read_config(directory).await?;
        Ok(store)
    }

    /// Opens the store in `directory` for its devices alone, as [`open`](Self::open) does but
    /// without reading `hub.json`, whose settings it then leaves at their defaults: listing or
    /// revoking a device must not wait on a typo there.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created or tightened to 0700, or `devices.json` cannot
    /// be read or is malformed.
    pub async fn open_devices(directory: &Path) -> io::Result<Store> {
        create_private_dir(directory).await?;
        if fs::metadata(directory).await?.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).await?;
        }
        let (devices, stamp) = read_devices(directory).await?;
        Ok(Store {
            directory: directory.to_owned(),
            devices,
            config: HubConfig::default(),
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
        let pairing = self.begin_pair(secret, name, platform, login, now)?;
        match self.update(pair_change(pairing.device.clone())).await {
            Ok(Ok(())) => Ok((pairing.token, pairing.device)),
            Ok(Err(reason)) => {
                self.unspend(pairing);
                Err(reason)
            }
            Err(_) => {
                self.unspend(pairing);
                Err(PAIR_NOT_SAVED)
            }
        }
    }

    /// [`pair`](Self::pair) up to the write: checks `secret` and the fields, and spends the
    /// secret. The device is saved by [`pair_change`]; if that fails, [`unspend`](Self::unspend)
    /// gives the secret back.
    ///
    /// # Errors
    ///
    /// Fails with the reason to show the phone when the secret is unknown, used or older than 10
    /// minutes, or a field is empty or too long.
    pub(crate) fn begin_pair(
        &mut self,
        secret: &str,
        name: &str,
        platform: &str,
        login: &str,
        now: SystemTime,
    ) -> Result<Pairing, &'static str> {
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
            push_kinds: PushKinds::default(),
            activity: None,
        };
        let secret = self.secrets.swap_remove(position);
        Ok(Pairing {
            token,
            device,
            secret,
        })
    }

    /// A pairing whose device was not saved gives its secret back, for a retry.
    pub(crate) fn unspend(&mut self, pairing: Pairing) {
        self.secrets.push(pairing.secret);
    }

    /// The device whose token this is, if it also connects from the login it
    /// paired with. Records the time it was seen.
    pub async fn authenticate(
        &mut self,
        token: &str,
        login: &str,
        now: SystemTime,
    ) -> Option<Device> {
        let (device, seen) = self.recognise(token, login, now)?;
        if let Some(seen) = seen {
            // The write re-reads the file, so a device revoked elsewhere is
            // refused here; a failed write only leaves last_seen older on disk.
            let written = self.update(seen_change(&device.token_sha256, seen)).await;
            if matches!(written, Ok(false)) {
                return None;
            }
        }
        Some(device)
    }

    /// [`authenticate`](Self::authenticate) in memory: the device, and the time it was seen if
    /// `devices.json` should be told ([`seen_change`]), at most once a minute.
    pub(crate) fn recognise(
        &mut self,
        token: &str,
        login: &str,
        now: SystemTime,
    ) -> Option<(Device, Option<u64>)> {
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
        Some((device.clone(), stale.then_some(seen)))
    }

    /// Deletes the device called `name`; false if there is none or the file
    /// cannot be written.
    pub async fn revoke(&mut self, name: &str) -> bool {
        let name = name.to_owned();
        self.update(Box::new(move |devices: &mut Vec<Device>| {
            let before = devices.len();
            devices.retain(|device| device.name != name);
            let removed = devices.len() != before;
            (removed, removed)
        }))
        .await
        .unwrap_or(false)
    }

    /// Sets, or with `None` clears, the push token of the device whose token hash is
    /// `token_sha256`, keeping its kinds: [`set_push`](Self::set_push) without kinds.
    ///
    /// # Errors
    ///
    /// Fails if `devices.json` cannot be locked, read or written; the error names no token.
    pub async fn set_push_token(
        &mut self,
        token_sha256: &str,
        token: Option<String>,
    ) -> io::Result<bool> {
        self.set_push(token_sha256, token, None).await
    }

    /// Sets, or with `None` clears, the push token of the device whose token hash is
    /// `token_sha256`, and its kinds when `kinds` says them (`None` keeps them), in one write.
    /// False if there is no such device. A token is on one device only: any other device holding
    /// it loses it, in the same write.
    ///
    /// # Errors
    ///
    /// Fails if `devices.json` cannot be locked, read or written; the error names no token.
    pub async fn set_push(
        &mut self,
        token_sha256: &str,
        token: Option<String>,
        kinds: Option<PushKinds>,
    ) -> io::Result<bool> {
        self.update(push_change(token_sha256, token, kinds)).await
    }

    /// Clears the push token `token` from the device holding it, in the locked write; returns
    /// that device's name, or none if no device holds it. For a token Expo says is dead.
    ///
    /// # Errors
    ///
    /// Fails if `devices.json` cannot be locked, read or written; the error names no token.
    #[cfg(test)]
    pub(crate) async fn drop_push_token(&mut self, token: &str) -> io::Result<Option<String>> {
        self.update(push_token_drop(token)).await
    }

    /// Sets the `ActivityKit` tokens of the device whose token hash is `token_sha256` to the ones
    /// the phone has now, replacing both it had: a token left out (`None`) is one it no longer
    /// has, and with neither the device has no activity. False if there is no such device. A
    /// token is on one device only, the newest to register it, as a push token is: another device
    /// holding it, in either place, loses that token alone, in the same write. A revoke removes
    /// the device, and its tokens with it. The same activity token registered again keeps when it
    /// was first registered, so its age is the activity's; a registration that changes nothing
    /// else is not written.
    ///
    /// # Errors
    ///
    /// Fails if `devices.json` cannot be locked, read or written; the error names no token.
    pub async fn set_activity(
        &mut self,
        token_sha256: &str,
        activity: ActivityTokens,
    ) -> io::Result<bool> {
        self.update(activity_change(token_sha256, activity)).await
    }

    /// Removes every `ActivityKit` token of the device whose token hash is `token_sha256`, for a
    /// phone that turned its Live Activity off (`clear_activity`). False if there is no such
    /// device; a device with none is left as it is, unwritten. A token APNs says is gone is
    /// [`drop_activity_token`](Self::drop_activity_token)'s.
    ///
    /// # Errors
    ///
    /// Fails if `devices.json` cannot be locked, read or written; the error names no token.
    pub async fn drop_activity(&mut self, token_sha256: &str) -> io::Result<bool> {
        self.update(activity_clear(token_sha256)).await
    }

    /// Clears the `ActivityKit` token `token` wherever a device holds it, as its activity token
    /// or its start token, and only that one, in the locked write; returns that device's name, or
    /// none if no device holds it. For a token APNs says is gone (410): a phone that has since
    /// registered newer tokens keeps them.
    ///
    /// # Errors
    ///
    /// Fails if `devices.json` cannot be locked, read or written; the error names no token.
    pub async fn drop_activity_token(&mut self, token: &str) -> io::Result<Option<String>> {
        self.update(activity_token_drop(token)).await
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
    async fn update<R>(&mut self, change: Change<R>) -> io::Result<R> {
        let (result, disk) = locked_update(&self.directory, change).await?;
        self.adopt(disk);
        Ok(result)
    }

    /// Takes `devices.json` as a [`Writer`]'s write left it: the list in memory becomes what is
    /// on disk.
    pub(crate) fn adopt(&mut self, disk: Disk) {
        self.devices = disk.devices;
        self.stamp = disk.stamp;
    }
}

/// [`Store`]'s locked read-modify-write of `devices.json` in `directory`: the result, and the
/// file as it is after.
async fn locked_update<R>(directory: &Path, change: Change<R>) -> io::Result<(R, Disk)> {
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(directory.join("devices.lock"))
        .await?
        .into_std()
        .await;
    // The lock is released when `lock` drops, even if this future does.
    let lock = tokio::task::spawn_blocking(move || lock.lock().map(|()| lock))
        .await
        .map_err(io::Error::other)??;
    let (mut devices, stamp) = read_devices(directory).await?;
    let (result, write) = change(&mut devices);
    let stamp = if write {
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
                let kinds = device.push_kinds;
                entry["push_kinds"] = serde_json::json!({
                    "approvals": kinds.approvals,
                    "errors": kinds.errors,
                    "turns": kinds.turns,
                });
                if let Some(activity) = &device.activity {
                    entry["activity"] = serde_json::json!({});
                    if let Some(token) = &activity.token {
                        entry["activity"]["token"] = serde_json::json!(token);
                    }
                    if let Some(start) = &activity.start_token {
                        entry["activity"]["start_token"] = serde_json::json!(start);
                    }
                    if let Some(registered) = activity.registered {
                        entry["activity"]["registered"] = serde_json::json!(registered);
                    }
                }
                entry
            })
            .collect::<Vec<_>>();
        let mut bytes = serde_json::to_vec_pretty(&serde_json::json!({ "devices": devices }))?;
        bytes.push(b'\n');
        write_private(directory, "devices.json", &bytes).await?;
        file_stamp(&directory.join("devices.json")).await
    } else {
        stamp
    };
    drop(lock);
    Ok((result, Disk { devices, stamp }))
}

/// Why a pairing was not saved, when the file could not be written.
pub(crate) const PAIR_NOT_SAVED: &str = "Could not save the device";

/// Saves the new `device` of a pairing, unless a device of its name is already paired.
pub(crate) fn pair_change(device: Device) -> Change<Result<(), &'static str>> {
    Box::new(move |devices| {
        if devices.iter().any(|kept| kept.name == device.name) {
            return (Err("A device with that name is already paired"), false);
        }
        devices.push(device);
        (Ok(()), true)
    })
}

/// Records that the device whose token hash is `token_sha256` was seen at `seen`; false if it
/// is not paired (any more).
pub(crate) fn seen_change(token_sha256: &str, seen: u64) -> Change<bool> {
    let token_sha256 = token_sha256.to_owned();
    Box::new(move |devices| {
        let Some(kept) = devices
            .iter_mut()
            .find(|kept| kept.token_sha256 == token_sha256)
        else {
            return (false, false);
        };
        kept.last_seen = kept.last_seen.max(seen);
        (true, true)
    })
}

/// [`Store::set_push`]'s change.
pub(crate) fn push_change(
    token_sha256: &str,
    token: Option<String>,
    kinds: Option<PushKinds>,
) -> Change<bool> {
    let token_sha256 = token_sha256.to_owned();
    Box::new(move |devices| {
        if !devices
            .iter()
            .any(|device| device.token_sha256 == token_sha256)
        {
            return (false, false);
        }
        let mut changed = false;
        for device in devices.iter_mut() {
            let wanted = if device.token_sha256 == token_sha256 {
                token.clone()
            } else if token.is_some() && device.push_token == token {
                None
            } else {
                continue;
            };
            changed |= device.push_token != wanted;
            device.push_token = wanted;
        }
        if let Some(kinds) = kinds
            && let Some(device) = devices
                .iter_mut()
                .find(|device| device.token_sha256 == token_sha256)
        {
            changed |= device.push_kinds != kinds;
            device.push_kinds = kinds;
        }
        (true, changed)
    })
}

/// [`Store::drop_push_token`]'s change.
pub(crate) fn push_token_drop(token: &str) -> Change<Option<String>> {
    let token = token.to_owned();
    Box::new(move |devices| {
        let mut dropped = None;
        for device in devices
            .iter_mut()
            .filter(|device| device.push_token.as_deref() == Some(token.as_str()))
        {
            device.push_token = None;
            dropped = Some(device.name.clone());
        }
        let changed = dropped.is_some();
        (dropped, changed)
    })
}

/// [`Store::set_activity`]'s change.
pub(crate) fn activity_change(token_sha256: &str, activity: ActivityTokens) -> Change<bool> {
    let token_sha256 = token_sha256.to_owned();
    Box::new(move |devices| {
        if !devices
            .iter()
            .any(|device| device.token_sha256 == token_sha256)
        {
            return (false, false);
        }
        let ours = |token: &str| activity.holds(token);
        let mut changed = false;
        for device in devices.iter_mut() {
            let wanted = if device.token_sha256 == token_sha256 {
                let mut wanted = activity.clone();
                if let Some(held) = &device.activity
                    && held.token == wanted.token
                {
                    wanted.registered = held.registered.or(wanted.registered);
                }
                wanted.or_none()
            } else if let Some(held) = &device.activity {
                held.without(ours)
            } else {
                continue;
            };
            changed |= device.activity != wanted;
            device.activity = wanted;
        }
        (true, changed)
    })
}

/// [`Store::drop_activity`]'s change.
pub(crate) fn activity_clear(token_sha256: &str) -> Change<bool> {
    let token_sha256 = token_sha256.to_owned();
    Box::new(move |devices| {
        let Some(device) = devices
            .iter_mut()
            .find(|device| device.token_sha256 == token_sha256)
        else {
            return (false, false);
        };
        let changed = device.activity.take().is_some();
        (true, changed)
    })
}

/// [`Store::drop_activity_token`]'s change.
pub(crate) fn activity_token_drop(token: &str) -> Change<Option<String>> {
    let token = token.to_owned();
    Box::new(move |devices| {
        let mut dropped = None;
        for device in devices.iter_mut() {
            let Some(held) = &device.activity else {
                continue;
            };
            let wanted = held.without(|held| held == token);
            if device.activity != wanted {
                device.activity = wanted;
                dropped = Some(device.name.clone());
            }
        }
        let changed = dropped.is_some();
        (dropped, changed)
    })
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
                push_token: text("push_token").filter(|token| is_push_token(token)),
                push_kinds: push_kinds(&entry["push_kinds"]),
                activity: activity(&entry["activity"]),
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

/// A device's stored kinds: each one the file says as a boolean, and on for the rest. A device
/// stored without kinds (or with kinds that are not an object) paired before there were kinds,
/// when it was told of every one, so it still is; [`PushKinds::default`] is for new pairings.
fn push_kinds(stored: &serde_json::Value) -> PushKinds {
    let kind = |key: &str| stored[key].as_bool().unwrap_or(true);
    PushKinds {
        approvals: kind("approvals"),
        errors: kind("errors"),
        turns: kind("turns"),
    }
}

/// A device's stored `ActivityKit` tokens. A file from before them has none; a token that is not
/// lowercase hex is dropped alone, and with neither left there are none.
fn activity(stored: &serde_json::Value) -> Option<ActivityTokens> {
    let checked = |key: &str| {
        stored[key]
            .as_str()
            .filter(|token| is_activity_token(token))
            .map(str::to_owned)
    };
    ActivityTokens {
        token: checked("token"),
        start_token: checked("start_token"),
        registered: stored["registered"].as_u64(),
    }
    .or_none()
}

/// The settings when `hub.json` names none: port 47823, no extra workspaces and no APNs.
impl Default for HubConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            workspaces: Vec::new(),
            apns: None,
        }
    }
}

/// The settings in `hub.json`; defaults if it is missing.
async fn read_config(directory: &Path) -> io::Result<HubConfig> {
    let mut config = HubConfig::default();
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
    // A malformed `apns` block turns APNs off, with one line naming the field; the hub still
    // starts.
    config.apns = ApnsConfig::from_hub_json(&value["apns"]).unwrap_or_else(|line| {
        eprintln!("{line}");
        None
    });
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
                .unwrap()
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
        assert!(
            store
                .set_push_token(&device.token_sha256, None)
                .await
                .unwrap()
        );
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
                .unwrap()
        );

        // An unknown device is reported, and a revoke drops the token with the device.
        assert!(
            !store
                .set_push_token(&"0".repeat(64), Some(PUSH.to_owned()))
                .await
                .unwrap()
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
    async fn a_stored_push_token_that_is_not_an_expo_token_is_dropped_on_load() {
        let dir = TempDir::new("hub-badpush");
        let hash = |c: &str| c.repeat(64);
        let entry = |name: &str, c: &str, push: &str| {
            format!(
                r#"{{"name":"{name}","platform":"ios","login":"{LOGIN}","created":1,"last_seen":2,"token_sha256":"{}","push_token":{push}}}"#,
                hash(c)
            )
        };
        let file = format!(
            r#"{{"devices":[{},{},{},{}]}}"#,
            entry("Good", "a", &format!("\"{PUSH}\"")),
            entry("Url", "b", r#""https://evil.example/x""#),
            entry("Number", "c", "7"),
            entry("Null", "d", "null"),
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("devices.json"), file).unwrap();
        let store = store(&dir).await;
        let tokens: Vec<_> = store
            .devices()
            .iter()
            .map(|d| (d.name.as_str(), d.push_token.as_deref()))
            .collect();
        assert_eq!(
            tokens,
            [
                ("Good", Some(PUSH)),
                ("Url", None),
                ("Number", None),
                ("Null", None)
            ]
        );
    }

    #[tokio::test]
    async fn two_stores_keep_each_others_pairings_revokes_and_push_tokens() {
        let dir = TempDir::new("hub-two");
        let mut a = store(&dir).await;
        let mut b = Store::open(dir.path()).await.unwrap();
        let (_, one) = {
            let secret = a.pairing_secret(epoch(1));
            a.pair(&secret, "One", "ios", LOGIN, epoch(1))
                .await
                .unwrap()
        };
        let (_, two) = {
            let secret = b.pairing_secret(epoch(2));
            b.pair(&secret, "Two", "ios", LOGIN, epoch(2))
                .await
                .unwrap()
        };
        // b's own list was stale when it paired; the file has both.
        assert_eq!(Store::open(dir.path()).await.unwrap().devices().len(), 2);

        let (p1, p2) = ("ExpoPushToken[one-1]", "ExpoPushToken[two-2]");
        assert!(
            a.set_push_token(&one.token_sha256, Some(p1.into()))
                .await
                .unwrap()
        );
        assert!(
            b.set_push_token(&two.token_sha256, Some(p2.into()))
                .await
                .unwrap()
        );
        let on_disk = Store::open(dir.path()).await.unwrap();
        let tokens: Vec<_> = on_disk
            .devices()
            .iter()
            .map(|d| (d.name.as_str(), d.push_token.as_deref()))
            .collect();
        assert_eq!(tokens, [("One", Some(p1)), ("Two", Some(p2))]);

        // b revokes One; a, which has not looked, cannot set a token on it, and keeps Two's.
        assert!(b.revoke("One").await);
        assert!(
            !a.set_push_token(&one.token_sha256, Some(p1.into()))
                .await
                .unwrap()
        );
        assert_eq!(a.devices().len(), 1);
        assert_eq!(a.devices()[0].push_token.as_deref(), Some(p2));

        // A store that never wrote sees the token after a reload.
        let mut c = Store::open(dir.path()).await.unwrap();
        assert!(b.set_push_token(&two.token_sha256, None).await.unwrap());
        assert!(c.reload_if_changed().await);
        assert_eq!(c.devices()[0].push_token, None);
        assert!(
            a.set_push_token(&two.token_sha256, Some(p1.into()))
                .await
                .unwrap()
        );
        assert!(b.reload_if_changed().await);
        assert_eq!(b.devices()[0].push_token.as_deref(), Some(p1));
    }

    #[tokio::test]
    async fn a_push_token_is_never_held_by_two_devices() {
        let dir = TempDir::new("hub-unique");
        let mut store = store(&dir).await;
        let mut hashes = Vec::new();
        for name in ["One", "Two", "Three"] {
            let secret = store.pairing_secret(epoch(1));
            let (_, device) = store
                .pair(&secret, name, "ios", LOGIN, epoch(1))
                .await
                .unwrap();
            hashes.push(device.token_sha256);
        }
        assert!(
            store
                .set_push_token(&hashes[0], Some(PUSH.into()))
                .await
                .unwrap()
        );
        // The same token on another device (a phone handed on, or reinstalled) moves it.
        assert!(
            store
                .set_push_token(&hashes[1], Some(PUSH.into()))
                .await
                .unwrap()
        );
        let held = |store: &Store| -> Vec<(String, Option<String>)> {
            store
                .devices()
                .iter()
                .map(|d| (d.name.clone(), d.push_token.clone()))
                .collect()
        };
        let want = vec![
            ("One".to_owned(), None),
            ("Two".to_owned(), Some(PUSH.to_owned())),
            ("Three".to_owned(), None),
        ];
        assert_eq!(held(&store), want);
        assert_eq!(held(&Store::open(dir.path()).await.unwrap()), want);
        let text = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert_eq!(text.matches(PUSH).count(), 1, "{text}");
        // Another token on the holder leaves the others alone.
        assert!(
            store
                .set_push_token(&hashes[1], Some("ExpoPushToken[other]".into()))
                .await
                .unwrap()
        );
        assert!(
            store
                .set_push_token(&hashes[2], Some(PUSH.into()))
                .await
                .unwrap()
        );
        assert_eq!(held(&store)[2].1.as_deref(), Some(PUSH));
        assert_eq!(held(&store)[1].1.as_deref(), Some("ExpoPushToken[other]"));
    }

    #[tokio::test]
    async fn a_dead_push_token_is_cleared_by_value_and_only_that_one() {
        let dir = TempDir::new("hub-dead");
        let mut store = store(&dir).await;
        let mut hashes = Vec::new();
        for name in ["One", "Two"] {
            let secret = store.pairing_secret(epoch(1));
            let (_, device) = store
                .pair(&secret, name, "ios", LOGIN, epoch(1))
                .await
                .unwrap();
            hashes.push(device.token_sha256);
        }
        let other = "ExpoPushToken[other]";
        store
            .set_push_token(&hashes[0], Some(PUSH.into()))
            .await
            .unwrap();
        store
            .set_push_token(&hashes[1], Some(other.into()))
            .await
            .unwrap();
        // Not a token anyone holds: nothing changes.
        assert_eq!(
            store
                .drop_push_token("ExpoPushToken[nobody]")
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store.drop_push_token(PUSH).await.unwrap().as_deref(),
            Some("One")
        );
        let held: Vec<_> = Store::open(dir.path())
            .await
            .unwrap()
            .devices()
            .iter()
            .map(|d| d.push_token.clone())
            .collect();
        assert_eq!(held, [None, Some(other.to_owned())]);
        let text = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(!text.contains(PUSH), "{text}");
        // Once cleared, a second report of the same token finds no one.
        assert_eq!(store.drop_push_token(PUSH).await.unwrap(), None);
    }

    #[tokio::test]
    async fn a_failed_push_write_is_an_error_that_names_no_token() {
        let dir = TempDir::new("hub-pushfail");
        let mut store = store(&dir).await;
        let secret = store.pairing_secret(epoch(1));
        let (_, device) = store
            .pair(&secret, "One", "ios", LOGIN, epoch(1))
            .await
            .unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        // Root (a container) writes into a read-only directory, so there is nothing to test.
        let probe = dir.path().join("probe");
        if std::fs::write(&probe, b"x").is_ok() {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let result = store
            .set_push_token(&device.token_sha256, Some(PUSH.into()))
            .await;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let error = result.expect_err("the directory is read-only");
        let shown = format!("{error} {error:?}");
        assert!(
            !shown.contains("secretvalue") && !shown.contains(&device.token_sha256),
            "{shown}"
        );
        // The list in memory still says what is on disk.
        assert_eq!(store.devices()[0].push_token, None);
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

    const ACTIVITY: &str = "5ec7e7aaaa0123456789abcdef";
    const START: &str = "5ec7e7bbbb0123456789abcdef";

    fn activity(token: Option<&str>, start_token: Option<&str>) -> ActivityTokens {
        ActivityTokens {
            token: token.map(str::to_owned),
            start_token: start_token.map(str::to_owned),
            registered: None,
        }
    }

    #[tokio::test]
    async fn activity_tokens_keep_when_they_were_registered() {
        let dir = TempDir::new("hub-activitywhen");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(1)).await;
        let device = store.authenticate(&token, LOGIN, epoch(2)).await.unwrap();
        let when = ActivityTokens {
            registered: Some(1_790_000_000),
            ..activity(Some(ACTIVITY), Some(START))
        };
        assert!(
            store
                .set_activity(&device.token_sha256, when.clone())
                .await
                .unwrap()
        );
        let text = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(text.contains("\"registered\""), "{text}");
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(reopened.devices()[0].activity, Some(when.clone()));
        // A 410 on one token keeps when the other was registered.
        store.drop_activity_token(ACTIVITY).await.unwrap();
        assert_eq!(
            store.devices()[0].activity,
            Some(ActivityTokens {
                token: None,
                ..when
            })
        );
        // A file from before times were kept loads them as unknown.
        let mut file: serde_json::Value = serde_json::from_str(&text).unwrap();
        file["devices"][0]["activity"]
            .as_object_mut()
            .unwrap()
            .remove("registered");
        let old = file.to_string();
        assert!(!old.contains("registered"), "{old}");
        std::fs::write(dir.path().join("devices.json"), old).unwrap();
        let older = Store::open(dir.path()).await.unwrap();
        assert_eq!(
            older.devices()[0].activity,
            Some(activity(Some(ACTIVITY), Some(START)))
        );
    }

    #[tokio::test]
    async fn the_same_token_registered_again_keeps_its_first_time_and_is_not_rewritten() {
        use std::os::unix::fs::MetadataExt as _;
        let dir = TempDir::new("hub-activityagain");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 11", epoch(1)).await;
        let device = store.authenticate(&token, LOGIN, epoch(2)).await.unwrap();
        let at = |registered: u64, start: &str| ActivityTokens {
            registered: Some(registered),
            ..activity(Some(ACTIVITY), Some(start))
        };
        let hash = &device.token_sha256;
        assert!(
            store
                .set_activity(hash, at(1_790_000_000, START))
                .await
                .unwrap()
        );
        let file = || {
            std::fs::metadata(dir.path().join("devices.json"))
                .unwrap()
                .ino()
        };
        let written = file();
        // A cold launch sends the same tokens again: the activity is as old as it was, and the
        // file is not written.
        assert!(
            store
                .set_activity(hash, at(1_790_000_500, START))
                .await
                .unwrap()
        );
        assert_eq!(store.devices()[0].activity, Some(at(1_790_000_000, START)));
        assert_eq!(file(), written, "nothing but the time changed");
        // A new push-to-start token with the same activity: saved, still as old.
        let rotated = "5ec7e7cccc0123456789abcdef";
        assert!(
            store
                .set_activity(hash, at(1_790_000_900, rotated))
                .await
                .unwrap()
        );
        assert_eq!(
            store.devices()[0].activity,
            Some(at(1_790_000_000, rotated))
        );
        assert_ne!(file(), written);
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(
            reopened.devices()[0].activity,
            Some(at(1_790_000_000, rotated))
        );
        // A new activity token starts its own time.
        let newer = ActivityTokens {
            token: Some("5ec7e7dddd0123456789abcdef".into()),
            ..at(1_790_001_000, rotated)
        };
        assert!(store.set_activity(hash, newer.clone()).await.unwrap());
        assert_eq!(store.devices()[0].activity, Some(newer));
    }

    #[tokio::test]
    async fn activity_tokens_are_kept_private_replaced_and_dropped_on_revoke() {
        let dir = TempDir::new("hub-activity");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(1)).await;
        let other = paired(&mut store, "Other", epoch(1)).await;
        let device = store.authenticate(&token, LOGIN, epoch(2)).await.unwrap();
        assert_eq!(device.activity, None);

        // Setting stores both in the 0600 file, and they read back.
        assert!(
            store
                .set_activity(&device.token_sha256, activity(Some(ACTIVITY), Some(START)))
                .await
                .unwrap()
        );
        let file = dir.path().join("devices.json");
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("\"activity\""), "{text}");
        assert!(text.contains(ACTIVITY) && text.contains(START), "{text}");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let reopened = Store::open(dir.path()).await.unwrap();
        let held: Vec<_> = reopened
            .devices()
            .iter()
            .map(|d| (d.name.as_str(), d.activity.clone()))
            .collect();
        assert_eq!(
            held,
            [
                ("Gemini 10", Some(activity(Some(ACTIVITY), Some(START)))),
                ("Other", None)
            ]
        );
        let seen = store.authenticate(&token, LOGIN, epoch(3)).await.unwrap();
        assert_eq!(seen.activity, Some(activity(Some(ACTIVITY), Some(START))));

        // Debug shows neither token, on the device or on its own.
        for shown in [
            format!("{:?}", reopened.devices()[0]),
            format!("{:?}", activity(Some(ACTIVITY), Some(START))),
        ] {
            assert!(!shown.contains("5ec7e7"), "{shown}");
            assert!(shown.contains("redacted"), "{shown}");
        }
        assert!(format!("{:?}", reopened.devices()[0]).contains("activity"));

        // A registration states the phone's tokens: one left out is none.
        let newer = "0123456789abcdef5ec7e7cccc";
        assert!(
            store
                .set_activity(&device.token_sha256, activity(Some(newer), None))
                .await
                .unwrap()
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains(newer)
                && !text.contains(ACTIVITY)
                && !text.contains(START)
                && !text.contains("start_token"),
            "{text}"
        );
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(
            reopened.devices()[0].activity,
            Some(activity(Some(newer), None))
        );
        assert!(
            store
                .set_activity(&device.token_sha256, activity(None, Some(START)))
                .await
                .unwrap()
        );
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            text.contains(START) && !text.contains(newer) && !text.contains("\"token\""),
            "{text}"
        );
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(
            reopened.devices()[0].activity,
            Some(activity(None, Some(START)))
        );
        // Neither token is no activity at all.
        assert!(
            store
                .set_activity(&device.token_sha256, activity(None, None))
                .await
                .unwrap()
        );
        assert_eq!(store.devices()[0].activity, None);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("\"activity\""), "{text}");
        assert!(store.authenticate(&other, LOGIN, epoch(4)).await.is_some());
        assert_eq!(store.devices()[1].activity, None);
    }

    #[tokio::test]
    async fn activity_tokens_need_their_device_and_go_with_its_revoke() {
        let dir = TempDir::new("hub-activityrevoke");
        let mut store = store(&dir).await;
        let token = paired(&mut store, "Gemini 10", epoch(1)).await;
        paired(&mut store, "Other", epoch(1)).await;
        let device = store.authenticate(&token, LOGIN, epoch(2)).await.unwrap();
        let file = dir.path().join("devices.json");
        let newer = "0123456789abcdef5ec7e7cccc";
        // An unknown device is reported and nothing is written.
        assert!(
            !store
                .set_activity(&"0".repeat(64), activity(Some(ACTIVITY), Some(START)))
                .await
                .unwrap()
        );
        assert!(!std::fs::read_to_string(&file).unwrap().contains(ACTIVITY));

        // A revoke drops the tokens with the device.
        assert!(
            store
                .set_activity(&device.token_sha256, activity(Some(newer), Some(START)))
                .await
                .unwrap()
        );
        assert!(store.revoke("Gemini 10").await);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains(newer) && !text.contains(START) && !text.contains("\"activity\""),
            "{text}"
        );
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(reopened.devices().len(), 1);
        assert_eq!(reopened.devices()[0].activity, None);
    }

    #[tokio::test]
    async fn drop_activity_removes_only_that_devices_tokens() {
        let dir = TempDir::new("hub-dropactivity");
        let mut store = store(&dir).await;
        let one = paired(&mut store, "One", epoch(1)).await;
        let two = paired(&mut store, "Two", epoch(1)).await;
        let one = store.authenticate(&one, LOGIN, epoch(2)).await.unwrap();
        let two = store.authenticate(&two, LOGIN, epoch(2)).await.unwrap();
        let theirs = "0123456789abcdef5ec7e7dddd";
        assert!(
            store
                .set_activity(&one.token_sha256, activity(Some(ACTIVITY), Some(START)))
                .await
                .unwrap()
        );
        assert!(
            store
                .set_activity(&two.token_sha256, activity(Some(theirs), None))
                .await
                .unwrap()
        );
        let file = dir.path().join("devices.json");
        assert!(store.drop_activity(&one.token_sha256).await.unwrap());
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(
            !text.contains(ACTIVITY) && !text.contains(START) && text.contains(theirs),
            "{text}"
        );
        let reopened = Store::open(dir.path()).await.unwrap();
        let held: Vec<_> = reopened
            .devices()
            .iter()
            .map(|d| (d.name.as_str(), d.activity.clone()))
            .collect();
        assert_eq!(
            held,
            [("One", None), ("Two", Some(activity(Some(theirs), None)))]
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        // Dropping again finds the device with nothing to drop, and writes nothing.
        let before = std::fs::metadata(&file).unwrap().modified().unwrap();
        assert!(store.drop_activity(&one.token_sha256).await.unwrap());
        assert_eq!(
            std::fs::metadata(&file).unwrap().modified().unwrap(),
            before
        );
        // An unknown device is reported.
        assert!(!store.drop_activity(&"0".repeat(64)).await.unwrap());
        assert_eq!(
            store.devices()[1].activity,
            Some(activity(Some(theirs), None))
        );
    }

    #[tokio::test]
    async fn a_dead_activity_token_is_cleared_by_value_and_only_that_field() {
        let dir = TempDir::new("hub-deadactivity");
        let mut store = store(&dir).await;
        let one = paired(&mut store, "One", epoch(1)).await;
        let two = paired(&mut store, "Two", epoch(1)).await;
        let one = store.authenticate(&one, LOGIN, epoch(2)).await.unwrap();
        let two = store.authenticate(&two, LOGIN, epoch(2)).await.unwrap();
        let theirs = "0123456789abcdef5ec7e7dddd";
        let rotated = "0123456789abcdef5ec7e7ffff";
        assert!(
            store
                .set_activity(&one.token_sha256, activity(Some(ACTIVITY), Some(START)))
                .await
                .unwrap()
        );
        assert!(
            store
                .set_activity(&two.token_sha256, activity(Some(theirs), None))
                .await
                .unwrap()
        );
        let held = |store: &Store| -> Vec<Option<ActivityTokens>> {
            store.devices().iter().map(|d| d.activity.clone()).collect()
        };
        // A 410 on the activity token keeps a different start token, and the other device.
        assert_eq!(
            store
                .drop_activity_token(ACTIVITY)
                .await
                .unwrap()
                .as_deref(),
            Some("One")
        );
        assert_eq!(
            held(&Store::open(dir.path()).await.unwrap()),
            [
                Some(activity(None, Some(START))),
                Some(activity(Some(theirs), None))
            ]
        );
        // A 410 on a stale token leaves a newer registration intact, unwritten.
        assert!(
            store
                .set_activity(&one.token_sha256, activity(Some(rotated), Some(START)))
                .await
                .unwrap()
        );
        let file = dir.path().join("devices.json");
        let before = std::fs::read_to_string(&file).unwrap();
        assert_eq!(store.drop_activity_token(ACTIVITY).await.unwrap(), None);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert_eq!(
            held(&store),
            [
                Some(activity(Some(rotated), Some(START))),
                Some(activity(Some(theirs), None))
            ]
        );
        // A 410 on the start token keeps the activity token.
        assert_eq!(
            store.drop_activity_token(START).await.unwrap().as_deref(),
            Some("One")
        );
        assert_eq!(held(&store)[0], Some(activity(Some(rotated), None)));
        // With neither left, the device has no activity at all.
        assert_eq!(
            store.drop_activity_token(rotated).await.unwrap().as_deref(),
            Some("One")
        );
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(held(&reopened), [None, Some(activity(Some(theirs), None))]);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains(rotated) && !text.contains(START), "{text}");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn an_activity_token_is_held_by_the_newest_device_only() {
        let dir = TempDir::new("hub-activityunique");
        let mut store = store(&dir).await;
        let mut hashes = Vec::new();
        for name in ["One", "Two", "Three"] {
            let token = paired(&mut store, name, epoch(1)).await;
            let device = store.authenticate(&token, LOGIN, epoch(2)).await.unwrap();
            hashes.push(device.token_sha256);
        }
        let held = |store: &Store| -> Vec<Option<ActivityTokens>> {
            store.devices().iter().map(|d| d.activity.clone()).collect()
        };
        let other = "0123456789abcdef5ec7e7eeee";
        assert!(
            store
                .set_activity(&hashes[0], activity(Some(ACTIVITY), Some(START)))
                .await
                .unwrap()
        );
        // Two takes One's activity token: One keeps its unrelated start token.
        assert!(
            store
                .set_activity(&hashes[1], activity(Some(ACTIVITY), None))
                .await
                .unwrap()
        );
        assert_eq!(
            held(&store),
            [
                Some(activity(None, Some(START))),
                Some(activity(Some(ACTIVITY), None)),
                None
            ]
        );
        // Three takes the start token: One has neither left, and Two keeps its own.
        assert!(
            store
                .set_activity(&hashes[2], activity(Some(other), Some(START)))
                .await
                .unwrap()
        );
        assert_eq!(
            held(&store),
            [
                None,
                Some(activity(Some(ACTIVITY), None)),
                Some(activity(Some(other), Some(START)))
            ]
        );
        // A token seen in the other place moves too: One registers Three's start token as its
        // activity token, and Three keeps its unrelated activity token.
        assert!(
            store
                .set_activity(&hashes[0], activity(Some(START), None))
                .await
                .unwrap()
        );
        assert_eq!(
            held(&Store::open(dir.path()).await.unwrap()),
            [
                Some(activity(Some(START), None)),
                Some(activity(Some(ACTIVITY), None)),
                Some(activity(Some(other), None))
            ]
        );
    }

    #[tokio::test]
    async fn a_devices_file_from_before_activity_tokens_still_loads_and_bad_ones_are_dropped() {
        let dir = TempDir::new("hub-oldactivity");
        let entry = |name: &str, c: &str, extra: &str| {
            format!(
                r#"{{"name":"{name}","platform":"ios","login":"{LOGIN}","created":1,"last_seen":2,"token_sha256":"{}","push_token":"{PUSH}","push_kinds":{{"approvals":true,"errors":true,"turns":false}}{extra}}}"#,
                c.repeat(64)
            )
        };
        let file = format!(
            r#"{{"devices":[{},{},{},{},{},{},{},{},{},{}]}}"#,
            entry("Old", "a", ""),
            entry(
                "Good",
                "b",
                &format!(r#","activity":{{"token":"{ACTIVITY}","start_token":"{START}"}}"#)
            ),
            entry(
                "NoStart",
                "c",
                &format!(r#","activity":{{"token":"{ACTIVITY}"}}"#)
            ),
            entry(
                "StartOnly",
                "d",
                &format!(r#","activity":{{"start_token":"{START}"}}"#)
            ),
            entry(
                "BadStart",
                "e",
                &format!(r#","activity":{{"token":"{ACTIVITY}","start_token":"NOPE"}}"#)
            ),
            entry(
                "BadToken",
                "f",
                &format!(r#","activity":{{"token":7,"start_token":"{START}"}}"#)
            ),
            entry("Upper", "1", r#","activity":{"token":"ABCDEF"}"#),
            entry("Empty", "2", r#","activity":{}"#),
            entry("Word", "3", r#","activity":"on""#),
            entry("Null", "4", r#","activity":null"#),
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("devices.json"), file).unwrap();
        let store = store(&dir).await;
        let held: Vec<_> = store
            .devices()
            .iter()
            .map(|d| (d.name.as_str(), d.activity.clone()))
            .collect();
        assert_eq!(
            held,
            [
                ("Old", None),
                ("Good", Some(activity(Some(ACTIVITY), Some(START)))),
                ("NoStart", Some(activity(Some(ACTIVITY), None))),
                ("StartOnly", Some(activity(None, Some(START)))),
                ("BadStart", Some(activity(Some(ACTIVITY), None))),
                ("BadToken", Some(activity(None, Some(START)))),
                ("Upper", None),
                ("Empty", None),
                ("Word", None),
                ("Null", None),
            ]
        );
        // The rest of an old device is untouched.
        assert_eq!(store.devices()[0].push_token.as_deref(), Some(PUSH));
    }

    #[tokio::test]
    async fn kinds_default_and_persist() {
        let dir = TempDir::new("hub-kinds");
        let hash = "ab".repeat(32);
        // A file from before kinds, one whose kinds are not what this hub writes, and one whose
        // kinds are not an object. A device that paired before kinds was told of every kind, and
        // still is: the defaults are for new pairings only.
        let entry = |name: &str, hash: &str, extra: &str| {
            format!(
                r#"{{"name":"{name}","platform":"ios","login":"{LOGIN}","created":1,"last_seen":2,"token_sha256":"{hash}"{extra}}}"#
            )
        };
        let legacy = format!(
            r#"{{"devices":[{},{},{}]}}"#,
            entry("Old", &hash, &format!(r#","push_token":"{PUSH}""#)),
            entry(
                "Odd",
                &"cd".repeat(32),
                r#","push_kinds":{"turns":1,"errors":false}"#
            ),
            entry("Word", &"ef".repeat(32), r#","push_kinds":"all""#),
        );
        std::fs::create_dir_all(dir.path()).unwrap();
        std::fs::write(dir.path().join("devices.json"), legacy).unwrap();
        let mut store = store(&dir).await;
        let kinds = |store: &Store| -> Vec<PushKinds> {
            store.devices().iter().map(|d| d.push_kinds).collect()
        };
        let defaults = PushKinds {
            approvals: true,
            errors: true,
            turns: false,
        };
        let every = PushKinds {
            approvals: true,
            errors: true,
            turns: true,
        };
        let odd = PushKinds {
            errors: false,
            ..every
        };
        assert_eq!(kinds(&store), [every, odd, every]);
        // Kinds go into devices.json with the token, in the same write, and read back.
        let all = PushKinds {
            approvals: false,
            errors: true,
            turns: true,
        };
        assert!(
            store
                .set_push(&hash, Some(PUSH.to_owned()), Some(all))
                .await
                .unwrap()
        );
        let text = std::fs::read_to_string(dir.path().join("devices.json")).unwrap();
        assert!(text.contains("\"push_kinds\""), "{text}");
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(reopened.devices()[0].push_kinds, all);
        assert_eq!(reopened.devices()[0].push_token.as_deref(), Some(PUSH));
        // A registration that does not say its kinds keeps them, on or off.
        assert!(
            store
                .set_push(&hash, Some(PUSH.to_owned()), None)
                .await
                .unwrap()
        );
        assert_eq!(store.devices()[0].push_kinds, all);
        assert!(store.set_push_token(&hash, None).await.unwrap());
        let reopened = Store::open(dir.path()).await.unwrap();
        assert_eq!(
            (
                reopened.devices()[0].push_kinds,
                reopened.devices()[0].push_token.clone()
            ),
            (all, None)
        );
        // A newly paired device starts with the defaults.
        paired(&mut store, "New", epoch(1_790_000_000)).await;
        let new = store.devices().iter().find(|d| d.name == "New").unwrap();
        assert_eq!(new.push_kinds, defaults);
        // Debug shows the kinds, which are not secret.
        assert!(format!("{new:?}").contains("push_kinds"));
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
                workspaces: Vec::new(),
                apns: None
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
    async fn hub_json_names_apns_and_a_malformed_block_only_turns_it_off() {
        let dir = TempDir::new("hub-apns");
        std::fs::create_dir_all(dir.path()).unwrap();
        let write = |apns: &str| {
            std::fs::write(
                dir.path().join("hub.json"),
                format!(r#"{{"port":50001,"apns":{apns}}}"#),
            )
            .unwrap();
        };
        write(
            r#"{"team_id":"TEAM123456","key_id":"KEY1234567",
                "key_path":"/keys/AuthKey.p8","sandbox":false}"#,
        );
        let store = Store::open(dir.path()).await.unwrap();
        assert_eq!(
            store.config().apns,
            Some(ApnsConfig {
                team_id: "TEAM123456".into(),
                key_id: "KEY1234567".into(),
                key_path: PathBuf::from("/keys/AuthKey.p8"),
                sandbox: false,
            })
        );
        // Malformed: APNs is off and the rest of hub.json still holds.
        write(r#"{"team_id":"team","key_id":"KEY1234567","key_path":"/k","sandbox":true}"#);
        let store = Store::open(dir.path()).await.unwrap();
        assert_eq!(store.config().apns, None);
        assert_eq!(store.config().port, 50001);
        write(r#""on""#);
        assert_eq!(Store::open(dir.path()).await.unwrap().config().apns, None);
        write("null");
        assert_eq!(Store::open(dir.path()).await.unwrap().config().apns, None);
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
