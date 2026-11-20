//! Инструмент `api_request`: запрос к любому относительному пути AppGallery Connect API на
//! хосте региона профиля.
//!
//! Нужен для операций без своего инструмента (наборы, которые не включены, исключённые
//! вызываемые операции, пути, которых нет в снимке спецификации).
//!
//! Граница безопасности — исполнитель запросов: хост — только по региону профиля, путь
//! проверяет [`validate_api_path`] (только `/api/…`, без полных адресов, `..`, `//`, раздела
//! получения токена), служебные заголовки отклоняет [`is_forbidden_request_header`],
//! `Authorization` и `client_id` добавляет он сам. Здесь — проверки уровня ядра, которые
//! нужны до запроса и дают агенту понятный текст:
//!
//! - аргументы: `method`, `path`, `query` (объект), `headers` (объект), `body`; другие
//!   отклоняются. Query можно дописать к `path` после `?`;
//! - полный адрес (`https://…`, `//host/…`) отклоняется до разбора пути с подсказкой
//!   передать только путь;
//! - заголовки и параметры query, похожие на учётные данные (`client_id`, `client_secret`,
//!   `access_token`, имена с `token`, `secret`, `password`, `auth`, `cookie`), отклоняются:
//!   учётные данные добавляет только сервер;
//! - операции с `callable = false` (получение токена, шаги загрузки Upload Management API) и
//!   их закрытые каталоги отклоняются при любом методе ([`routes::blocked_operation`]);
//! - уровень доступа — по операции, которой соответствует путь ([`routes::route`]), иначе по
//!   методу; запрос выше уровня сервера отклоняется с названием нужного флага. Наборы
//!   инструментов `api_request` не ограничивают;
//! - конверт результата — конверт совпавшей операции ([`ResultCheck::Envelope`]), иначе
//!   определяется по телу ([`ResultCheck::Detect`]);
//! - путь операции, которой нужен API-клиент проекта (`OperationDef::project_param`),
//!   подписывается клиентом проекта из того же параметра пути, query или заголовка, что и у
//!   её инструмента ([`ApiRequest::select_project`]); без него — ошибка до запроса. Путь
//!   без операции в таблице подписывает клиент команды.
//!
//! Ответ проходит ту же постобработку, что и ответы инструментов операций (вырезание
//! секретов, фильтр `jq`, пометка `note`, лимит размера): её делает сервер. Аргумент `jq`
//! сервер забирает до [`prepare`].

pub mod routes;

use std::sync::Arc;

use appgallery_connect_api::executor::{
    ApiRequest, ResultCheck, is_forbidden_request_header, query_values, validate_api_path,
};
use appgallery_connect_api::spec::{AccessLevel, HttpMethod, OperationDef};
use rmcp::model::{JsonObject, Tool};
use serde_json::{Value, json};

use crate::error::ToolError;
use crate::jq::{JQ_ARG, add_jq_argument};
use crate::registry::{access_hint, access_name, annotations};
use crate::validate::kind;

use routes::{RouteMatch, blocked_operation, percent_decode, route};

/// Имя инструмента.
pub const API_REQUEST_TOOL: &str = "api_request";

/// Аргументы инструмента.
const ARGS: &[&str] = &["method", "path", "query", "headers", "body"];

/// Части имён заголовков и параметров query, по которым они считаются учётными данными
/// (в нижнем регистре, без `-` и `_`).
const CREDENTIAL_NAME_PARTS: &[&str] = &[
    "token", "secret", "password", "passwd", "auth", "cookie", "apikey", "clientid",
];

/// Описание инструмента; аннотации — по максимальному уровню сервера.
pub fn api_request_tool(max_access: AccessLevel, http: bool) -> Tool {
    let methods = match max_access {
        AccessLevel::Read => {
            "GET, and POST requests that only read (such as product and promotion searches)"
        }
        AccessLevel::Write => "GET, POST, PUT and PATCH",
        AccessLevel::Destructive => "GET, POST, PUT, PATCH and DELETE",
    };
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["path"],
        "properties": {
            "method": {
                "type": "string",
                "enum": ["GET", "POST", "PUT", "PATCH", "DELETE"],
                "description": "HTTP method. Default GET."
            },
            "path": {
                "type": "string",
                "description": "AppGallery Connect API path relative to the API host, starting \
                    with `/api/`, as in the operation paths of `api_search`, for example \
                    `/api/publish/v2/app-info` or `/api/reviews/v1/manage/dev/reviews`. Path \
                    parameters are written into the path. Query may follow `?`. Full URLs are \
                    not accepted: the host is the one of the profile region."
            },
            "query": {
                "type": "object",
                "description": "Query parameters, for example `{\"appId\": \"100000001\", \
                    \"lang\": \"en-US\"}`. Arrays are sent as repeated parameters.",
                "additionalProperties": {"type": ["string", "number", "boolean", "array"]}
            },
            "headers": {
                "type": "object",
                "description": "Header parameters of the operation, for example \
                    `{\"appId\": \"100000001\"}` for the PMS API. Authorization, `client_id`, \
                    other credentials and HTTP service headers cannot be set: the server adds \
                    them itself.",
                "additionalProperties": {"type": ["string", "number", "boolean"]}
            },
            "body": {
                "type": ["object", "array"],
                "description": "JSON request body."
            }
        }
    });
    let mut schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    // Своего `jq` в схеме нет, поэтому конфликта имён здесь быть не может.
    let added = add_jq_argument(API_REQUEST_TOOL, &mut schema);
    debug_assert!(added.is_ok(), "{added:?}");
    let description = format!(
        "Call any AppGallery Connect API path (Publishing, Comments, Reports, PMS, Project \
         Management, Provisioning) on the API host of the profile region within this server's \
         access level ({}: {}). Use it for operations without a registered tool: toolsets \
         that are not enabled, and paths that `api_search` finds without a tool. Prefer the \
         dedicated tool when one is registered: it validates arguments and documents the \
         schema. The access level is taken from the operation the method and path match; \
         requests above the server's level are rejected with {}. Errors \
         reported with HTTP 200 and a non-zero result code are returned as tool errors. \
         Authorization and `client_id` are added by the server and cannot be overridden; only \
         relative `/api/...` paths are accepted. The token endpoint and the upload steps of the \
         Upload Management API cannot be called (use the composite upload tool instead). An \
         optional `jq` filter selects parts of a JSON response.",
        access_name(max_access),
        methods,
        if http {
            "a hint to ask for an access token with that level"
        } else {
            "the flag that enables them"
        },
    );
    let mut tool = Tool::new(API_REQUEST_TOOL, description, Arc::new(schema));
    tool.title = Some("AppGallery Connect API request".to_owned());
    tool.annotations = Some(annotations(
        "AppGallery Connect API request",
        max_access,
        None,
    ));
    tool
}

/// Подготовленный запрос `api_request`.
#[derive(Debug)]
pub struct Prepared {
    pub request: ApiRequest,
    /// Операция спецификации, которой соответствует запрос; `None` — путь неизвестен.
    pub matched: Option<RouteMatch>,
    /// Уровень доступа запроса.
    pub access: AccessLevel,
}

impl Prepared {
    /// Операция таблицы (инструмент), которой соответствует запрос: её секретные поля
    /// скрываются так же, как в ответе инструмента.
    pub fn operation(&self) -> Option<&'static OperationDef> {
        match self.matched {
            Some(RouteMatch::Operation(op)) => Some(op),
            _ => None,
        }
    }
}

/// Проверяет аргументы и собирает запрос (см. описание модуля).
pub fn prepare(
    args: &JsonObject,
    max_access: AccessLevel,
    http: bool,
) -> Result<Prepared, ToolError> {
    reject_unknown(args)?;
    let get = |name: &str| args.get(name).filter(|v| !v.is_null());
    let text = |name: &str| -> Result<Option<&str>, ToolError> {
        match get(name) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim())),
            Some(other) => Err(ToolError::new(format!(
                "argument `{name}` must be a string, got {}",
                kind(other)
            ))),
        }
    };
    let method = match text("method")? {
        None | Some("") => HttpMethod::Get,
        Some(m) => parse_method(m)?,
    };
    let Some(raw_path) = text("path")?.filter(|p| !p.is_empty()) else {
        return Err(ToolError::new(
            "argument `path` is required: an AppGallery Connect API path starting with \
             `/api/`, such as `/api/publish/v2/app-info`",
        ));
    };
    if raw_path.contains("://") || raw_path.starts_with("//") {
        return Err(ToolError::new(
            "`path` must be a path relative to the API host, starting with `/api/` (for \
             example `/api/publish/v2/app-info`), not a URL: requests go only to the \
             AppGallery Connect API host of the profile region. Pass only the path part",
        ));
    }
    let (path, inline) = match raw_path.split_once('?') {
        Some((path, query)) => (path, parse_query(query)),
        None => (raw_path, Vec::new()),
    };
    // Та же проверка, что у исполнителя: ошибка видна до проверки уровня доступа.
    validate_api_path(path).map_err(|err| ToolError::new(err.to_string()))?;

    if let Some(op) = blocked_operation(path) {
        let mut message = format!(
            "`{path}` cannot be called through `{API_REQUEST_TOOL}`: it belongs to the \
             operation `{}` ({} {}).",
            op.operation_id,
            op.method.as_str(),
            op.path,
        );
        for text in [op.note, op.alternative] {
            if !text.is_empty() {
                message.push(' ');
                message.push_str(text);
            }
        }
        return Err(ToolError::new(message));
    }

    let mut query = inline;
    if let Some(value) = get("query") {
        query.extend(query_object(value)?);
    }
    for (key, _) in &query {
        if key.trim().is_empty() {
            return Err(ToolError::new("query parameter names must not be empty"));
        }
        if looks_like_credential(key) {
            return Err(ToolError::new(format!(
                "query parameter `{key}` is not accepted: credentials are added by the server \
                 and cannot be passed in requests"
            )));
        }
    }
    let headers = match get("headers") {
        Some(value) => header_object(value)?,
        None => Vec::new(),
    };

    let route = route(method, path);
    if !max_access.allows(route.access) {
        let what = match route.matched {
            Some(RouteMatch::Operation(op)) => format!(" (operation `{}`)", op.tool_name),
            Some(RouteMatch::Excluded(op)) => format!(" (operation `{}`)", op.operation_id),
            None => " (unknown path: the level is taken from the method)".to_owned(),
        };
        return Err(ToolError::new(format!(
            "`{} {path}`{what} needs {} access, but {} {} access. {}",
            method.as_str(),
            access_name(route.access),
            if http {
                "this access token has"
            } else {
                "the server runs with"
            },
            access_name(max_access),
            access_hint(route.access, http),
        )));
    }

    let body = match get("body") {
        Some(value) => Some(parse_body(value)?),
        None => None,
    };
    if body.is_some() && method == HttpMethod::Get {
        return Err(ToolError::new(
            "a GET request cannot have a body; pass parameters in `query`",
        ));
    }

    let mut request = ApiRequest::new(method, path);
    request.query = query;
    request.headers = headers;
    request.body = body;
    // Операции чтения через `POST` (поиск товаров и акций) можно повторить после 5xx.
    request.idempotent = method.is_idempotent() || route.access == AccessLevel::Read;
    match route.matched {
        Some(RouteMatch::Operation(op)) => {
            request.result = match op.result {
                Some(def) => ResultCheck::Envelope(def),
                None => ResultCheck::Detect,
            };
            request.operation = Some(op.tool_name.to_owned());
            request.select_project(op).map_err(|err| {
                ToolError::new(format!(
                    "`{} {path}` is the operation `{}`: {err}",
                    method.as_str(),
                    op.tool_name
                ))
            })?;
        }
        Some(RouteMatch::Excluded(op)) => {
            request.result = ResultCheck::Detect;
            request.operation = Some(op.operation_id.to_owned());
        }
        None => request.result = ResultCheck::Detect,
    }
    Ok(Prepared {
        request,
        matched: route.matched,
        access: route.access,
    })
}

fn reject_unknown(args: &JsonObject) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| !ARGS.contains(&key.as_str()))
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let hint = if unknown.iter().any(|key| looks_like_credential(key)) {
        " (credentials cannot be passed: the server adds Authorization and `client_id` itself)"
    } else if unknown
        .iter()
        .any(|key| key.contains("file") || key.contains("save"))
    {
        " (`api_request` does not upload or save files)"
    } else {
        ""
    };
    Err(ToolError::new(format!(
        "unknown argument(s) {}{hint}; allowed arguments: {}",
        unknown.join(", "),
        ARGS.iter()
            .chain([JQ_ARG].iter())
            .map(|a| format!("`{a}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )))
}

/// Похоже ли имя заголовка или параметра на учётные данные (см.
/// [`CREDENTIAL_NAME_PARTS`]).
fn looks_like_credential(name: &str) -> bool {
    let normalized: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect();
    CREDENTIAL_NAME_PARTS
        .iter()
        .any(|part| normalized.contains(part))
}

fn parse_method(method: &str) -> Result<HttpMethod, ToolError> {
    match method.to_ascii_uppercase().as_str() {
        "GET" => Ok(HttpMethod::Get),
        "POST" => Ok(HttpMethod::Post),
        "PUT" => Ok(HttpMethod::Put),
        "PATCH" => Ok(HttpMethod::Patch),
        "DELETE" => Ok(HttpMethod::Delete),
        other => Err(ToolError::new(format!(
            "unsupported method `{other}`; use GET, POST, PUT, PATCH or DELETE"
        ))),
    }
}

/// Query из строки после `?`: пары `ключ=значение`, percent-кодирование раскрывается.
fn parse_query(query: &str) -> Vec<(String, String)> {
    let decode = |text: &str| percent_decode(&text.replace('+', " "));
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

/// Query из объекта: массивы — повторяющиеся параметры.
fn query_object(value: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let Value::Object(map) = value else {
        return Err(ToolError::new(format!(
            "argument `query` must be an object of query parameters, got {}",
            kind(value)
        )));
    };
    let mut pairs = Vec::new();
    for (key, value) in map {
        let values = query_values(key, value).map_err(|err| ToolError::new(err.to_string()))?;
        for text in values {
            pairs.push((key.clone(), text));
        }
    }
    Ok(pairs)
}

/// Заголовки из объекта: только скалярные значения; служебные заголовки и заголовки,
/// похожие на учётные данные, отклоняются.
fn header_object(value: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let Value::Object(map) = value else {
        return Err(ToolError::new(format!(
            "argument `headers` must be an object of header parameters, got {}",
            kind(value)
        )));
    };
    let mut headers = Vec::new();
    for (name, value) in map {
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(ToolError::new("header names must not be empty"));
        }
        if is_forbidden_request_header(trimmed) || looks_like_credential(trimmed) {
            return Err(ToolError::new(format!(
                "header `{trimmed}` cannot be set: authorization, `client_id`, other \
                 credentials and HTTP service headers are added by the server. Pass only \
                 header parameters of the operation, such as `appId` of the PMS API"
            )));
        }
        let text = match value {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(flag) => flag.to_string(),
            Value::Null => continue,
            other => {
                return Err(ToolError::new(format!(
                    "header `{trimmed}` must be a string, number or boolean, got {}",
                    kind(other)
                )));
            }
        };
        headers.push((trimmed.to_owned(), text));
    }
    Ok(headers)
}

/// Тело: JSON-объект или массив (или строка с ним: так тело передают некоторые клиенты).
fn parse_body(value: &Value) -> Result<Value, ToolError> {
    let parsed = match value {
        Value::String(text) => serde_json::from_str::<Value>(text).ok(),
        other => Some(other.clone()),
    };
    match parsed {
        Some(body @ (Value::Object(_) | Value::Array(_))) => Ok(body),
        _ => Err(ToolError::new(format!(
            "argument `body` must be a JSON object (the request body), got {}",
            kind(value)
        ))),
    }
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::envelope::RESULT_RET_RTN_CODE;

    use super::*;

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => panic!("не объект"),
        }
    }

    fn prepare_with(value: Value, access: AccessLevel) -> Result<Prepared, ToolError> {
        prepare(&args(value), access, false)
    }

    fn error(value: Value, access: AccessLevel) -> String {
        prepare_with(value.clone(), access)
            .map(|p| format!("unexpected success: {p:?}"))
            .unwrap_or_else(|err| err.message().to_owned())
    }

    #[test]
    fn builds_requests_for_known_operations() {
        let prepared = prepare_with(
            json!({
                "path": "/api/reviews/v1/manage/dev/reviews?appId=100000001&countries=RU%2CDE",
                "query": {"pageSize": 10, "lang": ["ru", "en"]}
            }),
            AccessLevel::Read,
        )
        .unwrap();
        let request = &prepared.request;
        assert_eq!(request.method, HttpMethod::Get);
        assert_eq!(request.path, "/api/reviews/v1/manage/dev/reviews");
        assert_eq!(
            request.query,
            [
                ("appId", "100000001"),
                ("countries", "RU,DE"),
                ("lang", "ru"),
                ("lang", "en"),
                ("pageSize", "10"),
            ]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
        );
        assert!(matches!(
            prepared.matched,
            Some(RouteMatch::Operation(op)) if op.tool_name == "list_reviews"
        ));
        assert_eq!(request.result, ResultCheck::Envelope(RESULT_RET_RTN_CODE));
        assert_eq!(request.operation.as_deref(), Some("list_reviews"));
        assert!(request.idempotent);

        // POST, который только читает, доступен на чтении и повторяется после 5xx.
        let prepared = prepare_with(
            json!({
                "method": "post",
                "path": "/api/pms/product-price-service/v1/manage/product/list",
                "headers": {"appId": 100000001},
                "body": "{\"pageNum\": 1}"
            }),
            AccessLevel::Read,
        )
        .unwrap();
        assert_eq!(prepared.access, AccessLevel::Read);
        assert!(prepared.request.idempotent);
        assert_eq!(
            prepared.request.headers,
            [("appId".to_owned(), "100000001".to_owned())]
        );
        assert_eq!(prepared.request.body, Some(json!({"pageNum": 1})));
    }

    #[test]
    fn unknown_paths_detect_the_envelope_and_use_the_method() {
        let prepared = prepare_with(
            json!({"path": "/api/publish/v2/something-new"}),
            AccessLevel::Read,
        )
        .unwrap();
        assert!(prepared.matched.is_none());
        assert_eq!(prepared.request.result, ResultCheck::Detect);
        assert_eq!(prepared.request.operation, None);

        let prepared = prepare_with(
            json!({"method": "PATCH", "path": "/api/publish/v2/something-new", "body": {}}),
            AccessLevel::Write,
        )
        .unwrap();
        assert_eq!(prepared.access, AccessLevel::Write);
        assert!(!prepared.request.idempotent);
        let message = error(
            json!({"method": "DELETE", "path": "/api/publish/v2/something-new"}),
            AccessLevel::Write,
        );
        assert!(message.contains("--allow-destructive"), "{message}");
        assert!(message.contains("unknown path"), "{message}");
    }

    #[test]
    fn http_mode_asks_for_an_access_token_instead_of_flags() {
        let submit = json!({"method": "POST", "path": "/api/publish/v2/app-submit"});
        let message = prepare(&args(submit.clone()), AccessLevel::Write, true)
            .unwrap_err()
            .message()
            .to_owned();
        assert!(
            message.contains("this access token has write access"),
            "{message}"
        );
        assert!(
            message.contains("access token with the destructive level"),
            "{message}"
        );
        assert!(message.contains("server operator"), "{message}");
        assert!(!message.contains("--allow-"), "{message}");
        assert!(prepare(&args(submit), AccessLevel::Destructive, true).is_ok());

        let description = api_request_tool(AccessLevel::Read, true)
            .description
            .unwrap();
        assert!(!description.contains("flag"), "{description}");
        assert!(description.contains("access token"), "{description}");
    }

    #[test]
    fn access_comes_from_the_operation_table() {
        let submit = "/api/publish/v2/app-submit";
        let message = error(json!({"method": "POST", "path": submit}), AccessLevel::Read);
        assert!(message.contains("--allow-destructive"), "{message}");
        assert!(message.contains("`submit_app`"), "{message}");
        let message = error(
            json!({"method": "POST", "path": submit}),
            AccessLevel::Write,
        );
        assert!(message.contains("--allow-destructive"), "{message}");
        assert!(
            prepare_with(
                json!({"method": "POST", "path": submit}),
                AccessLevel::Destructive
            )
            .is_ok()
        );

        // Изменение страницы приложения — запись.
        let message = error(
            json!({"method": "PUT", "path": "/api/publish/v2/app-language-info", "body": {}}),
            AccessLevel::Read,
        );
        assert!(message.contains("--allow-write"), "{message}");
        let prepared = prepare_with(
            json!({"method": "PUT", "path": "/api/publish/v2/app-language-info", "body": {}}),
            AccessLevel::Write,
        )
        .unwrap();
        assert_eq!(prepared.access, AccessLevel::Write);
        // Удаление POST-запросом и ответ на отзыв — удаление, не запись по методу.
        for (method, path) in [
            ("POST", "/api/publish/v2/cert/delete"),
            ("POST", "/api/reviews/v1/manage/dev/reviews"),
            ("PUT", "/api/publish/v2/PHASED-RELEASE"),
        ] {
            let message = error(json!({"method": method, "path": path}), AccessLevel::Write);
            assert!(message.contains("--allow-destructive"), "{path}: {message}");
        }
    }

    #[test]
    fn rejects_urls_foreign_hosts_and_unsafe_paths() {
        for path in [
            "https://connect-api.cloud.huawei.com/api/publish/v2/app-info",
            "https://example.com/api/publish/v2/app-info",
            "//example.com/api/publish/v2/app-info",
            "http:/example.com/api",
            "api/publish/v2/app-info",
            "/publish/v2/app-info",
            "/api/publish/v2/../v2/app-info",
            "/api/publish/v2/%2e%2e/app-info",
            "/api/publish//v2/app-info",
            "/api/publish/v2/app-info#x",
            "/api/publish/v2/app\\info",
        ] {
            let message = error(json!({ "path": path }), AccessLevel::Destructive);
            assert!(
                !message.starts_with("unexpected success"),
                "{path}: {message}"
            );
        }
        let message = error(
            json!({"path": "https://example.com/api/publish/v2/app-info"}),
            AccessLevel::Read,
        );
        assert!(message.contains("not a URL"), "{message}");
        let message = error(
            json!({"path": "/api/publish/v2/../app-info"}),
            AccessLevel::Read,
        );
        assert!(message.contains("`..`"), "{message}");
        let message = error(json!({"path": "/api//publish"}), AccessLevel::Read);
        assert!(
            message.contains("empty path segment") || message.contains("API resource"),
            "{message}"
        );
    }

    #[test]
    fn rejects_credentials_and_service_headers() {
        let info = "/api/publish/v2/app-info";
        for headers in [
            json!({"Authorization": "Bearer x"}),
            json!({"authorization": "Bearer x"}),
            json!({"client_id": "1"}),
            json!({"Client-Id": "1"}),
            json!({"oauth2Token": "x"}),
            json!({"teamId": "1"}),
            json!({"Host": "example.com"}),
            json!({"Content-Type": "text/plain"}),
            json!({"Proxy-Authorization": "x"}),
            json!({"x-client-secret": "x"}),
            json!({"Cookie": "a=b"}),
        ] {
            let message = error(json!({"path": info, "headers": headers}), AccessLevel::Read);
            assert!(message.contains("cannot be set"), "{headers}: {message}");
        }
        for query in [
            json!({"client_secret": "x"}),
            json!({"access_token": "x"}),
            json!({"client_id": "x"}),
        ] {
            let message = error(json!({"path": info, "query": query}), AccessLevel::Read);
            assert!(message.contains("not accepted"), "{query}: {message}");
        }
        let message = error(
            json!({"path": format!("{info}?appId=1&access_token=x")}),
            AccessLevel::Read,
        );
        assert!(message.contains("credentials"), "{message}");
        let message = error(
            json!({"path": info, "authorization": "Bearer x"}),
            AccessLevel::Read,
        );
        assert!(
            message.contains("credentials cannot be passed"),
            "{message}"
        );
        // Обычный заголовок операции проходит.
        assert!(
            prepare_with(
                json!({"path": info, "headers": {"appId": "1"}}),
                AccessLevel::Read
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_the_token_endpoint_and_non_callable_operations() {
        for method in ["POST", "GET"] {
            let message = error(
                json!({"method": method, "path": "/api/oauth2/v1/token"}),
                AccessLevel::Destructive,
            );
            assert!(message.contains("token endpoint"), "{message}");
        }
        for (method, path) in [
            ("GET", "/api/publish/v2/upload-url/for-obs"),
            ("POST", "/api/publish/v2/upload/multipart/init"),
            ("PUT", "/api/publish/v2/upload/multipart/compose"),
            ("GET", "/api/publish/v2/upload-url"),
        ] {
            let message = error(
                json!({"method": method, "path": path, "query": {"appId": "1"}}),
                AccessLevel::Destructive,
            );
            assert!(message.contains("cannot be called"), "{path}: {message}");
            assert!(message.contains("package_upload"), "{path}: {message}");
        }
    }

    #[test]
    fn arguments_are_checked() {
        let info = "/api/publish/v2/app-info";
        for (value, expected) in [
            (json!({}), "`path` is required"),
            (json!({"path": 1}), "must be a string"),
            (
                json!({"path": info, "method": "TRACE"}),
                "unsupported method",
            ),
            (
                json!({"path": info, "query": [1]}),
                "`query` must be an object",
            ),
            (
                json!({"path": info, "query": {"a": {"b": 1}}}),
                "query parameter `a`",
            ),
            (
                json!({"path": info, "headers": "x"}),
                "`headers` must be an object",
            ),
            (
                json!({"path": info, "headers": {"a": [1]}}),
                "header `a` must be",
            ),
            (
                json!({"path": info, "body": {}}),
                "GET request cannot have a body",
            ),
            (
                json!({"method": "PUT", "path": info, "body": 1}),
                "must be a JSON object",
            ),
            (json!({"path": info, "file": "x"}), "does not upload"),
            (
                json!({"path": info, "nope": 1}),
                "unknown argument(s) `nope`",
            ),
        ] {
            let message = error(value.clone(), AccessLevel::Write);
            assert!(message.contains(expected), "{value}: {message}");
        }
    }

    #[test]
    fn tool_annotations_follow_server_access() {
        for (access, read_only, destructive) in [
            (AccessLevel::Read, Some(true), None),
            (AccessLevel::Write, Some(false), Some(false)),
            (AccessLevel::Destructive, Some(false), Some(true)),
        ] {
            let tool = api_request_tool(access, false);
            let a = tool.annotations.unwrap();
            assert_eq!(
                (a.read_only_hint, a.destructive_hint),
                (read_only, destructive)
            );
            for name in ARGS {
                assert!(
                    tool.input_schema["properties"].get(*name).is_some(),
                    "{name}"
                );
            }
            assert!(
                tool.description
                    .as_deref()
                    .unwrap()
                    .contains(access_name(access))
            );
        }
    }
}
