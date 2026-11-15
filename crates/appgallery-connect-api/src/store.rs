//! Хранение учётных данных API-клиентов в системном хранилище.
//!
//! Каждый профиль (один профиль — один API-клиент одного кабинета AppGallery Connect)
//! хранит учётные данные одной записью: сервис [`KEYRING_SERVICE`], имя учётной записи —
//! имя профиля, значение — JSON `{"client_id": "...", "client_secret": "..."}`
//! ([`ApiClientCredentials::to_stored_json`]). Несекретные настройки профиля (регион)
//! лежат в файле профилей CLI.
//!
//! Запись хранится байтами UTF-8 (а не «паролем»): так она помещается в одну запись
//! Credential Manager на Windows (предел 2560 байт) при секрете до 2048 символов.
//!
//! Запасного файлового хранилища нет, из переменных окружения и `.env` учётные данные не
//! читаются: если системное хранилище недоступно (Linux без Secret Service), операции
//! завершаются ошибкой [`StoreError::Unavailable`] с подсказкой.
//!
//! Секреты, которые создают или получают сами инструменты сервера (не учётные данные
//! профиля), лежат отдельно: сервис [`SECRETS_KEYRING_SERVICE`], записи
//! `<профиль>/<имя>` ([`KeyringVault`]). Наружу их значения не отдаются.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use zeroize::Zeroizing;

use crate::credentials::ApiClientCredentials;

/// Имя сервиса записей в Keychain / Credential Manager / Secret Service.
pub const KEYRING_SERVICE: &str = "dev.filipov.appgallery-connect-mcp";

/// Учётная запись, которой проверяется доступность хранилища: в неё ничего не пишется.
const PROBE_ACCOUNT: &str = "availability-check";

/// Системное хранилище на этой платформе — Secret Service (D-Bus). На Linux и BSD keyring
/// работает только через него, и «хранилище недоступно» там почти всегда означает, что в
/// сессии нет демона Secret Service.
const USES_SECRET_SERVICE: bool = cfg!(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd"
));

/// Подсказка для Linux без Secret Service.
const SECRET_SERVICE_HINT: &str = "no Secret Service (org.freedesktop.secrets) is available \
on the D-Bus session bus. appgallery-connect-mcp stores API client credentials only in the \
system keychain. Run it in a desktop session with a keyring daemon (GNOME Keyring, KDE Wallet, \
KeePassXC with Secret Service integration). On a headless machine start a session bus and an \
unlocked keyring first, for example: dbus-run-session -- sh, then inside it \
gnome-keyring-daemon --unlock --components=secrets (reads the keyring password from stdin)";

/// Ошибки хранилища. Тексты не содержат ни идентификатора, ни секрета.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(
        "no API client credentials are stored for profile `{profile}`; store them with \
         `appgallery-connect-mcp auth login --profile {profile}`"
    )]
    NotFound { profile: String },
    /// В хранилище лежит значение, которое не является учётными данными API-клиента.
    #[error(
        "the value stored for profile `{profile}` is not a valid API client ID and secret; \
         store them again with `appgallery-connect-mcp auth login --profile {profile}`"
    )]
    Corrupted { profile: String },
    #[error("the system keychain is not available: {0}")]
    Unavailable(String),
    #[error("system keychain error: {0}")]
    Backend(String),
}

/// Хранилище учётных данных API-клиентов по имени профиля.
pub trait CredentialStore: Send + Sync {
    /// Учётные данные профиля.
    fn get(&self, profile: &str) -> Result<ApiClientCredentials, StoreError>;

    /// Сохраняет учётные данные профиля, заменяя прежние.
    fn set(&self, profile: &str, credentials: &ApiClientCredentials) -> Result<(), StoreError>;

    /// Удаляет запись профиля; если её нет — [`StoreError::NotFound`].
    fn delete(&self, profile: &str) -> Result<(), StoreError>;

    /// Доступно ли хранилище (есть ли Keychain, Credential Manager или Secret Service).
    /// Ничего не записывает и не читает учётные данные профилей.
    fn check_available(&self) -> Result<(), StoreError>;

    /// Есть ли запись у профиля. Значение наружу не возвращается; запись, которая не
    /// разбирается, тоже считается существующей.
    fn contains(&self, profile: &str) -> Result<bool, StoreError> {
        match self.get(profile) {
            Ok(_) | Err(StoreError::Corrupted { .. }) => Ok(true),
            Err(StoreError::NotFound { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }
}

/// Хранилище на базе системного keyring (Keychain на macOS, Credential Manager на Windows,
/// Secret Service на Linux).
#[derive(Debug, Default, Clone, Copy)]
pub struct KeyringStore;

impl KeyringStore {
    fn entry(profile: &str) -> Result<keyring::Entry, StoreError> {
        keyring::Entry::new(KEYRING_SERVICE, profile).map_err(|err| map_error(profile, err))
    }
}

/// Переводит ошибку keyring в [`StoreError`]. Значение записи в текст ошибки не попадает.
fn map_error(profile: &str, err: keyring::Error) -> StoreError {
    map_error_for(profile, err, USES_SECRET_SERVICE)
}

/// [`map_error`] с явной платформой: так подсказку для Secret Service можно проверить в
/// тестах на любой ОС.
fn map_error_for(profile: &str, err: keyring::Error, secret_service: bool) -> StoreError {
    match err {
        keyring::Error::NoEntry => StoreError::NotFound {
            profile: profile.to_owned(),
        },
        // `BadEncoding` содержит байты сохранённого значения — их не выводим.
        keyring::Error::BadEncoding(_) => StoreError::Corrupted {
            profile: profile.to_owned(),
        },
        // Нет сессионной шины D-Bus, нет демона Secret Service или хранилище заблокировано.
        keyring::Error::NoStorageAccess(details) | keyring::Error::PlatformFailure(details)
            if secret_service =>
        {
            StoreError::Unavailable(format!("{SECRET_SERVICE_HINT}. Details: {details}"))
        }
        keyring::Error::NoStorageAccess(details) => StoreError::Unavailable(details.to_string()),
        other => StoreError::Backend(other.to_string()),
    }
}

impl CredentialStore for KeyringStore {
    fn get(&self, profile: &str) -> Result<ApiClientCredentials, StoreError> {
        let bytes = Zeroizing::new(
            Self::entry(profile)?
                .get_secret()
                .map_err(|err| map_error(profile, err))?,
        );
        let corrupted = || StoreError::Corrupted {
            profile: profile.to_owned(),
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| corrupted())?;
        ApiClientCredentials::from_stored_json(text).map_err(|_| corrupted())
    }

    fn set(&self, profile: &str, credentials: &ApiClientCredentials) -> Result<(), StoreError> {
        let json = credentials.to_stored_json();
        Self::entry(profile)?
            .set_secret(json.as_bytes())
            .map_err(|err| map_error(profile, err))
    }

    fn delete(&self, profile: &str) -> Result<(), StoreError> {
        Self::entry(profile)?
            .delete_credential()
            .map_err(|err| map_error(profile, err))
    }

    fn check_available(&self) -> Result<(), StoreError> {
        // Чтение несуществующей записи: «записи нет» означает, что хранилище отвечает.
        match Self::entry(PROBE_ACCOUNT)?.get_secret() {
            Ok(value) => {
                drop(Zeroizing::new(value));
                Ok(())
            }
            Err(keyring::Error::NoEntry | keyring::Error::BadEncoding(_)) => Ok(()),
            Err(err) => Err(map_error(PROBE_ACCOUNT, err)),
        }
    }
}

/// Имя сервиса записей с секретами, которые создают или получают инструменты сервера.
/// Отдельно от [`KEYRING_SERVICE`]: учётные данные API-клиента и эти секреты не
/// смешиваются, и `auth logout` их не трогает.
pub const SECRETS_KEYRING_SERVICE: &str = "dev.filipov.appgallery-connect-mcp.secrets";

/// Секреты инструментов профиля в системном keyring: сервис [`SECRETS_KEYRING_SERVICE`],
/// учётная запись `<профиль>/<имя записи>`. Значения только записываются и проверяются на
/// наличие: метода чтения нет, в ответы инструментов они не попадают.
#[derive(Debug, Clone)]
pub struct KeyringVault {
    profile: String,
}

impl KeyringVault {
    pub fn new(profile: impl Into<String>) -> Self {
        Self {
            profile: profile.into(),
        }
    }

    /// Имя учётной записи keyring для записи `name`.
    pub fn account(&self, name: &str) -> String {
        format!("{}/{name}", self.profile)
    }

    fn entry(&self, name: &str) -> Result<(keyring::Entry, String), StoreError> {
        let account = self.account(name);
        let entry = keyring::Entry::new(SECRETS_KEYRING_SERVICE, &account)
            .map_err(|err| map_error(&account, err))?;
        Ok((entry, account))
    }

    /// Сохраняет значение записи, заменяя прежнее.
    pub fn store(&self, name: &str, value: &str) -> Result<(), StoreError> {
        let (entry, account) = self.entry(name)?;
        entry
            .set_secret(value.as_bytes())
            .map_err(|err| map_error(&account, err))
    }

    /// Есть ли запись. Значение не возвращается.
    pub fn contains(&self, name: &str) -> Result<bool, StoreError> {
        let (entry, account) = self.entry(name)?;
        match entry.get_secret() {
            Ok(value) => {
                drop(Zeroizing::new(value));
                Ok(true)
            }
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(keyring::Error::BadEncoding(_)) => Ok(true),
            Err(err) => Err(map_error(&account, err)),
        }
    }
}

/// Хранилище в памяти для тестов: системный keychain в автотестах не используется.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<String, ApiClientCredentials>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, ApiClientCredentials>>, StoreError> {
        self.entries
            .lock()
            .map_err(|_| StoreError::Backend("the in-memory credential store is poisoned".into()))
    }
}

impl CredentialStore for MemoryStore {
    fn get(&self, profile: &str) -> Result<ApiClientCredentials, StoreError> {
        self.lock()?
            .get(profile)
            .cloned()
            .ok_or_else(|| StoreError::NotFound {
                profile: profile.to_owned(),
            })
    }

    fn set(&self, profile: &str, credentials: &ApiClientCredentials) -> Result<(), StoreError> {
        self.lock()?.insert(profile.to_owned(), credentials.clone());
        Ok(())
    }

    fn delete(&self, profile: &str) -> Result<(), StoreError> {
        self.lock()?
            .remove(profile)
            .map(|_| ())
            .ok_or_else(|| StoreError::NotFound {
                profile: profile.to_owned(),
            })
    }

    fn check_available(&self) -> Result<(), StoreError> {
        self.lock().map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(tag: &str) -> ApiClientCredentials {
        ApiClientCredentials::from_parts(
            ["1000", "2000", tag].concat(),
            ["Se", "cr", "et", "-", tag].concat().repeat(8),
        )
        .unwrap()
    }

    #[test]
    fn memory_store_round_trip() {
        let store = MemoryStore::new();
        store.check_available().unwrap();
        assert!(!store.contains("default").unwrap());
        assert!(matches!(
            store.get("default"),
            Err(StoreError::NotFound { profile }) if profile == "default"
        ));

        let (one, two) = (credentials("1"), credentials("2"));
        store.set("default", &one).unwrap();
        store.set("other", &two).unwrap();
        assert!(store.contains("default").unwrap());
        assert_eq!(store.get("default").unwrap(), one);
        assert_eq!(store.get("other").unwrap(), two);

        // Повторная запись заменяет прежнюю.
        store.set("default", &two).unwrap();
        assert_eq!(store.get("default").unwrap(), two);

        store.delete("default").unwrap();
        assert!(!store.contains("default").unwrap());
        assert!(matches!(
            store.delete("default"),
            Err(StoreError::NotFound { .. })
        ));
        assert_eq!(store.get("other").unwrap(), two);
    }

    #[test]
    fn vault_accounts_are_per_profile() {
        let vault = KeyringVault::new("work");
        assert_eq!(vault.account("app-secret-1"), "work/app-secret-1");
        assert_ne!(SECRETS_KEYRING_SERVICE, KEYRING_SERVICE);
    }

    #[test]
    fn store_is_object_safe() {
        let store: Box<dyn CredentialStore> = Box::new(MemoryStore::new());
        store.set("p", &credentials("3")).unwrap();
        assert!(store.contains("p").unwrap());
    }

    #[test]
    fn keyring_errors_do_not_leak_values() {
        let secret = credentials("4").client_secret().expose().to_owned();
        let err = map_error_for(
            "default",
            keyring::Error::BadEncoding(secret.clone().into_bytes()),
            false,
        );
        assert!(matches!(err, StoreError::Corrupted { .. }));
        assert!(!err.to_string().contains(&secret));

        let err = map_error_for("default", keyring::Error::NoEntry, true);
        assert!(err.to_string().contains("auth login --profile default"));

        let err = map_error_for(
            "default",
            keyring::Error::NoStorageAccess("no session bus".into()),
            true,
        );
        let text = err.to_string();
        assert!(matches!(err, StoreError::Unavailable(_)));
        assert!(text.contains("Secret Service"), "{text}");
        assert!(text.contains("no session bus"), "{text}");

        let err = map_error_for(
            "default",
            keyring::Error::NoStorageAccess("locked".into()),
            false,
        );
        assert!(!err.to_string().contains("Secret Service"));
    }
}
