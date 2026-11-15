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

/// Текст ответа AppGallery Connect со статусом не 2xx: статус, код Huawei и подсказка.
fn describe_http_error(err: &HttpError) -> String {
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

/// Ненулевой код результата при HTTP 200: запрос не выполнен, хотя статус успешный.
fn describe_result_error(err: &ResultError) -> String {
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
    }
}
