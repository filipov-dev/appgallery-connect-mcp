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
//! API-клиенты уровня проекта ([`crate::project`]) — вторые учётные данные того же
//! профиля: запись `<профиль>/project/<projectId>` того же сервиса, значение того же вида
//! ([`project_account`], [`CredentialStore::get_project`]). Имя профиля не содержит `/`,
//! поэтому запись проекта не совпадёт с записью другого профиля. Хранилище не умеет
//! перечислять записи: список проектов профиля (без секретов) лежит в файле профилей.
//!
//! Вход пользователя (режим OAuth-клиента, [`crate::user_oauth`]) — отдельная запись
//! `<профиль>/oauth` того же сервиса ([`oauth_account`], [`CredentialStore::get_oauth`]):
//! JSON `{"type": "oauth", "client_id", "client_secret"?, "refresh_token", "scopes"}`.
//! Профиль пользуется либо API-клиентом, либо входом пользователя (поле `type` файла
//! профилей); `auth login` при смене способа удаляет запись прежнего.
//!
//! Секреты приложений ([`crate::app`]: подпись запросов HMS IAP) — третьи записи того же
//! профиля: `<профиль>/app/<appId>` того же сервиса, значение того же вида (client ID и
//! секрет приложения; [`app_account`], [`CredentialStore::get_app_secret`]). Список
//! приложений с сохранённым секретом (без секретов) тоже лежит в файле профилей.
//!
//! Секреты, которые создают или получают сами инструменты сервера (не учётные данные
//! профиля), лежат отдельно: сервис [`SECRETS_KEYRING_SERVICE`], записи
//! `<профиль>/<имя>` ([`KeyringVault`]). Наружу их значения не отдаются.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use zeroize::Zeroizing;

use crate::app::AppCredentials;
use crate::credentials::ApiClientCredentials;
use crate::user_oauth::OAuthCredentials;

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
    #[error(
        "no project-level API client of project `{project_id}` is stored for profile \
         `{profile}`; store it with `appgallery-connect-mcp auth project add --profile {profile} \
         --project-id {project_id}`"
    )]
    ProjectNotFound { profile: String, project_id: String },
    #[error(
        "the value stored for project `{project_id}` of profile `{profile}` is not a valid API \
         client ID and secret; store it again with `appgallery-connect-mcp auth project add \
         --profile {profile} --project-id {project_id}`"
    )]
    ProjectCorrupted { profile: String, project_id: String },
    #[error(
        "no user sign-in is stored for profile `{profile}`; sign in with \
         `appgallery-connect-mcp auth login --oauth --profile {profile}`"
    )]
    OAuthNotFound { profile: String },
    #[error(
        "the user sign-in stored for profile `{profile}` is not valid; sign in again with \
         `appgallery-connect-mcp auth login --oauth --profile {profile}`"
    )]
    OAuthCorrupted { profile: String },
    #[error(
        "no app secret of app `{app_id}` is stored for profile `{profile}`; store it with \
         `appgallery-connect-mcp auth app-secret set --profile {profile} --app {app_id}`"
    )]
    AppSecretNotFound { profile: String, app_id: String },
    #[error(
        "the value stored for the app secret of app `{app_id}` of profile `{profile}` is not \
         a valid client ID and secret; store it again with `appgallery-connect-mcp auth \
         app-secret set --profile {profile} --app {app_id}`"
    )]
    AppSecretCorrupted { profile: String, app_id: String },
    #[error("the system keychain is not available: {0}")]
    Unavailable(String),
    #[error("system keychain error: {0}")]
    Backend(String),
}

/// Имя записи хранилища для API-клиента проекта `project_id` профиля `profile`.
pub fn project_account(profile: &str, project_id: &str) -> String {
    format!("{profile}/project/{project_id}")
}

/// Имя записи хранилища со входом пользователя профиля `profile`.
pub fn oauth_account(profile: &str) -> String {
    format!("{profile}/oauth")
}

impl StoreError {
    /// Ошибка записи входа пользователя: называет `auth login --oauth`.
    fn for_oauth(self, profile: &str) -> Self {
        match self {
            Self::NotFound { .. } => Self::OAuthNotFound {
                profile: profile.to_owned(),
            },
            Self::Corrupted { .. } => Self::OAuthCorrupted {
                profile: profile.to_owned(),
            },
            other => other,
        }
    }
}

/// Имя записи хранилища для секрета приложения `app_id` профиля `profile`.
pub fn app_account(profile: &str, app_id: &str) -> String {
    format!("{profile}/app/{app_id}")
}

impl StoreError {
    /// Ошибка записи секрета приложения: называет приложение и `auth app-secret set`.
    fn for_app(self, profile: &str, app_id: &str) -> Self {
        match self {
            Self::NotFound { .. } => Self::AppSecretNotFound {
                profile: profile.to_owned(),
                app_id: app_id.to_owned(),
            },
            Self::Corrupted { .. } => Self::AppSecretCorrupted {
                profile: profile.to_owned(),
                app_id: app_id.to_owned(),
            },
            other => other,
        }
    }

    /// Ошибка записи проекта: «нет записи» и «запись испорчена» называют проект и команду
    /// `auth project add`, а не `auth login`.
    fn for_project(self, profile: &str, project_id: &str) -> Self {
        match self {
            Self::NotFound { .. } => Self::ProjectNotFound {
                profile: profile.to_owned(),
                project_id: project_id.to_owned(),
            },
            Self::Corrupted { .. } => Self::ProjectCorrupted {
                profile: profile.to_owned(),
                project_id: project_id.to_owned(),
            },
            other => other,
        }
    }
}

/// Хранилище учётных данных API-клиентов по имени профиля.
///
/// Методы `*_project` работают с записями клиентов проектов ([`project_account`]) через
/// основные методы; реализации их не переопределяют.
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

    /// Учётные данные API-клиента проекта `project_id` профиля.
    fn get_project(
        &self,
        profile: &str,
        project_id: &str,
    ) -> Result<ApiClientCredentials, StoreError> {
        self.get(&project_account(profile, project_id))
            .map_err(|err| err.for_project(profile, project_id))
    }

    /// Сохраняет API-клиент проекта, заменяя прежний.
    fn set_project(
        &self,
        profile: &str,
        project_id: &str,
        credentials: &ApiClientCredentials,
    ) -> Result<(), StoreError> {
        self.set(&project_account(profile, project_id), credentials)
            .map_err(|err| err.for_project(profile, project_id))
    }

    /// Удаляет API-клиент проекта; если записи нет — [`StoreError::ProjectNotFound`].
    fn delete_project(&self, profile: &str, project_id: &str) -> Result<(), StoreError> {
        self.delete(&project_account(profile, project_id))
            .map_err(|err| err.for_project(profile, project_id))
    }

    /// Вход пользователя профиля (запись [`oauth_account`]).
    fn get_oauth(&self, profile: &str) -> Result<OAuthCredentials, StoreError>;

    /// Сохраняет вход пользователя профиля, заменяя прежний.
    fn set_oauth(&self, profile: &str, credentials: &OAuthCredentials) -> Result<(), StoreError>;

    /// Удаляет вход пользователя; если записи нет — [`StoreError::OAuthNotFound`].
    fn delete_oauth(&self, profile: &str) -> Result<(), StoreError>;

    /// Есть ли вход пользователя у профиля (испорченная запись тоже считается).
    fn contains_oauth(&self, profile: &str) -> Result<bool, StoreError> {
        match self.get_oauth(profile) {
            Ok(_) | Err(StoreError::OAuthCorrupted { .. }) => Ok(true),
            Err(StoreError::OAuthNotFound { .. }) => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Секрет приложения `app_id` профиля.
    fn get_app_secret(&self, profile: &str, app_id: &str) -> Result<AppCredentials, StoreError> {
        let stored = self
            .get(&app_account(profile, app_id))
            .map_err(|err| err.for_app(profile, app_id))?;
        AppCredentials::from_stored(app_id, stored).map_err(|_| StoreError::AppSecretCorrupted {
            profile: profile.to_owned(),
            app_id: app_id.to_owned(),
        })
    }

    /// Сохраняет секрет приложения, заменяя прежний.
    fn set_app_secret(
        &self,
        profile: &str,
        credentials: &AppCredentials,
    ) -> Result<(), StoreError> {
        let app_id = credentials.app_id();
        self.set(&app_account(profile, app_id), credentials.stored())
            .map_err(|err| err.for_app(profile, app_id))
    }

    /// Удаляет секрет приложения; если записи нет — [`StoreError::AppSecretNotFound`].
    fn delete_app_secret(&self, profile: &str, app_id: &str) -> Result<(), StoreError> {
        self.delete(&app_account(profile, app_id))
            .map_err(|err| err.for_app(profile, app_id))
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

    fn get_oauth(&self, profile: &str) -> Result<OAuthCredentials, StoreError> {
        let account = oauth_account(profile);
        let bytes = Zeroizing::new(
            Self::entry(&account)?
                .get_secret()
                .map_err(|err| map_error(&account, err).for_oauth(profile))?,
        );
        let corrupted = || StoreError::OAuthCorrupted {
            profile: profile.to_owned(),
        };
        let text = std::str::from_utf8(&bytes).map_err(|_| corrupted())?;
        OAuthCredentials::from_stored_json(text).map_err(|_| corrupted())
    }

    fn set_oauth(&self, profile: &str, credentials: &OAuthCredentials) -> Result<(), StoreError> {
        let account = oauth_account(profile);
        let json = credentials.to_stored_json();
        Self::entry(&account)?
            .set_secret(json.as_bytes())
            .map_err(|err| map_error(&account, err).for_oauth(profile))
    }

    fn delete_oauth(&self, profile: &str) -> Result<(), StoreError> {
        let account = oauth_account(profile);
        Self::entry(&account)?
            .delete_credential()
            .map_err(|err| map_error(&account, err).for_oauth(profile))
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
    oauth: Mutex<HashMap<String, OAuthCredentials>>,
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

    fn lock_oauth(&self) -> Result<MutexGuard<'_, HashMap<String, OAuthCredentials>>, StoreError> {
        self.oauth
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

    fn get_oauth(&self, profile: &str) -> Result<OAuthCredentials, StoreError> {
        self.lock_oauth()?
            .get(profile)
            .cloned()
            .ok_or_else(|| StoreError::OAuthNotFound {
                profile: profile.to_owned(),
            })
    }

    fn set_oauth(&self, profile: &str, credentials: &OAuthCredentials) -> Result<(), StoreError> {
        self.lock_oauth()?
            .insert(profile.to_owned(), credentials.clone());
        Ok(())
    }

    fn delete_oauth(&self, profile: &str) -> Result<(), StoreError> {
        self.lock_oauth()?
            .remove(profile)
            .map(|_| ())
            .ok_or_else(|| StoreError::OAuthNotFound {
                profile: profile.to_owned(),
            })
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
    fn project_entries_are_separate_from_profiles() {
        let store = MemoryStore::new();
        let (team, project) = (credentials("5"), credentials("6"));
        store.set("work", &team).unwrap();
        let err = store.get_project("work", "100").unwrap_err();
        assert!(matches!(err, StoreError::ProjectNotFound { .. }), "{err:?}");
        let text = err.to_string();
        assert!(
            text.contains("auth project add --profile work --project-id 100"),
            "{text}"
        );
        store.set_project("work", "100", &project).unwrap();
        assert_eq!(project_account("work", "100"), "work/project/100");
        assert_eq!(store.get_project("work", "100").unwrap(), project);
        assert_eq!(store.get("work").unwrap(), team);
        assert!(store.get_project("other", "100").is_err());
        store.delete("work").unwrap();
        assert_eq!(store.get_project("work", "100").unwrap(), project);
        store.delete_project("work", "100").unwrap();
        assert!(matches!(
            store.delete_project("work", "100"),
            Err(StoreError::ProjectNotFound { .. })
        ));
    }

    #[test]
    fn oauth_entries_are_separate_from_api_clients() {
        use crate::credentials::ClientId;
        use crate::user_oauth::{OAuthClient, OAuthCredentials};
        let store = MemoryStore::new();
        let sign_in = OAuthCredentials::new(
            OAuthClient::new(ClientId::new("1088").unwrap(), None),
            &["Rt", "-", "7"].concat().repeat(4),
            vec!["openid".into()],
        )
        .unwrap();
        store.set("work", &credentials("7")).unwrap();
        let err = store.get_oauth("work").unwrap_err();
        assert!(matches!(err, StoreError::OAuthNotFound { .. }), "{err:?}");
        assert!(
            err.to_string()
                .contains("auth login --oauth --profile work")
        );
        assert!(!store.contains_oauth("work").unwrap());
        store.set_oauth("work", &sign_in).unwrap();
        assert!(store.contains_oauth("work").unwrap());
        assert_eq!(store.get_oauth("work").unwrap(), sign_in);
        assert_eq!(oauth_account("work"), "work/oauth");
        store.delete("work").unwrap();
        assert_eq!(store.get_oauth("work").unwrap(), sign_in);
        store.delete_oauth("work").unwrap();
        assert!(matches!(
            store.delete_oauth("work"),
            Err(StoreError::OAuthNotFound { .. })
        ));
    }

    #[test]
    fn app_secret_entries_are_separate() {
        let store = MemoryStore::new();
        let secret = ["a1b2", "c3d4"].concat().repeat(8);
        let app = AppCredentials::new("100000001", None, secret).unwrap();
        let err = store.get_app_secret("work", "100000001").unwrap_err();
        assert!(
            matches!(err, StoreError::AppSecretNotFound { .. }),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("auth app-secret set --profile work --app 100000001"),
            "{err}"
        );
        store.set_app_secret("work", &app).unwrap();
        assert_eq!(app_account("work", "100000001"), "work/app/100000001");
        assert_eq!(store.get_app_secret("work", "100000001").unwrap(), app);
        assert!(store.get("work").is_err());
        assert!(store.get_project("work", "100000001").is_err());
        store.delete_app_secret("work", "100000001").unwrap();
        assert!(matches!(
            store.delete_app_secret("work", "100000001"),
            Err(StoreError::AppSecretNotFound { .. })
        ));
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
