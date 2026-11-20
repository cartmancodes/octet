//! The hub's own keys, kept in `hub.key`: the X25519 identity a phone pins at pairing, and the
//! Ed25519 key the hub proves its channel with to the relay.
use crate::store::{hex, unhex};
use ed25519_dalek::{Signer, SigningKey};
use octet_store::{create_private, create_private_dir, sync_directory};
use sha2::{Digest, Sha256};
use snow::{
    params::DHChoice,
    resolvers::{CryptoResolver, DefaultResolver},
};
use std::{
    io,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};
use tokio::{fs, io::AsyncWriteExt};

/// The key file's name in the data directory.
const FILE: &str = "hub.key";
/// What the relay expects ahead of the challenge in the bytes a registration signs.
const SIGNED_PREFIX: &[u8] = b"octet-relay-v1";

/// The hub's keys. The private halves never leave this type except the identity's, which the
/// Noise responder takes.
pub(crate) struct HubKeys {
    /// The X25519 private key.
    identity: [u8; 32],
    /// Its public key, which the pairing link carries.
    identity_public: [u8; 32],
    /// The Ed25519 key that signs the relay's challenge.
    relay: SigningKey,
    /// The lowercase hex SHA-256 of the relay key's public half.
    channel: String,
}

// The private keys never show.
impl std::fmt::Debug for HubKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HubKeys")
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

impl HubKeys {
    /// The keys in `<directory>/hub.key`, made there first if it does not exist; true when this
    /// call made the file. The file is made with an exclusive create (owner-only from the start),
    /// whole, so a caller that loses the race to make it reads the winner's keys. A file that
    /// cannot be read or understood is an error naming it, and is never replaced.
    ///
    /// # Errors
    ///
    /// Fails if the directory or file cannot be made or read, or the file is not a key file.
    pub(crate) async fn load_or_create(directory: &Path) -> io::Result<(HubKeys, bool)> {
        create_private_dir(directory).await?;
        let path = directory.join(FILE);
        match read(&path).await {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            read => return Ok((read?, false)),
        }
        let made = Self::generate()?;
        if publish(directory, &made.to_file()).await? {
            return Ok((made, true));
        }
        Ok((read(&path).await?, false))
    }

    /// The keys for `identity` and the Ed25519 `seed`.
    fn from_parts(identity: [u8; 32], seed: &[u8; 32]) -> io::Result<Self> {
        let mut dh = DefaultResolver
            .resolve_dh(&DHChoice::Curve25519)
            .ok_or_else(|| io::Error::other("no X25519 in this build"))?;
        dh.set(&identity);
        let identity_public = <[u8; 32]>::try_from(dh.pubkey())
            .map_err(|_| io::Error::other("an X25519 public key is not 32 bytes"))?;
        let relay = SigningKey::from_bytes(seed);
        let channel = hex(Sha256::digest(relay.verifying_key().as_bytes()).as_slice());
        Ok(Self {
            identity,
            identity_public,
            relay,
            channel,
        })
    }

    /// Fresh random keys.
    fn generate() -> io::Result<Self> {
        let mut bytes = [0_u8; 64];
        getrandom::fill(&mut bytes).map_err(|error| {
            io::Error::other(format!("no random bytes for the hub's keys: {error}"))
        })?;
        let (identity, seed) = bytes.split_at(32);
        let (identity, seed) = (
            <[u8; 32]>::try_from(identity).map_err(io::Error::other)?,
            <[u8; 32]>::try_from(seed).map_err(io::Error::other)?,
        );
        Self::from_parts(identity, &seed)
    }

    /// The file's bytes: `{"v":1,"identity":<hex>,"relay":<hex>}` and a newline.
    fn to_file(&self) -> Vec<u8> {
        let mut bytes = serde_json::json!({
            "v": 1,
            "identity": hex(&self.identity),
            "relay": hex(self.relay.as_bytes()),
        })
        .to_string()
        .into_bytes();
        bytes.push(b'\n');
        bytes
    }

    /// The X25519 private key the Noise responder answers with.
    pub(crate) fn identity_private(&self) -> &[u8; 32] {
        &self.identity
    }

    /// The X25519 public key a phone pins; the pairing link carries it.
    pub(crate) fn identity_public(&self) -> [u8; 32] {
        self.identity_public
    }

    /// The Ed25519 public key the relay checks a registration against.
    pub(crate) fn relay_public(&self) -> [u8; 32] {
        self.relay.verifying_key().to_bytes()
    }

    /// The channel id: the lowercase hex SHA-256 of [`relay_public`](Self::relay_public).
    pub(crate) fn channel(&self) -> &str {
        &self.channel
    }

    /// The signature a registration answers the relay's `challenge` with: Ed25519 over
    /// `octet-relay-v1` and then the challenge's 32 bytes.
    pub(crate) fn sign(&self, challenge: &[u8; 32]) -> [u8; 64] {
        self.relay
            .sign(&[SIGNED_PREFIX, challenge.as_slice()].concat())
            .to_bytes()
    }
}

/// An error that names the key file `path`.
fn named(path: &Path, what: impl std::fmt::Display) -> String {
    format!("{}: {what}", path.display())
}

/// The keys in the file at `path`. A file that is not there is `NotFound`; any other failure
/// names the file.
async fn read(path: &Path) -> io::Result<HubKeys> {
    let bytes = match fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Err(error),
        Err(error) => return Err(io::Error::new(error.kind(), named(path, error))),
    };
    let corrupt = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            named(
                path,
                "is not a hub key file ({\"v\":1,\"identity\":<hex>,\"relay\":<hex>}); the hub leaves it \
                 alone. Delete it to give the hub a new identity: paired phones must pair again",
            ),
        )
    };
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
    let key = |name: &str| value[name].as_str().and_then(unhex::<32>);
    if value["v"].as_u64() != Some(1) {
        return Err(corrupt());
    }
    let (Some(identity), Some(seed)) = (key("identity"), key("relay")) else {
        return Err(corrupt());
    };
    HubKeys::from_parts(identity, &seed)
}

/// Makes `<directory>/hub.key` with `bytes` if it does not exist, whole and owner-only from the
/// first moment: the bytes go to a private file of their own, which is then linked to the name,
/// and a link fails if the name is taken. True if this call made the name.
async fn publish(directory: &Path, bytes: &[u8]) -> io::Result<bool> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let count = NEXT.fetch_add(1, Ordering::Relaxed);
    let temporary = directory.join(format!("{FILE}.{}.{count}.tmp", std::process::id()));
    let mut file = create_private(&temporary).await?;
    let written = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await
    }
    .await;
    drop(file);
    let linked = match written {
        Ok(()) => fs::hard_link(&temporary, directory.join(FILE)).await,
        Err(error) => Err(error),
    };
    // The temporary name goes either way; the key file, if made, keeps the data.
    let _ = fs::remove_file(&temporary).await;
    match linked {
        Ok(()) => {
            sync_directory(directory).await?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::VerifyingKey;
    use octet_testkit::TempDir;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[tokio::test]
    async fn a_new_key_file_is_private_and_stable_across_loads() {
        let dir = TempDir::new("keys-new");
        let (first, made) = HubKeys::load_or_create(dir.path()).await.expect("keys");
        assert!(made);
        let file = dir.path().join("hub.key");
        assert_eq!(mode(&file), 0o600);
        let text = std::fs::read_to_string(&file).expect("the file");
        let value: serde_json::Value = serde_json::from_str(&text).expect("JSON");
        assert_eq!(value["v"], 1);
        assert_eq!(value["identity"].as_str().map(str::len), Some(64));
        assert_eq!(value["relay"].as_str().map(str::len), Some(64));
        assert_eq!(value.as_object().map(serde_json::Map::len), Some(3));
        let (second, made) = HubKeys::load_or_create(dir.path()).await.expect("keys");
        assert!(!made);
        assert_eq!(first.identity_private(), second.identity_private());
        assert_eq!(first.identity_public(), second.identity_public());
        assert_eq!(first.relay_public(), second.relay_public());
        assert_eq!(first.channel(), second.channel());
        assert_eq!(std::fs::read_to_string(&file).expect("the file"), text);
        // Nothing is left beside it, and a later load never loosens it.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("the directory")
            .map(|entry| entry.expect("an entry").file_name())
            .collect();
        assert_eq!(names, ["hub.key"]);
        // The keys differ between hubs, and the debug form shows no key.
        let other = TempDir::new("keys-other");
        let (other, _) = HubKeys::load_or_create(other.path()).await.expect("keys");
        assert_ne!(first.identity_private(), other.identity_private());
        assert_ne!(first.channel(), other.channel());
        let shown = format!("{first:?}");
        assert!(
            !shown.contains(value["identity"].as_str().expect("hex")),
            "{shown}"
        );
        assert!(
            !shown.contains(value["relay"].as_str().expect("hex")),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn a_corrupt_key_file_is_an_error_naming_the_file_and_is_left_alone() {
        let hex64 = "ab".repeat(32);
        for (case, content) in [
            ("empty", String::new()),
            ("garbage", "not json".to_owned()),
            ("a list", "[]".to_owned()),
            (
                "no version",
                format!(r#"{{"identity":"{hex64}","relay":"{hex64}"}}"#),
            ),
            (
                "version 2",
                format!(r#"{{"v":2,"identity":"{hex64}","relay":"{hex64}"}}"#),
            ),
            ("no relay key", format!(r#"{{"v":1,"identity":"{hex64}"}}"#)),
            (
                "short key",
                format!(r#"{{"v":1,"identity":"ab","relay":"{hex64}"}}"#),
            ),
            (
                "upper case",
                format!(
                    r#"{{"v":1,"identity":"{}","relay":"{hex64}"}}"#,
                    hex64.to_uppercase()
                ),
            ),
            (
                "not hex",
                format!(
                    r#"{{"v":1,"identity":"{}","relay":"{hex64}"}}"#,
                    "g".repeat(64)
                ),
            ),
            (
                "a number",
                format!(r#"{{"v":1,"identity":7,"relay":"{hex64}"}}"#),
            ),
        ] {
            let dir = TempDir::new("keys-corrupt");
            std::fs::create_dir_all(dir.path()).expect("the directory");
            let file = dir.path().join("hub.key");
            std::fs::write(&file, &content).expect("a bad file");
            let error = HubKeys::load_or_create(dir.path()).await.expect_err(case);
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{case}");
            assert!(
                error.to_string().contains(&file.display().to_string()),
                "{case}: {error}"
            );
            assert_eq!(
                std::fs::read_to_string(&file).expect("the file"),
                content,
                "{case}"
            );
            assert_eq!(
                std::fs::read_dir(dir.path()).expect("dir").count(),
                1,
                "{case}"
            );
        }
    }

    #[tokio::test]
    async fn a_key_file_that_cannot_be_read_is_an_error_naming_the_file() {
        let dir = TempDir::new("keys-unreadable");
        std::fs::create_dir_all(dir.path()).expect("the directory");
        let file = dir.path().join("hub.key");
        // A directory where the file belongs: not a file, and not "missing" either.
        std::fs::create_dir(&file).expect("a directory");
        let error = HubKeys::load_or_create(dir.path())
            .await
            .expect_err("not a file");
        assert!(
            error.to_string().contains(&file.display().to_string()),
            "{error}"
        );
        assert!(file.is_dir());
    }

    #[tokio::test]
    async fn the_channel_is_the_hash_of_the_relay_public_key() {
        let dir = TempDir::new("keys-channel");
        let (keys, _) = HubKeys::load_or_create(dir.path()).await.expect("keys");
        let digest = Sha256::digest(keys.relay_public());
        assert_eq!(keys.channel(), hex(digest.as_slice()));
        assert_eq!(keys.channel().len(), 64);
        assert!(
            keys.channel()
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
    }

    #[test]
    fn the_identity_public_key_is_the_x25519_one() {
        // RFC 7748 section 6.1, Alice.
        let private =
            unhex::<32>("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a")
                .expect("hex");
        let public =
            unhex::<32>("8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a")
                .expect("hex");
        let keys = HubKeys::from_parts(private, &[1; 32]).expect("keys");
        assert_eq!(keys.identity_private(), &private);
        assert_eq!(keys.identity_public(), public);
    }

    #[tokio::test]
    async fn a_signature_verifies() {
        let dir = TempDir::new("keys-sign");
        let (keys, _) = HubKeys::load_or_create(dir.path()).await.expect("keys");
        let challenge = [9_u8; 32];
        let signature = ed25519_dalek::Signature::from_bytes(&keys.sign(&challenge));
        let key = VerifyingKey::from_bytes(&keys.relay_public()).expect("a key");
        // What the relay checks: the prefix and the challenge, not the challenge alone.
        let signed = [b"octet-relay-v1".as_slice(), challenge.as_slice()].concat();
        assert!(key.verify_strict(&signed, &signature).is_ok());
        assert!(key.verify_strict(&challenge, &signature).is_err());
        assert!(
            key.verify_strict(
                &[b"octet-relay-v1".as_slice(), &[8; 32]].concat(),
                &signature
            )
            .is_err()
        );
        // A key that is not this hub's does not verify it.
        let other = TempDir::new("keys-sign-other");
        let (other, _) = HubKeys::load_or_create(other.path()).await.expect("keys");
        let theirs = VerifyingKey::from_bytes(&other.relay_public()).expect("a key");
        assert!(theirs.verify_strict(&signed, &signature).is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_loads_at_once_agree_on_one_key() {
        for round in 0..10 {
            let dir = TempDir::new(&format!("keys-race-{round}"));
            let path = dir.path().to_owned();
            let loads: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    tokio::spawn(async move { HubKeys::load_or_create(&path).await })
                })
                .collect();
            let mut keys = Vec::new();
            for load in loads {
                keys.push(load.await.expect("joined").expect("keys"));
            }
            let first = &keys[0].0;
            for (other, _) in &keys {
                assert_eq!(first.identity_private(), other.identity_private());
                assert_eq!(first.channel(), other.channel());
            }
            assert_eq!(
                std::fs::read_dir(&path).expect("dir").count(),
                1,
                "only hub.key is left"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn only_the_load_that_made_the_file_says_so() {
        for round in 0..10 {
            let dir = TempDir::new(&format!("keys-first-{round}"));
            let path = dir.path().to_owned();
            let loads: Vec<_> = (0..8)
                .map(|_| {
                    let path = path.clone();
                    tokio::spawn(async move { HubKeys::load_or_create(&path).await })
                })
                .collect();
            let mut made = 0;
            for load in loads {
                made += usize::from(load.await.expect("joined").expect("keys").1);
            }
            assert_eq!(made, 1, "round {round}");
            assert!(!HubKeys::load_or_create(&path).await.expect("keys").1);
        }
    }
}
