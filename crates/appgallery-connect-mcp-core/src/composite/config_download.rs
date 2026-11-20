//! `config_download`: файл конфигурации приложения `agconnect-services.json` в локальный
//! файл.
//!
//! Файл приходит целиком в поле `content` ответа Project Management API
//! (`project.configFile.get`) и содержит учётные данные приложения (ключ API, секреты
//! клиента). Поэтому инструмент работает только через `save_to`: путь обязателен и
//! проверяется [`Context::files`] до запроса, файл создаётся только новым, содержимое
//! записывает та же функция, что и `save_to` инструментов операций
//! ([`write_file_content`]). Какое поле сохранять, говорит таблица `[[secrets]]`
//! спецификации ([`savable_values`]), а не строка в коде.
//!
//! Агенту и в лог не попадает ни содержимое, ни значения из него: только путь, размер и,
//! если содержимое — JSON-объект, имена его ключей верхнего уровня (`agcgw`, `client`,
//! `app_info`…) без значений. Имена проверяются по виду ([`safe_key`]): ключ, похожий на
//! что-то кроме простого имени, не показывается.
//!
//! Project Management API справочник описывает только для OAuth-клиента и Service Account:
//! с API-клиентом сервера запрос может закончиться ошибкой доступа, она возвращается как
//! ошибка инструмента с подсказкой.

use appgallery_connect_api::AccessLevel;
use serde_json::{Value, json};

use super::reviews_summary::{app_id, operation};
use super::search_apps::permission_denied;
use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};
use crate::files::{display_path, write_file_content};
use crate::redact::{is_credential_key, savable_values};

/// Имя инструмента.
pub const NAME: &str = "config_download";

/// `project.configFile.get` (`get_app_config_file`).
const CONFIG_FILE_GET: &str = "project.configFile.get";

/// Сколько имён ключей верхнего уровня показывать.
const MAX_KEYS: usize = 50;
/// Длина имени ключа, которое ещё показывается.
const MAX_KEY_LEN: usize = 64;

const DESCRIPTION: &str = "Saves the app configuration file agconnect-services.json of one app \
to a new local file (`save_to`, required). The file holds the app's credentials (API key, \
client secrets), so its content is never returned: the result has the path, the size in bytes \
and the names of the top-level keys of the JSON file (`topLevelKeys`, without values). Use it \
when the user wants the configuration file of an app for an Android or HarmonyOS project. Do \
not print, paste or commit the saved file, and never ask the user to paste it into the \
conversation. Existing files are never overwritten. Uses the Project Management API, which the \
Huawei reference describes only for OAuth clients and service accounts: with the API client of \
this server it may fail with a permission error; then the user downloads the file in \
AppGallery Connect (Project settings > General information) themselves.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: "apps",
        access: AccessLevel::Read,
        description: DESCRIPTION,
        input_schema,
        run,
    }]
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "appId": {
                "type": ["string", "integer"],
                "description": "Numeric app ID (like 100000001). Optional when the profile has \
                    exactly one default app; `search_apps` finds it by package name or app name."
            },
            "save_to": {
                "type": "string",
                "minLength": 1,
                "description": "Path of a new local file for agconnect-services.json \
                    (absolute, or relative to the server's working directory; only inside the \
                    allowed directories listed in the server instructions). The parent \
                    directory must exist; existing files are never overwritten."
            }
        },
        "required": ["save_to"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(config_download(ctx, args))
}

async fn config_download(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let app = app_id(ctx, args.get("appId"))?;
    let save_to = match args.get("save_to") {
        Some(Value::String(path)) if !path.trim().is_empty() => path.as_str(),
        _ => {
            return Err(ToolError::new(
                "`save_to` is required: the configuration file holds the app's credentials, \
                 so the server only saves it to a new local file and never returns its content",
            ));
        }
    };
    // Путь проверяется до запроса: ошибка в пути не должна стоить вызова API.
    let target = ctx.files.resolve_write(save_to)?;
    let op = operation(CONFIG_FILE_GET)?;
    tracing::debug!(tool = NAME, app_id = %app, "файл конфигурации приложения");
    let response = ctx
        .api
        .call(op, &json!({"appId": app}))
        .await
        .map_err(|err| {
            let hint = if permission_denied(&err) {
                " The Huawei reference describes the Project Management API only for OAuth \
                 clients and service accounts, so it may be unavailable to the API client of \
                 this server: the user can download agconnect-services.json in AppGallery \
                 Connect (Project settings > General information) themselves."
            } else {
                ""
            };
            ToolError::new(format!(
                "could not get the configuration file of app {app}: {}{hint}",
                describe_api_error(&err)
            ))
        })?;
    let Some(body) = response.body.json() else {
        return Err(ToolError::new(
            "the configuration file was not saved: the API response is not JSON",
        ));
    };
    let content = match savable_values(op, body).as_slice() {
        [(_, content)] => (*content).clone(),
        [] => {
            return Err(ToolError::new(format!(
                "the configuration file was not saved: the response of app {app} has no file \
                 content; check that the app ID is right and that the app has a project"
            )));
        }
        many => {
            return Err(ToolError::new(format!(
                "the configuration file was not saved: the response has {} file contents \
                 instead of one",
                many.len()
            )));
        }
    };
    let bytes = write_file_content(&target, &content)?;
    tracing::debug!(tool = NAME, bytes, "файл конфигурации сохранён");

    let mut result = json!({
        "appId": app,
        "saved": {"path": display_path(&target), "bytes": bytes},
    });
    if let Some(name) = body
        .get("fileName")
        .and_then(Value::as_str)
        .filter(|name| safe_file_name(name))
    {
        result["fileName"] = json!(name);
    }
    let mut notes = vec![
        "The file holds the app's credentials: add it to the app project (for example \
         app/agconnect-services.json), but do not print it, paste it into the conversation or \
         commit it to public repositories."
            .to_owned(),
    ];
    match top_level_keys(&content) {
        Some((keys, hidden)) => {
            result["topLevelKeys"] = json!(keys);
            if hidden > 0 {
                notes.push(format!(
                    "{hidden} top-level key name(s) are not listed: they are not plain names \
                     or there are more than {MAX_KEYS} keys."
                ));
            }
        }
        None => notes.push(
            "The saved content is not a JSON object, so no keys are listed; check the file \
             before using it."
                .to_owned(),
        ),
    }
    result["notes"] = json!(notes);
    Ok(result)
}

/// Имена ключей верхнего уровня JSON-объекта (без значений) и число непоказанных имён;
/// `None` — содержимое не JSON-объект.
fn top_level_keys(content: &Value) -> Option<(Vec<String>, usize)> {
    let parsed;
    let object = match content {
        Value::Object(object) => object,
        Value::String(text) => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            parsed.as_object()?
        }
        _ => return None,
    };
    let mut keys = Vec::new();
    let mut hidden = 0;
    for key in object.keys() {
        if keys.len() < MAX_KEYS && safe_key(key) {
            keys.push(key.clone());
        } else {
            hidden += 1;
        }
    }
    Some((keys, hidden))
}

/// Имя ключа можно показать: короткое простое имя (буквы, цифры, `_`, `-`, `.`), не имя
/// учётных данных.
fn safe_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_LEN
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !is_credential_key(key)
}

/// Имя файла из ответа можно показать: короткое простое имя без каталогов.
fn safe_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use appgallery_connect_api::token::StaticToken;
    use appgallery_connect_api::{
        ApiClient, ApiClientCredentials, ClientConfig, Region, RetryPolicy,
    };
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::{MemoryVault, ProfileInfo};
    use crate::files::FilePolicy;

    const CONFIG: &str = "/api/cpms/project-management-service/v1/config-file";
    const APP: &str = "100000001";

    /// Ключ API внутри файла конфигурации (собран из частей).
    fn api_key() -> String {
        ["Cg", "Fy", "Ke", "y0", "12", "34", "56"]
            .concat()
            .repeat(3)
    }

    fn content() -> String {
        json!({
            "agcgw": {"url": "connect-drru.hispace.dbankcloud.ru"},
            "client": {"app_id": APP, "api_key": api_key(), "package_name": "com.example.app"},
            "configuration_version": "3.0",
        })
        .to_string()
    }

    fn context(server: &MockServer, dir: &std::path::Path, apps: &[&str]) -> Context {
        let id = ["2718", "2818", "2845"].concat();
        let secret = ["QW12", "ER34", "TY56"].concat().repeat(4);
        let credentials = ApiClientCredentials::from_parts(id, secret).unwrap();
        let mut config = ClientConfig::new(Region::Ru, Some(credentials));
        config.base_url = Some(server.uri());
        config.retry = RetryPolicy {
            max_retries: 0,
            default_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
        };
        let token = ["config", "-test-", "token"].concat();
        let api = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token))).unwrap();
        Context {
            api: Arc::new(api),
            files: Arc::new(
                FilePolicy::new(&[dir.to_path_buf()])
                    .unwrap()
                    .with_base_dir(dir),
            ),
            vault: Arc::new(MemoryVault::default()),
            profile: ProfileInfo {
                name: "work".into(),
                apps: apps.iter().map(|app| (*app).to_owned()).collect(),
                packages: Vec::new(),
            },
            access: AccessLevel::Read,
            http: false,
        }
    }

    async fn mount_config(server: &MockServer, content: Value) {
        Mock::given(method("GET"))
            .and(path(CONFIG))
            .and(query_param("appId", APP))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 0, "msg": "success"},
                "fileName": "agconnect-services.json",
                "content": content,
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn saves_the_file_and_returns_only_path_size_and_keys() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        mount_config(&server, json!(content())).await;
        let ctx = context(&server, dir.path(), &[APP]);

        // `appId` — из профиля, путь — относительный.
        let out = config_download(&ctx, json!({"save_to": "agconnect-services.json"}))
            .await
            .unwrap();
        let target = dir.path().join("agconnect-services.json");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), content());
        assert_eq!(out["appId"], APP);
        assert_eq!(out["saved"]["bytes"], content().len());
        assert!(
            out["saved"]["path"]
                .as_str()
                .unwrap()
                .ends_with("agconnect-services.json")
        );
        assert_eq!(out["fileName"], "agconnect-services.json");
        assert_eq!(
            out["topLevelKeys"],
            json!(["agcgw", "client", "configuration_version"])
        );
        let text = out.to_string();
        assert!(!text.contains(&api_key()), "{text}");
        assert!(!text.contains("com.example.app"), "{text}");
        assert!(!text.contains("hispace"), "{text}");

        // Повторное сохранение в тот же файл — отказ до запроса, файл не меняется.
        let before = server.received_requests().await.unwrap().len();
        let err = config_download(
            &ctx,
            json!({"appId": APP, "save_to": target.display().to_string()}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        assert_eq!(server.received_requests().await.unwrap().len(), before);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), content());
    }

    #[tokio::test]
    async fn save_to_is_required_and_checked_before_the_request() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        mount_config(&server, json!(content())).await;
        let ctx = context(&server, dir.path(), &[APP]);

        let err = config_download(&ctx, json!({"appId": APP}))
            .await
            .unwrap_err();
        assert!(err.message().contains("`save_to` is required"), "{err}");
        let target = outside.path().join("agconnect-services.json");
        let err = config_download(
            &ctx,
            json!({"appId": APP, "save_to": target.display().to_string()}),
        )
        .await
        .unwrap_err();
        assert!(err.message().contains("outside the allowed"), "{err}");
        let err = config_download(&ctx, json!({"appId": APP, "save_to": "../x.json"}))
            .await
            .unwrap_err();
        assert!(err.message().contains("outside the allowed"), "{err}");
        assert!(server.received_requests().await.unwrap().is_empty());
        assert!(!target.exists());

        // Схема требует `save_to`.
        assert_eq!(input_schema()["required"], json!(["save_to"]));
    }

    #[tokio::test]
    async fn object_content_and_odd_keys() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        mount_config(
            &server,
            json!({"client": {"api_key": api_key()}, "client_secret": "x", "bad key": 1}),
        )
        .await;
        let ctx = context(&server, dir.path(), &[]);
        let out = config_download(&ctx, json!({"appId": APP, "save_to": "cfg.json"}))
            .await
            .unwrap();
        // Объект сохраняется как JSON, имена учётных данных и странные имена не показываются.
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.path().join("cfg.json")).unwrap())
                .unwrap();
        assert_eq!(saved["client"]["api_key"], api_key());
        assert_eq!(out["topLevelKeys"], json!(["client"]));
        assert!(
            out["notes"][1]
                .as_str()
                .unwrap()
                .contains("2 top-level key")
        );
        assert!(!out.to_string().contains(&api_key()));
    }

    #[tokio::test]
    async fn errors_do_not_create_files() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        Mock::given(method("GET"))
            .and(path(CONFIG))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 0, "msg": "success"},
                "fileName": "agconnect-services.json",
            })))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(CONFIG))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "ret": {"code": 403, "msg": "no permission"},
            })))
            .mount(&server)
            .await;
        let ctx = context(&server, dir.path(), &[]);
        let args = json!({"appId": APP, "save_to": "cfg.json"});
        let err = config_download(&ctx, args.clone()).await.unwrap_err();
        assert!(err.message().contains("has no file content"), "{err}");
        let err = config_download(&ctx, args).await.unwrap_err();
        assert!(err.message().contains("HTTP 403"), "{err}");
        assert!(err.message().contains("roles of the API client"), "{err}");
        assert!(err.message().contains("Project settings"), "{err}");
        assert!(!dir.path().join("cfg.json").exists());
        let err = config_download(&ctx, json!({"save_to": "cfg.json"}))
            .await
            .unwrap_err();
        assert!(err.message().contains("no default app"), "{err}");
    }

    #[test]
    fn tool_is_registered_as_read_only_in_apps() {
        let tools = tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, NAME);
        assert_eq!(tools[0].toolset, "apps");
        assert_eq!(tools[0].access, AccessLevel::Read);
        assert!(operation(CONFIG_FILE_GET).is_ok_and(|op| op.saves_file()));
        assert!(safe_key("app_info") && !safe_key("api_key") && !safe_key("a b"));
        assert!(safe_file_name("agconnect-services.json") && !safe_file_name("../x"));
    }
}
