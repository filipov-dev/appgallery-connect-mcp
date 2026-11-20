//! Выполнение запросов к AppGallery Connect API.
//!
//! Все сгенерированные инструменты и `api_request` работают через
//! [`ApiClient::execute`]: операция из таблицы превращается в [`ApiRequest`]
//! ([`ApiRequest::for_operation`], [`ApiClient::call`]), произвольный запрос собирается
//! вручную ([`ApiRequest::new`]). Это граница безопасности:
//! - хост — только по региону профиля ([`crate::region::Region`]); подмена корня API
//!   принимается лишь для loopback-адресов (тесты с `wiremock`);
//! - путь проверяется [`validate_api_path`]: только `/api/…`, без полных URL, `..`, `//`,
//!   обратных косых и управляющих символов; раздел получения токена (`/api/oauth2`)
//!   запросам недоступен;
//! - заголовки `client_id` и `Authorization: Bearer <token>` добавляет только исполнитель;
//!   запрос не может задать их и другие служебные заголовки
//!   ([`FORBIDDEN_REQUEST_HEADERS`]); оба помечены как чувствительные;
//! - редиректы не выполняются: токен и тело запроса не уходят на другой адрес;
//! - размер тела ответа ограничен `max_response_bytes`, тела ошибки — [`ERROR_BODY_LIMIT`];
//! - HTTP 200 с ненулевым кодом в конверте результата — ошибка [`ApiError::Result`]
//!   ([`crate::envelope`]);
//! - повторы: `429`, `500`, `502`, `503`, `504` — с экспоненциальной задержкой и учётом
//!   `Retry-After`, только для идемпотентных запросов (`GET`, `PUT`, `DELETE` и операции
//!   чтения; `POST` и `PATCH` не повторяются); `401` — один повтор с новым токеном для
//!   любого метода; ошибка соединения до отправки запроса — повтор для любого метода;
//! - токены, секрет и идентификатор клиента вырезаются из текстов ошибок и ответов
//!   ([`ApiClient::scrub`], [`crate::scrub::scrub_secrets`]) — и клиента команды, и
//!   клиентов проектов.
//!
//! Обычные запросы подписывает API-клиент команды ([`ClientConfig::credentials`]).
//! Операции сервисов проекта ([`OperationDef::project_param`]) подписывает API-клиент
//! проекта из [`ClientConfig::projects`]: проект берётся из аргумента операции
//! ([`ApiRequest::project`], [`ApiRequest::select_project`]), у каждого проекта свой
//! источник токенов на том же хосте региона. Нет клиента этого проекта —
//! [`ApiError::MissingProjectCredentials`] без запроса; к клиенту команды такой запрос не
//! откатывается.
//!
//! Вход пользователя (режим OAuth-клиента, [`ClientConfig::oauth`],
//! [`crate::user_oauth`]) вместо API-клиента команды: запрос несёт `oauth2Token` (access
//! token пользователя, без `Bearer`) и `teamId` (команда профиля), без `client_id` и
//! `Authorization`. Оба заголовка задаёт только исполнитель. Операция, для которой
//! справочник не называет этот режим ([`OperationDef::auth_modes`],
//! [`ApiRequest::auth_modes`]), отклоняется до запроса: [`ApiError::UnsupportedAuthMode`].
//! Операции проектов и в этом режиме подписывает API-клиент проекта.
//!
//! Операции HMS IAP ([`OperationDef::service`], [`ApiService`]) идут на хост своего
//! сервиса в регионе профиля (`orders-*`, `subscr-*`; путь проверяет
//! [`crate::path::validate_service_path`]) и подписываются токеном приложения
//! ([`crate::app`]): заголовок `Authorization: Basic base64("APPAT:" + token)`, без
//! `client_id`. Приложение берётся из аргумента `appId` операции ([`ApiRequest::app`]),
//! секрет — из [`ClientConfig::apps`]; нет секрета этого приложения —
//! [`ApiError::MissingAppCredentials`] без запроса. Произвольные запросы
//! ([`ApiRequest::new`]) идут только в AppGallery Connect API: `api_request` не становится
//! клиентом IAP.
//!
//! Запросы на адреса, которые выдаёт сам API (загрузка файлов, ссылки на отчёты), сюда не
//! входят: у них нет заголовков авторизации Connect API, а адрес — секрет. Для них —
//! [`crate::transfer`] ([`ApiClient::upload_file`], [`ApiClient::download_to_file`]).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode, redirect};
use serde_json::{Map, Value};

use crate::app::{AppCredentials, AppTokens, app_authorization};
use crate::credentials::ApiClientCredentials;
use crate::envelope::{ResultStatus, detect_result, read_result, result_of};
use crate::http::{
    api_base, content_type, describe_client_error, describe_reqwest_error, lossy_prefix,
    read_limited, truncate,
};
use crate::path::{shorten, validate_service_path};
use crate::project::{ProjectCredentials, validate_project_id};
use crate::region::Region;
use crate::scrub::{MASK, scrub_secrets};
use crate::service::ApiService;
use crate::spec::{AccessLevel, AuthMode, OperationDef, ParamLocation, ResultDef};
use crate::token::{AccessToken, ApiClientTokens, TokenError, TokenSource};
use crate::transfer::{
    DownloadedFile, TransferClient, TransferConfig, UploadRequest, UploadResponse,
};
use crate::user_oauth::{OAuthSession, UserTokens};

pub use crate::http::{CONNECT_TIMEOUT, USER_AGENT};
pub use crate::path::{render_path, validate_api_path};
/// HTTP-метод запроса: тот же тип, что и в таблице операций.
pub use crate::spec::HttpMethod;

/// Таймаут запроса по умолчанию (одна попытка, включая чтение тела).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// Лимит тела ответа по умолчанию, байт.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// Максимальный размер тела ошибки (не-2xx), который читается в [`HttpError`].
pub const ERROR_BODY_LIMIT: usize = 16 * 1024;

/// Сколько символов тела ошибки и текста Huawei попадает в текст ошибки.
pub const ERROR_TEXT_LIMIT: usize = 2000;

/// Имя заголовка с идентификатором API-клиента.
pub const CLIENT_ID_HEADER: &str = "client_id";

/// Заголовок с access token пользователя (режим OAuth-клиента).
pub const OAUTH_TOKEN_HEADER: &str = "oauth2Token";

/// Заголовок с командой пользователя (режим OAuth-клиента).
pub const TEAM_ID_HEADER: &str = "teamId";

/// Заголовки (в нижнем регистре), которые запрос задать не может: авторизацию и
/// идентификатор клиента добавляет исполнитель, остальные — служебные и hop-by-hop
/// заголовки HTTP. Кроме них отклоняются имена с префиксами `proxy-` и `sec-`.
pub const FORBIDDEN_REQUEST_HEADERS: &[&str] = &[
    "authorization",
    CLIENT_ID_HEADER,
    // Режим OAuth-клиента: токен и команду задаёт только исполнитель.
    "oauth2token",
    "teamid",
    "host",
    "content-type",
    "content-length",
    "content-encoding",
    "accept-encoding",
    "user-agent",
    "cookie",
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "expect",
    "forwarded",
    "via",
];

/// Заголовки ответа, которые возвращаются в [`ApiResponse::headers`].
pub const KEPT_RESPONSE_HEADERS: &[&str] = &[
    "content-type",
    "content-disposition",
    "etag",
    "retry-after",
    "x-request-id",
    "x-trace-id",
    "x-traceid",
];

/// `Accept` запросов исполнителя.
const ACCEPT_JSON: &str = "application/json, */*;q=0.8";

/// Символы, которые кодируются в ключах и значениях query. Оставляем unreserved-символы
/// RFC 3986; `&`, `=`, `+`, `#`, `%`, `:` и пробел кодируются.
const QUERY_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

impl HttpMethod {
    /// Повтор запроса не меняет результат. `PATCH` и `POST` идемпотентными не считаются:
    /// повтор после 5xx мог бы применить изменение дважды.
    pub fn is_idempotent(self) -> bool {
        matches!(self, Self::Get | Self::Put | Self::Delete)
    }

    pub(crate) fn to_reqwest(self) -> Method {
        match self {
            Self::Get => Method::GET,
            Self::Post => Method::POST,
            Self::Put => Method::PUT,
            Self::Patch => Method::PATCH,
            Self::Delete => Method::DELETE,
        }
    }
}

/// Политика повторов при 429, 5xx и ошибках соединения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Сколько раз повторять запрос после первой попытки. `0` — без повторов. Повтор после
    /// 401 с новым токеном сюда не входит.
    pub max_retries: u32,
    /// Пауза, если в ответе нет `Retry-After` (удваивается с каждой попыткой).
    pub default_delay: Duration,
    /// Максимальная пауза. Если `Retry-After` больше, запрос не повторяется, а ошибка
    /// возвращается сразу с [`HttpError::retry_after`].
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 3,
            default_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(30),
        }
    }
}

/// Настройки клиента. `Debug` безопасен: учётные данные не раскрываются.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Регион профиля: от него зависит хост API.
    pub region: Region,
    /// Учётные данные API-клиента. `None` — учётных данных нет: [`ApiClient::execute`]
    /// возвращает [`ApiError::MissingCredentials`] и ничего не отправляет.
    pub credentials: Option<ApiClientCredentials>,
    /// Подмена корня API для тестов (`wiremock`): только loopback-адрес без пути. В
    /// рабочей конфигурации — `None`.
    pub base_url: Option<String>,
    /// Таймаут одной попытки, включая чтение тела.
    pub timeout: Duration,
    /// Таймаут установки соединения.
    pub connect_timeout: Duration,
    /// Максимальный размер тела ответа в байтах. Больший ответ —
    /// [`ApiError::ResponseTooLarge`].
    pub max_response_bytes: usize,
    pub retry: RetryPolicy,
    /// Передача файлов на адреса из ответов API (загрузка, отчёты).
    pub transfer: TransferConfig,
    /// API-клиенты уровня проекта профиля: ими подписываются операции сервисов проекта
    /// ([`OperationDef::project_param`]). Один клиент на проект.
    pub projects: Vec<ProjectCredentials>,
    /// Имя профиля — только для подсказок в ошибках (`auth project add --profile …`).
    pub profile: Option<String>,
    /// Вход пользователя (режим OAuth-клиента) вместо API-клиента команды. Вместе с
    /// [`Self::credentials`] не задаётся.
    pub oauth: Option<OAuthSession>,
    /// Секреты приложений профиля: ими подписываются операции HMS IAP
    /// ([`OperationDef::service`]). Один секрет на приложение.
    pub apps: Vec<AppCredentials>,
}

impl ClientConfig {
    /// Настройки по умолчанию для региона и учётных данных профиля.
    pub fn new(region: Region, credentials: Option<ApiClientCredentials>) -> Self {
        Self {
            region,
            credentials,
            base_url: None,
            timeout: DEFAULT_TIMEOUT,
            connect_timeout: CONNECT_TIMEOUT,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            retry: RetryPolicy::default(),
            transfer: TransferConfig::default(),
            projects: Vec::new(),
            profile: None,
            apps: Vec::new(),
            oauth: None,
        }
    }

    /// Настройки по умолчанию для региона и входа пользователя.
    pub fn with_oauth(region: Region, session: OAuthSession) -> Self {
        Self {
            oauth: Some(session),
            ..Self::new(region, None)
        }
    }
}

/// Как проверять конверт результата в теле успешного ответа.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResultCheck {
    /// Определить конверт по телу (`ret.code`, `ret.rtnCode`, `error.errorCode`): для
    /// запросов без известной операции.
    #[default]
    Detect,
    /// Конверт операции из таблицы; если его в теле нет — любой известный.
    Envelope(ResultDef),
    /// Не проверять: тело возвращается как есть.
    Skip,
}

/// Запрос к API. `path` — путь от корня хоста с уже подставленными параметрами (см.
/// [`render_path`]); query передаётся отдельно парами, повторяющиеся параметры —
/// повтором пары (`?a=1&a=2`).
#[derive(Debug, Clone)]
pub struct ApiRequest {
    pub method: HttpMethod,
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Дополнительные заголовки (например `appId` у PMS API). Служебные заголовки
    /// ([`FORBIDDEN_REQUEST_HEADERS`]) отклоняются.
    pub headers: Vec<(String, String)>,
    /// JSON-тело, отправляется как `application/json`. Строка с JSON-объектом или массивом
    /// разбирается и отправляется как объект: MCP-клиенты иногда передают тело строкой.
    pub body: Option<Value>,
    /// Можно ли повторить запрос после 429 и 5xx. По умолчанию — по методу
    /// ([`HttpMethod::is_idempotent`]); у операций чтения из таблицы — всегда.
    pub idempotent: bool,
    pub result: ResultCheck,
    /// Имя операции (инструмента) для текстов ошибок.
    pub operation: Option<String>,
    /// Идентификатор проекта, чей API-клиент подписывает запрос; `None` — клиент команды.
    /// У операций с [`OperationDef::project_param`] его задаёт [`Self::select_project`].
    pub project: Option<String>,
    /// Режимы авторизации операции ([`OperationDef::auth_modes`]); пусто — любой. Задаёт
    /// [`Self::select_project`].
    pub auth_modes: &'static [AuthMode],
    /// Сервис, на хост которого уходит запрос. [`Self::new`] — всегда AppGallery Connect
    /// API; сервис IAP задаёт только [`Self::for_operation`].
    pub service: ApiService,
    /// Приложение, чей секрет подписывает запрос IAP (аргумент
    /// [`ParamLocation::Credential`]); у запросов AppGallery Connect API — `None`.
    pub app: Option<String>,
}

impl ApiRequest {
    /// Запрос без query, заголовков и тела; конверт результата определяется по телу.
    pub fn new(method: HttpMethod, path: impl Into<String>) -> Self {
        Self {
            method,
            path: path.into(),
            query: Vec::new(),
            headers: Vec::new(),
            body: None,
            idempotent: method.is_idempotent(),
            result: ResultCheck::Detect,
            operation: None,
            project: None,
            auth_modes: &[],
            service: ApiService::Connect,
            app: None,
        }
    }

    /// Запрос операции из таблицы по аргументам инструмента (JSON-объект):
    /// - параметры пути подставляются в шаблон и кодируются как один сегмент;
    /// - параметры запроса идут в query под именем из справочника, массивы — повтором;
    /// - параметры-заголовки идут заголовками (служебные имена отклоняются);
    /// - аргумент `body` — JSON-тело.
    ///
    /// Неизвестные аргументы, недостающие обязательные и значения неверного вида —
    /// [`ApiError::InvalidRequest`]. Проверку по JSON Schema делает ядро; здесь — только
    /// то, без чего запрос нельзя собрать.
    pub fn for_operation(op: &'static OperationDef, args: &Value) -> Result<Self, ApiError> {
        let empty = Map::new();
        let args = match args {
            Value::Null => &empty,
            Value::Object(map) => map,
            _ => {
                return Err(ApiError::InvalidRequest(
                    "tool arguments must be a JSON object".into(),
                ));
            }
        };
        if let Some(unknown) = args.keys().find(|key| op.param(key).is_none()) {
            return Err(ApiError::InvalidRequest(format!(
                "unknown argument `{}` of `{}`",
                shorten(unknown),
                op.tool_name
            )));
        }
        let present = |name: &str| args.get(name).filter(|value| !value.is_null());
        for param in op.params {
            if param.required && present(param.name).is_none() {
                return Err(match param.location {
                    ParamLocation::Path => ApiError::MissingPathParam(param.name.to_owned()),
                    _ => ApiError::InvalidRequest(format!(
                        "missing required argument `{}`",
                        param.name
                    )),
                });
            }
        }

        // Путь: значение — скаляр; имя в шаблоне — имя из справочника или имя аргумента.
        for param in op.params_in(ParamLocation::Path) {
            if let Some(value) = present(param.name)
                && scalar_text(value).is_none()
            {
                return Err(ApiError::InvalidRequest(format!(
                    "path parameter `{}` must be a string or a number",
                    param.name
                )));
            }
        }
        let path = if op.service.is_iap() {
            // Пути IAP без параметров: генератор не допускает в них `{…}`.
            validate_service_path(op.path)?;
            op.path.to_owned()
        } else {
            render_path(op.path, |name| {
                op.params_in(ParamLocation::Path)
                    .find(|param| param.api_name == name || param.name == name)
                    .and_then(|param| present(param.name))
                    .and_then(scalar_text)
            })?
        };

        let mut request = Self::new(op.method, path);
        request.service = op.service;
        request.idempotent = op.method.is_idempotent() || op.access == AccessLevel::Read;
        request.result = match op.result {
            Some(def) => ResultCheck::Envelope(def),
            None => ResultCheck::Detect,
        };
        request.operation = Some(op.tool_name.to_owned());

        for param in op.params_in(ParamLocation::Query) {
            let Some(value) = present(param.name) else {
                continue;
            };
            if value.is_array() && !param.repeated {
                return Err(ApiError::InvalidRequest(format!(
                    "query parameter `{}` does not accept an array",
                    param.name
                )));
            }
            for item in query_values(param.name, value)? {
                request.query.push((param.api_name.to_owned(), item));
            }
        }
        for param in op.params_in(ParamLocation::Header) {
            let Some(value) = present(param.name) else {
                continue;
            };
            let text = scalar_text(value).ok_or_else(|| {
                ApiError::InvalidRequest(format!(
                    "header parameter `{}` must be a string or a number",
                    param.name
                ))
            })?;
            request.headers.push((param.api_name.to_owned(), text));
        }
        if let Some(param) = op.params_in(ParamLocation::Body).next()
            && let Some(value) = present(param.name)
        {
            if op.body.is_none() {
                return Err(ApiError::InvalidRequest(format!(
                    "`{}` does not take a request body",
                    op.tool_name
                )));
            }
            request.body = Some(value.clone());
        }
        // Заголовки проверяются сразу: ошибка в описании операции видна до запроса.
        request_headers(&request.headers)?;
        request.select_project(op)?;
        request.select_app(op, present)?;
        Ok(request)
    }

    /// Выбирает секрет приложения для операции HMS IAP: значение аргумента
    /// [`ParamLocation::Credential`] (`appId`). У операции AppGallery Connect API ничего не
    /// меняет; у операции IAP без такого параметра — [`ApiError::InvalidRequest`].
    fn select_app<'a>(
        &mut self,
        op: &OperationDef,
        present: impl Fn(&str) -> Option<&'a Value>,
    ) -> Result<(), ApiError> {
        if !op.service.is_iap() {
            return Ok(());
        }
        let Some(param) = op.app_param() else {
            return Err(ApiError::InvalidRequest(format!(
                "`{}` is signed with an app secret, but its app parameter is not described",
                op.tool_name
            )));
        };
        let app_id = present(param.name)
            .and_then(scalar_text)
            .map(|value| value.trim().to_owned())
            .ok_or_else(|| {
                ApiError::InvalidRequest(format!(
                    "`{}` is signed with the app secret of an app: pass the app ID in `{}`",
                    op.tool_name, param.name
                ))
            })?;
        crate::app::validate_app_id(&app_id).map_err(|err| {
            ApiError::InvalidRequest(format!(
                "invalid `{}` of `{}`: {err}",
                param.name, op.tool_name
            ))
        })?;
        self.app = Some(app_id);
        Ok(())
    }

    /// Выбирает API-клиент проекта для операции с [`OperationDef::project_param`]: значение
    /// параметра берётся из уже собранного запроса (сегмент пути по шаблону операции,
    /// параметр query или заголовок; имена query и заголовков — без учёта регистра).
    /// Так выбирают клиента и инструменты операций, и `api_request` к пути такой операции.
    /// Нет значения или значения расходятся — [`ApiError::InvalidRequest`]. Заодно
    /// переносит в запрос режимы авторизации операции ([`Self::auth_modes`]).
    pub fn select_project(&mut self, op: &OperationDef) -> Result<(), ApiError> {
        self.auth_modes = op.auth_modes;
        let Some(name) = op.project_param else {
            return Ok(());
        };
        let Some(param) = op.param(name) else {
            return Err(ApiError::InvalidRequest(format!(
                "`{}` needs a project-level API client, but its project parameter `{name}` is \
                 not described",
                op.tool_name
            )));
        };
        let mut values: Vec<String> = match param.location {
            ParamLocation::Path => path_value(op.path, &self.path, param.api_name)
                .into_iter()
                .collect(),
            ParamLocation::Query => self
                .query
                .iter()
                .filter(|(key, _)| key.eq_ignore_ascii_case(param.api_name))
                .map(|(_, value)| value.clone())
                .collect(),
            ParamLocation::Header => self
                .headers
                .iter()
                .filter(|(key, _)| key.trim().eq_ignore_ascii_case(param.api_name))
                .map(|(_, value)| value.clone())
                .collect(),
            ParamLocation::Body | ParamLocation::Credential => Vec::new(),
        };
        values
            .iter_mut()
            .for_each(|value| *value = value.trim().to_owned());
        values.sort();
        values.dedup();
        let project = match values.as_slice() {
            [value] => value.clone(),
            [] => {
                return Err(ApiError::InvalidRequest(format!(
                    "`{}` is signed with the API client of a project: pass the project ID in \
                     `{name}`",
                    op.tool_name
                )));
            }
            _ => {
                return Err(ApiError::InvalidRequest(format!(
                    "`{name}` is given more than once with different values: the request is \
                     signed with the API client of one project"
                )));
            }
        };
        validate_project_id(&project).map_err(|err| {
            ApiError::InvalidRequest(format!("invalid `{name}` of `{}`: {err}", op.tool_name))
        })?;
        self.project = Some(project);
        Ok(())
    }

    /// Добавляет параметр query (повторный вызов с тем же именем — повтор параметра).
    pub fn with_query(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.query.push((key.into(), value.into()));
        self
    }

    /// Добавляет параметр query из значения аргумента (JSON): массив — повтором имени
    /// (см. [`query_values`]), `null` — ничего.
    pub fn with_query_value(mut self, key: &str, value: &Value) -> Result<Self, ApiError> {
        for item in query_values(key, value)? {
            self.query.push((key.to_owned(), item));
        }
        Ok(self)
    }

    /// Добавляет заголовок. Служебные заголовки отклоняются при выполнении.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Задаёт JSON-тело.
    pub fn with_json(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    /// Разрешает или запрещает повтор после 429 и 5xx.
    pub fn idempotent(mut self, idempotent: bool) -> Self {
        self.idempotent = idempotent;
        self
    }

    /// Задаёт проверку конверта результата.
    pub fn with_result(mut self, result: ResultCheck) -> Self {
        self.result = result;
        self
    }

    /// Задаёт имя операции для текстов ошибок.
    pub fn with_operation(mut self, operation: impl Into<String>) -> Self {
        self.operation = Some(operation.into());
        self
    }
}

/// Значение параметра пути `name` в пути `path` по шаблону `template` (сегмент
/// целиком `{name}`), после percent-декодирования. Число сегментов должно совпадать.
fn path_value(template: &str, path: &str, name: &str) -> Option<String> {
    let parts = |text: &str| -> Vec<String> {
        let trimmed = text.trim_start_matches('/');
        let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
        trimmed.split('/').map(str::to_owned).collect()
    };
    let (template, path) = (parts(template), parts(path));
    if template.len() != path.len() {
        return None;
    }
    let placeholder = format!("{{{name}}}");
    template
        .iter()
        .zip(&path)
        .find(|(segment, _)| **segment == placeholder)
        .map(|(_, value)| {
            percent_encoding::percent_decode_str(value)
                .decode_utf8_lossy()
                .into_owned()
        })
}

/// Текст скалярного значения аргумента: строка как есть, число и логическое — текстом.
fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(flag) => Some(flag.to_string()),
        _ => None,
    }
}

/// Значения параметра query из аргумента инструмента (JSON).
///
/// Строки передаются как есть, числа и логические значения — текстом, массивы — отдельными
/// значениями (повтор имени). `null` — параметра нет. Объекты и вложенные массивы — ошибка.
pub fn query_values(name: &str, value: &Value) -> Result<Vec<String>, ApiError> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                scalar_text(item).ok_or_else(|| {
                    ApiError::InvalidRequest(format!(
                        "query parameter `{name}`: array items must be strings, numbers or booleans"
                    ))
                })
            })
            .collect(),
        other => scalar_text(other).map(|v| vec![v]).ok_or_else(|| {
            ApiError::InvalidRequest(format!(
                "query parameter `{name}` must be a string, number, boolean or array"
            ))
        }),
    }
}

/// Можно ли запросу задать заголовок `name` (см. [`FORBIDDEN_REQUEST_HEADERS`]).
pub fn is_forbidden_request_header(name: &str) -> bool {
    let lower = name.trim().to_ascii_lowercase();
    FORBIDDEN_REQUEST_HEADERS.contains(&lower.as_str())
        || lower.starts_with("proxy-")
        || lower.starts_with("sec-")
}

/// Проверяет дополнительные заголовки запроса и собирает их.
fn request_headers(headers: &[(String, String)]) -> Result<HeaderMap, ApiError> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if is_forbidden_request_header(name) {
            return Err(ApiError::InvalidRequest(format!(
                "header `{}` cannot be set: authorization and service headers are added by \
                 the server",
                shorten(name.trim())
            )));
        }
        let header_name = HeaderName::from_bytes(name.trim().as_bytes()).map_err(|_| {
            ApiError::InvalidRequest(format!("invalid header name `{}`", shorten(name)))
        })?;
        let header_value = HeaderValue::from_str(value).map_err(|_| {
            ApiError::InvalidRequest(format!(
                "invalid value of header `{header_name}`: control characters and non-ASCII \
                 characters are not allowed"
            ))
        })?;
        map.append(header_name, header_value);
    }
    Ok(map)
}

/// Тело ответа.
#[derive(Debug, Clone, PartialEq)]
pub enum ResponseBody {
    Empty,
    Json(Value),
    /// Ответ, который не разобрался как JSON (секреты по виду вырезаны).
    Text(String),
}

impl ResponseBody {
    /// JSON-тело, если ответ — JSON.
    pub fn json(&self) -> Option<&Value> {
        match self {
            Self::Json(value) => Some(value),
            _ => None,
        }
    }
}

/// Успешный ответ API: статус 2xx и нулевой код результата (или конверта нет).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    /// Заголовки ответа из списка [`KEPT_RESPONSE_HEADERS`] (имена в нижнем регистре).
    pub headers: BTreeMap<String, String>,
    pub body: ResponseBody,
    /// Конверт результата, если он есть в теле (код — успех).
    pub result: Option<ResultStatus>,
}

/// Ответ API со статусом не 2xx.
///
/// Если тело — JSON с конвертом результата, код и текст Huawei разобраны в поля; начало
/// тела (обрезанное до [`ERROR_TEXT_LIMIT`]) — в `body`. Токены и секреты из текстов
/// вырезаны.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HttpError {
    pub status: u16,
    /// Код результата Huawei из тела ошибки, если он есть.
    pub code: Option<String>,
    /// Текст Huawei из тела ошибки, если он есть.
    pub message: Option<String>,
    /// Начало тела ответа.
    pub body: String,
    /// Через сколько секунд можно повторить запрос (`Retry-After`), если сервер сообщил.
    pub retry_after: Option<u64>,
    /// Имя операции (инструмента), если запрос — операция из таблицы.
    pub operation: Option<String>,
    /// Сервис, который ответил (AppGallery Connect API или HMS IAP).
    pub service: ApiService,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} returned HTTP {}", self.service.title(), self.status)?;
        if let Some(reason) = StatusCode::from_u16(self.status)
            .ok()
            .and_then(|s| s.canonical_reason())
        {
            write!(f, " {reason}")?;
        }
        if let Some(operation) = &self.operation {
            write!(f, " for `{operation}`")?;
        }
        match (&self.code, &self.message) {
            (Some(code), Some(message)) => write!(f, ": code {code}: {message}")?,
            (Some(code), None) => write!(f, ": code {code}")?,
            (None, Some(message)) => write!(f, ": {message}")?,
            (None, None) if !self.body.trim().is_empty() => write!(f, ": {}", self.body.trim())?,
            (None, None) => {}
        }
        if let Some(secs) = self.retry_after {
            write!(f, " (retry after {secs} s)")?;
        }
        Ok(())
    }
}

/// Ненулевой код результата в конверте ответа со статусом 2xx.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultError {
    /// HTTP-статус ответа (обычно 200).
    pub status: u16,
    /// Код результата Huawei текстом.
    pub code: String,
    /// Текст Huawei (обрезан, без секретов).
    pub message: Option<String>,
    /// Имя операции (инструмента), если запрос — операция из таблицы.
    pub operation: Option<String>,
    /// Конверт, в котором пришёл код.
    pub envelope: ResultDef,
    /// Сервис, который ответил (AppGallery Connect API или HMS IAP).
    pub service: ApiService,
}

impl fmt::Display for ResultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} returned an error", self.service.title())?;
        if let Some(operation) = &self.operation {
            write!(f, " for `{operation}`")?;
        }
        write!(f, ": code {}", self.code)?;
        if let Some(message) = &self.message {
            write!(f, ": {message}")?;
        }
        if self.envelope.field.is_empty() {
            write!(f, " (HTTP {}, {})", self.status, self.envelope.code_field)
        } else {
            write!(
                f,
                " (HTTP {}, {}.{})",
                self.status, self.envelope.field, self.envelope.code_field
            )
        }
    }
}

/// Ошибки запроса. Тексты на английском (их видит агент) и не содержат ни секрета, ни
/// токена, ни адресов загрузки.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("invalid API path: {0}")]
    InvalidPath(String),
    #[error("missing path parameter `{0}`")]
    MissingPathParam(String),
    /// Некорректные аргументы запроса, не связанные с путём (query, заголовки, тело, файл).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Некорректная конфигурация клиента (подмена адреса, таймауты).
    #[error("invalid AppGallery Connect client configuration: {0}")]
    InvalidConfig(String),
    /// У клиента нет учётных данных (см. [`ClientConfig::credentials`]); запрос не
    /// отправлялся.
    #[error("no AppGallery Connect API client credentials are configured for this request")]
    MissingCredentials,
    /// Операции нужен API-клиент проекта, а клиента этого проекта у профиля нет; запрос не
    /// отправлялся.
    #[error("{}", describe_missing_project(.project_id, .profile.as_deref()))]
    MissingProjectCredentials {
        project_id: String,
        profile: Option<String>,
    },
    /// Операцию нельзя вызвать от имени пользователя (режим OAuth-клиента): справочник
    /// описывает её только для других режимов. Запрос не отправлялся.
    #[error(
        "`{operation}` is not available when the server works as a signed-in user (OAuth \
         client): the Huawei reference describes it only for {modes}. Use a profile with an API \
         client for it (`appgallery-connect-mcp auth login --profile <name>`)"
    )]
    UnsupportedAuthMode { operation: String, modes: String },
    /// Операции HMS IAP нужен секрет приложения, а секрета этого приложения у профиля нет;
    /// запрос не отправлялся.
    #[error("{}", describe_missing_app(.app_id, .profile.as_deref()))]
    MissingAppCredentials {
        app_id: String,
        profile: Option<String>,
    },
    /// Не удалось получить access token; запрос не отправлялся.
    #[error("{0}")]
    Token(#[from] TokenError),
    /// API ответил статусом не 2xx.
    #[error("{0}")]
    Http(Box<HttpError>),
    /// API ответил статусом 2xx с ненулевым кодом результата.
    #[error("{0}")]
    Result(Box<ResultError>),
    /// Тело ответа больше лимита; ответ не возвращается.
    #[error(
        "the AppGallery Connect API response is larger than {limit} bytes; narrow the request \
         (filters, a smaller page)"
    )]
    ResponseTooLarge { limit: usize },
    /// Файл по ссылке больше лимита; частично записанный файл удалён.
    #[error("the file to download is larger than {limit} bytes")]
    DownloadTooLarge { limit: u64 },
    /// Адрес загрузки или скачивания ведёт не на хост Huawei из разрешённого списка. В
    /// тексте — только причина и имя хоста, без пути и query.
    #[error("upload or download URL is not allowed: {0}")]
    HostNotAllowed(String),
    /// Сеть, TLS, таймаут.
    #[error("could not reach AppGallery Connect: {0}")]
    Transport(String),
    #[error("local file error: {0}")]
    Io(String),
}

fn describe_missing_project(project_id: &str, profile: Option<&str>) -> String {
    let profile = profile.unwrap_or("<profile>");
    format!(
        "this request is signed with a project-level API client, and no API client of project \
         `{project_id}` is stored for this server: the team-level API client or user sign-in of \
         the server is not accepted by the services of a project. Add one with `appgallery-connect-mcp auth \
         project add --profile {profile} --project-id {project_id}` (create the API client in \
         AppGallery Connect > the project > Project settings > Server SDK, or in Users and \
         permissions > API key > Connect API with this project selected) and restart the \
         server"
    )
}

fn describe_missing_app(app_id: &str, profile: Option<&str>) -> String {
    let profile = profile.unwrap_or("<profile>");
    format!(
        "this request is signed with an app-level access token, and no app secret of app \
         `{app_id}` is stored for this server: HMS In-App Purchases server APIs do not accept \
         the API client of the server. Store the client ID and client secret of the app with \
         `appgallery-connect-mcp auth app-secret set --profile {profile} --app {app_id}` (they \
         are in AppGallery Connect > My apps > the app > App information, or All services > \
         In-App Purchases) and restart the server"
    )
}

impl ApiError {
    /// HTTP-статус ответа API, если ошибка пришла от API.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http(e) => Some(e.status),
            Self::Result(e) => Some(e.status),
            _ => None,
        }
    }

    /// Код результата Huawei, если он есть в ошибке.
    pub fn result_code(&self) -> Option<&str> {
        match self {
            Self::Http(e) => e.code.as_deref(),
            Self::Result(e) => Some(&e.code),
            Self::Token(
                TokenError::Rejected { code, .. } | TokenError::AppRejected { code, .. },
            ) => code.as_deref(),
            _ => None,
        }
    }

    /// Ошибка HTTP, если API ответил статусом не 2xx.
    pub fn http(&self) -> Option<&HttpError> {
        match self {
            Self::Http(e) => Some(e),
            _ => None,
        }
    }
}

/// Способ авторизации: API-клиент (`client_id` и `Authorization`) или пользователь
/// (`oauth2Token` и `teamId`).
#[derive(Clone)]
enum AuthKind {
    ApiClient { client_id: HeaderValue },
    User { team_id: Option<HeaderValue> },
}

/// Авторизация запросов: способ и источник токенов.
#[derive(Clone)]
struct Auth {
    kind: AuthKind,
    tokens: Arc<dyn TokenSource>,
}

impl Auth {
    fn new(
        credentials: &ApiClientCredentials,
        tokens: Arc<dyn TokenSource>,
    ) -> Result<Self, ApiError> {
        let mut client_id =
            HeaderValue::from_str(credentials.client_id().expose()).map_err(|_| {
                ApiError::InvalidConfig("the client ID cannot be sent in an HTTP header".into())
            })?;
        client_id.set_sensitive(true);
        Ok(Self {
            kind: AuthKind::ApiClient { client_id },
            tokens,
        })
    }

    fn user(session: &OAuthSession, tokens: Arc<dyn TokenSource>) -> Result<Self, ApiError> {
        let team_id = match &session.team_id {
            Some(team) => {
                crate::user_oauth::validate_team_id(team)
                    .map_err(|err| ApiError::InvalidConfig(err.to_string()))?;
                Some(HeaderValue::from_str(team).map_err(|_| {
                    ApiError::InvalidConfig("the team ID cannot be sent in an HTTP header".into())
                })?)
            }
            None => None,
        };
        Ok(Self {
            kind: AuthKind::User { team_id },
            tokens,
        })
    }

    /// Заголовки авторизации запроса с токеном `token`.
    fn apply(
        &self,
        builder: reqwest::RequestBuilder,
        token: &AccessToken,
    ) -> Result<reqwest::RequestBuilder, ApiError> {
        Ok(match &self.kind {
            AuthKind::ApiClient { client_id } => builder
                .header(CLIENT_ID_HEADER, client_id.clone())
                .header(header::AUTHORIZATION, authorization(token)?),
            AuthKind::User { team_id } => {
                let builder = builder.header(OAUTH_TOKEN_HEADER, raw_token(token)?);
                match team_id {
                    Some(team) => builder.header(TEAM_ID_HEADER, team.clone()),
                    None => builder,
                }
            }
        })
    }
}

/// Чем подписывается запрос: API-клиентом (команды или проекта) или токеном приложения.
enum Signer<'a> {
    Connect(&'a Auth),
    App(&'a Arc<dyn TokenSource>),
}

impl Signer<'_> {
    fn tokens(&self) -> &Arc<dyn TokenSource> {
        match self {
            Self::Connect(auth) => &auth.tokens,
            Self::App(tokens) => tokens,
        }
    }
}

/// Клиент AppGallery Connect API.
#[derive(Clone)]
pub struct ApiClient {
    config: ClientConfig,
    base: url::Url,
    http: reqwest::Client,
    auth: Option<Auth>,
    /// Авторизация API-клиентов проектов по идентификатору проекта.
    projects: BTreeMap<String, Auth>,
    /// Источники токенов приложений по App ID (операции HMS IAP).
    apps: BTreeMap<String, Arc<dyn TokenSource>>,
    transfer: TransferClient,
}

impl fmt::Debug for ApiClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClient")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ApiClient {
    /// Клиент с токенами API-клиента из [`ClientConfig::credentials`] или входа
    /// пользователя из [`ClientConfig::oauth`].
    pub fn new(config: ClientConfig) -> Result<Self, ApiError> {
        let tokens: Option<Arc<dyn TokenSource>> = match (&config.credentials, &config.oauth) {
            (Some(credentials), None) => Some(Arc::new(ApiClientTokens::with_base_url(
                config.region,
                config.base_url.as_deref(),
                credentials.clone(),
            )?)),
            (None, Some(session)) => Some(Arc::new(UserTokens::new(session)?)),
            _ => None,
        };
        Self::build(config, tokens)
    }

    /// Клиент с заданным источником токенов (тесты: [`crate::token::StaticToken`] или
    /// [`ApiClientTokens`] с управляемыми часами). Способ авторизации и идентификатор
    /// клиента для заголовка `client_id` берутся из [`ClientConfig::credentials`] или
    /// [`ClientConfig::oauth`].
    pub fn with_token_source(
        config: ClientConfig,
        tokens: Arc<dyn TokenSource>,
    ) -> Result<Self, ApiError> {
        Self::build(config, Some(tokens))
    }

    fn build(config: ClientConfig, tokens: Option<Arc<dyn TokenSource>>) -> Result<Self, ApiError> {
        let base =
            api_base(config.region, config.base_url.as_deref()).map_err(ApiError::InvalidConfig)?;
        let auth = match (&config.credentials, &config.oauth, tokens) {
            (Some(_), Some(_), _) => {
                return Err(ApiError::InvalidConfig(
                    "both an API client and a user sign-in are configured".into(),
                ));
            }
            (Some(credentials), None, Some(tokens)) => Some(Auth::new(credentials, tokens)?),
            (None, Some(session), Some(tokens)) => Some(Auth::user(session, tokens)?),
            _ => None,
        };
        // У каждого проекта свой источник токенов на том же хосте региона: токен получается
        // при первом запросе к операции этого проекта.
        let mut projects = BTreeMap::new();
        for project in &config.projects {
            let tokens: Arc<dyn TokenSource> = Arc::new(ApiClientTokens::with_base_url(
                config.region,
                config.base_url.as_deref(),
                project.credentials().clone(),
            )?);
            let auth = Auth::new(project.credentials(), tokens)?;
            if projects
                .insert(project.project_id().to_owned(), auth)
                .is_some()
            {
                return Err(ApiError::InvalidConfig(format!(
                    "project `{}` has two API clients",
                    project.project_id()
                )));
            }
        }
        // У каждого приложения свой источник токенов Huawei OAuth: токен получается при
        // первом запросе IAP этого приложения.
        let mut apps: BTreeMap<String, Arc<dyn TokenSource>> = BTreeMap::new();
        for app in &config.apps {
            let tokens: Arc<dyn TokenSource> = Arc::new(AppTokens::with_base_url(
                config.base_url.as_deref(),
                app.clone(),
            )?);
            if apps.insert(app.app_id().to_owned(), tokens).is_some() {
                return Err(ApiError::InvalidConfig(format!(
                    "app `{}` has two app secrets",
                    app.app_id()
                )));
            }
        }
        // Редиректы не выполняются: токен и тело запроса не должны уйти на другой адрес, а
        // клиент — стать прокси. Ответ 3xx возвращается как `ApiError::Http`.
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout.min(config.timeout))
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|e| ApiError::InvalidConfig(describe_client_error(&e)))?;
        let transfer = TransferClient::new(config.transfer.clone())?;
        Ok(Self {
            config,
            base,
            http,
            auth,
            projects,
            apps,
            transfer,
        })
    }

    /// Приложения, для которых у клиента есть секрет приложения, по порядку.
    pub fn app_ids(&self) -> Vec<&str> {
        self.apps.keys().map(String::as_str).collect()
    }

    /// Проверяет секрет приложения: получает токен уровня приложения (из памяти или
    /// новый), ничего больше не вызывая. Для `auth app-secret set` и `auth status`.
    pub async fn authenticate_app(&self, app_id: &str) -> Result<(), ApiError> {
        let tokens = self
            .apps
            .get(app_id)
            .ok_or_else(|| ApiError::MissingAppCredentials {
                app_id: app_id.to_owned(),
                profile: self.config.profile.clone(),
            })?;
        tokens.token().await?;
        Ok(())
    }

    /// Проекты, для которых у клиента есть API-клиент проекта, по порядку.
    pub fn project_ids(&self) -> Vec<&str> {
        self.projects.keys().map(String::as_str).collect()
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Клиент передачи файлов на адреса из ответов API.
    pub fn transfer(&self) -> &TransferClient {
        &self.transfer
    }

    /// Проверяет учётные данные: получает access token (из памяти или новый), ничего
    /// больше не вызывая. Для `auth login` и `auth status`.
    pub async fn authenticate(&self) -> Result<(), ApiError> {
        let auth = self.auth.as_ref().ok_or(ApiError::MissingCredentials)?;
        auth.tokens.token().await?;
        Ok(())
    }

    /// Подпись запроса: токен приложения у запросов IAP, иначе API-клиент.
    fn signer_for(&self, request: &ApiRequest) -> Result<Signer<'_>, ApiError> {
        if !request.service.is_iap() {
            return self.auth_for(request).map(Signer::Connect);
        }
        if request.service == ApiService::IapKit {
            // Подписи JWT ключом IAP Kit нет: такие операции исключены и сюда не доходят.
            return Err(ApiError::InvalidRequest(
                "the HarmonyOS IAP Kit server API is not supported by this server".into(),
            ));
        }
        if request.project.is_some() {
            return Err(ApiError::InvalidRequest(
                "an HMS IAP request cannot be signed with a project-level API client".into(),
            ));
        }
        let Some(app_id) = &request.app else {
            return Err(ApiError::InvalidRequest(
                "an HMS IAP request needs the app ID of the app whose secret signs it".into(),
            ));
        };
        self.apps
            .get(app_id)
            .map(Signer::App)
            .ok_or_else(|| ApiError::MissingAppCredentials {
                app_id: app_id.clone(),
                profile: self.config.profile.clone(),
            })
    }

    /// Авторизация запроса: клиента проекта ([`ApiRequest::project`]) или команды.
    fn auth_for(&self, request: &ApiRequest) -> Result<&Auth, ApiError> {
        match &request.project {
            Some(project_id) => {
                self.projects
                    .get(project_id)
                    .ok_or_else(|| ApiError::MissingProjectCredentials {
                        project_id: project_id.clone(),
                        profile: self.config.profile.clone(),
                    })
            }
            None => self.auth.as_ref().ok_or(ApiError::MissingCredentials),
        }
    }

    /// Заменяет известные клиенту секреты (текущий и недавние токены, секрет и
    /// идентификатор клиента) и секреты по виду на `***`. Защита в глубину для текстов,
    /// которые уходят агенту: секрет не должен попасть в ответ, даже если API его отразил.
    pub fn scrub(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in self.known_secrets() {
            if !secret.is_empty() && text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), MASK);
            }
        }
        scrub_secrets(&text)
    }

    /// Значения, которые вырезаются точным совпадением.
    fn known_secrets(&self) -> Vec<zeroize::Zeroizing<String>> {
        let mut secrets = Vec::new();
        for auth in self.auth.iter().chain(self.projects.values()) {
            for token in auth.tokens.known_tokens() {
                secrets.push(zeroize::Zeroizing::new(token.expose().to_owned()));
            }
        }
        let projects = self
            .config
            .projects
            .iter()
            .map(ProjectCredentials::credentials);
        for credentials in self.config.credentials.iter().chain(projects) {
            secrets.push(zeroize::Zeroizing::new(
                credentials.client_secret().expose().to_owned(),
            ));
            secrets.push(zeroize::Zeroizing::new(
                credentials.client_id().expose().to_owned(),
            ));
        }
        // Вход пользователя: refresh token и client secret. Client ID OAuth-клиента — это
        // идентификатор приложения (`appId`), он законно встречается в ответах и не
        // вырезается.
        if let Some(session) = &self.config.oauth {
            let credentials = &session.credentials;
            secrets.push(zeroize::Zeroizing::new(
                credentials.expose_refresh_token().to_owned(),
            ));
            if let Some(secret) = credentials.client().client_secret() {
                secrets.push(zeroize::Zeroizing::new(secret.expose().to_owned()));
            }
        }
        // Секреты приложений и их токены — и как есть, и в виде значения заголовка
        // `Basic base64("APPAT:" + token)`. App ID — обычный аргумент инструментов, его не
        // вырезаем.
        for app in &self.config.apps {
            secrets.extend(app.secret_values());
        }
        for tokens in self.apps.values() {
            for token in tokens.known_tokens() {
                secrets.push(zeroize::Zeroizing::new(token.expose().to_owned()));
                let header = app_authorization(&token);
                if let Some(encoded) = header.strip_prefix("Basic ") {
                    secrets.push(zeroize::Zeroizing::new(encoded.to_owned()));
                }
            }
        }
        secrets
    }

    /// Выполняет операцию из таблицы по аргументам инструмента
    /// ([`ApiRequest::for_operation`]).
    pub async fn call(
        &self,
        op: &'static OperationDef,
        args: &Value,
    ) -> Result<ApiResponse, ApiError> {
        self.execute(ApiRequest::for_operation(op, args)?).await
    }

    /// Выполняет запрос. Ответы не-2xx возвращаются как [`ApiError::Http`], ненулевой код
    /// результата — как [`ApiError::Result`].
    pub async fn execute(&self, request: ApiRequest) -> Result<ApiResponse, ApiError> {
        self.execute_with_limit(request, self.config.max_response_bytes)
            .await
    }

    /// Как [`Self::execute`], но тело ответа читается до `max_response_bytes` вместо лимита
    /// из конфигурации (ответ обрабатывается перед возвратом агенту, например фильтром).
    pub async fn execute_with_limit(
        &self,
        request: ApiRequest,
        max_response_bytes: usize,
    ) -> Result<ApiResponse, ApiError> {
        let signer = self.signer_for(&request)?;
        if let Signer::Connect(auth) = &signer
            && matches!(auth.kind, AuthKind::User { .. })
            && !request.auth_modes.is_empty()
            && !request.auth_modes.contains(&AuthMode::OAuthClient)
        {
            let modes: Vec<&str> = request.auth_modes.iter().map(|mode| mode.title()).collect();
            return Err(ApiError::UnsupportedAuthMode {
                operation: request
                    .operation
                    .clone()
                    .unwrap_or_else(|| request.path.clone()),
                modes: modes.join(", "),
            });
        }
        let url = self.build_url(request.service, &request.path, &request.query)?;
        let extra_headers = request_headers(&request.headers)?;
        let body = request.body.clone().map(normalize_json_body);

        tracing::debug!(
            method = request.method.as_str(),
            path = %request.path,
            operation = request.operation.as_deref().unwrap_or(""),
            project_client = request.project.is_some(),
            service = request.service.slug(),
            "AppGallery Connect API request"
        );

        let retry = self.config.retry;
        let mut attempt: u32 = 0;
        let mut token_renewed = false;
        let response = loop {
            // Токен берётся перед каждой попыткой: после долгой паузы на повтор он мог
            // подойти к концу срока, тогда источник получит новый.
            let token = signer.tokens().token().await?;
            let builder = self
                .http
                .request(request.method.to_reqwest(), url.clone())
                .headers(extra_headers.clone())
                .header(header::ACCEPT, ACCEPT_JSON);
            let mut builder = match &signer {
                Signer::Connect(auth) => auth.apply(builder, &token)?,
                Signer::App(_) => builder.header(header::AUTHORIZATION, app_header(&token)?),
            };
            if let Some(body) = &body {
                builder = builder.json(body);
            }
            let response = match builder.send().await {
                Ok(response) => response,
                Err(err) => {
                    let err = err.without_url();
                    // Соединение не установлено — запрос не отправлен, повтор безопасен для
                    // любого метода. Таймаут после отправки — только для идемпотентных.
                    let retryable = err.is_connect() || (request.idempotent && err.is_timeout());
                    if retryable && attempt < retry.max_retries {
                        let delay = backoff(retry.default_delay, attempt).min(retry.max_delay);
                        attempt += 1;
                        tracing::debug!(
                            attempt,
                            delay_ms = delay.as_millis() as u64,
                            "retrying AppGallery Connect API request after a transport error"
                        );
                        tokio::time::sleep(delay).await;
                        continue;
                    }
                    return Err(ApiError::Transport(describe_reqwest_error(&err)));
                }
            };
            let status = response.status();
            if status.is_success() {
                break response;
            }

            // 401: токен истёк раньше срока или отозван. Запрос отклонён до выполнения,
            // поэтому один повтор с новым токеном безопасен для любого метода.
            if status == StatusCode::UNAUTHORIZED && !token_renewed {
                token_renewed = true;
                drop(response);
                signer.tokens().invalidate(&token).await;
                tracing::debug!("AppGallery Connect API returned 401, retrying with a new token");
                continue;
            }

            let retry_after = parse_retry_after(response.headers());
            let mut error = self
                .http_error(status, retry_after, response, request.result)
                .await;
            if request.idempotent && is_retryable_status(status) && attempt < retry.max_retries {
                let delay = retry_after.unwrap_or_else(|| backoff(retry.default_delay, attempt));
                if delay <= retry.max_delay {
                    attempt += 1;
                    tracing::debug!(
                        status = status.as_u16(),
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "retrying AppGallery Connect API request"
                    );
                    tokio::time::sleep(delay).await;
                    continue;
                }
            }
            error.operation = request.operation.clone();
            error.service = request.service;
            return Err(ApiError::Http(Box::new(error)));
        };

        let status = response.status().as_u16();
        let headers = kept_headers(response.headers());
        let body = self.read_body(response, max_response_bytes).await?;
        let result = match (&body, request.result) {
            (ResponseBody::Json(value), ResultCheck::Detect) => detect_result(value),
            (ResponseBody::Json(value), ResultCheck::Envelope(def)) => result_of(value, Some(&def)),
            _ => None,
        };
        if let Some(result) = &result
            && !result.is_success()
        {
            return Err(ApiError::Result(Box::new(ResultError {
                status,
                code: self.redact_text(&result.code),
                message: result.message.as_deref().map(|m| self.redact_text(m)),
                operation: request.operation.clone(),
                envelope: result.envelope,
                service: request.service,
            })));
        }
        Ok(ApiResponse {
            status,
            headers,
            body,
            result,
        })
    }

    /// Загружает локальный файл (или его часть) на адрес из ответа API
    /// ([`TransferClient::upload_file`]). Дополнительно проверяет, что в заголовках
    /// загрузки нет учётных данных Connect API: токен и секрет клиента на внешние адреса
    /// не отправляются.
    pub async fn upload_file(&self, request: UploadRequest) -> Result<UploadResponse, ApiError> {
        let secrets = self.known_secrets();
        let leaks = request.headers.iter().any(|(_, value)| {
            secrets
                .iter()
                .any(|secret| !secret.is_empty() && value.contains(secret.as_str()))
        });
        if leaks {
            return Err(ApiError::InvalidRequest(
                "upload headers must not contain the Connect API access token or client \
                 credentials"
                    .into(),
            ));
        }
        self.transfer.upload_file(request).await
    }

    /// Скачивает файл по ссылке из ответа API в локальный файл
    /// ([`TransferClient::download_to_file`]).
    pub async fn download_to_file(
        &self,
        url: &str,
        destination: &Path,
    ) -> Result<DownloadedFile, ApiError> {
        self.transfer.download_to_file(url, destination).await
    }

    /// Корень сервиса: хост региона профиля, а при подмене корня (тесты) — тот же
    /// loopback-адрес для всех сервисов.
    fn service_base(&self, service: ApiService) -> Result<url::Url, ApiError> {
        if !service.is_iap() || self.config.base_url.is_some() {
            return Ok(self.base.clone());
        }
        url::Url::parse(&service.base_url(self.config.region)).map_err(|err| {
            ApiError::InvalidConfig(format!("address of {}: {err}", service.title()))
        })
    }

    /// Собирает URL: корень сервиса + проверенный относительный путь + query.
    fn build_url(
        &self,
        service: ApiService,
        path: &str,
        query: &[(String, String)],
    ) -> Result<url::Url, ApiError> {
        if service.is_iap() {
            validate_service_path(path)?;
        } else {
            validate_api_path(path)?;
        }
        for (key, _) in query {
            if key.is_empty() {
                return Err(ApiError::InvalidRequest(
                    "empty query parameter name".into(),
                ));
            }
        }

        let base = self.service_base(service)?;
        let mut url = base.clone();
        // Путь уже состоит только из pchar и `%XX`, поэтому set_path его не меняет.
        url.set_path(path);
        url.set_fragment(None);
        if query.is_empty() {
            url.set_query(None);
        } else {
            let encoded: Vec<String> = query
                .iter()
                .map(|(key, value)| {
                    format!(
                        "{}={}",
                        utf8_percent_encode(key, QUERY_COMPONENT),
                        utf8_percent_encode(value, QUERY_COMPONENT)
                    )
                })
                .collect();
            url.set_query(Some(&encoded.join("&")));
        }

        // Повторная проверка: итоговый адрес обязан остаться в origin API, а путь — тем,
        // что проверяли.
        if url.origin() != base.origin() || url.path() != path {
            return Err(ApiError::InvalidPath(format!(
                "the path leads outside the AppGallery Connect API host: `{}`",
                shorten(path)
            )));
        }
        Ok(url)
    }

    async fn http_error(
        &self,
        status: StatusCode,
        retry_after: Option<Duration>,
        mut response: reqwest::Response,
        result: ResultCheck,
    ) -> HttpError {
        let mut error = HttpError {
            status: status.as_u16(),
            retry_after: retry_after.map(|d| d.as_secs()),
            ..HttpError::default()
        };
        if status.is_redirection() {
            // Location не возвращаем: там может быть подписанная ссылка.
            error.body = "a redirect was not followed".to_string();
            return error;
        }
        match read_limited(&mut response, ERROR_BODY_LIMIT).await {
            Ok((bytes, truncated)) => {
                if !truncated
                    && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                    && let Some(result) = match result {
                        // Сначала конверт операции: у IAP он в корне тела, его нет среди
                        // известных конвертов AppGallery Connect API.
                        ResultCheck::Envelope(def) => {
                            read_result(&value, &def).or_else(|| detect_result(&value))
                        }
                        _ => detect_result(&value),
                    }
                {
                    error.code = Some(self.redact_text(&result.code));
                    error.message = result.message.as_deref().map(|m| self.redact_text(m));
                }
                let mut text = lossy_prefix(&bytes).trim().to_string();
                if truncated {
                    text.push_str(" …(truncated)");
                }
                if text.is_empty() {
                    text = status
                        .canonical_reason()
                        .unwrap_or("empty response")
                        .to_string();
                }
                error.body = self.redact_text(&text);
            }
            Err(e) => {
                error.body = format!(
                    "could not read the error body: {}",
                    describe_reqwest_error(&e.without_url())
                );
            }
        }
        error
    }

    async fn read_body(
        &self,
        mut response: reqwest::Response,
        limit: usize,
    ) -> Result<ResponseBody, ApiError> {
        if response
            .content_length()
            .is_some_and(|len| len > limit as u64)
        {
            return Err(ApiError::ResponseTooLarge { limit });
        }
        let content_type = content_type(&response);
        let (bytes, truncated) = read_limited(&mut response, limit)
            .await
            .map_err(|e| ApiError::Transport(describe_reqwest_error(&e.without_url())))?;
        if truncated {
            return Err(ApiError::ResponseTooLarge { limit });
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(ResponseBody::Empty);
        }
        if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
            return Ok(ResponseBody::Json(value));
        }
        // Не JSON: двоичное содержимое агенту не отдаём, текст — без секретов.
        if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
            return Ok(ResponseBody::Text(format!(
                "binary response ({} bytes, content type {})",
                bytes.len(),
                content_type.as_deref().unwrap_or("unknown")
            )));
        }
        Ok(ResponseBody::Text(self.scrub(&lossy_prefix(&bytes))))
    }

    /// Текст ошибки: без секретов и не длиннее [`ERROR_TEXT_LIMIT`].
    fn redact_text(&self, text: &str) -> String {
        truncate(&self.scrub(text), ERROR_TEXT_LIMIT)
    }
}

/// Статусы, после которых идемпотентный запрос повторяется.
fn is_retryable_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 500 | 502 | 503 | 504)
}

/// Заголовок `Authorization: Bearer <token>`, помеченный как sensitive.
fn authorization(token: &AccessToken) -> Result<HeaderValue, ApiError> {
    let mut value = HeaderValue::from_str(&format!("Bearer {}", token.expose())).map_err(|_| {
        ApiError::Token(TokenError::InvalidResponse(
            "the access token is not a valid header value".into(),
        ))
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// Заголовок `oauth2Token: <token>` (без `Bearer`), помеченный как sensitive.
fn raw_token(token: &AccessToken) -> Result<HeaderValue, ApiError> {
    let mut value = HeaderValue::from_str(token.expose()).map_err(|_| {
        ApiError::Token(TokenError::InvalidResponse(
            "the access token is not a valid header value".into(),
        ))
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// Заголовок `Authorization: Basic base64("APPAT:" + token)` запроса IAP, помеченный как
/// sensitive.
fn app_header(token: &AccessToken) -> Result<HeaderValue, ApiError> {
    let text = app_authorization(token);
    let mut value = HeaderValue::from_str(&text).map_err(|_| {
        ApiError::Token(TokenError::InvalidResponse(
            "the app-level access token is not a valid header value".into(),
        ))
    })?;
    value.set_sensitive(true);
    Ok(value)
}

/// Пауза без `Retry-After`: `default_delay * 2^attempt`.
fn backoff(default_delay: Duration, attempt: u32) -> Duration {
    default_delay.saturating_mul(1u32 << attempt.min(16))
}

/// `Retry-After` в секундах. Формат HTTP-date не поддерживается: тогда пауза по умолчанию.
fn parse_retry_after(headers: &HeaderMap) -> Option<Duration> {
    headers
        .get(header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
}

fn kept_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    KEPT_RESPONSE_HEADERS
        .iter()
        .filter_map(|name| {
            let value = headers.get(*name)?.to_str().ok()?;
            Some(((*name).to_owned(), scrub_secrets(value.trim())))
        })
        .collect()
}

/// Строка с JSON-объектом или массивом отправляется как объект, а не как JSON-строка с
/// экранированным JSON внутри.
fn normalize_json_body(value: Value) -> Value {
    if let Value::String(text) = &value {
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            match serde_json::from_str::<Value>(text) {
                Ok(parsed) if parsed.is_object() || parsed.is_array() => return parsed,
                _ => {}
            }
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn forbidden_headers_cover_auth_and_hop_by_hop() {
        for name in [
            "Authorization",
            "authorization",
            "client_id",
            "CLIENT_ID",
            " Client_Id ",
            "Host",
            "Content-Type",
            "Content-Length",
            "Cookie",
            "Connection",
            "Transfer-Encoding",
            "TE",
            "Upgrade",
            "Proxy-Authorization",
            "Proxy-Connection",
            "Sec-Fetch-Site",
            "oauth2Token",
            "teamId",
            "TEAMID",
            "User-Agent",
        ] {
            assert!(is_forbidden_request_header(name), "{name}");
        }
        for name in ["appId", "userID", "X-Custom", "Accept-Language"] {
            assert!(!is_forbidden_request_header(name), "{name}");
        }
    }

    #[test]
    fn header_names_and_values_are_validated() {
        assert!(request_headers(&[("appId".into(), "100".into())]).is_ok());
        for (name, value) in [
            ("bad name", "x"),
            ("", "x"),
            ("x\r\nInjected", "x"),
            ("appId", "1\r\nAuthorization: Bearer x"),
            ("appId", "x\ny"),
        ] {
            let err = request_headers(&[(name.into(), value.into())]).unwrap_err();
            assert!(matches!(err, ApiError::InvalidRequest(_)), "{err:?}");
        }
    }

    #[test]
    fn query_values_accept_scalars_and_arrays() {
        assert_eq!(query_values("a", &json!("x")).unwrap(), ["x"]);
        assert_eq!(query_values("a", &json!(5)).unwrap(), ["5"]);
        assert_eq!(query_values("a", &json!(true)).unwrap(), ["true"]);
        assert_eq!(query_values("a", &json!(["x", 2])).unwrap(), ["x", "2"]);
        assert!(query_values("a", &json!(null)).unwrap().is_empty());
        assert!(query_values("a", &json!({"x": 1})).is_err());
        assert!(query_values("a", &json!([["x"]])).is_err());
    }

    #[test]
    fn json_string_body_is_sent_as_object() {
        assert_eq!(normalize_json_body(json!("{\"a\": 1}")), json!({"a": 1}));
        assert_eq!(normalize_json_body(json!("plain")), json!("plain"));
        assert_eq!(normalize_json_body(json!({"a": 1})), json!({"a": 1}));
    }

    #[test]
    fn iap_requests_go_to_the_service_host_of_the_region() {
        let client = ApiClient::new(ClientConfig::new(Region::Ru, None)).unwrap();
        let url = client
            .build_url(
                ApiService::IapSubscriptions,
                "/sub/applications/v2/purchases/get",
                &[],
            )
            .unwrap();
        assert_eq!(
            url.as_str(),
            "https://subscr-drru.iap.cloud.huawei.ru/sub/applications/v2/purchases/get"
        );
        let url = client
            .build_url(ApiService::Connect, "/api/publish/v2/app-info", &[])
            .unwrap();
        assert_eq!(url.host_str(), Some("connect-api-drru.cloud.huawei.com"));
        assert!(
            client
                .build_url(ApiService::IapOrders, "/api/publish/v2/app-info", &[])
                .is_err()
        );
        assert!(
            client
                .build_url(
                    ApiService::Connect,
                    "/applications/v2/purchases/confirm",
                    &[]
                )
                .is_err()
        );
    }

    #[test]
    fn backoff_doubles() {
        let base = Duration::from_millis(100);
        assert_eq!(backoff(base, 0), base);
        assert_eq!(backoff(base, 1), base * 2);
        assert_eq!(backoff(base, 3), base * 8);
    }

    #[test]
    fn errors_describe_status_code_and_operation() {
        let http = HttpError {
            status: 403,
            code: Some("204144647".into()),
            message: Some("no permission".into()),
            operation: Some("get_app_info".into()),
            retry_after: Some(5),
            ..HttpError::default()
        };
        assert_eq!(
            http.to_string(),
            "AppGallery Connect API returned HTTP 403 Forbidden for `get_app_info`: code \
             204144647: no permission (retry after 5 s)"
        );
        let result = ResultError {
            status: 200,
            code: "204144660".into(),
            message: Some("app not found".into()),
            operation: None,
            envelope: crate::envelope::RESULT_RET_CODE,
            service: ApiService::Connect,
        };
        assert_eq!(
            result.to_string(),
            "AppGallery Connect API returned an error: code 204144660: app not found (HTTP \
             200, ret.code)"
        );
        let err = ApiError::Result(Box::new(result));
        assert_eq!(err.status(), Some(200));
        assert_eq!(err.result_code(), Some("204144660"));
    }
}
