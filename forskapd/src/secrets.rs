//! OS-level credential storage.
//!
//! macOS: Keychain (data-protection / iCloud-synchronized item).
//! Linux: secret-service via `oo7`.
//!
//! A single entry holds both host and token as a JSON blob — one logged-in
//! account at a time, matching the daemon's single-host model.
//!
//! The daemon reaches the OS keychain only through [`Keychain::Os`]; a dry
//! run (`--dry-run`) holds [`Keychain::Disabled`], which answers every call
//! itself.
//!
//! A locked keyring (Linux) is no failure of the keychain: a call says
//! through [`Unlock`] whether it may ask the user to unlock it, and reports
//! [`Error::KeychainLocked`] where it may not or nobody answered.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tracing::warn;

use crate::error::{Error, Result};

/// Whether a keychain call may ask the user to unlock a locked keyring.
/// Only the Secret Service has the question; the macOS Keychain ignores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlock {
    /// Ask, and wait for the answer: the user is at the terminal
    /// (`forskap auth login`).
    Ask,
    /// Ask, but wait only this long for the answer. The prompt stays up
    /// after that, and the call reports the keyring locked.
    AskFor(Duration),
    /// Never ask, as every background task must: a prompt out of nowhere
    /// every time the daemon looks is no way to treat a locked keyring.
    Never,
}

/// Where the daemon keeps its credentials.
#[derive(Debug, Clone)]
pub enum Keychain {
    /// The OS keychain: Secret Service on Linux, the Keychain on macOS.
    Os,
    /// None at all, for a dry run: every call fails with
    /// [`Error::NoKeychain`] without reaching the OS. The calls refused so
    /// far are counted, so a test can tell none was even tried.
    Disabled(Arc<AtomicUsize>),
    /// One a test locks and unlocks, counting the calls that would have
    /// asked for an unlock.
    #[cfg(test)]
    Fake(Arc<FakeKeychain>),
}

impl Keychain {
    /// A keychain that refuses every call.
    pub fn disabled() -> Self {
        Self::Disabled(Arc::default())
    }

    /// How many calls a disabled keychain refused; always 0 for the OS one.
    pub fn refused(&self) -> usize {
        match self {
            Self::Os => 0,
            Self::Disabled(refused) => refused.load(Ordering::SeqCst),
            #[cfg(test)]
            Self::Fake(_) => 0,
        }
    }

    /// Whether credentials can be kept here at all: a login needs somewhere
    /// to store its token, a logout something to forget. Checked before
    /// either starts, so a disabled keychain turns them down before GitLab
    /// is asked or the session touched.
    pub fn require(&self) -> Result<()> {
        match self {
            Self::Os => Ok(()),
            Self::Disabled(_) => Err(Error::NoKeychain),
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
        }
    }

    pub async fn load(&self, unlock: Unlock) -> Result<Option<Credentials>> {
        match self {
            Self::Os => load(unlock).await,
            Self::Disabled(refused) => Err(refuse(refused, "read")),
            #[cfg(test)]
            Self::Fake(fake) => fake.open(unlock).map(|held| held.clone()),
        }
    }

    pub async fn store(&self, creds: &Credentials, unlock: Unlock) -> Result<()> {
        match self {
            Self::Os => store(creds, unlock).await,
            Self::Disabled(refused) => Err(refuse(refused, "write")),
            #[cfg(test)]
            Self::Fake(fake) => fake
                .open(unlock)
                .map(|mut held| *held = Some(creds.clone())),
        }
    }

    pub async fn delete(&self, unlock: Unlock) -> Result<()> {
        match self {
            Self::Os => delete(unlock).await,
            Self::Disabled(refused) => Err(refuse(refused, "delete")),
            #[cfg(test)]
            Self::Fake(fake) => fake.open(unlock).map(|mut held| *held = None),
        }
    }
}

/// A keychain in memory, for tests of what the daemon does around a locked
/// one: nobody ever answers its unlock prompt.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct FakeKeychain {
    locked: std::sync::atomic::AtomicBool,
    creds: std::sync::Mutex<Option<Credentials>>,
    asked: AtomicUsize,
}

#[cfg(test)]
impl FakeKeychain {
    /// A locked keychain holding `creds`.
    pub fn locked(creds: Option<Credentials>) -> Arc<Self> {
        Arc::new(Self {
            locked: true.into(),
            creds: creds.into(),
            asked: AtomicUsize::new(0),
        })
    }

    /// What a login to the desktop session does.
    pub fn unlock(&self) {
        self.locked.store(false, Ordering::SeqCst);
    }

    /// How many calls came with leave to ask for an unlock.
    pub fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }

    /// What it holds.
    pub fn held(&self) -> Option<Credentials> {
        self.creds.lock().unwrap().clone()
    }

    fn open(&self, unlock: Unlock) -> Result<std::sync::MutexGuard<'_, Option<Credentials>>> {
        if unlock != Unlock::Never {
            self.asked.fetch_add(1, Ordering::SeqCst);
        }
        if self.locked.load(Ordering::SeqCst) {
            return Err(Error::KeychainLocked);
        }
        Ok(self.creds.lock().unwrap())
    }
}

fn refuse(refused: &AtomicUsize, op: &'static str) -> Error {
    refused.fetch_add(1, Ordering::SeqCst);
    warn!(
        op,
        "refused a keychain call: this daemon runs without a keychain"
    );
    Error::NoKeychain
}

const SERVICE: &str = "forskapd";
/// Service name from before the rename; read once, then moved to [`SERVICE`].
const LEGACY_SERVICE: &str = "gitlab-trackrd";

/// A GitLab token. `Debug` is redacted, so it can't leak through a log line.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The secret itself: for GitLab and the keychain only, never for a log.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

#[derive(Debug, Clone)]
pub struct Credentials {
    pub host: String,
    pub token: Token,
}

async fn load(unlock: Unlock) -> Result<Option<Credentials>> {
    if let Some(creds) = platform::load(SERVICE, unlock).await? {
        return Ok(Some(creds));
    }
    let Some(creds) = platform::load(LEGACY_SERVICE, unlock).await? else {
        return Ok(None);
    };
    match platform::store(&creds, unlock).await {
        Ok(()) => {
            if let Err(e) = platform::delete(LEGACY_SERVICE, unlock).await {
                warn!(error = %e, "removing the legacy keychain entry failed");
            }
        }
        Err(e) => warn!(error = %e, "moving the legacy keychain entry failed"),
    }
    Ok(Some(creds))
}

async fn store(creds: &Credentials, unlock: Unlock) -> Result<()> {
    platform::store(creds, unlock).await
}

async fn delete(unlock: Unlock) -> Result<()> {
    platform::delete(SERVICE, unlock).await?;
    platform::delete(LEGACY_SERVICE, unlock).await
}

fn encode(creds: &Credentials) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::json!({
        "host": creds.host,
        "token": creds.token.expose(),
    }))?)
}

fn decode(bytes: &[u8]) -> Result<Credentials> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let host = v["host"]
        .as_str()
        .ok_or_else(|| Error::Secrets("stored secret missing 'host'".to_string()))?
        .to_string();
    let token = v["token"]
        .as_str()
        .map(Token::new)
        .ok_or_else(|| Error::Secrets("stored secret missing 'token'".to_string()))?;
    Ok(Credentials { host, token })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{Credentials, Error, Result, SERVICE, Unlock, decode, encode};
    use security_framework::passwords::{
        PasswordOptions, delete_generic_password_options, generic_password,
        set_generic_password_options,
    };

    const ACCOUNT: &str = "default";

    fn options(service: &str, synchronized: Option<bool>) -> PasswordOptions {
        let mut opts = PasswordOptions::new_generic_password(service, ACCOUNT);
        opts.set_access_synchronized(synchronized);
        opts
    }

    pub async fn load(service: &'static str, _unlock: Unlock) -> Result<Option<Credentials>> {
        let blob = tokio::task::spawn_blocking(move || generic_password(options(service, None)))
            .await
            .map_err(|e| Error::Secrets(format!("join: {e}")))?;
        match blob {
            Ok(bytes) => Ok(Some(decode(&bytes)?)),
            Err(e) if e.code() == security_framework_sys::base::errSecItemNotFound => Ok(None),
            Err(e) => Err(Error::Secrets(e.to_string())),
        }
    }

    pub async fn store(creds: &Credentials, _unlock: Unlock) -> Result<()> {
        let payload = encode(creds)?;
        tokio::task::spawn_blocking(move || {
            set_generic_password_options(&payload, options(SERVICE, Some(true)))
        })
        .await
        .map_err(|e| Error::Secrets(format!("join: {e}")))?
        .map_err(|e| Error::Secrets(e.to_string()))
    }

    pub async fn delete(service: &'static str, _unlock: Unlock) -> Result<()> {
        let r = tokio::task::spawn_blocking(move || {
            delete_generic_password_options(options(service, None))
        })
        .await
        .map_err(|e| Error::Secrets(format!("join: {e}")))?;
        match r {
            Ok(()) => Ok(()),
            Err(e) if e.code() == security_framework_sys::base::errSecItemNotFound => Ok(()),
            Err(e) => Err(Error::Secrets(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn encode_decode_roundtrips_any_credentials(host in ".*", token in ".*") {
            let creds = Credentials {
                host: host.clone(),
                token: Token::new(token.clone()),
            };
            let back = decode(&encode(&creds).unwrap()).unwrap();
            prop_assert_eq!(back.host, host);
            prop_assert_eq!(back.token.expose(), token);
        }
    }

    #[tokio::test]
    async fn a_disabled_keychain_refuses_every_call_without_reaching_the_os() {
        let keychain = Keychain::disabled();
        let creds = Credentials {
            host: "gitlab.test".into(),
            token: Token::new("glpat-secret"),
        };
        assert!(matches!(
            keychain.load(Unlock::Ask).await,
            Err(Error::NoKeychain)
        ));
        assert!(matches!(
            keychain.store(&creds, Unlock::Ask).await,
            Err(Error::NoKeychain)
        ));
        assert!(matches!(
            keychain.delete(Unlock::Ask).await,
            Err(Error::NoKeychain)
        ));
        assert!(matches!(keychain.require(), Err(Error::NoKeychain)));
        // A clone counts into the same tally: the one the handlers hold is
        // the one a test reads.
        assert_eq!(keychain.clone().refused(), 3, "require() is no call");
    }

    #[test]
    fn only_a_disabled_keychain_is_required_in_vain() {
        assert!(Keychain::Os.require().is_ok());
        assert_eq!(Keychain::Os.refused(), 0);
        assert!(Keychain::disabled().require().is_err());
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_dismissed_prompt_and_a_locked_collection_read_as_locked() {
        use oo7::dbus::{Error as DBusError, ServiceError};

        let dbus = |e| oo7::Error::DBus(e);
        // What gnome-keyring answers when its prompt has no display to show on.
        assert!(platform::locked(&dbus(DBusError::Dismissed)));
        let refused = ServiceError::IsLocked("collection is locked".into());
        assert!(platform::locked(&dbus(DBusError::Service(refused))));
        // A session the service doesn't know is a failed call, not a lock.
        let gone = ServiceError::NoSession("The session does not exist".into());
        assert!(!platform::locked(&dbus(DBusError::Service(gone))));
        assert!(!platform::locked(&dbus(DBusError::Deleted)));
    }

    /// The keyring service was replaced under the daemon's client — it
    /// crashed, or the login started its own: every call on that client
    /// fails though the service answers. A call on such a client is tried
    /// once more on a new one; a failure on a client just made is final,
    /// and a locked keyring is no failure of the client at all.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_failure_on_a_client_from_before_is_tried_once_more_on_a_new_one() {
        use oo7::dbus::{Error as DBusError, ServiceError};

        let gone = || {
            let gone = ServiceError::NoSession("The session does not exist".into());
            oo7::Error::DBus(DBusError::Service(gone))
        };
        assert!(platform::stale(true, &gone()));
        assert!(!platform::stale(false, &gone()), "a new client had its try");
        let locked = oo7::Error::DBus(DBusError::Dismissed);
        assert!(!platform::stale(true, &locked));
    }

    #[tokio::test]
    async fn a_fake_keychain_counts_the_calls_that_would_prompt() {
        let creds = Credentials {
            host: "gitlab.test".into(),
            token: Token::new("glpat-secret"),
        };
        let fake = FakeKeychain::locked(Some(creds.clone()));
        let keychain = Keychain::Fake(Arc::clone(&fake));

        let looked = keychain.load(Unlock::Never).await;
        assert!(matches!(looked, Err(Error::KeychainLocked)));
        assert_eq!(fake.asked(), 0, "a quiet look asks nobody");
        let asked = keychain.load(Unlock::AskFor(Duration::ZERO)).await;
        assert!(matches!(asked, Err(Error::KeychainLocked)));
        assert_eq!(fake.asked(), 1);

        fake.unlock();
        let read = keychain.load(Unlock::Never).await.unwrap().unwrap();
        assert_eq!(read.token, creds.token);
        keychain.delete(Unlock::Never).await.unwrap();
        assert!(fake.held().is_none());
        assert_eq!(fake.asked(), 1);
    }

    #[test]
    fn a_token_never_shows_in_debug_output() {
        let creds = Credentials {
            host: "gitlab.test".into(),
            token: Token::new("glpat-secret"),
        };
        assert!(!format!("{creds:?}").contains("glpat-secret"));
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::collections::HashMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

    use oo7::Keyring;
    use oo7::dbus::{Error as DBusError, ServiceError};
    use tokio::sync::Mutex;

    use super::{Credentials, Error, Result, SERVICE, Unlock, decode, encode};

    /// The daemon's one Secret Service client. Every `Keyring::new` is a new
    /// D-Bus connection whose first call is OpenSession, and gnome-keyring 50
    /// can abort on exactly that (upstream #190) — one client per daemon life
    /// keeps that to the start. Dropped after a failed call, which is tried
    /// once more on a new one where the client was from before (see
    /// [`stale`]); a locked keyring is no failed call, and keeps it.
    static KEYRING: Mutex<Option<Arc<Keyring>>> = Mutex::const_new(None);

    fn attributes(service: &'static str) -> HashMap<&'static str, &'static str> {
        HashMap::from([("service", service)])
    }

    fn secrets(e: impl std::fmt::Display) -> Error {
        Error::Secrets(e.to_string())
    }

    /// The client, and whether it is one from an earlier call.
    async fn open() -> Result<(Arc<Keyring>, bool)> {
        let mut slot = KEYRING.lock().await;
        if let Some(kr) = slot.as_ref() {
            return Ok((Arc::clone(kr), true));
        }
        let kr = Arc::new(Keyring::new().await.map_err(secrets)?);
        *slot = Some(Arc::clone(&kr));
        Ok((kr, false))
    }

    /// Drop `kr` from the slot, unless a newer client took its place.
    async fn forget(kr: &Arc<Keyring>) {
        let mut slot = KEYRING.lock().await;
        if slot.as_ref().is_some_and(|held| Arc::ptr_eq(held, kr)) {
            *slot = None;
        }
    }

    /// Whether `e` says the keyring is locked: the unlock prompt was
    /// dismissed (or couldn't be shown: no display yet), or the service
    /// refused a call on a locked collection.
    pub(super) fn locked(e: &oo7::Error) -> bool {
        matches!(
            e,
            oo7::Error::DBus(DBusError::Dismissed | DBusError::Service(ServiceError::IsLocked(_)))
        )
    }

    /// Whether a call that failed with `e` is worth one more try on a new
    /// client: it ran on one from an earlier call (`cached`), whose session
    /// belongs to a keyring service that may have been replaced since — a
    /// crash, a login that starts its own — and then fails every call
    /// ("The session does not exist") though the service is fine. Not a
    /// locked keyring, which a new client finds just as locked.
    pub(super) fn stale(cached: bool, e: &oo7::Error) -> bool {
        cached && !locked(e)
    }

    /// One run of a keychain operation on a client. Boxed, and handed the
    /// client whole: an operation may run twice (see [`stale`]), and a
    /// closure borrowing its client can't promise the daemon's tasks a
    /// `Send` future for every such run.
    type Run<T> = Pin<Box<dyn Future<Output = oo7::Result<T>> + Send>>;

    /// `op` on `kr` once it is open, asking the user to unlock it where
    /// `unlock` allows; `None` where it stays locked.
    async fn attempt<T>(
        kr: &Arc<Keyring>,
        unlock: Unlock,
        op: &impl Fn(Arc<Keyring>) -> Run<T>,
    ) -> oo7::Result<Option<T>> {
        let open = match unlock {
            Unlock::Ask => {
                kr.unlock().await?;
                true
            }
            // Dropping the wait leaves the prompt to the user: once they
            // answer it, the next look finds the keyring open.
            Unlock::AskFor(limit) => match tokio::time::timeout(limit, kr.unlock()).await {
                Ok(asked) => {
                    asked?;
                    true
                }
                Err(_) => false,
            },
            // A property read: no prompt, whatever it says.
            Unlock::Never => !kr.is_locked().await?,
        };
        if !open {
            return Ok(None);
        }
        op(Arc::clone(kr)).await.map(Some)
    }

    /// Run `op` on the keyring once it is open, asking the user to unlock it
    /// where `unlock` allows.
    async fn with_keyring<T>(unlock: Unlock, op: impl Fn(Arc<Keyring>) -> Run<T>) -> Result<T> {
        let (mut kr, cached) = open().await?;
        let mut result = attempt(&kr, unlock, &op).await;
        if result.as_ref().is_err_and(|e| stale(cached, e)) {
            forget(&kr).await;
            (kr, _) = open().await?;
            result = attempt(&kr, unlock, &op).await;
        }
        match result {
            Ok(Some(value)) => Ok(value),
            Ok(None) => Err(Error::KeychainLocked),
            // The client is as good as before: forgetting it would cost a
            // new one, and its OpenSession, per look at a locked keyring.
            Err(e) if locked(&e) => Err(Error::KeychainLocked),
            Err(e) => {
                forget(&kr).await;
                Err(secrets(e))
            }
        }
    }

    pub async fn load(service: &'static str, unlock: Unlock) -> Result<Option<Credentials>> {
        let secret = with_keyring(unlock, |kr| {
            Box::pin(async move {
                let items = kr.search_items(&attributes(service)).await?;
                match items.first() {
                    Some(item) => item.secret().await.map(Some),
                    None => Ok(None),
                }
            })
        })
        .await?;
        secret.map(|s| decode(&s)).transpose()
    }

    pub async fn store(creds: &Credentials, unlock: Unlock) -> Result<()> {
        let payload = encode(creds)?;
        with_keyring(unlock, |kr| {
            let secret = payload.clone();
            Box::pin(async move {
                kr.create_item("forskapd credentials", &attributes(SERVICE), secret, true)
                    .await
                    .map(drop)
            })
        })
        .await
    }

    pub async fn delete(service: &'static str, unlock: Unlock) -> Result<()> {
        with_keyring(unlock, |kr| {
            Box::pin(async move { kr.delete(&attributes(service)).await })
        })
        .await
    }
}
