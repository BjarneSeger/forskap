//! OS-level credential storage.
//!
//! macOS: Keychain (data-protection / iCloud-synchronized item).
//! Linux: secret-service via `oo7`.
//!
//! A single entry holds both host and token as a JSON blob — one logged-in
//! account at a time, matching the daemon's single-host model.

use tracing::warn;

use crate::error::{Error, Result};

const SERVICE: &str = "forskapd";
/// Service name from before the rename; read once, then moved to [`SERVICE`].
const LEGACY_SERVICE: &str = "gitlab-trackrd";

#[derive(Debug, Clone)]
pub struct Credentials {
    pub host: String,
    pub token: String,
}

pub async fn load() -> Result<Option<Credentials>> {
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

pub async fn store(creds: &Credentials) -> Result<()> {
    platform::store(creds).await
}

pub async fn delete() -> Result<()> {
    platform::delete(SERVICE).await?;
    platform::delete(LEGACY_SERVICE).await
}

fn encode(creds: &Credentials) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(&serde_json::json!({
        "host": creds.host,
        "token": creds.token,
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
        .ok_or_else(|| Error::Secrets("stored secret missing 'token'".to_string()))?
        .to_string();
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
                token: token.clone(),
            };
            let back = decode(&encode(&creds).unwrap()).unwrap();
            prop_assert_eq!(back.host, host);
            prop_assert_eq!(back.token, token);
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use std::collections::HashMap;

    use super::{Credentials, Error, Result, SERVICE, decode, encode};
    use oo7::Keyring;

    fn attributes(service: &'static str) -> HashMap<&'static str, &'static str> {
        HashMap::from([("service", service)])
    }

    async fn open() -> Result<Keyring> {
        let kr = Keyring::new()
            .await
            .map_err(|e| Error::Secrets(e.to_string()))?;
        kr.unlock()
            .await
            .map_err(|e| Error::Secrets(e.to_string()))?;
        Ok(kr)
    }

    pub async fn load(service: &'static str) -> Result<Option<Credentials>> {
        let kr = open().await?;
        let items = kr
            .search_items(&attributes(service))
            .await
            .map_err(|e| Error::Secrets(e.to_string()))?;
        let Some(item) = items.first() else {
            return Ok(None);
        };
        let secret = item
            .secret()
            .await
            .map_err(|e| Error::Secrets(e.to_string()))?;
        Ok(Some(decode(&secret)?))
    }

    pub async fn store(creds: &Credentials) -> Result<()> {
        let kr = open().await?;
        let payload = encode(creds)?;
        kr.create_item("forskapd credentials", &attributes(SERVICE), payload, true)
            .await
            .map_err(|e| Error::Secrets(e.to_string()))
    }

    pub async fn delete(service: &'static str) -> Result<()> {
        let kr = open().await?;
        kr.delete(&attributes(service))
            .await
            .map_err(|e| Error::Secrets(e.to_string()))
    }
}
