//! Проверка и сборка относительных путей AppGallery Connect API.
//!
//! Это граница безопасности: всё, что уходит в [`crate::executor::ApiClient::execute`],
//! проходит через [`validate_api_path`], поэтому запрос не может уйти на другой хост,
//! выйти за пределы `/api/` через `..` (в том числе в percent-кодированном виде) или
//! попасть на метод получения токена. Хост задаёт исполнитель по региону профиля.
//!
//! Пути операций — от корня хоста, например `/api/publish/v2/app-info`.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};

use crate::executor::ApiError;

/// Префикс всех путей AppGallery Connect API.
pub const API_PREFIX: &str = "/api/";

/// Раздел методов получения токена: исполнитель обращается к нему сам, запросам он
/// недоступен (иначе `api_request` мог бы выпускать токены).
pub const TOKEN_PATH_PREFIX: &str = "/api/oauth2";

/// Путь метода получения токена API-клиента.
pub const TOKEN_PATH: &str = "/api/oauth2/v1/token";

/// Символы, которые кодируются в значении параметра пути.
///
/// Оставляем только unreserved-символы RFC 3986, поэтому `/`, `?`, `#`, `%`, `:` и прочие
/// разделители внутри значения становятся `%XX` и не могут изменить структуру пути.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Сколько раз раскодировать сегмент при проверке. Защищает от двойного кодирования
/// (`%252e%252e`) на случай, если что-то между нами и API раскодирует путь повторно.
const MAX_DECODE_ROUNDS: usize = 3;

/// Проверяет относительный путь AppGallery Connect API, например
/// `/api/publish/v2/app-info`.
///
/// Требования:
/// - начинается с `/api/` (поэтому не может быть полным URL, схемой или хостом);
/// - содержит только символы `pchar` из RFC 3986 и `/`: `?`, `#`, `\`, пробелы,
///   управляющие и не-ASCII символы отклоняются (query передаётся отдельно);
/// - `%` всегда образует корректную тройку `%XX`;
/// - нет пустых сегментов (`//`) кроме завершающего `/`;
/// - ни один сегмент ни в исходном, ни в раскодированном виде не равен `.` или `..`,
///   а раскодированный сегмент не содержит `.`/`..` между `/` или `\` и управляющих
///   символов;
/// - путь не ведёт в раздел получения токена ([`TOKEN_PATH_PREFIX`]), в том числе в
///   другом регистре и в percent-кодированном виде.
pub fn validate_api_path(path: &str) -> Result<(), ApiError> {
    let invalid = |reason: &str| {
        Err(ApiError::InvalidPath(format!(
            "{reason}: `{}`",
            shorten(path)
        )))
    };

    if !path.starts_with('/') {
        return invalid(
            "the path must be relative to the API host and start with `/api/`, for example \
             `/api/publish/v2/app-info`; full URLs are not accepted",
        );
    }
    if path.starts_with("//") {
        return invalid("the path must not start with `//`");
    }
    if let Some(c) = path.chars().find(|c| !is_allowed_char(*c)) {
        return Err(ApiError::InvalidPath(format!(
            "character {c:?} is not allowed in the path `{}`; pass query parameters \
             separately and encode other characters as %XX",
            shorten(path)
        )));
    }
    if !has_valid_percent_triplets(path) {
        return invalid("invalid percent-encoding in the path");
    }
    if !path.starts_with(API_PREFIX) {
        return invalid("AppGallery Connect API paths start with `/api/`");
    }

    let segments: Vec<&str> = path[1..].split('/').collect();
    if segments.get(1).is_none_or(|second| second.is_empty()) {
        return invalid("the path must name an API resource after `/api/`");
    }
    let last = segments.len().saturating_sub(1);
    for (index, segment) in segments.iter().enumerate() {
        if segment.is_empty() {
            // Пустой сегмент допустим только в конце (`/api/x/`).
            if index == last {
                continue;
            }
            return invalid("empty path segment");
        }
        if !segment_is_safe(segment) {
            return invalid("`.` and `..` segments are not allowed, encoded or not");
        }
    }
    // Раздел получения токена: сравниваем раскодированный второй сегмент без учёта
    // регистра, чтобы `/api/OAuth2/…` и `/api/oauth%32/…` тоже не прошли.
    let token_segment = TOKEN_PATH_PREFIX.rsplit('/').next().unwrap_or_default();
    let second = fully_decoded(segments.get(1).copied().unwrap_or_default());
    if second.eq_ignore_ascii_case(token_segment) {
        return invalid(
            "the token endpoint cannot be called: the server obtains access tokens itself",
        );
    }
    Ok(())
}

/// Проверяет путь операции сервиса HMS IAP ([`crate::service::ApiService`]): те же
/// требования к символам и сегментам, что у [`validate_api_path`], но без префикса
/// `/api/` (пути IAP — `/applications/…` и `/sub/applications/…`). Префикс `/api/` здесь
/// отклоняется: такие пути — у AppGallery Connect API.
pub fn validate_service_path(path: &str) -> Result<(), ApiError> {
    if !path.starts_with('/') || path.starts_with(API_PREFIX) || path.starts_with("//") {
        return Err(ApiError::InvalidPath(format!(
            "a path of an HMS IAP operation starts with `/` and not with `/api/`: `{}`",
            shorten(path)
        )));
    }
    // Остальные проверки — как у пути Connect API под префиксом `/api`.
    validate_api_path(&format!("/api{path}"))
}

/// Подставляет параметры в шаблон пути операции и проверяет результат через
/// [`validate_api_path`].
///
/// `{name}` — значение кодируется как один сегмент пути: `a/b` превращается в `a%2Fb`,
/// поэтому значение не может добавить сегменты или выйти из пути. Отсутствующий или пустой
/// параметр даёт [`ApiError::MissingPathParam`]. Значения `.` и `..` отклоняются проверкой
/// итогового пути.
pub fn render_path<F>(template: &str, mut lookup: F) -> Result<String, ApiError>
where
    F: FnMut(&str) -> Option<String>,
{
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(open) = rest.find(['{', '}']) {
        if rest.as_bytes()[open] == b'}' {
            return Err(ApiError::InvalidPath(format!(
                "unmatched `}}` in the path template `{}`",
                shorten(template)
            )));
        }
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            return Err(ApiError::InvalidPath(format!(
                "unclosed `{{` in the path template `{}`",
                shorten(template)
            )));
        };
        let name = &after[..close];
        let valid_name = !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-');
        if !valid_name {
            return Err(ApiError::InvalidPath(format!(
                "invalid parameter in the path template `{}`",
                shorten(template)
            )));
        }
        let value = lookup(name)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::MissingPathParam(name.to_string()))?;
        out.extend(utf8_percent_encode(&value, SEGMENT));
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    validate_api_path(&out)?;
    Ok(out)
}

/// `pchar` из RFC 3986 плюс разделитель `/`; `%` проверяется отдельно.
fn is_allowed_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | ':'
                | '@'
                | '/'
                | '%'
        )
}

fn has_valid_percent_triplets(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let ok = bytes
                .get(i + 1..i + 3)
                .is_some_and(|hex| hex.iter().all(u8::is_ascii_hexdigit));
            if !ok {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    true
}

/// Сегмент после нескольких раундов percent-декодирования.
fn fully_decoded(segment: &str) -> String {
    let mut current = segment.to_string();
    for _ in 0..=MAX_DECODE_ROUNDS {
        if !current.contains('%') {
            break;
        }
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            break;
        }
        current = decoded;
    }
    current
}

/// Проверяет сегмент и его раскодированные варианты.
///
/// Закодированный `/` внутри значения (`a%2Fb`) допустим — так `render_path` передаёт
/// значения со слешем. Но если сервер раскодирует `%2F` до маршрутизации, `..%2F..`
/// превратится в обход каталога, поэтому раскодированный сегмент дополнительно делим
/// по `/` и `\` и ищем `.`/`..`.
fn segment_is_safe(segment: &str) -> bool {
    let mut current = segment.to_string();
    for _ in 0..=MAX_DECODE_ROUNDS {
        if current
            .split(['/', '\\'])
            .any(|part| part == "." || part == "..")
        {
            return false;
        }
        if current.chars().any(char::is_control) {
            return false;
        }
        if !current.contains('%') {
            return true;
        }
        let decoded = percent_decode_str(&current)
            .decode_utf8_lossy()
            .into_owned();
        if decoded == current {
            return true;
        }
        current = decoded;
    }
    // Слишком глубокое вложенное кодирование — подозрительно, отклоняем.
    false
}

/// Укорачивает путь для текста ошибки, чтобы не раздувать ответ MCP.
pub(crate) fn shorten(path: &str) -> String {
    const MAX: usize = 200;
    if path.len() <= MAX {
        return path.escape_debug().to_string();
    }
    let mut end = MAX;
    while !path.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", path[..end].escape_debug())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn accepts_valid_paths() {
        for path in [
            "/api/publish/v2/app-info",
            "/api/publish/v2/upload-url/for-obs",
            "/api/report/distribution-operation-quality/v1/appDownloadExport/100000001",
            "/api/pms/product-price-service/v1/manage/product",
            "/api/publish/v2/app-info/",
            "/api/x/b..c/.hidden/d.",
            "/api/x/a%2Fb",
            "/api/oauth2x/v1/thing",
        ] {
            assert!(validate_api_path(path).is_ok(), "{path} must pass");
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "",
            "/",
            "//",
            "/api",
            "/api/",
            "api/publish/v2/app-info",
            "/publish/v2/app-info",
            "/API/publish/v2/app-info",
            "/apix/publish",
            "https://evil.example/api/publish",
            "https://connect-api.cloud.huawei.com/api/publish/v2/app-info",
            "http://evil.example",
            "//evil.example/api/publish",
            "///evil",
            "/api//publish",
            "/api/publish/../admin",
            "/api/../admin",
            "/../api/publish",
            "/api/./x",
            "/api/publish/..",
            "/api/publish/%2e%2e/admin",
            "/api/publish/%2E%2E/admin",
            "/api/publish/.%2e/admin",
            "/api/publish/%2e/admin",
            "/api/publish/..%2fadmin",
            "/api/publish/x%2F..%2Fadmin",
            "/api/publish/..%5cadmin",
            "/api/publish/%252e%252e/admin",
            "/api/publish/%25252e%25252e/admin",
            "/api/publish?appId=1",
            "/api/publish#frag",
            "/api/publish\\..\\admin",
            "/api\\publish",
            "/api/publish/a b",
            "/api/publish/\n",
            "/api/publish/\t",
            "/api/publish/%00",
            "/api/publish/%0a",
            "/api/publish/%zz",
            "/api/publish/%2",
            "/api/publish/{id}",
            "/api/publish/кириллица",
            "/api/publish/x\u{0}",
            "/api/oauth2/v1/token",
            "/api/oauth2",
            "/api/oauth2/",
            "/api/OAuth2/v1/token",
            "/api/oauth%32/v1/token",
            "/api/%6fauth2/v1/token",
            "/api/%256fauth2/v1/token",
        ] {
            assert!(
                matches!(validate_api_path(path), Err(ApiError::InvalidPath(_))),
                "{path:?} must be rejected"
            );
        }
    }

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn render_substitutes_and_encodes() {
        let p = params(&[
            ("appId", "100000001"),
            ("lang", "a/b"),
            ("name", "K?#% x:y"),
        ]);
        let rendered = render_path("/api/publish/v2/{appId}/langs/{lang}/{name}", |n| {
            p.get(n).cloned()
        })
        .unwrap();
        assert_eq!(
            rendered,
            "/api/publish/v2/100000001/langs/a%2Fb/K%3F%23%25%20x%3Ay"
        );
        let p = params(&[("id", "файл")]);
        assert_eq!(
            render_path("/api/x/{id}", |n| p.get(n).cloned()).unwrap(),
            "/api/x/%D1%84%D0%B0%D0%B9%D0%BB"
        );
    }

    #[test]
    fn render_reports_missing_and_empty_params() {
        let p = params(&[("id", "1"), ("empty", "")]);
        let lookup = |n: &str| p.get(n).cloned();
        assert!(matches!(
            render_path("/api/x/{id}/{missing}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "missing"
        ));
        assert!(matches!(
            render_path("/api/x/{id}/y/{empty}", lookup),
            Err(ApiError::MissingPathParam(name)) if name == "empty"
        ));
    }

    #[test]
    fn render_rejects_traversal_values_and_bad_templates() {
        for value in ["..", ".", "../admin", "..\\x"] {
            let result = render_path("/api/x/{id}/y", |_| Some(value.to_string()));
            assert!(
                matches!(result, Err(ApiError::InvalidPath(_))),
                "value {value:?} must be rejected"
            );
        }
        for template in [
            "/api/x/{id",
            "/api/x/id}",
            "/api/x/{}",
            "/api/x/{{x}}",
            "/api/x/{a b}",
            "/api/x/{+name}",
            "https://evil/{x}",
            "/{x}",
        ] {
            assert!(
                matches!(
                    render_path(template, |_| Some("v".into())),
                    Err(ApiError::InvalidPath(_))
                ),
                "{template} must be rejected"
            );
        }
        // Значение параметра не может увести запрос в раздел токенов.
        assert!(render_path("/api/{x}/v1/token", |_| Some("oauth2".into())).is_err());
    }

    #[test]
    fn service_paths_have_no_api_prefix() {
        for path in [
            "/applications/purchases/tokens/verify",
            "/sub/applications/v2/purchases/get",
        ] {
            validate_service_path(path).unwrap();
        }
        for path in [
            "/api/publish/v2/app-info",
            "applications/x",
            "//evil.example/x",
            "/applications/../api/x",
            "/applications/%2e%2e/x",
            "/applications/x?y=1",
            "https://evil.example/x",
        ] {
            assert!(validate_service_path(path).is_err(), "{path}");
        }
    }
}
