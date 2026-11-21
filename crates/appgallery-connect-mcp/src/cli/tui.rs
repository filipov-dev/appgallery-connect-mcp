//! `appgallery-connect-mcp ui` — TUI профилей ([`mcp_kit::tui`]): профили и их настройки,
//! права и одобрение на macOS, регистрации в Claude Code и Codex, проверка API-клиента в
//! AppGallery Connect и инструменты с вызовом.
//!
//! Здесь только сервисная часть ([`AgcAdapter`]): чтение и запись `profiles.toml`, состояние
//! учётных данных (регион, API-клиент или вход пользователя, API-клиенты проектов, секреты
//! приложений — без значений), проверка их запросами к API, сборка сервера и повторная
//! регистрация без флагов доступа. Сервер для инструментов собирается так же, как `serve`
//! без флагов: права профиля, урезанные до одобренных.
//!
//! Роль API-клиента AppGallery Connect клиенту не сообщает: вкладка Account показывает, какие
//! пробные запросы проходят, и где роль видна в консоли.

use std::path::{Path, PathBuf};

use anyhow::anyhow;
use appgallery_connect_api::{ApiClient, ClientConfig, CredentialStore, KeyringStore};
use clap::Args;
use mcp_kit::ProfileAccess;
use mcp_kit::approval::{self, ApprovalStore};
use mcp_kit::call::Session;
use mcp_kit::registrations::{self, Registration, ServerIdentity};
use mcp_kit::tui::{self, Field, ProfileView, ServiceAdapter};
use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool};
use serde_json::{Value, json};

use crate::cli::serve::{ServeArgs, Setup};
use crate::clients::claude::{self, SystemRunner};
use crate::clients::{EXE_STEM, RegisterOptions, Scope, ServeFlags, ServerSpec, codex};
use crate::profiles::{
    DEFAULT_PROFILE, PROFILE_ENV, ProfileKind, ProfileRepo, ProfileSettings, describe_region,
    load_app_secrets, load_projects, save_rotated_sign_in,
};
use crate::ui::CaptureUi;

const UI_HELP: &str = "\
Shows the profiles in profiles.toml with their settings and the state of the credentials \
(never their values): the region, the API client or the user sign-in, the project-level API \
clients and the app secrets; the access of each profile and whether it is approved; where the \
server is registered in Claude Code and Codex and whether those registrations agree with the \
profile; what the API client can do in AppGallery Connect; and the tools the profile gives the \
agent.

The access (read, write or destructive, and the extra directories for local files) is changed \
here: r, w and d set the level, a adds a directory, x removes one, s saves. On macOS, saving \
more access than was approved asks for Touch ID or the macOS password; access that is not \
approved does not take effect. f registers the selected entry again without access flags. \
Press ? for the keys.";

/// Сколько приложений по умолчанию проверяет вкладка Account: проверка — по запросу на
/// приложение.
const MAX_APP_PROBES: usize = 3;

#[derive(Debug, Clone, Args)]
#[command(after_help = UI_HELP)]
pub struct UiArgs {}

pub async fn run(_args: UiArgs) -> anyhow::Result<()> {
    let adapter = AgcAdapter {
        repo: ProfileRepo::open_default()?,
        store: &KeyringStore,
        exe: std::env::current_exe()
            .map_err(|err| anyhow!("cannot determine the path of appgallery-connect-mcp: {err}"))?,
        codex_home: codex::default_codex_home().ok(),
        base_url: None,
        approvals: approval::platform_store(EXE_STEM),
    };
    tui::run(&adapter).await.map_err(|err| anyhow!(err))
}

/// Сервисная часть TUI для AppGallery Connect.
pub struct AgcAdapter<'a> {
    pub repo: ProfileRepo,
    pub store: &'a dyn CredentialStore,
    /// Путь к бинарнику для повторной регистрации.
    pub exe: PathBuf,
    /// `$CODEX_HOME`; `None` — не определить (тогда Codex не регистрируется заново).
    pub codex_home: Option<PathBuf>,
    /// Подмена корня API (`wiremock` в тестах); в рабочей конфигурации — `None`.
    pub base_url: Option<String>,
    /// Одобренные права (macOS, [`approval::platform_store`]); в тестах — `None` или
    /// хранилище в памяти, настоящий Keychain не трогается.
    pub approvals: Option<Box<dyn ApprovalStore>>,
}

impl AgcAdapter<'_> {
    /// Сервер с правами профиля без флагов (как `serve` без флагов), со всеми наборами.
    fn setup(&self, profile: &str) -> anyhow::Result<Setup> {
        let args = ServeArgs {
            toolsets: vec![appgallery_connect_mcp_core::ALL_TOOLSETS.to_owned()],
            ..ServeArgs::for_profile(profile)
        };
        let mut setup = Setup::load(&args, self.store, &self.repo, self.approvals.as_deref())?;
        setup.options.dynamic_toolsets = false;
        if let Some(base_url) = &self.base_url {
            setup.config.base_url = Some(base_url.clone());
        }
        // Новый refresh token входа пользователя (если HUAWEI ID его выдаст) — в хранилище.
        save_rotated_sign_in(&mut setup.config, profile);
        Ok(setup)
    }

    async fn session(&self, setup: Setup) -> Result<Session, String> {
        let server = setup.server().map_err(|err| format!("{err:#}"))?;
        Session::start(server).await.map_err(|err| err.to_string())
    }

    async fn profile_session(&self, profile: &str) -> Result<Session, String> {
        let setup = self.setup(profile).map_err(|err| format!("{err:#}"))?;
        self.session(setup).await
    }

    fn profile_fields(&self, name: &str, settings: &ProfileSettings) -> Vec<Field> {
        let credentials = match settings.kind {
            ProfileKind::ApiClient => match self.store.contains(name) {
                Ok(true) => "API client, stored in the system keychain".to_owned(),
                Ok(false) => {
                    format!("missing: run `appgallery-connect-mcp auth login --profile {name}`")
                }
                Err(err) => format!("cannot check the keychain: {err}"),
            },
            ProfileKind::Oauth => {
                let team = settings.team_id.as_deref().unwrap_or("- (sign in again)");
                match self.store.contains_oauth(name) {
                    Ok(true) => {
                        format!("{}, team {team}, stored", ProfileKind::Oauth.title())
                    }
                    Ok(false) => format!(
                        "{}: missing: run `appgallery-connect-mcp auth login --oauth --profile \
                         {name}`",
                        ProfileKind::Oauth.title()
                    ),
                    Err(err) => format!("cannot check the keychain: {err}"),
                }
            }
        };
        let toolsets = if settings.toolsets.is_empty() {
            "default".to_owned()
        } else {
            settings.toolsets.join(",")
        };
        let projects = stored_list(
            &settings.projects,
            load_projects(self.store, name, &settings.projects).map(|(found, _)| {
                found
                    .iter()
                    .map(|p| p.project_id().to_owned())
                    .collect::<Vec<_>>()
            }),
            &format!("appgallery-connect-mcp auth project add --profile {name}"),
        );
        let app_secrets = stored_list(
            &settings.app_secrets,
            load_app_secrets(self.store, name, &settings.app_secrets).map(|(found, _)| {
                found
                    .iter()
                    .map(|app| app.app_id().to_owned())
                    .collect::<Vec<_>>()
            }),
            &format!("appgallery-connect-mcp auth app-secret set --profile {name}"),
        );
        vec![
            Field::new("Region", describe_region(settings.region)),
            Field::new("Credentials", credentials),
            Field::new("Project clients", projects),
            Field::new("App secrets", app_secrets),
            Field::new("Default apps", list_or_dash(&settings.apps)),
            Field::new("Packages", list_or_dash(&settings.packages)),
            Field::new("Toolsets", toolsets),
            Field::new(
                "Dynamic toolsets",
                if settings.dynamic_toolsets {
                    "on"
                } else {
                    "off"
                },
            ),
            Field::new(
                "Connected to",
                settings.client.clone().unwrap_or_else(|| "-".into()),
            ),
        ]
    }
}

impl ServiceAdapter for AgcAdapter<'_> {
    fn bin(&self) -> &str {
        EXE_STEM
    }

    fn service_name(&self) -> &str {
        "AppGallery Connect"
    }

    fn identity(&self) -> ServerIdentity<'_> {
        ServerIdentity {
            exe_stem: EXE_STEM,
            default_profile: DEFAULT_PROFILE,
            profile_env: PROFILE_ENV,
        }
    }

    fn load_profiles(&self) -> Result<Vec<ProfileView>, String> {
        let profiles = self.repo.list().map_err(|err| format!("{err:#}"))?;
        Ok(profiles
            .iter()
            .map(|(name, settings)| ProfileView {
                name: name.clone(),
                access: settings.access(),
                fields: self.profile_fields(name, settings),
            })
            .collect())
    }

    fn save_access(&self, profile: &str, access: &ProfileAccess) -> Result<(), String> {
        let mut settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        settings.set_access(access);
        self.repo
            .upsert(profile, settings)
            .map_err(|err| format!("{err:#}"))
    }

    async fn check_account(&self, profile: &str) -> Result<Vec<Field>, String> {
        let settings = self
            .repo
            .get(profile)
            .map_err(|err| format!("{err:#}"))?
            .unwrap_or_default();
        let setup = self.setup(profile).map_err(|err| format!("{err:#}"))?;
        let mut fields = credential_checks(&setup.config).await;

        let session = self.session(setup).await?;
        for app in settings.apps.iter().take(MAX_APP_PROBES) {
            let value = probe(
                &session,
                "/api/publish/v2/app-info",
                json!({"appId": app}),
                app_name,
            )
            .await;
            fields.push(Field::new(format!("App {app}"), value));
        }
        if settings.apps.is_empty() {
            for package in settings.packages.iter().take(MAX_APP_PROBES) {
                let value = probe(
                    &session,
                    "/api/publish/v2/appid-list",
                    json!({"packageName": package}),
                    app_ids,
                )
                .await;
                fields.push(Field::new(format!("Package {package}"), value));
            }
        }
        let devices = probe(
            &session,
            "/api/publish/v2/device/list",
            json!({}),
            device_count,
        )
        .await;
        session.close().await;
        fields.push(Field::new("HarmonyOS devices", devices));
        fields.push(Field::new(
            "Client role",
            "AppGallery Connect does not report it to the client; see Users and permissions > \
             API key > Connect API. A permission error above shows what the role lacks",
        ));
        Ok(fields)
    }

    async fn tools(&self, profile: &str) -> Result<Vec<Tool>, String> {
        let session = self.profile_session(profile).await?;
        let tools = session.tools().await.map_err(|err| err.to_string());
        session.close().await;
        tools
    }

    async fn call_tool(
        &self,
        profile: &str,
        tool: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, String> {
        let session = self.profile_session(profile).await?;
        let result = session
            .call(tool, arguments)
            .await
            .map_err(|err| err.to_string());
        session.close().await;
        result
    }

    fn reregister(&self, registration: &Registration) -> Result<String, String> {
        reregister(
            registration,
            &self.exe,
            self.codex_home.as_deref(),
            std::env::current_dir().ok().as_deref(),
        )
    }
}

/// Проверка учётных данных: access token API-клиента команды (или входа пользователя),
/// API-клиентов проектов и токены секретов приложений. Ни токены, ни секреты не выводятся:
/// тексты ошибок исполнителя уже без них.
async fn credential_checks(config: &ClientConfig) -> Vec<Field> {
    let mut fields = Vec::new();
    let label = if config.oauth.is_some() {
        "User sign-in"
    } else {
        "API client"
    };
    let main = match ApiClient::new(config.clone()) {
        Ok(client) => {
            let value = match client.authenticate().await {
                Ok(()) => "works: an access token was issued".to_owned(),
                Err(err) => format!("failed: {err}"),
            };
            for app in client.app_ids() {
                let value = match client.authenticate_app(app).await {
                    Ok(()) => "works: an app-level token was issued".to_owned(),
                    Err(err) => format!("failed: {err}"),
                };
                fields.push(Field::new(format!("App secret {app}"), value));
            }
            value
        }
        Err(err) => format!("failed: {err}"),
    };
    fields.insert(0, Field::new(label, main));
    for project in &config.projects {
        let mut project_config =
            ClientConfig::new(config.region, Some(project.credentials().clone()));
        project_config.base_url = config.base_url.clone();
        project_config.timeout = config.timeout;
        let value = match ApiClient::new(project_config) {
            Ok(client) => match client.authenticate().await {
                Ok(()) => "works: an access token was issued".to_owned(),
                Err(err) => format!("failed: {err}"),
            },
            Err(err) => format!("failed: {err}"),
        };
        fields.push(Field::new(
            format!("Project {}", project.project_id()),
            value,
        ));
    }
    fields
}

/// Пробный запрос чтения через `api_request`: краткий итог или «нет доступа» с первой
/// строкой ошибки.
async fn probe(
    session: &Session,
    path: &str,
    query: Value,
    summarize: fn(&Value) -> Option<String>,
) -> String {
    let arguments = object(json!({"path": path, "query": query}));
    match session.call("api_request", arguments).await {
        Ok(result) => probe_text(&result, summarize),
        Err(err) => format!("failed: {err}"),
    }
}

/// Ответ проверки: итог `summarize` по телу ответа или «нет доступа» с первой строкой
/// ошибки.
fn probe_text(result: &CallToolResult, summarize: fn(&Value) -> Option<String>) -> String {
    let text = result
        .content
        .iter()
        .find_map(|content| match content {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    if result.is_error == Some(true) {
        let first = text.lines().next().unwrap_or("error");
        let first: String = first.chars().take(160).collect();
        return format!("no access: {first}");
    }
    let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    // Ответ со страницами обёрнут: тело — в `result`.
    let body = match &value {
        Value::Object(map) if map.contains_key("pagination") => {
            map.get("result").cloned().unwrap_or(Value::Null)
        }
        other => other.clone(),
    };
    summarize(&body).unwrap_or_else(|| "accessible".to_owned())
}

/// Название приложения из `app-info`.
fn app_name(body: &Value) -> Option<String> {
    body["languages"]
        .as_array()?
        .iter()
        .find_map(|language| language["appName"].as_str())
        .map(|name| format!("accessible: {name}"))
}

/// App ID из `appid-list`.
fn app_ids(body: &Value) -> Option<String> {
    let ids: Vec<&str> = body["appids"]
        .as_array()?
        .iter()
        .filter_map(|pair| pair["value"].as_str())
        .collect();
    Some(if ids.is_empty() {
        "accessible, no app with this package".to_owned()
    } else {
        format!("accessible: app ID {}", ids.join(", "))
    })
}

/// Число устройств из списка устройств Provisioning API.
fn device_count(body: &Value) -> Option<String> {
    let count = body["totalCount"]
        .as_u64()
        .or_else(|| body["deviceList"].as_array().map(|list| list.len() as u64))?;
    Some(format!("accessible: {count} registered"))
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

fn list_or_dash(values: &[String]) -> String {
    if values.is_empty() {
        "-".to_owned()
    } else {
        values.join(", ")
    }
}

/// Список ID из профиля с пометкой, для каких нет записи в хранилище.
fn stored_list(ids: &[String], found: anyhow::Result<Vec<String>>, fix: &str) -> String {
    if ids.is_empty() {
        return "-".to_owned();
    }
    let found = match found {
        Ok(found) => found,
        Err(err) => return format!("{}; cannot check the keychain: {err:#}", ids.join(", ")),
    };
    let missing: Vec<&str> = ids
        .iter()
        .map(String::as_str)
        .filter(|id| !found.iter().any(|known| known == id))
        .collect();
    if missing.is_empty() {
        format!("{} (stored)", ids.join(", "))
    } else {
        format!(
            "{}; not usable: {} (`{fix}`)",
            ids.join(", "),
            missing.join(", ")
        )
    }
}

/// Регистрирует запись заново: та же команда `serve --profile <профиль>` с теми же наборами,
/// но без флагов доступа. Записи проекта и локальные записи Claude Code — только из каталога
/// проекта.
fn reregister(
    registration: &Registration,
    exe: &Path,
    codex_home: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<String, String> {
    let serve = &registration.serve;
    let flags = ServeFlags {
        toolsets: serve.toolsets.clone(),
        dynamic_toolsets: serve.dynamic_toolsets.unwrap_or(false),
    };
    let spec = ServerSpec::new(&registration.name, exe, &serve.profile, &flags)
        .map_err(|err| format!("{err:#}"))?;
    let options = RegisterOptions {
        yes: true,
        force: false,
        dry_run: false,
    };
    let mut ui = CaptureUi::default();
    let outcome = match registration.client {
        registrations::Client::ClaudeCode => {
            let scope = match &registration.scope {
                registrations::Scope::User => Scope::User,
                registrations::Scope::Project(dir) if Some(dir.as_path()) == cwd => Scope::Project,
                registrations::Scope::Local(dir) if Some(dir.as_path()) == cwd => Scope::Local,
                other => {
                    let scope = match other {
                        registrations::Scope::Local(_) => "local",
                        _ => "project",
                    };
                    return Err(format!(
                        "`{}` is registered for {other}; run `appgallery-connect-mcp connect \
                         claude-code --scope {scope} --name {} --profile {}` in that project",
                        registration.name, registration.name, serve.profile
                    ));
                }
            };
            claude::register(&mut SystemRunner, &mut ui, scope, &spec, options)
        }
        registrations::Client::Codex => {
            let home = codex_home.ok_or("cannot determine $CODEX_HOME")?;
            codex::register(&mut ui, home, &spec, options)
        }
    }
    .map_err(|err| format!("{err:#}"))?;
    match outcome {
        crate::clients::Registration::Changed | crate::clients::Registration::UpToDate => {
            Ok(format!(
                "{} now runs `{}` as `{}`; restart the client to pick it up",
                registration.client.display_name(),
                spec.display(),
                spec.name
            ))
        }
        crate::clients::Registration::Declined | crate::clients::Registration::DryRun => Err(ui
            .lines
            .last()
            .cloned()
            .unwrap_or_else(|| "not changed".into())),
    }
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::MemoryStore;
    use mcp_kit::registrations::ServeInvocation;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::clients::TEST_EXE;
    use crate::testing::{TOKEN_PATH, access_token, client_id, client_secret, credentials};

    fn adapter<'a>(dir: &Path, store: &'a MemoryStore) -> AgcAdapter<'a> {
        AgcAdapter {
            repo: ProfileRepo::in_dir(dir.join("config")),
            store,
            exe: PathBuf::from(TEST_EXE),
            codex_home: Some(dir.join("codex")),
            base_url: None,
            approvals: None,
        }
    }

    #[test]
    fn shows_profiles_without_credentials_and_saves_access() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        store.set("work", &credentials("tui")).unwrap();
        store
            .set_project("work", "100", &credentials("project"))
            .unwrap();
        let adapter = adapter(dir.path(), &store);
        adapter
            .repo
            .upsert(
                "work",
                ProfileSettings {
                    region: appgallery_connect_api::Region::Ru,
                    apps: vec!["100000001".into()],
                    allow_write: true,
                    projects: vec!["100".into(), "200".into()],
                    ..ProfileSettings::default()
                },
            )
            .unwrap();
        adapter
            .repo
            .upsert("empty", ProfileSettings::default())
            .unwrap();

        let profiles = adapter.load_profiles().unwrap();
        assert_eq!(profiles.len(), 2);
        let work = profiles.iter().find(|p| p.name == "work").unwrap();
        assert_eq!(work.access.level, mcp_kit::AccessLevel::Write);
        let text = format!("{:?}", work.fields);
        assert!(
            text.contains("API client, stored in the system keychain"),
            "{text}"
        );
        assert!(text.contains("ru (Russia"), "{text}");
        assert!(text.contains("100, 200; not usable: 200"), "{text}");
        for secret in [
            client_id("tui"),
            client_secret("tui"),
            client_secret("project"),
        ] {
            assert!(!text.contains(&secret), "{text}");
        }
        let empty = profiles.iter().find(|p| p.name == "empty").unwrap();
        assert!(format!("{:?}", empty.fields).contains("auth login --profile empty"));

        let root = dir.path().join("files");
        adapter
            .save_access(
                "work",
                &ProfileAccess::new(mcp_kit::AccessLevel::Destructive, vec![root.clone()]),
            )
            .unwrap();
        let saved = adapter.repo.get("work").unwrap().unwrap();
        assert!(saved.allow_write && saved.allow_destructive);
        assert_eq!(saved.allow_path, vec![root]);
        // Остальные настройки профиля не тронуты.
        assert_eq!(saved.apps, ["100000001"]);
        assert_eq!(saved.projects, ["100", "200"]);
    }

    #[tokio::test]
    async fn account_check_shows_what_the_client_can_do() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": access_token(), "expires_in": 172_800})),
            )
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/publish/v2/app-info"))
            .and(query_param("appId", "100000001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 0, "msg": "success"},
                "appInfo": {"defaultLang": "en-US"},
                "languages": [{"lang": "en-US", "appName": "Demo"}]
            })))
            .expect(1)
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/publish/v2/device/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 204144645, "msg": "no permission"}
            })))
            .expect(1)
            .mount(&api)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let store = MemoryStore::new();
        store.set("work", &credentials("tui")).unwrap();
        store
            .set_project("work", "100", &credentials("project"))
            .unwrap();
        let mut adapter = adapter(dir.path(), &store);
        adapter.base_url = Some(api.uri());
        adapter
            .repo
            .upsert(
                "work",
                ProfileSettings {
                    apps: vec!["100000001".into()],
                    projects: vec!["100".into()],
                    ..ProfileSettings::default()
                },
            )
            .unwrap();

        let fields = adapter.check_account("work").await.unwrap();
        let text = format!("{fields:?}");
        let value = |label: &str| {
            fields
                .iter()
                .find(|field| field.label == label)
                .map(|field| field.value.clone())
                .unwrap_or_else(|| panic!("no {label}: {text}"))
        };
        assert!(value("API client").starts_with("works"), "{text}");
        assert!(value("Project 100").starts_with("works"), "{text}");
        assert_eq!(value("App 100000001"), "accessible: Demo");
        assert!(
            value("HarmonyOS devices").starts_with("no access"),
            "{text}"
        );
        assert!(
            value("Client role").contains("API key > Connect API"),
            "{text}"
        );
        assert!(!text.contains(&access_token()), "{text}");
        assert!(!text.contains(&client_secret("tui")), "{text}");
    }

    #[test]
    fn probe_results_are_short() {
        let ok = CallToolResult::success(vec![ContentBlock::text(
            r#"{"ret": {"code": 0}, "totalCount": 2, "deviceList": []}"#,
        )]);
        assert_eq!(probe_text(&ok, device_count), "accessible: 2 registered");
        let ids = CallToolResult::success(vec![ContentBlock::text(
            r#"{"appids": [{"key": "com.example.app", "value": "100000001"}]}"#,
        )]);
        assert_eq!(probe_text(&ids, app_ids), "accessible: app ID 100000001");
        let plain = CallToolResult::success(vec![ContentBlock::text("{}")]);
        assert_eq!(probe_text(&plain, app_name), "accessible");
        let err = CallToolResult::error(vec![ContentBlock::text(
            "AppGallery Connect returned 403 Forbidden\nmore",
        )]);
        assert_eq!(
            probe_text(&err, app_name),
            "no access: AppGallery Connect returned 403 Forbidden"
        );
    }

    fn registration(client: registrations::Client, scope: registrations::Scope) -> Registration {
        Registration {
            client,
            scope,
            name: "agc-work".into(),
            command: PathBuf::from(TEST_EXE),
            args: vec![],
            env_names: vec![],
            enabled: true,
            serve: ServeInvocation {
                subcommand: Some("serve".into()),
                profile: "work".into(),
                toolsets: vec!["apps".into()],
                dynamic_toolsets: Some(true),
                ..ServeInvocation::default()
            },
        }
    }

    #[test]
    fn reregisters_codex_without_access_flags() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("codex");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            codex::config_path(&home),
            format!(
                "[mcp_servers.agc-work]\ncommand = {TEST_EXE:?}\nargs = [\"serve\", \"--profile\", \"work\", \"--allow-write\", \"--allow-destructive\"]\n"
            ),
        )
        .unwrap();
        let reg = registration(registrations::Client::Codex, registrations::Scope::User);
        let message = reregister(&reg, Path::new(TEST_EXE), Some(&home), None).unwrap();
        assert!(message.contains("Codex now runs"), "{message}");
        let config = std::fs::read_to_string(codex::config_path(&home)).unwrap();
        assert!(!config.contains("--allow"), "{config}");
        assert!(
            config.contains(
                r#"args = ["serve", "--profile", "work", "--toolsets", "apps", "--dynamic-toolsets"]"#
            ),
            "{config}"
        );
    }

    #[test]
    fn project_registration_elsewhere_is_explained() {
        let (here, elsewhere) = if cfg!(windows) {
            (PathBuf::from(r"C:\here"), PathBuf::from(r"C:\elsewhere"))
        } else {
            (PathBuf::from("/here"), PathBuf::from("/elsewhere"))
        };
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Project(elsewhere.clone()),
        );
        let err = reregister(&reg, Path::new(TEST_EXE), None, Some(&here)).unwrap_err();
        assert!(err.contains("--scope project"), "{err}");
        let reg = registration(
            registrations::Client::ClaudeCode,
            registrations::Scope::Local(elsewhere),
        );
        let err = reregister(&reg, Path::new(TEST_EXE), None, Some(&here)).unwrap_err();
        assert!(err.contains("--scope local"), "{err}");
    }
}
