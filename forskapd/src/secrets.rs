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

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tracing::warn;

use crate::error::{Error, Result};

/// Where the daemon keeps its credentials.
#[derive(Debug, Clone)]
pub enum Keychain {
    /// The OS keychain: Secret Service on Linux, the Keychain on macOS.
    Os,
    /// None at all, for a dry run: every call fails with
    /// [`Error::NoKeychain`] without reaching the OS. The calls refused so
    /// far are counted, so a test can tell none was even tried.
    Disabled(Arc<AtomicUsize>),
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
        }
    }

    pub async fn load(&self) -> Result<Option<Credentials>> {
        match self {
            Self::Os => load().await,
            Self::Disabled(refused) => Err(refuse(refused, "read")),
        }
    }

    pub async fn store(&self, creds: &Credentials) -> Result<()> {
        match self {
            Self::Os => store(creds).await,
            Self::Disabled(refused) => Err(refuse(refused, "write")),
        }
    }

    pub async fn delete(&self) -> Result<()> {
        match self {
            Self::Os => delete().await,
            Self::Disabled(refused) => Err(refuse(refused, "delete")),
        }
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

async fn load() -> Result<Option<Credentials>> {
    if let Some(creds) = platform::load(SERVICE).await? {
        return Ok(Some(creds));
    }
    let Some(creds) = platform::load(LEGACY_SERVICE).await? else {
        return Ok(None);
    };
    match platform::store(&creds).await {
        Ok(()) => {
            if let Err(e) = platform::delete(LEGACY_SERVICE).await {
                warn!(error = %e, "removing the legacy keychain entry failed");
            }
        }
        Err(e) => warn!(error = %e, "moving the legacy keychain entry failed"),
    }
    Ok(Some(creds))
}

async fn store(creds: &Credentials) -> Result<()> {
    platform::store(creds).await
}

async fn delete() -> Result<()> {
    platform::delete(SERVICE).await?;
    platform::delete(LEGACY_SERVICE).await
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
    use super::{Credentials, Error, Result, SERVICE, decode, encode};
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

    pub async fn load(service: &'static str) -> Result<Option<Credentials>> {
        let blob = tokio::task::spawn_blocking(move || generic_password(options(service, None)))
            .await
            .map_err(|e| Error::Secrets(format!("join: {e}")))?;
        match blob {
            Ok(bytes) => Ok(Some(decode(&bytes)?)),
            Err(e) if e.code() == security_framework_sys::base::errSecItemNotFound => Ok(None),
            Err(e) => Err(Error::Secrets(e.to_string())),
        }
    }

    pub async fn store(creds: &Credentials) -> Result<()> {
        let payload = encode(creds)?;
        tokio::task::spawn_blocking(move || {
            set_generic_password_options(&payload, options(SERVICE, Some(true)))
        })
        .await
        .map_err(|e| Error::Secrets(format!("join: {e}")))?
        .map_err(|e| Error::Secrets(e.to_string()))
    }

    pub async fn delete(service: &'static str) -> Result<()> {
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
        assert!(matches!(keychain.load().await, Err(Error::NoKeychain)));
        assert!(matches!(
            keychain.store(&creds).await,
            Err(Error::NoKeychain)
        ));
        assert!(matches!(keychain.delete().await, Err(Error::NoKeychain)));
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
    use std::sync::Arc;

    use oo7::Keyring;
    use tokio::sync::Mutex;

    use super::{Credentials, Error, Result, SERVICE, decode, encode};

    /// The daemon's one Secret Service client. Every `Keyring::new` is a new
    /// D-Bus connection whose first call is OpenSession, and gnome-keyring 50
    /// can abort on exactly that (upstream #190) — one client per daemon life
    /// keeps that to the start. Cleared after a failed call so the next one
    /// reconnects, e.g. to a daemon that was replaced after a crash.
    static KEYRING: Mutex<Option<Arc<Keyring>>> = Mutex::const_new(None);

    fn attributes(service: &'static str) -> HashMap<&'static str, &'static str> {
        HashMap::from([("service", service)])
    }

    fn secrets(e: impl std::fmt::Display) -> Error {
        Error::Secrets(e.to_string())
    }

    async fn open() -> Result<Arc<Keyring>> {
        let mut slot = KEYRING.lock().await;
        if slot.is_none() {
            *slot = Some(Arc::new(Keyring::new().await.map_err(secrets)?));
        }
        Ok(Arc::clone(slot.as_ref().expect("filled above")))
    }

    async fn forget() {
        *KEYRING.lock().await = None;
    }

    async fn with_keyring<T>(op: impl AsyncFnOnce(&Keyring) -> oo7::Result<T>) -> Result<T> {
        let kr = open().await?;
        let result = async {
            kr.unlock().await?;
            op(&kr).await
        }
        .await;
        if result.is_err() {
            forget().await;
        }
        result.map_err(secrets)
    }

    pub async fn load(service: &'static str) -> Result<Option<Credentials>> {
        let secret = with_keyring(async |kr| {
            let items = kr.search_items(&attributes(service)).await?;
            match items.first() {
                Some(item) => item.secret().await.map(Some),
                None => Ok(None),
            }
        })
        .await?;
        secret.map(|s| decode(&s)).transpose()
    }

    pub async fn store(creds: &Credentials) -> Result<()> {
        let payload = encode(creds)?;
        with_keyring(async |kr| {
            kr.create_item("forskapd credentials", &attributes(SERVICE), payload, true)
                .await
                .map(drop)
        })
        .await
    }

    pub async fn delete(service: &'static str) -> Result<()> {
        with_keyring(async |kr| kr.delete(&attributes(service)).await).await
    }
}
