//! Профили: настройки в `~/.config/appgallery-connect-mcp/profiles.toml`, учётные данные
//! API-клиента — в системном хранилище под именем профиля ([`KeyringStore`]).
//!
//! Один профиль — один API-клиент одного кабинета AppGallery Connect. В файле профилей нет
//! ни секрета, ни идентификатора клиента: только регион, приложения по умолчанию,
//! права профиля (уровень доступа и каталоги `allow_path`: флаги `serve` их только сужают,
//! на macOS они ещё и сверяются с одобренными, см. [`mcp_kit::approval`]), наборы
//! инструментов и список проектов, для которых в хранилище
//! лежат API-клиенты уровня проекта (вторые учётные данные того же профиля, запись
//! `<профиль>/project/<projectId>`; хранилище не умеет перечислять записи) и список
//! приложений с секретом приложения в хранилище (запись `<профиль>/app/<appId>`: подпись
//! серверных API покупок в приложении).
//!
//! Профиль может работать и от имени пользователя (режим OAuth-клиента, `type = "oauth"`):
//! тогда в хранилище лежит вход пользователя (запись `<профиль>/oauth`: OAuth-клиент и
//! refresh token), а в файле — команда (`team_id`), от имени которой идут запросы.
//!
//! [`KeyringStore`]: appgallery_connect_api::KeyringStore

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use appgallery_connect_api::user_oauth::validate_team_id;
use appgallery_connect_api::{
    ApiClient, ApiClientCredentials, AppCredentials, ClientConfig, CredentialStore, KeyringStore,
    OAuthCredentials, OAuthSession, ProjectCredentials, Region, StoreError,
};
use mcp_kit::ProfileAccess;
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

/// Как профиль входит в AppGallery Connect.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileKind {
    /// API-клиент команды (`client_id` и секрет).
    #[default]
    ApiClient,
    /// Пользователь HUAWEI ID через OAuth-клиент (`auth login --oauth`).
    Oauth,
}

impl ProfileKind {
    fn is_api_client(&self) -> bool {
        *self == Self::ApiClient
    }

    /// Название для вывода.
    pub fn title(self) -> &'static str {
        match self {
            Self::ApiClient => "API client",
            Self::Oauth => "user sign-in (OAuth client)",
        }
    }
}

/// Настройки профиля. Учётных данных здесь нет.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSettings {
    /// Способ входа: API-клиент (по умолчанию, в файл не пишется) или вход пользователя.
    #[serde(rename = "type", skip_serializing_if = "ProfileKind::is_api_client")]
    pub kind: ProfileKind,
    /// Команда пользователя (`teamId`) для входа пользователя.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
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
    /// Разрешены операции записи (`serve --allow-write` только сужает).
    pub allow_write: bool,
    /// Разрешены операции уровня удаления (`serve --allow-destructive` только сужает).
    pub allow_destructive: bool,
    /// Дополнительные каталоги для локальных файлов инструментов (`serve --allow-path`
    /// только сужает их). Абсолютные пути.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub allow_path: Vec<PathBuf>,
    /// Включённые наборы инструментов (`serve --toolsets`); пустой список — набор по умолчанию.
    pub toolsets: Vec<String>,
    /// Наборы инструментов включаются агентом по запросу (`serve --dynamic-toolsets`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub dynamic_toolsets: bool,
    /// Проекты с API-клиентом уровня проекта в хранилище (`auth project add`), по
    /// порядку. Только идентификаторы проектов: учётные данные — в хранилище.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<String>,
    /// Приложения с секретом приложения в хранилище (`auth app-secret set`), по порядку.
    /// Только App ID: секреты — в хранилище.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub app_secrets: Vec<String>,
}

impl ProfileSettings {
    /// Действующее разрешение на запись: удаление подразумевает запись.
    pub fn writes_allowed(&self) -> bool {
        self.allow_write || self.allow_destructive
    }

    /// Приводит флаги доступа к действующему уровню (`allow_destructive` включает
    /// `allow_write`), а список проектов — к порядку без повторов.
    pub fn normalized(mut self) -> Self {
        self.allow_write = self.writes_allowed();
        self.projects.sort();
        self.projects.dedup();
        self.app_secrets.sort();
        self.app_secrets.dedup();
        self
    }

    /// Права профиля: уровень доступа и каталоги. Флаги `serve` их только сужают, на macOS
    /// они ещё и сверяются с одобренными (см. [`mcp_kit::approval`]).
    pub fn access(&self) -> ProfileAccess {
        ProfileAccess::new(
            mcp_kit::AccessLevel::from_flags(self.allow_write, self.allow_destructive),
            self.allow_path.clone(),
        )
    }

    /// Записывает права в настройки.
    pub fn set_access(&mut self, access: &ProfileAccess) {
        (self.allow_write, self.allow_destructive) = access.level.to_flags();
        self.allow_path = access.allow_path.clone();
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

/// Учётные данные профиля: API-клиент или вход пользователя с командой. `Debug` значений
/// не раскрывает.
#[derive(Debug, Clone)]
pub enum ProfileAuth {
    ApiClient(ApiClientCredentials),
    User {
        credentials: OAuthCredentials,
        team_id: String,
    },
}

/// Профиль, готовый к работе: настройки и учётные данные (API-клиент или вход
/// пользователя). `Debug` учётных данных скрыт.
// Загрузку профиля целиком используют будущие `serve` и `connect`; `auth status` проверяет
// части профиля по отдельности, чтобы показать каждую проблему.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub settings: ProfileSettings,
    pub auth: ProfileAuth,
    /// API-клиенты проектов из `settings.projects`, которые нашлись в хранилище.
    pub projects: Vec<ProjectCredentials>,
    /// Проекты из `settings.projects` без пригодной записи в хранилище: их операции
    /// вернут ошибку с подсказкой `auth project add`. Текст — для предупреждения в stderr.
    pub project_problems: Vec<String>,
    /// Секреты приложений из `settings.app_secrets`, которые нашлись в хранилище.
    pub apps: Vec<AppCredentials>,
    /// Приложения из `settings.app_secrets` без пригодной записи: их операции IAP вернут
    /// ошибку с подсказкой `auth app-secret set`. Текст — для предупреждения в stderr.
    pub app_problems: Vec<String>,
}

#[allow(dead_code)]
impl Profile {
    /// Настройки клиента API для региона и учётных данных профиля.
    pub fn client_config(&self) -> ClientConfig {
        let region = self.settings.region;
        let mut config = match &self.auth {
            ProfileAuth::ApiClient(credentials) => {
                ClientConfig::new(region, Some(credentials.clone()))
            }
            ProfileAuth::User {
                credentials,
                team_id,
            } => ClientConfig::with_oauth(
                region,
                OAuthSession::new(credentials.clone(), Some(team_id.clone())),
            ),
        };
        config.projects = self.projects.clone();
        config.apps = self.apps.clone();
        config.profile = Some(self.name.clone());
        config
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
    let auth = match settings.kind {
        ProfileKind::ApiClient => match store.get(name) {
            Ok(credentials) => ProfileAuth::ApiClient(credentials),
            Err(err @ (StoreError::NotFound { .. } | StoreError::Corrupted { .. })) => {
                return Err(err.into());
            }
            Err(err) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "cannot load the API client credentials of profile `{name}`"
                )));
            }
        },
        ProfileKind::Oauth => {
            let team_id = profile_team(name, &settings)?;
            match store.get_oauth(name) {
                Ok(credentials) => ProfileAuth::User {
                    credentials,
                    team_id,
                },
                Err(
                    err @ (StoreError::OAuthNotFound { .. } | StoreError::OAuthCorrupted { .. }),
                ) => {
                    return Err(err.into());
                }
                Err(err) => {
                    return Err(anyhow::Error::new(err)
                        .context(format!("cannot load the user sign-in of profile `{name}`")));
                }
            }
        }
    };
    let (projects, project_problems) = load_projects(store, name, &settings.projects)?;
    let (apps, app_problems) = load_app_secrets(store, name, &settings.app_secrets)?;
    Ok(Profile {
        name: name.to_owned(),
        settings,
        auth,
        projects,
        project_problems,
        apps,
        app_problems,
    })
}

/// Команда профиля со входом пользователя: без неё запросы не имеют смысла.
pub fn profile_team(name: &str, settings: &ProfileSettings) -> anyhow::Result<String> {
    let Some(team) = settings.team_id.as_deref() else {
        bail!(
            "profile `{name}` signs in as a user but has no team in {PROFILES_FILE}; sign in \
             again with `appgallery-connect-mcp auth login --oauth --profile {name}`"
        );
    };
    validate_team_id(team).map_err(|err| {
        anyhow::anyhow!(
            "invalid team_id of profile `{name}` in {PROFILES_FILE}: {err}; sign in again with \
             `appgallery-connect-mcp auth login --oauth --profile {name}`"
        )
    })?;
    Ok(team.to_owned())
}

/// Сохраняет новый refresh token входа пользователя в системное хранилище, если сервер
/// входа выдаст его при обновлении access token (`serve`). У API-клиента ничего не делает.
pub fn save_rotated_sign_in(config: &mut ClientConfig, profile: &str) {
    if let Some(session) = config.oauth.as_mut() {
        let profile = profile.to_owned();
        session.sink = Some(std::sync::Arc::new(
            move |credentials: &OAuthCredentials| {
                KeyringStore
                    .set_oauth(&profile, credentials)
                    .map_err(|err| err.to_string())
            },
        ));
    }
}

/// Секреты приложений профиля из хранилища. Нет записи или запись испорчена — приложение
/// пропускается с текстом проблемы; недоступное хранилище — ошибка.
pub fn load_app_secrets(
    store: &dyn CredentialStore,
    profile: &str,
    app_ids: &[String],
) -> anyhow::Result<(Vec<AppCredentials>, Vec<String>)> {
    let mut apps = Vec::new();
    let mut problems = Vec::new();
    for app_id in app_ids {
        // Значение из файла профилей попадает в имя записи хранилища: сначала проверка.
        if let Err(err) = appgallery_connect_api::validate_app_id(app_id) {
            problems.push(format!(
                "app `{}` in {PROFILES_FILE}: {err}",
                app_id.escape_debug()
            ));
            continue;
        }
        match store.get_app_secret(profile, app_id) {
            Ok(app) => apps.push(app),
            Err(
                err
                @ (StoreError::AppSecretNotFound { .. } | StoreError::AppSecretCorrupted { .. }),
            ) => problems.push(err.to_string()),
            Err(err) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "cannot load the app secret of app `{}`",
                    app_id.escape_debug()
                )));
            }
        }
    }
    Ok((apps, problems))
}

/// API-клиенты проектов профиля из хранилища. Нет записи или запись испорчена — проект
/// пропускается с текстом проблемы (запуск сервера из-за одного проекта не ломается);
/// недоступное хранилище — ошибка.
pub fn load_projects(
    store: &dyn CredentialStore,
    profile: &str,
    project_ids: &[String],
) -> anyhow::Result<(Vec<ProjectCredentials>, Vec<String>)> {
    let mut projects = Vec::new();
    let mut problems = Vec::new();
    for project_id in project_ids {
        match store.get_project(profile, project_id) {
            Ok(credentials) => match ProjectCredentials::new(project_id.clone(), credentials) {
                Ok(project) => projects.push(project),
                Err(err) => problems.push(format!(
                    "project `{}` in {PROFILES_FILE}: {err}",
                    project_id.escape_debug()
                )),
            },
            Err(
                err @ (StoreError::ProjectNotFound { .. } | StoreError::ProjectCorrupted { .. }),
            ) => {
                problems.push(err.to_string());
            }
            Err(err) => {
                return Err(anyhow::Error::new(err).context(format!(
                    "cannot load the project-level API client of project `{project_id}`"
                )));
            }
        }
    }
    Ok((projects, problems))
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::MemoryStore;

    use super::*;
    use crate::testing::credentials;

    fn sample() -> ProfileSettings {
        ProfileSettings {
            kind: ProfileKind::ApiClient,
            team_id: None,
            region: Region::Ru,
            apps: vec!["100000001".into()],
            packages: vec!["com.example.app".into()],
            client: Some("codex".into()),
            allow_write: true,
            allow_destructive: false,
            allow_path: vec![],
            toolsets: vec!["apps".into(), "releases".into()],
            dynamic_toolsets: true,
            projects: vec!["100".into()],
            app_secrets: vec!["100000001".into()],
        }
    }

    #[test]
    fn access_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let mut settings = sample();
        assert_eq!(settings.access().level, mcp_kit::AccessLevel::Write);
        let root = dir.path().join("files");
        settings.set_access(&ProfileAccess::new(
            mcp_kit::AccessLevel::Destructive,
            vec![root.clone()],
        ));
        assert!(settings.allow_write && settings.allow_destructive);
        repo.upsert("work", settings.clone()).unwrap();
        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("allow_path = ["), "{text}");
        let loaded = repo.get("work").unwrap().unwrap();
        assert_eq!(loaded.access().allow_path, vec![root]);
        assert_eq!(loaded, settings.clone().normalized());
        settings.set_access(&ProfileAccess::read_only());
        assert!(!settings.allow_write && !settings.allow_destructive);
        assert!(settings.allow_path.is_empty());
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
        assert!(
            matches!(&profile.auth, ProfileAuth::ApiClient(c) if *c == credentials("1")),
            "{profile:?}"
        );
        let config = profile.client_config();
        assert_eq!(config.region, Region::Ru);
        assert!(config.base_url.is_none());
        let client = profile.api_client().unwrap();
        assert!(!format!("{client:?}").contains(credentials("1").client_secret().expose()));
        assert!(load_profile_with(&repo, &store, "bad name").is_err());

        // Клиент проекта из списка профиля попадает в настройки клиента API; проект без
        // записи — проблема, а не ошибка.
        assert!(profile.projects.is_empty());
        assert_eq!(profile.project_problems.len(), 1);
        assert!(
            profile.project_problems[0]
                .contains("auth project add --profile work --project-id 100"),
            "{:?}",
            profile.project_problems
        );
        store.set_project("work", "100", &credentials("3")).unwrap();
        let profile = load_profile_with(&repo, &store, "work").unwrap();
        assert!(profile.project_problems.is_empty());
        let config = profile.client_config();
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.projects[0].project_id(), "100");
        assert_eq!(config.projects[0].credentials(), &credentials("3"));
        assert_eq!(config.profile.as_deref(), Some("work"));

        // Без файла профилей — регион по умолчанию.
        store.set(DEFAULT_PROFILE, &credentials("2")).unwrap();
        let profile = load_profile_with(&repo, &store, DEFAULT_PROFILE).unwrap();
        assert_eq!(profile.settings.region, Region::Cn);
    }

    #[test]
    fn user_sign_in_profiles_need_a_team_and_the_oauth_record() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        let settings = ProfileSettings {
            kind: ProfileKind::Oauth,
            region: Region::Ru,
            ..ProfileSettings::default()
        };
        repo.upsert("guest", settings.clone()).unwrap();
        // API-клиент того же имени не подходит профилю со входом пользователя.
        store.set("guest", &credentials("4")).unwrap();
        let err = load_profile_with(&repo, &store, "guest").unwrap_err();
        assert!(err.to_string().contains("has no team"), "{err}");
        repo.upsert(
            "guest",
            ProfileSettings {
                team_id: Some("7700".into()),
                ..settings
            },
        )
        .unwrap();
        let text = fs::read_to_string(repo.path()).unwrap();
        assert!(text.contains("type = \"oauth\""), "{text}");
        assert!(text.contains("team_id = \"7700\""), "{text}");
        let err = load_profile_with(&repo, &store, "guest").unwrap_err();
        assert!(
            err.to_string()
                .contains("auth login --oauth --profile guest"),
            "{err}"
        );
        store
            .set_oauth("guest", &crate::testing::sign_in("4"))
            .unwrap();
        let profile = load_profile_with(&repo, &store, "guest").unwrap();
        let ProfileAuth::User { team_id, .. } = &profile.auth else {
            panic!("{profile:?}");
        };
        assert_eq!(team_id, "7700");
        let config = profile.client_config();
        assert!(config.credentials.is_none());
        let session = config.oauth.as_ref().unwrap();
        assert_eq!(session.team_id.as_deref(), Some("7700"));
        profile.api_client().unwrap();
        // API-клиентский профиль в файле — без поля `type`.
        repo.upsert("plain", ProfileSettings::default()).unwrap();
        let text = fs::read_to_string(repo.path()).unwrap();
        assert_eq!(text.matches("type =").count(), 1, "{text}");
        fs::write(repo.path(), "[profiles.x]\ntype = \"service-account\"\n").unwrap();
        assert!(repo.get("x").is_err());
    }
}
