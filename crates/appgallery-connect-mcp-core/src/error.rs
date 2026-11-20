//! Ошибки вызова инструмента.
//!
//! Любая ошибка вызова возвращается MCP-клиенту как `CallToolResult` с `isError: true`
//! и текстом для агента на английском: агент должен понять, что исправить в аргументах
//! или что попросить у пользователя. У всех инструментов сервера (операций и составных)
//! одна и та же ошибка — [`ToolError`].
//!
//! Секрет API-клиента и access token в тексты ошибок не попадают: [`ApiError`] их не
//! содержит (тексты Huawei исполнитель уже пропустил через вырезание секретов), аргументы
//! инструментов учётных данных не принимают, а сервер дополнительно пропускает итоговый
//! текст через [`appgallery_connect_api::ApiClient::scrub`].

use appgallery_connect_api::TokenError;
use appgallery_connect_api::executor::{ApiError, HttpError, ResultError};

/// Ошибка вызова инструмента: текст для агента (английский), без секретов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub message: String,
}

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// Текст ошибки для агента.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<ApiError> for ToolError {
    /// Текст исполнителя и подсказка для агента ([`describe_api_error`]).
    fn from(err: ApiError) -> Self {
        Self::new(describe_api_error(&err))
    }
}

/// Текст ошибки исполнителя запросов для агента: текст [`ApiError`] (он на английском)
/// и подсказка, что делать дальше.
pub fn describe_api_error(err: &ApiError) -> String {
    match err {
        ApiError::Http(http) => describe_http_error(http),
        ApiError::Result(result) => describe_result_error(result),
        ApiError::MissingPathParam(name) => format!("missing path argument `{name}`"),
        ApiError::InvalidPath(detail) => format!(
            "an identifier argument does not form a valid API path ({detail}): empty values, \
             values like `.` or `..` and empty segments are rejected"
        ),
        ApiError::InvalidConfig(detail) => {
            // Подробности — для лога: они относятся к настройке сервера.
            tracing::warn!(%detail, "некорректная конфигурация клиента AppGallery Connect API");
            "the server is misconfigured (invalid API address or timeouts); ask the user to \
             check the server log"
                .to_owned()
        }
        ApiError::MissingCredentials => "no AppGallery Connect API client is configured for \
             this server; ask the user to set one up as described in the server instructions"
            .to_owned(),
        // Текст исполнителя уже называет проект и команду `auth project add`.
        ApiError::MissingProjectCredentials { .. } => format!(
            "{err}. The request was not sent. Ask the user to run the command in their \
             terminal; never ask for the client ID or secret in the conversation"
        ),
        ApiError::Token(detail @ TokenError::SignIn(_)) => format!(
            "the saved HUAWEI ID sign-in of this server was rejected, so the request was not \
             sent: {detail} Ask the user to run the command in their terminal; never ask for \
             tokens or the client secret in the conversation"
        ),
        ApiError::UnsupportedAuthMode { .. } => format!("{err}. The request was not sent"),
        // Текст исполнителя уже называет приложение и команду `auth app-secret set`.
        ApiError::MissingAppCredentials { .. } => format!(
            "{err}. The request was not sent. Ask the user to run the command in their \
             terminal; never ask for the app secret in the conversation"
        ),
        ApiError::Token(detail @ TokenError::AppRejected { app_id, .. }) => format!(
            "could not get an app-level access token for app `{app_id}`, so the request was \
             not sent: {detail} Ask the user to store the app secret again with \
             `appgallery-connect-mcp auth app-secret set --app {app_id}`; never ask for the app \
             secret in the conversation"
        ),
        ApiError::Token(detail @ TokenError::Rejected { .. }) => format!(
            "could not get an access token for the API client of this server, so the request \
             was not sent: {detail} Ask the user to check the profile and to sign in again as \
             described in the server instructions; never ask for the client secret in the \
             conversation"
        ),
        ApiError::Token(detail) => format!(
            "could not get an access token for the API client of this server, so the request \
             was not sent: {detail}. Retry later; if it keeps failing, ask the user to check \
             the profile as described in the server instructions"
        ),
        ApiError::ResponseTooLarge { limit } => format!(
            "the AppGallery Connect API response is larger than {limit} bytes and was not \
             returned. Narrow the request: a smaller page, stricter filters or a shorter time \
             range"
        ),
        ApiError::Transport(detail) => format!(
            "could not reach AppGallery Connect (network error or timeout); retry later. \
             Details: {detail}"
        ),
        other => other.to_string(),
    }
}

/// Как [`describe_api_error`], с учётом режима сервера: в HTTP-режиме (`serve --http`)
/// API-клиента проекта, вход пользователя и секреты приложений настраивает оператор сервера
/// на своём хосте, а не пользователь в своём терминале, поэтому подсказка называет
/// оператора.
pub fn describe_api_error_for(err: &ApiError, http: bool) -> String {
    match err {
        ApiError::MissingProjectCredentials { project_id, .. } if http => format!(
            "this request is signed with a project-level API client, and the server has no API \
             client of project `{project_id}`: the team-level API client or user sign-in of the \
             server is not accepted by the services of a project. The request was not sent. This server runs \
             remotely over HTTP: ask the user to have the server operator add the API client \
             of this project to the server profile (`appgallery-connect-mcp auth project add \
             --project-id {project_id}` on the server host; a server started with \
             --secret-file has no project-level API clients) and restart the server; never ask \
             for the client ID or secret in the conversation"
        ),
        // Вход пользователя на HTTP-сервере настраивает оператор на своём хосте.
        ApiError::Token(detail @ TokenError::SignIn(_)) if http => format!(
            "the HUAWEI ID sign-in of this server was rejected, so the request was not sent: \
             {detail} This server runs remotely over HTTP: ask the user to have the server \
             operator sign in again on the server host; never ask for tokens or the client \
             secret in the conversation"
        ),
        ApiError::UnsupportedAuthMode { operation, modes } if http => format!(
            "`{operation}` is not available when the server works as a signed-in user (OAuth \
             client): the Huawei reference describes it only for {modes}. The request was not \
             sent. This server runs remotely over HTTP: ask the user for a server with an API \
             client from the server operator"
        ),
        // Секреты приложений на HTTP-сервере тоже хранит оператор.
        ApiError::MissingAppCredentials { app_id, .. } if http => format!(
            "no app secret of app `{app_id}` is stored on this server, so the HMS In-App \
             Purchases request was not sent: these APIs are signed with an app-level access \
             token, not with the API client of the server. Ask the server operator to store \
             the app secret of this app; never ask for the app secret in the conversation"
        ),
        ApiError::Token(detail @ TokenError::AppRejected { app_id, .. }) if http => format!(
            "could not get an app-level access token for app `{app_id}`, so the request was \
             not sent: {detail} Ask the server operator to check the app secret stored on the \
             server; never ask for the app secret in the conversation"
        ),
        _ => describe_api_error(err),
    }
}

/// Текст ответа AppGallery Connect со статусом не 2xx: статус, код Huawei и подсказка.
fn describe_http_error(err: &HttpError) -> String {
    if err.service.is_iap() {
        return describe_iap_http_error(err);
    }
    let hint = match err.status {
        300..=399 => {
            " The resource is served from another address, and redirects are not followed."
        }
        400 | 409 | 412 | 422 => {
            " The request was rejected: check the arguments and the request body against the \
             tool schema and the API reference linked in the tool description."
        }
        401 => {
            " The access token of the API client was rejected: the API client may be deleted \
             or its secret reset. Ask the user to check the profile and to sign in again as \
             described in the server instructions; never ask for the client secret in the \
             conversation."
        }
        403 => {
            " The API client is not allowed to do this: ask the user to check the roles of the \
             API client in AppGallery Connect (Users and permissions > API key > Connect API), \
             that the app belongs to this account, and that the profile region matches the \
             account."
        }
        404 => {
            " Not found: check the app ID and other IDs, and that the profile region matches \
             the account of the app."
        }
        429 => " Rate limited: the API quota is used up for now; retry later.",
        500..=599 => " AppGallery Connect server error: retry later.",
        _ => "",
    };
    format!("{err}.{hint}")
}

/// Ответ HMS IAP со статусом не 2xx: подсказки про секрет приложения и сервис IAP.
fn describe_iap_http_error(err: &HttpError) -> String {
    let hint = match err.status {
        400 | 409 | 422 => {
            " The request was rejected: check the purchase token, the product or subscription \
             ID and the request body against the tool schema and the API reference."
        }
        401 => {
            " The app-level access token was rejected even after a new one was obtained: the \
             app secret may be reset or belong to another app. Ask the user to store it again \
             with `appgallery-connect-mcp auth app-secret set`; never ask for the app secret \
             in the conversation."
        }
        403 => {
            " The app is not allowed to call this HMS IAP API: check that In-App Purchases is \
             enabled for the app (AppGallery Connect > the app > In-App Purchases) and that \
             the profile region matches the site of the app."
        }
        404 => " Not found: check the IDs and that the profile region matches the app.",
        429 => " Rate limited: retry later.",
        500..=599 => " HMS IAP server error: retry later.",
        _ => "",
    };
    format!("{err}.{hint}")
}

/// Ненулевой код результата при HTTP 200: запрос не выполнен, хотя статус успешный.
fn describe_result_error(err: &ResultError) -> String {
    if err.service.is_iap() {
        // Коды из раздела «Result Codes» справочника IAP.
        let hint = match err.code.as_str() {
            "5" => "In-App Purchases is not enabled for the app: enable it in AppGallery Connect.",
            "8" => {
                "The user does not own the product: check the purchase token and the product ID."
            }
            "9" => "The product has already been consumed or confirmed: do not repeat the call.",
            "11" => "The account of the user is abnormal (for example, deregistered).",
            "12" => {
                "The order does not exist: only the latest purchase token of a product can be \
                 verified, older tokens are historical orders."
            }
            _ => {
                "Check the purchase token and the IDs against the tool schema and the API \
                  reference linked in the tool description."
            }
        };
        return format!(
            "{err}. The request did not succeed even though the HTTP status is {status}: HMS \
             IAP reports errors with a non-zero `responseCode` in the response body. {hint}",
            status = err.status,
        );
    }
    format!(
        "{err}. The request did not succeed even though the HTTP status is {status}: \
         AppGallery Connect reports errors with a non-zero result code in the response body. \
         Check the arguments (app ID, language, IDs) against the tool schema and the API \
         reference linked in the tool description; if the message says that permission is \
         missing, ask the user to check the roles of the API client.",
        status = err.status,
    )
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::envelope::RESULT_RET_CODE;

    use super::*;

    #[test]
    fn http_errors_have_status_code_and_hint() {
        let err = ApiError::Http(Box::new(HttpError {
            status: 403,
            code: Some("204144647".into()),
            message: Some("no permission".into()),
            operation: Some("get_app_info".into()),
            ..HttpError::default()
        }));
        let text = describe_api_error(&err);
        assert!(text.contains("HTTP 403"), "{text}");
        assert!(text.contains("code 204144647: no permission"), "{text}");
        assert!(text.contains("Users and permissions"), "{text}");

        let err = ApiError::Http(Box::new(HttpError {
            status: 401,
            ..HttpError::default()
        }));
        assert!(describe_api_error(&err).contains("never ask for the client secret"));
    }

    #[test]
    fn result_errors_explain_http_200() {
        let err = ApiError::Result(Box::new(ResultError {
            status: 200,
            code: "204144660".into(),
            message: Some("app not found".into()),
            operation: Some("get_app_info".into()),
            envelope: RESULT_RET_CODE,
            service: appgallery_connect_api::ApiService::Connect,
        }));
        let text = describe_api_error(&err);
        assert!(text.contains("code 204144660: app not found"), "{text}");
        assert!(
            text.contains("even though the HTTP status is 200"),
            "{text}"
        );
    }

    #[test]
    fn other_errors() {
        assert_eq!(
            describe_api_error(&ApiError::MissingPathParam("appId".into())),
            "missing path argument `appId`"
        );
        assert!(describe_api_error(&ApiError::InvalidPath("x".into())).contains("valid API path"));
        let text = describe_api_error(&ApiError::ResponseTooLarge { limit: 1024 });
        assert!(text.contains("1024 bytes"), "{text}");
        assert!(
            describe_api_error(&ApiError::Transport("timeout".into()))
                .contains("could not reach AppGallery Connect")
        );
        assert!(describe_api_error(&ApiError::MissingCredentials).contains("server instructions"));
        let text = describe_api_error(&ApiError::Token(TokenError::Rejected {
            status: 200,
            code: Some("1".into()),
            message: "bad".into(),
        }));
        assert!(text.contains("never ask for the client secret"), "{text}");
        let err: ToolError = ApiError::InvalidRequest("bad".into()).into();
        assert_eq!(err.message(), "invalid request: bad");
        let text = describe_api_error(&ApiError::MissingProjectCredentials {
            project_id: "100".into(),
            profile: Some("work".into()),
        });
        assert!(
            text.contains("auth project add --profile work --project-id 100"),
            "{text}"
        );
        assert!(text.contains("was not sent"), "{text}");
        // В HTTP-режиме клиента проекта добавляет оператор сервера.
        let missing = ApiError::MissingProjectCredentials {
            project_id: "100".into(),
            profile: Some("work".into()),
        };
        let text = describe_api_error_for(&missing, true);
        assert!(text.contains("server operator"), "{text}");
        assert!(text.contains("project `100`"), "{text}");
        assert!(!text.contains("--profile work"), "{text}");
        assert_eq!(
            describe_api_error_for(&missing, false),
            describe_api_error(&missing)
        );
        // Вход пользователя: в HTTP-режиме входит заново оператор сервера.
        let sign_in = ApiError::Token(TokenError::SignIn(
            "HUAWEI ID rejected the refresh token; run `auth login --oauth`.".into(),
        ));
        assert!(describe_api_error_for(&sign_in, false).contains("run the command"));
        let text = describe_api_error_for(&sign_in, true);
        assert!(text.contains("server operator sign in again"), "{text}");
        let mode = ApiError::UnsupportedAuthMode {
            operation: "comments.review.list".into(),
            modes: "API client".into(),
        };
        assert!(describe_api_error_for(&mode, false).contains("auth login --profile"));
        let text = describe_api_error_for(&mode, true);
        assert!(text.contains("server operator"), "{text}");
        assert!(!text.contains("auth login"), "{text}");
    }

    #[test]
    fn http_mode_sends_project_clients_to_the_server_operator() {
        let err = ApiError::MissingProjectCredentials {
            project_id: "100".into(),
            profile: Some("work".into()),
        };
        let text = describe_api_error_for(&err, true);
        assert!(text.contains("server operator"), "{text}");
        assert!(text.contains("--secret-file"), "{text}");
        assert!(text.contains("auth project add --project-id 100"), "{text}");
        assert!(!text.contains("their terminal"), "{text}");
        // Остальные ошибки от режима не зависят.
        let other = ApiError::InvalidRequest("bad".into());
        assert_eq!(
            describe_api_error_for(&other, true),
            describe_api_error(&other)
        );
    }

    #[test]
    fn iap_errors_name_the_app_secret() {
        use appgallery_connect_api::ApiService;
        use appgallery_connect_api::envelope::RESULT_RESPONSE_CODE;
        let text = describe_api_error(&ApiError::MissingAppCredentials {
            app_id: "100000001".into(),
            profile: Some("work".into()),
        });
        assert!(
            text.contains("auth app-secret set --profile work --app 100000001"),
            "{text}"
        );
        assert!(text.contains("never ask for the app secret"), "{text}");
        let text = describe_api_error(&ApiError::Token(TokenError::AppRejected {
            app_id: "100000001".into(),
            status: 400,
            code: Some("1101/20152".into()),
            message: "invalid client secret".into(),
        }));
        assert!(text.contains("app-level access token"), "{text}");
        assert!(
            text.contains("auth app-secret set --app 100000001"),
            "{text}"
        );
        let err = ApiError::Result(Box::new(ResultError {
            status: 200,
            code: "12".into(),
            message: Some("order not exist".into()),
            operation: Some("verify_product_purchase".into()),
            envelope: RESULT_RESPONSE_CODE,
            service: ApiService::IapOrders,
        }));
        let text = describe_api_error(&err);
        assert!(
            text.contains("HMS IAP Order service returned an error"),
            "{text}"
        );
        assert!(text.contains("(HTTP 200, responseCode)"), "{text}");
        assert!(text.contains("latest purchase token"), "{text}");
        let err = ApiError::Http(Box::new(HttpError {
            status: 401,
            service: ApiService::IapSubscriptions,
            ..HttpError::default()
        }));
        let text = describe_api_error(&err);
        assert!(
            text.contains("HMS IAP Subscription service returned HTTP 401"),
            "{text}"
        );
        assert!(text.contains("auth app-secret set"), "{text}");
    }
}
