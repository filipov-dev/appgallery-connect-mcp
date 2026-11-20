//! `search_apps`: приложения кабинета с их `appId`, именем пакета, названием и проектом, с
//! поиском по имени пакета и названию.
//!
//! Отдельного списка приложений у Publishing API нет. Список собирается через Project
//! Management API: список проектов (`project.project.list`, страницы `fromPage`/`pageSize`)
//! и приложения каждого проекта — из `appList` элемента списка, а если его там нет, из
//! проекта (`project.project.get` с `queryFlag=1`). Поиск идёт по подстроке без учёта
//! регистра в имени пакета и названии; точные имена пакетов — отдельным аргументом.
//! Приложения по умолчанию из профиля (`appId` или имя пакета) идут первыми, в порядке
//! профиля; остальные — в порядке API.
//!
//! Project Management API справочник описывает только для OAuth-клиента и Service Account,
//! а сервер работает API-клиентом. Если API отказал по правам ([`permission_denied`]: HTTP
//! 401 или 403, код результата `401`/`403` или текст про права в конверте при HTTP 200),
//! инструмент не падает, а ищет `appId` по именам пакетов из аргумента, профиля и запроса
//! (если запрос похож на имя пакета) операцией Publishing API `publishing.appId.list` —
//! по одному пакету на запрос, потому что ответ не связывает `appId` с именем пакета. В
//! ответе тогда `source: "publishing"`, `projectsAvailable: false` и причина отказа.
//!
//! Сбой списка проектов не по правам, сбой на второй и следующих страницах и сбой чтения
//! проекта после того, как приложения уже собраны, — ошибка инструмента с числом собранных
//! записей: неполный список без явной пометки агент принял бы за полный.

use std::collections::HashSet;

use appgallery_connect_api::{AccessLevel, ApiError};
use serde_json::{Map, Value, json};

use super::reviews_summary::operation;
use crate::composite::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};

/// Имя инструмента.
pub const NAME: &str = "search_apps";

/// `list_projects`.
const PROJECT_LIST: &str = "project.project.list";
/// `get_project`.
const PROJECT_GET: &str = "project.project.get";
/// `list_app_ids`.
const APP_ID_LIST: &str = "publishing.appId.list";

/// Размер страницы списка проектов (по справочнику — значение по умолчанию).
const PAGE_SIZE: usize = 100;
/// Сколько страниц списка проектов читать за вызов.
const MAX_PAGES: usize = 20;
/// Сколько проектов без `appList` дочитывать отдельным запросом.
const MAX_PROJECT_READS: usize = 100;
/// Сколько имён пакетов искать запасным путём.
const MAX_LOOKUPS: usize = 20;

const DEFAULT_LIMIT: u64 = 50;
const MAX_LIMIT: u64 = 500;

/// Слова в тексте ошибки, по которым отказ считается отказом по правам.
const PERMISSION_WORDS: &[&str] = &[
    "permission",
    "forbidden",
    "unauthori",
    "not authori",
    "authorization fail",
    "access denied",
    "no right",
    "not allowed",
];

const DESCRIPTION: &str = "Lists the apps of this AppGallery Connect account with their \
numeric app IDs, package names, app names and projects, and finds apps by package name or app \
name. Use it to get the `appId` that the other tools take when the user names an app. \
`query` matches a part of the package name or the app name, ignoring case; `packageNames` \
adds exact package names. Without both, all apps are listed. The default apps of the profile \
come first (`default: true`). The list comes from the Project Management API (projects and \
their apps). The Huawei reference describes that API only for OAuth clients and service \
accounts; if it refuses the API client of this server, the tool looks up the app IDs of the \
package names from `packageNames`, the profile and `query` (when it is a package name) with \
`list_app_ids` instead: then `source` is `publishing`, `projectsAvailable` is false, \
`projectsError` says why, and only apps with a known package name are found, without \
projects. `truncated: true` means more apps matched than `limit`; `complete: false` means the \
project list was not read to the end. `notes` explain anything else.";

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
            "query": {
                "type": "string",
                "minLength": 1,
                "maxLength": 200,
                "description": "Part of the package name or the app name, like `example` or \
                    `My App`; case is ignored."
            },
            "packageNames": {
                "type": "array",
                "items": {"type": "string", "minLength": 1, "maxLength": 255},
                "minItems": 1,
                "maxItems": MAX_LOOKUPS,
                "description": "Exact package names to find, like [\"com.example.app\"]. Also \
                    used to look up app IDs when the project list is not available."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": "Maximum number of apps to return. Default 50."
            }
        }
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(search_apps(ctx, args))
}

/// Разобранные аргументы.
#[derive(Debug, Clone, Default)]
struct Request {
    /// Запрос в нижнем регистре.
    query: Option<String>,
    /// Точные имена пакетов (как переданы).
    packages: Vec<String>,
    limit: usize,
}

/// Приложение: только поля для ответа.
#[derive(Debug, Clone, PartialEq)]
struct App {
    app_id: String,
    package: Option<String>,
    name: Option<String>,
    /// Идентификатор и имя проекта; `None` — найдено запасным путём.
    project: Option<(Option<String>, Option<String>)>,
}

/// Итог обхода проектов.
#[derive(Debug, Default)]
struct Walk {
    apps: Vec<App>,
    projects: usize,
    /// Обход остановлен лимитом страниц или чтений проектов.
    stopped: bool,
}

/// Проект из списка: идентификатор, имя и `appList`, если он пришёл в списке.
struct ProjectEntry {
    id: Option<String>,
    name: Option<String>,
    apps: Option<Vec<Value>>,
}

/// Почему обход проектов не удался.
enum WalkError {
    /// Project Management API отказал по правам: нужен запасной путь.
    Denied(ApiError),
    /// Любой другой сбой — ошибка инструмента.
    Failed(ToolError),
}

impl From<ToolError> for WalkError {
    fn from(err: ToolError) -> Self {
        Self::Failed(err)
    }
}

async fn search_apps(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let request = parse_request(&args)?;
    tracing::debug!(
        tool = NAME,
        query = request.query.is_some(),
        packages = request.packages.len(),
        "поиск приложений"
    );
    match walk_projects(ctx).await {
        Ok(walk) => Ok(from_projects(ctx, &request, walk)),
        Err(WalkError::Failed(err)) => Err(err),
        Err(WalkError::Denied(err)) => {
            tracing::debug!(tool = NAME, error = %err, "список проектов недоступен");
            from_publishing(ctx, &request, &err).await
        }
    }
}

fn parse_request(args: &Value) -> Result<Request, ToolError> {
    let query = match args.get("query") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) if text.trim().is_empty() => None,
        Some(Value::String(text)) => Some(text.trim().to_lowercase()),
        Some(_) => return Err(ToolError::new("`query` must be a string")),
    };
    let packages = match args.get("packageNames") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|text| text.trim().to_owned())
                    .filter(|text| !text.is_empty())
                    .ok_or_else(|| ToolError::new("`packageNames` must be a list of package names"))
            })
            .collect::<Result<_, _>>()?,
        Some(_) => {
            return Err(ToolError::new(
                "`packageNames` must be a list of package names",
            ));
        }
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(value) => value
            .as_u64()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or_else(|| {
                ToolError::new(format!("`limit` must be an integer from 1 to {MAX_LIMIT}"))
            })?,
    };
    Ok(Request {
        query,
        packages,
        limit: usize::try_from(limit).map_err(|_| ToolError::new("`limit` is too large"))?,
    })
}

/// Отказ API по правам доступа (а не ошибка в аргументах или сбой сервера).
pub(crate) fn permission_denied(err: &ApiError) -> bool {
    let (status, code, message) = match err {
        ApiError::Http(http) => (
            Some(http.status),
            http.code.as_deref(),
            http.message.as_deref(),
        ),
        ApiError::Result(result) => (None, Some(result.code.as_str()), result.message.as_deref()),
        _ => return false,
    };
    if matches!(status, Some(401 | 403)) || matches!(code.map(str::trim), Some("401" | "403")) {
        return true;
    }
    message.is_some_and(|message| {
        let message = message.to_lowercase();
        PERMISSION_WORDS.iter().any(|word| message.contains(word))
    })
}

/// Обход списка проектов и приложений в них.
async fn walk_projects(ctx: &Context) -> Result<Walk, WalkError> {
    let list_op = operation(PROJECT_LIST)?;
    let get_op = operation(PROJECT_GET)?;
    let mut walk = Walk::default();
    let mut projects: Vec<ProjectEntry> = Vec::new();
    let mut page = 1usize;
    loop {
        let args = json!({"fromPage": page.to_string(), "pageSize": PAGE_SIZE.to_string()});
        let response = match ctx.api.call(list_op, &args).await {
            Ok(response) => response,
            Err(err) if page == 1 && permission_denied(&err) => {
                return Err(WalkError::Denied(err));
            }
            Err(err) => {
                return Err(WalkError::Failed(ToolError::new(format!(
                    "reading the project list failed on page {page} after {} project(s) had \
                     been read, so no app list was returned (it would be incomplete). Retry \
                     later. Error: {}",
                    projects.len(),
                    describe_api_error(&err)
                ))));
            }
        };
        let body = response.body.json().cloned().unwrap_or(Value::Null);
        let list = body
            .get("projectList")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let count = list.len();
        for item in list {
            projects.push(ProjectEntry {
                id: text(item.get("projectId")),
                name: text(item.get("name")),
                // Пустой `appList` — проект без приложений; нет поля — читаем проект.
                apps: item.get("appList").and_then(Value::as_array).cloned(),
            });
        }
        let total = body
            .get("totalCount")
            .and_then(Value::as_u64)
            .and_then(|total| usize::try_from(total).ok());
        let more = count >= PAGE_SIZE && total.is_none_or(|total| projects.len() < total);
        if !more {
            break;
        }
        if page == MAX_PAGES {
            walk.stopped = true;
            break;
        }
        page += 1;
    }
    walk.projects = projects.len();

    let mut seen: HashSet<String> = HashSet::new();
    let mut reads = 0usize;
    for ProjectEntry { id, name, apps } in projects {
        let apps = match (apps, &id) {
            (Some(apps), _) => apps,
            (None, None) => Vec::new(),
            (None, Some(project_id)) => {
                if reads == MAX_PROJECT_READS {
                    walk.stopped = true;
                    continue;
                }
                reads += 1;
                let args = json!({"projectId": project_id, "queryFlag": "1"});
                match ctx.api.call(get_op, &args).await {
                    Ok(response) => response
                        .body
                        .json()
                        .and_then(|body| body.pointer("/project/appList"))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                    Err(err) if walk.apps.is_empty() && permission_denied(&err) => {
                        return Err(WalkError::Denied(err));
                    }
                    Err(err) => {
                        return Err(WalkError::Failed(ToolError::new(format!(
                            "reading the apps of project `{project_id}` failed after {} app(s) \
                             had been collected, so no app list was returned (it would be \
                             incomplete). Retry later. Error: {}",
                            walk.apps.len(),
                            describe_api_error(&err)
                        ))));
                    }
                }
            }
        };
        for item in &apps {
            let Some(app_id) = text(item.get("appId")).or_else(|| text(item.get("appID"))) else {
                continue;
            };
            if !seen.insert(app_id.clone()) {
                continue;
            }
            walk.apps.push(App {
                app_id,
                package: text(item.get("packageName")),
                name: text(item.get("appName")),
                project: Some((id.clone(), name.clone())),
            });
        }
    }
    Ok(walk)
}

/// Ответ по списку проектов.
fn from_projects(ctx: &Context, request: &Request, walk: Walk) -> Value {
    let mut notes = Vec::new();
    if walk.stopped {
        notes.push(format!(
            "The project list was not read to the end (at most {MAX_PAGES} pages of \
             {PAGE_SIZE} projects and {MAX_PROJECT_READS} project reads per call): narrow the \
             search with `packageNames` or `query`, or ask the user for the app ID."
        ));
    }
    if walk.apps.is_empty() {
        notes.push(
            "No apps were found in the projects of this account. If the user's app exists, \
             its account may be on another AppGallery Connect site (see Region in the server \
             instructions), or the API client may not see it."
                .to_owned(),
        );
    }
    let missing = missing_packages(&request.packages, &walk.apps);
    if !missing.is_empty() && !walk.apps.is_empty() {
        notes.push(format!(
            "Not found in the projects: {}. Check the package names and the account.",
            missing.join(", ")
        ));
    }
    let mut out = respond(ctx, request, walk.apps, "projects");
    insert(&mut out, "projectsAvailable", json!(true));
    insert(&mut out, "projects", json!(walk.projects));
    insert(&mut out, "complete", json!(!walk.stopped));
    add_notes(&mut out, notes);
    out
}

/// Запасной путь: `appId` по именам пакетов через Publishing API.
async fn from_publishing(
    ctx: &Context,
    request: &Request,
    denied: &ApiError,
) -> Result<Value, ToolError> {
    let reason = denied.to_string();
    let mut packages: Vec<String> = Vec::new();
    let mut add = |name: &str| {
        let name = name.trim();
        if !name.is_empty() && !packages.iter().any(|p| p.eq_ignore_ascii_case(name)) {
            packages.push(name.to_owned());
        }
    };
    request.packages.iter().for_each(|name| add(name));
    // Пакеты профиля нужны, только если они могут подойти: без отбора или с запросом
    // (он может совпасть с названием). С одними `packageNames` они отсеются в любом случае.
    if request.packages.is_empty() || request.query.is_some() {
        ctx.profile.packages.iter().for_each(|name| add(name));
    }
    if let Some(query) = request.query.as_deref().filter(|q| looks_like_package(q)) {
        add(query);
    }
    if packages.is_empty() {
        return Err(ToolError::new(format!(
            "the apps of the account cannot be listed: the Project Management API refused the \
             API client of this server ({reason}); the Huawei reference describes that API \
             only for OAuth clients and service accounts. Pass the package names in \
             `packageNames` to find their app IDs with `list_app_ids`, or ask the user for \
             the app ID (AppGallery Connect > My apps > the app > App information)."
        )));
    }
    let mut notes = vec![format!(
        "The project list is not available to the API client of this server ({reason}): the \
         Huawei reference describes the Project Management API only for OAuth clients and \
         service accounts. App IDs were looked up by package name with `list_app_ids` instead, \
         so only apps with a known package name are listed, without projects. For other apps \
         ask the user for the package name or the app ID."
    )];
    if packages.len() > MAX_LOOKUPS {
        notes.push(format!(
            "Only the first {MAX_LOOKUPS} of {} package names were looked up.",
            packages.len()
        ));
        packages.truncate(MAX_LOOKUPS);
    }
    let op = operation(APP_ID_LIST)?;
    let mut apps: Vec<App> = Vec::new();
    let mut not_found = Vec::new();
    let mut failures: Vec<(String, ApiError)> = Vec::new();
    for package in &packages {
        match ctx.api.call(op, &json!({"packageName": package})).await {
            Ok(response) => {
                let pairs = response
                    .body
                    .json()
                    .and_then(|body| body.get("appids"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let before = apps.len();
                for pair in &pairs {
                    let Some(app_id) = text(pair.get("value")) else {
                        continue;
                    };
                    if apps.iter().any(|app| app.app_id == app_id) {
                        continue;
                    }
                    apps.push(App {
                        app_id,
                        package: Some(package.clone()),
                        name: text(pair.get("key")),
                        project: None,
                    });
                }
                if apps.len() == before {
                    not_found.push(format!("`{package}`"));
                }
            }
            Err(err) => failures.push((package.clone(), err)),
        }
    }
    if apps.is_empty()
        && let Some((package, err)) = failures.first()
    {
        return Err(ToolError::new(format!(
            "the project list is not available to the API client of this server ({reason}), \
             and looking up the app ID of `{package}` with `list_app_ids` failed too: {}",
            describe_api_error(err)
        )));
    }
    if !not_found.is_empty() {
        notes.push(format!(
            "No app ID was found for {}: check the package names and that the app belongs to \
             this account and site.",
            not_found.join(", ")
        ));
    }
    if !failures.is_empty() {
        let list: Vec<String> = failures
            .iter()
            .map(|(package, err)| format!("`{package}`: {err}"))
            .collect();
        notes.push(format!(
            "Looking up some package names failed: {}.",
            list.join("; ")
        ));
    }
    let mut out = respond(ctx, request, apps, "publishing");
    insert(&mut out, "projectsAvailable", json!(false));
    insert(&mut out, "projectsError", json!(reason));
    add_notes(&mut out, notes);
    Ok(out)
}

/// Общая часть ответа: отбор по запросу, приложения профиля первыми, лимит.
fn respond(ctx: &Context, request: &Request, apps: Vec<App>, source: &str) -> Value {
    let mut matched: Vec<(Option<usize>, App)> = apps
        .into_iter()
        .filter(|app| matches(request, app))
        .map(|app| (default_rank(ctx, &app), app))
        .collect();
    // Сортировка устойчивая: приложения не из профиля остаются в порядке API.
    matched.sort_by_key(|(rank, _)| rank.unwrap_or(usize::MAX));
    let total = matched.len();
    let list: Vec<Value> = matched
        .iter()
        .take(request.limit)
        .map(|(rank, app)| app_json(app, rank.is_some()))
        .collect();
    json!({
        "source": source,
        "apps": list,
        "matched": total,
        "truncated": total > request.limit,
    })
}

/// Подходит ли приложение под запрос: без запроса и пакетов — любое.
fn matches(request: &Request, app: &App) -> bool {
    if request.query.is_none() && request.packages.is_empty() {
        return true;
    }
    let by_package = app.package.as_deref().is_some_and(|package| {
        request
            .packages
            .iter()
            .any(|wanted| wanted.eq_ignore_ascii_case(package))
    });
    let by_query = request.query.as_deref().is_some_and(|query| {
        [app.package.as_deref(), app.name.as_deref()]
            .into_iter()
            .flatten()
            .any(|value| value.to_lowercase().contains(query))
    });
    by_package || by_query
}

/// Место приложения среди приложений по умолчанию профиля: сначала `appId`, затем имена
/// пакетов, в порядке профиля. `None` — не приложение по умолчанию.
fn default_rank(ctx: &Context, app: &App) -> Option<usize> {
    let apps = &ctx.profile.apps;
    apps.iter()
        .position(|id| id.trim() == app.app_id)
        .or_else(|| {
            let package = app.package.as_deref()?;
            ctx.profile
                .packages
                .iter()
                .position(|name| name.trim().eq_ignore_ascii_case(package))
                .map(|index| apps.len() + index)
        })
}

fn app_json(app: &App, default: bool) -> Value {
    let mut object = Map::new();
    object.insert("appId".into(), json!(app.app_id));
    object.insert("packageName".into(), json!(app.package));
    object.insert("appName".into(), json!(app.name));
    if let Some((id, name)) = &app.project {
        object.insert("project".into(), json!({"id": id, "name": name}));
    }
    if default {
        object.insert("default".into(), json!(true));
    }
    Value::Object(object)
}

/// Имена пакетов из аргумента, которых нет среди приложений.
fn missing_packages(packages: &[String], apps: &[App]) -> Vec<String> {
    packages
        .iter()
        .filter(|wanted| {
            !apps.iter().any(|app| {
                app.package
                    .as_deref()
                    .is_some_and(|package| package.eq_ignore_ascii_case(wanted))
            })
        })
        .map(|wanted| format!("`{wanted}`"))
        .collect()
}

/// Похоже на имя пакета: не меньше двух частей через точку из букв, цифр и `_`.
fn looks_like_package(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').collect();
    parts.len() >= 2
        && parts.iter().all(|part| {
            !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// Строка или число из ответа как непустая строка.
fn text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn insert(out: &mut Value, key: &str, value: Value) {
    if let Value::Object(map) = out {
        map.insert(key.to_owned(), value);
    }
}

fn add_notes(out: &mut Value, notes: Vec<String>) {
    if !notes.is_empty() {
        insert(out, "notes", json!(notes));
    }
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

    const PROJECTS: &str = "/api/project-service/v1/projects";
    const APP_IDS: &str = "/api/publish/v2/appid-list";

    fn context(server: &MockServer, apps: &[&str], packages: &[&str]) -> Context {
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
        let token = ["apps", "-test-", "token"].concat();
        let api = ApiClient::with_token_source(config, Arc::new(StaticToken::new(token))).unwrap();
        Context {
            api: Arc::new(api),
            files: Arc::new(crate::files::FilePolicy::disabled()),
            vault: Arc::new(MemoryVault::default()),
            profile: ProfileInfo {
                name: "work".into(),
                apps: apps.iter().map(|app| (*app).to_owned()).collect(),
                packages: packages.iter().map(|p| (*p).to_owned()).collect(),
            },
            access: AccessLevel::Read,
            http: false,
        }
    }

    fn ok(mut data: Value) -> ResponseTemplate {
        data["ret"] = json!({"code": 0, "msg": "success"});
        ResponseTemplate::new(200).set_body_json(data)
    }

    fn app(id: &str, package: &str, name: &str) -> Value {
        json!({"appId": id, "packageName": package, "appName": name, "appType": 1})
    }

    fn project(id: &str, name: &str, apps: Option<Vec<Value>>) -> Value {
        let mut project = json!({"projectId": id, "name": name, "teamId": "1"});
        if let Some(apps) = apps {
            project["appList"] = json!(apps);
        }
        project
    }

    /// Первая страница — 100 проектов (у всех `appList` есть, кроме последнего), вторая —
    /// один проект без `appList`, его приложения читаются из проекта. Сколько раз вызван
    /// каждый адрес, проверяет [`walks_projects_and_pages`].
    async fn mount_projects(server: &MockServer) {
        let mut first: Vec<Value> = (0..99)
            .map(|i| project(&format!("p{i}"), &format!("Empty {i}"), Some(vec![])))
            .collect();
        first[0] = project(
            "p0",
            "Main",
            Some(vec![
                app("100000001", "com.example.notes", "Notes"),
                app("100000002", "com.example.Shop", "Магазин"),
            ]),
        );
        first.push(project("p99", "Games", None));
        Mock::given(method("GET"))
            .and(path(PROJECTS))
            .and(query_param("fromPage", "1"))
            .and(query_param("pageSize", "100"))
            .respond_with(ok(json!({"totalCount": 101, "projectList": first})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path(PROJECTS))
            .and(query_param("fromPage", "2"))
            .respond_with(ok(json!({
                "totalCount": 101,
                "projectList": [project("p100", "Tools", None)],
            })))
            .mount(server)
            .await;
        for (id, apps) in [
            (
                "p99",
                vec![app("100000003", "com.example.game", "Space Game")],
            ),
            (
                "p100",
                vec![app("100000004", "com.other.tool", "Notes Tool")],
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("{PROJECTS}/{id}")))
                .and(query_param("queryFlag", "1"))
                .respond_with(ok(json!({"project": {"projectId": id, "appList": apps}})))
                .mount(server)
                .await;
        }
    }

    fn ids(out: &Value) -> Vec<String> {
        out["apps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|app| app["appId"].as_str().unwrap().to_owned())
            .collect()
    }

    #[tokio::test]
    async fn walks_projects_and_pages() {
        let server = MockServer::start().await;
        mount_projects(&server).await;
        let ctx = context(&server, &[], &[]);
        let out = search_apps(&ctx, json!({})).await.unwrap();
        assert_eq!(out["source"], "projects");
        assert_eq!(out["projectsAvailable"], true);
        assert_eq!(out["projects"], 101);
        assert_eq!(out["complete"], true);
        assert_eq!(out["matched"], 4);
        assert_eq!(out["truncated"], false);
        assert_eq!(
            ids(&out),
            ["100000001", "100000002", "100000003", "100000004"]
        );
        assert_eq!(
            out["apps"][2],
            json!({
                "appId": "100000003",
                "packageName": "com.example.game",
                "appName": "Space Game",
                "project": {"id": "p99", "name": "Games"},
            })
        );
        assert!(out.get("notes").is_none(), "{out}");
        // Две страницы списка и два проекта без `appList`; пустой `appList` не дочитывается.
        let paths: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_owned())
            .collect();
        assert_eq!(
            paths,
            [
                PROJECTS.to_owned(),
                PROJECTS.to_owned(),
                format!("{PROJECTS}/p99"),
                format!("{PROJECTS}/p100"),
            ]
        );

        let out = search_apps(&ctx, json!({"limit": 2})).await.unwrap();
        assert_eq!(ids(&out).len(), 2);
        assert_eq!(out["matched"], 4);
        assert_eq!(out["truncated"], true);
    }

    #[tokio::test]
    async fn finds_by_package_and_name_ignoring_case() {
        let server = MockServer::start().await;
        mount_projects(&server).await;
        let ctx = context(&server, &[], &[]);
        // Название: «notes» совпадает с Notes и Notes Tool.
        let out = search_apps(&ctx, json!({"query": "NOTES"})).await.unwrap();
        assert_eq!(ids(&out), ["100000001", "100000004"]);
        // Имя пакета и кириллица без учёта регистра.
        let out = search_apps(&ctx, json!({"query": "example.shop"}))
            .await
            .unwrap();
        assert_eq!(ids(&out), ["100000002"]);
        let out = search_apps(&ctx, json!({"query": "МАГАЗ"})).await.unwrap();
        assert_eq!(ids(&out), ["100000002"]);
        // Точные пакеты добавляются к запросу; ненайденные — в пометке.
        let out = search_apps(
            &ctx,
            json!({"query": "space", "packageNames": ["COM.OTHER.TOOL", "com.missing.app"]}),
        )
        .await
        .unwrap();
        assert_eq!(ids(&out), ["100000003", "100000004"]);
        let notes = out["notes"].to_string();
        assert!(notes.contains("`com.missing.app`"), "{notes}");
        let out = search_apps(&ctx, json!({"query": "nothing"}))
            .await
            .unwrap();
        assert_eq!(out["matched"], 0);
        assert_eq!(out["apps"], json!([]));
    }

    #[tokio::test]
    async fn profile_apps_come_first() {
        let server = MockServer::start().await;
        mount_projects(&server).await;
        let ctx = context(&server, &["100000004"], &["com.example.game"]);
        let out = search_apps(&ctx, json!({})).await.unwrap();
        assert_eq!(
            ids(&out),
            ["100000004", "100000003", "100000001", "100000002"]
        );
        assert_eq!(out["apps"][0]["default"], true);
        assert_eq!(out["apps"][1]["default"], true);
        assert!(out["apps"][2].get("default").is_none());
        // Отбор действует и на приложения профиля.
        let out = search_apps(&ctx, json!({"query": "notes"})).await.unwrap();
        assert_eq!(ids(&out), ["100000004", "100000001"]);
    }

    #[tokio::test]
    async fn falls_back_to_app_ids_when_projects_are_denied() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(PROJECTS))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "ret": {"code": 403, "msg": "client token authorization fail"},
            })))
            .mount(&server)
            .await;
        for (package, data) in [
            (
                "com.example.notes",
                json!({"appids": [{"key": "Notes", "value": "100000001"}]}),
            ),
            (
                "com.example.game",
                json!({"appids": [{"key": "Space Game", "value": "100000003"}]}),
            ),
            ("com.example.gone", json!({"appids": []})),
        ] {
            Mock::given(method("GET"))
                .and(path(APP_IDS))
                .and(query_param("packageName", package))
                .respond_with(ok(data))
                .expect(1)
                .mount(&server)
                .await;
        }
        // Пакет профиля ищется вместе с переданными и подходит под запрос по названию.
        let ctx = context(&server, &[], &["com.example.game"]);
        let out = search_apps(
            &ctx,
            json!({"query": "GAME", "packageNames": ["com.example.notes", "com.example.gone"]}),
        )
        .await
        .unwrap();
        assert_eq!(out["source"], "publishing");
        assert_eq!(out["projectsAvailable"], false);
        assert!(
            out["projectsError"].as_str().unwrap().contains("HTTP 403"),
            "{out}"
        );
        // Приложение профиля первым; у найденных запасным путём проекта нет.
        assert_eq!(ids(&out), ["100000003", "100000001"]);
        assert_eq!(out["apps"][0]["default"], true);
        assert_eq!(out["apps"][1]["appName"], "Notes");
        assert_eq!(out["apps"][1]["packageName"], "com.example.notes");
        assert!(out["apps"][1].get("project").is_none());
        let notes = out["notes"].to_string();
        assert!(
            notes.contains("OAuth clients and service accounts"),
            "{notes}"
        );
        assert!(notes.contains("`list_app_ids`"), "{notes}");
        assert!(notes.contains("`com.example.gone`"), "{notes}");
    }

    #[tokio::test]
    async fn envelope_permission_errors_also_fall_back() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(PROJECTS))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 205524993, "msg": "The API client has no permission."},
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(APP_IDS))
            .and(query_param("packageName", "com.example.notes"))
            .respond_with(ok(
                json!({"appids": [{"key": "Notes", "value": "100000001"}]}),
            ))
            .mount(&server)
            .await;
        // Запрос, похожий на имя пакета, тоже ищется.
        let ctx = context(&server, &[], &[]);
        let out = search_apps(&ctx, json!({"query": "com.example.notes"}))
            .await
            .unwrap();
        assert_eq!(ids(&out), ["100000001"]);
        assert!(
            out["projectsError"]
                .as_str()
                .unwrap()
                .contains("code 205524993"),
            "{out}"
        );

        // Без имён пакетов искать нечего: ошибка с объяснением.
        let err = search_apps(&ctx, json!({"query": "notes"}))
            .await
            .unwrap_err();
        let text = err.message();
        assert!(text.contains("refused the API client"), "{text}");
        assert!(text.contains("`packageNames`"), "{text}");
    }

    #[tokio::test]
    async fn other_failures_are_tool_errors() {
        let server = MockServer::start().await;
        let first: Vec<Value> = (0..100)
            .map(|i| project(&format!("p{i}"), "P", Some(vec![])))
            .collect();
        Mock::given(method("GET"))
            .and(path(PROJECTS))
            .and(query_param("fromPage", "1"))
            .respond_with(ok(json!({"projectList": first})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(PROJECTS))
            .and(query_param("fromPage", "2"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let ctx = context(&server, &[], &["com.example.notes"]);
        let err = search_apps(&ctx, json!({})).await.unwrap_err();
        let text = err.message();
        assert!(text.contains("page 2 after 100 project(s)"), "{text}");
        assert!(text.contains("HTTP 500"), "{text}");
        // Запасной путь не используется: отказ не по правам.
        let requests = server.received_requests().await.unwrap();
        assert!(requests.iter().all(|r| r.url.path() != APP_IDS));
    }

    #[test]
    fn permission_errors_are_recognized() {
        use appgallery_connect_api::envelope::RESULT_RET_CODE;
        use appgallery_connect_api::executor::{HttpError, ResultError};
        let http = |status: u16, message: Option<&str>| {
            ApiError::Http(Box::new(HttpError {
                status,
                message: message.map(str::to_owned),
                ..HttpError::default()
            }))
        };
        let result = |code: &str, message: &str| {
            ApiError::Result(Box::new(ResultError {
                status: 200,
                code: code.into(),
                message: Some(message.into()),
                operation: None,
                envelope: RESULT_RET_CODE,
                service: appgallery_connect_api::ApiService::Connect,
            }))
        };
        assert!(permission_denied(&http(403, None)));
        assert!(permission_denied(&http(401, None)));
        assert!(permission_denied(&http(400, Some("Permission denied"))));
        assert!(!permission_denied(&http(500, None)));
        assert!(!permission_denied(&http(404, Some("not found"))));
        assert!(permission_denied(&result("403", "x")));
        assert!(permission_denied(&result("1", "Unauthorized client")));
        assert!(!permission_denied(&result("204144660", "Invalid appId.")));
        assert!(!permission_denied(&ApiError::Transport("timeout".into())));
    }

    #[test]
    fn package_names_and_registration() {
        assert!(looks_like_package("com.example.app"));
        assert!(looks_like_package("ru.filipov.prosto_2"));
        assert!(!looks_like_package("notes"));
        assert!(!looks_like_package("my app.x"));
        assert!(!looks_like_package("com..app"));

        let tools = tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, NAME);
        assert_eq!(tools[0].toolset, "apps");
        assert_eq!(tools[0].access, AccessLevel::Read);
        assert_eq!(input_schema()["additionalProperties"], false);
        assert!(DESCRIPTION.contains("`list_app_ids`"));
        for id in [PROJECT_LIST, PROJECT_GET, APP_ID_LIST] {
            assert!(operation(id).is_ok(), "{id}");
        }
    }
}
