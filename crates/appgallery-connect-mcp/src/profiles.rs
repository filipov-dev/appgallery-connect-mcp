//! Профили: настройки в `~/.config/appgallery-connect-mcp/profiles.toml`, учётные данные
//! API-клиента — в системном хранилище под именем профиля ([`KeyringStore`]).
//!
//! Один профиль — один API-клиент одного кабинета AppGallery Connect. В файле профилей нет
//! ни секрета, ни идентификатора клиента: только регион, приложения по умолчанию и
//! настройки доступа для `connect` и `serve`.
//!
//! [`KeyringStore`]: appgallery_connect_api::KeyringStore

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use appgallery_connect_api::{
    ApiClient, ApiClientCredentials, ClientConfig, CredentialStore, Region, StoreError,
};
use serde::{Deserialize, Serialize};

/// Переменная окружения, которая переопределяет каталог конфигурации (нужна для тестов).
pub const CONFIG_DIR_ENV: &str = "APPGALLERY_CONNECT_MCP_CONFIG_DIR";

/// Переменная окружения с именем профиля по умолчанию (только имя, не учётные данные).
pub const PROFILE_ENV: &str = "APPGALLERY_CONNECT_MCP_PROFILE";

/// Имя файла профилей в каталоге конфигурации.
pub const PROFILES_FILE: &str = "profiles.toml";

/// Профиль по умолчанию.
pub const DEFAULT_PROFILE: &str = "default";

/// Максимальная длина имени профиля.
const MAX_PROFILE_LEN: usize = 64;

/// Максимальная длина имени пакета.
const MAX_PACKAGE_LEN: usize = 255;

/// Максимальная длина `appId` (в AppGallery Connect — десятичное число).
const MAX_APP_ID_LEN: usize = 20;

/// Настройки профиля. Учётных данных здесь нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// Регион кабинета: от него зависит хост API.
    #[serde(with = "region_slug")]
    pub region: Region,
    /// `appId` приложений по умолчанию (десятичные числа строкой).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub apps: Vec<String>,
    /// Имена пакетов по умолчанию (`com.example.app`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    /// MCP-клиент, в котором `connect` зарегистрировал профиль (`claude-code`, `codex`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Разрешены операции записи (`serve --allow-write`).
    pub allow_write: bool,
    /// Разрешены операции уровня удаления (`serve --allow-destructive`).
    pub allow_destructive: bool,
    /// Включённые наборы инструментов (`serve --toolsets`); пустой список — набор по умолчанию.
    pub toolsets: Vec<String>,
    /// Наборы инструментов включаются агентом по запросу (`serve --dynamic-toolsets`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dynamic_toolsets: bool,
}

impl ProfileSettings {
    /// Действующее разрешение на запись: удаление подразумевает запись.
    pub fn writes_allowed(&self) -> bool {
        self.allow_write || self.allow_destructive
    }

    /// Приводит флаги доступа к действующему уровню: `allow_destructive` включает `allow_write`.
    pub fn normalized(mut self) -> Self {
        self.allow_write = self.writes_allowed();
        self
    }
}

/// Регион в файле профилей — коротким именем (`cn`, `de`, `sg`, `ru`).
mod region_slug {
    use appgallery_connect_api::Region;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(region: &Region, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(region.slug())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Region, D::Error> {
        let text = String::deserialize(deserializer)?;
        super::parse_region(&text).map_err(D::Error::custom)
    }
}

/// Регион по короткому имени; ошибка перечисляет допустимые значения.
pub fn parse_region(value: &str) -> Result<Region, String> {
    Region::from_slug(value.trim()).ok_or_else(|| {
        let known: Vec<&str> = Region::ALL.iter().map(|r| r.slug()).collect();
        format!(
            "unknown region `{}`: use one of {} (cn is the main site, de Germany, sg \
             Singapore, ru Russia)",
            value.trim(),
            known.join(", ")
        )
    })
}

/// Регион для вывода: `cn (China (default), connect-api.cloud.huawei.com)`.
pub fn describe_region(region: Region) -> String {
    format!("{} ({}, {})", region.slug(), region.title(), region.host())
}

/// Проверяет `appId`: десятичное число, как его показывает AppGallery Connect (App
/// information > App ID).
pub fn validate_app_id(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > MAX_APP_ID_LEN
        || !value.chars().all(|c| c.is_ascii_digit())
    {
        bail!(
            "invalid app ID `{value}`: expected the numeric App ID shown in AppGallery Connect \
             (My apps > the app > App information)"
        );
    }
    Ok(value.to_owned())
}

/// Проверяет имя пакета: не меньше двух частей через точку, каждая начинается с буквы и
/// состоит из букв, цифр и `_`.
pub fn validate_package(value: &str) -> anyhow::Result<String> {
    let value = value.trim();
    let valid = !value.is_empty()
        && value.len() <= MAX_PACKAGE_LEN
        && value.split('.').count() >= 2
        && value.split('.').all(|part| {
            part.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
    if !valid {
        bail!("invalid package name `{value}`: expected a package name like com.example.app");
    }
    Ok(value.to_owned())
}

/// Содержимое `profiles.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ProfilesDocument {
    profiles: BTreeMap<String, ProfileSettings>,
}

/// Файл профилей.
#[derive(Debug, Clone)]
pub struct ProfileRepo {
    path: PathBuf,
}

impl ProfileRepo {
    /// Файл профилей в каталоге `dir`.
    pub fn in_dir(dir: impl AsRef<Path>) -> Self {
        Self {
            path: dir.as_ref().join(PROFILES_FILE),
        }
    }

    /// Файл профилей в каталоге конфигурации пользователя.
    pub fn open_default() -> anyhow::Result<Self> {
        Ok(Self::in_dir(config_dir()?))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> anyhow::Result<ProfilesDocument> {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ProfilesDocument::default());
            }
            Err(err) => {
                return Err(err).with_context(|| format!("failed to read {}", self.path.display()));
            }
        };
        toml::from_str(&text).with_context(|| format!("failed to parse {}", self.path.display()))
    }

    fn save(&self, document: &ProfilesDocument) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text = toml::to_string_pretty(document).context("failed to serialize profiles")?;
        fs::write(&self.path, text)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    /// Настройки профиля, если он есть в файле.
    pub fn get(&self, name: &str) -> anyhow::Result<Option<ProfileSettings>> {
        Ok(self.load()?.profiles.remove(name))
    }

    /// Все профили файла, по имени.
    pub fn list(&self) -> anyhow::Result<BTreeMap<String, ProfileSettings>> {
        Ok(self.load()?.profiles)
    }

    /// Создаёт или заменяет профиль. Флаги доступа сохраняются нормализованными
    /// (см. [`ProfileSettings::normalized`]).
    pub fn upsert(&self, name: &str, settings: ProfileSettings) -> anyhow::Result<()> {
        validate_profile_name(name)?;
        let mut document = self.load()?;
        document
            .profiles
            .insert(name.to_owned(), settings.normalized());
        self.save(&document)
    }

    /// Удаляет профиль. Возвращает `true`, если профиль был.
    pub fn remove(&self, name: &str) -> anyhow::Result<bool> {
        let mut document = self.load()?;
        let removed = document.profiles.remove(name).is_some();
        if removed {
            self.save(&document)?;
        }
        Ok(removed)
    }
}

/// Каталог конфигурации: `APPGALLERY_CONNECT_MCP_CONFIG_DIR`, иначе
/// `~/.config/appgallery-connect-mcp`.
///
/// На macOS тоже используется `~/.config`, а не `~/Library/Application Support`: так путь
/// одинаков на macOS и Linux. На Linux учитывается `XDG_CONFIG_HOME`, на Windows —
/// `%APPDATA%\appgallery-connect-mcp` (перемещаемый профиль пользователя).
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os(CONFIG_DIR_ENV).filter(|dir| !dir.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let base = if cfg!(target_os = "macos") {
        dirs::home_dir().map(|home| home.join(".config"))
    } else {
        dirs::config_dir()
    };
    base.map(|base| base.join("appgallery-connect-mcp"))
        .context("cannot determine the user configuration directory")
}

/// Проверяет имя профиля. Имя попадает в аргументы команд MCP-клиентов, имя сервера
/// (`agc-<profile>`), ключи TOML и имя записи в keychain, поэтому допускаются только
/// `[A-Za-z0-9._-]` и имя не начинается с `-`.
pub fn validate_profile_name(name: &str) -> anyhow::Result<()> {
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if name.is_empty() || name.len() > MAX_PROFILE_LEN || !valid_chars || name.starts_with('-') {
        bail!(
            "invalid profile name `{name}`: use 1-{MAX_PROFILE_LEN} characters from A-Z, a-z, \
             0-9, `.`, `_`, `-`, not starting with `-`"
        );
    }
    Ok(())
}

/// Профиль, готовый к работе: настройки и учётные данные API-клиента. `Debug` учётных
/// данных скрыт (см. [`ApiClientCredentials`]).
// Загрузку профиля целиком используют будущие `serve` и `connect`; `auth status` проверяет
// части профиля по отдельности, чтобы показать каждую проблему.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub settings: ProfileSettings,
    pub credentials: ApiClientCredentials,
}

#[allow(dead_code)]
impl Profile {
    /// Настройки клиента API для региона и учётных данных профиля.
    pub fn client_config(&self) -> ClientConfig {
        ClientConfig::new(self.settings.region, Some(self.credentials.clone()))
    }

    /// Клиент AppGallery Connect API профиля. Токен получается при первом запросе.
    pub fn api_client(&self) -> anyhow::Result<ApiClient> {
        ApiClient::new(self.client_config()).context("cannot create the AppGallery Connect client")
    }
}

/// Загружает профиль: настройки из `profiles.toml` (если есть) и учётные данные из
/// системного хранилища. Отсутствие учётных данных — понятная ошибка с подсказкой.
#[allow(dead_code)]
pub fn load_profile_with(
    repo: &ProfileRepo,
    store: &dyn CredentialStore,
    name: &str,
) -> anyhow::Result<Profile> {
    validate_profile_name(name)?;
    let settings = repo.get(name)?.unwrap_or_default();
    let credentials = match store.get(name) {
        Ok(credentials) => credentials,
        Err(err @ (StoreError::NotFound { .. } | StoreError::Corrupted { .. })) => {
            return Err(err.into());
        }
        Err(err) => {
            return Err(anyhow::Error::new(err).context(format!(
                "cannot load the API client credentials of profile `{name}`"
            )));
        }
    };
    Ok(Profile {
        name: name.to_owned(),
        settings,
        credentials,
    })
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::MemoryStore;

    use super::*;
    use crate::testing::credentials;

    fn sample() -> ProfileSettings {
        ProfileSettings {
            region: Region::Ru,
            apps: vec!["100000001".into()],
            packages: vec!["com.example.app".into()],
            client: Some("codex".into()),
            allow_write: true,
            allow_destructive: false,
            toolsets: vec!["apps".into(), "releases".into()],
            dynamic_toolsets: true,
        }
    }

    #[test]
    fn round_trip_in_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("nested"));
        assert_eq!(repo.get("work").unwrap(), None);
        assert!(repo.list().unwrap().is_empty());

        repo.upsert("work", sample()).unwrap();
        repo.upsert(DEFAULT_PROFILE, ProfileSettings::default())
            .unwrap();
        assert_eq!(repo.get("work").unwrap(), Some(sample()));
        assert_eq!(
            repo.get(DEFAULT_PROFILE).unwrap(),
            Some(ProfileSettings::default())
        );
        assert_eq!(
            repo.list().unwrap().keys().collect::<Vec<_>>(),
            [DEFAULT_PROFILE, "work"]
        );

        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("[profiles.work]"), "{text}");
        assert!(text.contains("region = \"ru\""), "{text}");
        assert!(text.contains("region = \"cn\""), "{text}");
        assert!(text.contains("apps = [\"100000001\"]"), "{text}");
        assert!(text.contains("allow_write = true"), "{text}");
        // Пустые необязательные поля не записываются.
        assert_eq!(text.matches("packages").count(), 1, "{text}");

        assert!(repo.remove("work").unwrap());
        assert!(!repo.remove("work").unwrap());
        assert_eq!(repo.get("work").unwrap(), None);
        assert!(repo.get(DEFAULT_PROFILE).unwrap().is_some());
    }

    #[test]
    fn destructive_implies_write() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        fs::write(repo.path(), "[profiles.old]\nallow_destructive = true\n").unwrap();
        let old = repo.get("old").unwrap().unwrap();
        assert!(!old.allow_write);
        assert!(old.writes_allowed());
        assert_eq!(old.region, Region::Cn);
        repo.upsert("old", old).unwrap();
        assert!(repo.get("old").unwrap().unwrap().allow_write);
    }

    #[test]
    fn rejects_unknown_fields_and_regions() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        // Секрет в файле профилей не предусмотрен.
        fs::write(repo.path(), "[profiles.ci]\nclient_secret = \"x\"\n").unwrap();
        assert!(repo.get("ci").is_err());
        fs::write(repo.path(), "[profiles.ci]\nregion = \"us\"\n").unwrap();
        let err = format!("{:#}", repo.get("ci").unwrap_err());
        assert!(err.contains("unknown region `us`"), "{err}");
        fs::write(repo.path(), "[profiles.ci]\nregion = \"DE\"\n").unwrap();
        assert_eq!(repo.get("ci").unwrap().unwrap().region, Region::De);
    }

    #[test]
    fn validates_names_apps_and_packages() {
        for good in ["default", "claude-code", "ci_1", "a.b"] {
            validate_profile_name(good).unwrap();
        }
        for bad in ["", "-x", "a b", "a/b", "проф", &"x".repeat(65)] {
            assert!(validate_profile_name(bad).is_err(), "{bad}");
        }
        assert_eq!(validate_app_id(" 100000001 ").unwrap(), "100000001");
        for bad in ["", "abc", "12-3", "1 2", &"1".repeat(21)] {
            assert!(validate_app_id(bad).is_err(), "{bad}");
        }
        assert_eq!(
            validate_package(" com.example.app ").unwrap(),
            "com.example.app"
        );
        for bad in ["", "app", "com..app", "com.1app", "com.example-app"] {
            assert!(validate_package(bad).is_err(), "{bad}");
        }
        assert_eq!(parse_region(" sg ").unwrap(), Region::Sg);
        let err = parse_region("eu").unwrap_err();
        assert!(err.contains("cn, de, sg, ru"), "{err}");
    }

    #[test]
    fn load_profile_requires_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        let err = load_profile_with(&repo, &store, "work").unwrap_err();
        assert!(
            err.to_string().contains("auth login --profile work"),
            "{err}"
        );

        store.set("work", &credentials("1")).unwrap();
        repo.upsert("work", sample()).unwrap();
        let profile = load_profile_with(&repo, &store, "work").unwrap();
        assert_eq!(profile.name, "work");
        assert_eq!(profile.settings, sample());
        assert_eq!(profile.credentials, credentials("1"));
        let config = profile.client_config();
        assert_eq!(config.region, Region::Ru);
        assert!(config.base_url.is_none());
        let client = profile.api_client().unwrap();
        assert!(!format!("{client:?}").contains(credentials("1").client_secret().expose()));
        assert!(load_profile_with(&repo, &store, "bad name").is_err());

        // Без файла профилей — регион по умолчанию.
        store.set(DEFAULT_PROFILE, &credentials("2")).unwrap();
        let profile = load_profile_with(&repo, &store, DEFAULT_PROFILE).unwrap();
        assert_eq!(profile.settings.region, Region::Cn);
    }
}
