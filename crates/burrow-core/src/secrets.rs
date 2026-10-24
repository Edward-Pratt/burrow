//! Secret storage. Config files only hold references such as
//! `keyring:burrow/<node-id>/password`; the values live in a [`SecretStore`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use keyring_core::CredentialStore;

use crate::api::{NodeId, SecretValue};
use crate::error::{Error, Result};

const REF_SCHEME: &str = "keyring:";
const SERVICE: &str = "burrow";

/// Where a secret lives in the store: the keyring's service and user pair.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SecretKey {
    pub service: String,
    pub user: String,
}

impl SecretKey {
    pub(crate) fn for_field(id: NodeId, field: &str) -> Self {
        SecretKey {
            service: SERVICE.to_string(),
            user: format!("{id}/{field}"),
        }
    }

    /// The reference written to config files.
    pub fn to_ref(&self) -> String {
        format!("{REF_SCHEME}{}/{}", self.service, self.user)
    }

    pub fn parse_ref(reference: &str) -> Result<Self> {
        let rest = reference.strip_prefix(REF_SCHEME).ok_or_else(|| {
            Error::invalid(
                "secret reference",
                format!("{reference:?} does not start with {REF_SCHEME:?}"),
            )
        })?;
        match rest.split_once('/') {
            Some((service, user)) if !service.is_empty() && !user.is_empty() => Ok(SecretKey {
                service: service.to_string(),
                user: user.to_string(),
            }),
            _ => Err(Error::invalid(
                "secret reference",
                format!("{reference:?} is not keyring:<service>/<user>"),
            )),
        }
    }
}

/// A place to keep secrets. Calls may block (the keyring talks D-Bus), so the
/// core only calls them from blocking threads.
pub trait SecretStore: Send + Sync + 'static {
    fn get(&self, key: &SecretKey) -> Result<Option<SecretValue>>;
    fn set(&self, key: &SecretKey, value: &SecretValue) -> Result<()>;
    /// Succeeds when the secret does not exist.
    fn delete(&self, key: &SecretKey) -> Result<()>;
}

/// The desktop keyring through the freedesktop Secret Service API
/// (gnome-keyring, KWallet, KeePassXC).
pub struct KeyringStore {
    store: Arc<CredentialStore>,
}

impl KeyringStore {
    pub fn new() -> Result<Self> {
        let store = zbus_secret_service_keyring_store::Store::new().map_err(keyring_error)?;
        Ok(KeyringStore { store })
    }

    fn entry(&self, key: &SecretKey) -> Result<keyring_core::Entry> {
        self.store
            .build(&key.service, &key.user, None)
            .map_err(keyring_error)
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, key: &SecretKey) -> Result<Option<SecretValue>> {
        match self.entry(key)?.get_password() {
            Ok(value) => Ok(Some(SecretValue::new(value))),
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_error(e)),
        }
    }

    fn set(&self, key: &SecretKey, value: &SecretValue) -> Result<()> {
        self.entry(key)?
            .set_password(value.expose())
            .map_err(keyring_error)
    }

    fn delete(&self, key: &SecretKey) -> Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(keyring_error(e)),
        }
    }
}

fn keyring_error(e: keyring_core::Error) -> Error {
    Error::Secrets(e.to_string())
}

/// An in-memory store for tests. Nothing is persisted.
#[derive(Default)]
pub struct MemoryStore {
    secrets: Mutex<HashMap<SecretKey, SecretValue>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.secrets.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl SecretStore for MemoryStore {
    fn get(&self, key: &SecretKey) -> Result<Option<SecretValue>> {
        Ok(self.secrets.lock().unwrap().get(key).cloned())
    }

    fn set(&self, key: &SecretKey, value: &SecretValue) -> Result<()> {
        self.secrets
            .lock()
            .unwrap()
            .insert(key.clone(), value.clone());
        Ok(())
    }

    fn delete(&self, key: &SecretKey) -> Result<()> {
        self.secrets.lock().unwrap().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_round_trips() {
        let key = SecretKey::for_field(NodeId::new(), "password");
        assert_eq!(SecretKey::parse_ref(&key.to_ref()).unwrap(), key);
    }

    #[test]
    fn rejects_other_schemes() {
        assert!(SecretKey::parse_ref("vault:burrow/x").is_err());
        assert!(SecretKey::parse_ref("keyring:burrow").is_err());
        assert!(SecretKey::parse_ref("keyring:/x").is_err());
    }

    /// Talks to the real desktop keyring, so it only runs on request:
    /// `cargo test -p burrow-core -- --ignored`.
    #[test]
    #[ignore = "needs a running Secret Service (gnome-keyring, KWallet)"]
    fn keyring_round_trip() {
        let store = KeyringStore::new().unwrap();
        let key = SecretKey {
            service: SERVICE.to_string(),
            user: format!("test/{}", NodeId::new()),
        };
        store.set(&key, &SecretValue::new("hunter2")).unwrap();
        assert_eq!(store.get(&key).unwrap().unwrap().expose(), "hunter2");
        store.delete(&key).unwrap();
        assert!(store.get(&key).unwrap().is_none());
        store.delete(&key).unwrap();
    }
}
