//! Сопоставление конкретного пути запроса `api_request` с шаблонами операций спецификации.
//!
//! Уровень доступа произвольного запроса определяется по операции, которой он
//! соответствует (в том числе по исключённым операциям). Сопоставление намеренно широкое,
//! чтобы запрос нельзя было провести по более низкому уровню, записав путь иначе:
//!
//! - сегменты пути сравниваются после percent-декодирования (`app%2Dinfo` — это
//!   `app-info`);
//! - литералы сравниваются без учёта регистра (`/api/publish/v2/APP-SUBMIT`);
//! - параметр шаблона (`{appId}`, в том числе внутри сегмента) совпадает с любым непустым
//!   текстом;
//! - завершающий `/` не учитывается;
//! - версия API сравнивается точно: `/api/publish/v2/…` и `/api/publish/v3/…` — разные пути.
//!
//! Если запросу соответствует несколько операций, берутся самые точные (с наибольшей
//! литеральной частью шаблона) и из них наибольший уровень доступа. Совпадений нет —
//! уровень по методу: `GET` — чтение, `DELETE` — удаление, остальные — запись.
//!
//! Операции с `callable = false` закрыты для `api_request` при любом методе
//! ([`blocked_operation`]): шаги загрузки Upload Management API выдают одноразовые адреса и
//! коды загрузки, получение токена — учётные данные. Если в каталоге такой операции (путь
//! без последнего сегмента) нет ни одной вызываемой операции, закрыт весь каталог: так
//! соседние и старые пути того же назначения (`/api/publish/v2/upload-url` старой версии
//! Upload Management API) тоже не вызываются.

use appgallery_connect_api::spec::{
    AccessLevel, ExcludedOperationDef, HttpMethod, OperationDef, excluded_operations, operations,
};

/// Операция спецификации, которой соответствует запрос.
#[derive(Debug, Clone, Copy)]
pub enum RouteMatch {
    Operation(&'static OperationDef),
    Excluded(&'static ExcludedOperationDef),
}

impl RouteMatch {
    pub fn access(&self) -> AccessLevel {
        match self {
            Self::Operation(op) => op.access,
            Self::Excluded(op) => op.access,
        }
    }

    pub fn path(&self) -> &'static str {
        match self {
            Self::Operation(op) => op.path,
            Self::Excluded(op) => op.path,
        }
    }

    /// Длина литеральной части шаблона (без параметров): чем больше, тем точнее
    /// совпадение.
    fn literal_len(&self) -> usize {
        let mut len = 0;
        let mut depth = 0usize;
        for c in self.path().chars() {
            match c {
                '{' => depth += 1,
                '}' => depth = depth.saturating_sub(1),
                _ if depth == 0 => len += 1,
                _ => {}
            }
        }
        len
    }
}

/// Уровень доступа запроса и самая точная операция, которой он соответствует.
#[derive(Debug, Clone, Copy)]
pub struct Route {
    pub access: AccessLevel,
    /// Самое точное совпадение (сначала операции с инструментом); `None` — путь неизвестен.
    pub matched: Option<RouteMatch>,
}

/// Все операции (и исключённые), которым соответствует запрос `method path`, от самых
/// точных. `path` — относительный путь без query.
pub fn match_route(method: HttpMethod, path: &str) -> Vec<RouteMatch> {
    let segments = split(path);
    let mut found = Vec::new();
    for op in operations() {
        if op.method == method && matches(op.path, &segments) {
            found.push(RouteMatch::Operation(op));
        }
    }
    for op in excluded_operations() {
        if op.method == method && matches(op.path, &segments) {
            found.push(RouteMatch::Excluded(op));
        }
    }
    found.sort_by_key(|m| std::cmp::Reverse(m.literal_len()));
    found
}

/// Уровень доступа запроса (см. описание модуля).
pub fn route(method: HttpMethod, path: &str) -> Route {
    let found = match_route(method, path);
    let best = found.first().map_or(0, RouteMatch::literal_len);
    let precise: Vec<RouteMatch> = found
        .into_iter()
        .filter(|m| m.literal_len() == best)
        .collect();
    let access = precise
        .iter()
        .map(RouteMatch::access)
        .max()
        .unwrap_or_else(|| access_by_method(method));
    let matched = precise
        .iter()
        .find(|m| matches!(m, RouteMatch::Operation(_)))
        .or(precise.first())
        .copied();
    Route { access, matched }
}

/// Уровень доступа по методу, когда путь не совпал ни с одной операцией.
pub fn access_by_method(method: HttpMethod) -> AccessLevel {
    match method {
        HttpMethod::Get => AccessLevel::Read,
        HttpMethod::Delete => AccessLevel::Destructive,
        HttpMethod::Post | HttpMethod::Put | HttpMethod::Patch => AccessLevel::Write,
    }
}

/// Операция с `callable = false`, к которой относится путь при любом методе: путь совпадает
/// с её шаблоном или лежит в её закрытом каталоге (см. описание модуля).
pub fn blocked_operation(path: &str) -> Option<&'static ExcludedOperationDef> {
    let segments = split(path);
    excluded_operations()
        .iter()
        .filter(|op| !op.callable)
        .find(|op| {
            if matches(op.path, &segments) {
                return true;
            }
            closed_directory(op.path).is_some_and(|dir| starts_with(&dir, &segments))
        })
}

/// Каталог операции `template` (шаблон без последнего сегмента), если он закрыт целиком:
/// в нём нет вызываемых операций, и это не корень группы API (не короче `/api/x/y`).
fn closed_directory(template: &'static str) -> Option<Vec<&'static str>> {
    let mut parts = template_parts(template);
    parts.pop()?;
    if parts.len() < 3 {
        return None;
    }
    let callable = operations().iter().map(|op| op.path).chain(
        excluded_operations()
            .iter()
            .filter(|op| op.callable)
            .map(|op| op.path),
    );
    for path in callable {
        let other = template_parts(path);
        let inside = other.len() >= parts.len()
            && parts
                .iter()
                .zip(&other)
                .all(|(a, b)| a.eq_ignore_ascii_case(b));
        if inside {
            return None;
        }
    }
    Some(parts)
}

fn template_parts(template: &str) -> Vec<&str> {
    let trimmed = template.trim_start_matches('/');
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    trimmed.split('/').collect()
}

/// Сегменты пути после percent-декодирования, в нижнем регистре, без пустого последнего.
pub(crate) fn split(path: &str) -> Vec<String> {
    let trimmed = path.trim_start_matches('/');
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);
    trimmed
        .split('/')
        .map(|segment| percent_decode(segment).to_lowercase())
        .collect()
}

/// Раскрывает `%XX`; неполные тройки остаются как есть, байты не из UTF-8 заменяются.
/// Путь к этому моменту уже проверен исполнителем (`validate_api_path`).
pub(crate) fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = bytes.get(i + 1..i + 3)
            && let Ok(hex) = std::str::from_utf8(hex)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn matches(template: &str, segments: &[String]) -> bool {
    let parts = template_parts(template);
    parts.len() == segments.len()
        && parts
            .iter()
            .zip(segments)
            .all(|(part, segment)| match_part(part, segment))
}

/// Начинаются ли сегменты пути с литеральных частей каталога.
fn starts_with(dir: &[&str], segments: &[String]) -> bool {
    segments.len() >= dir.len()
        && dir
            .iter()
            .zip(segments)
            .all(|(part, segment)| match_part(part, segment))
}

/// Один сегмент шаблона: литерал или `префикс{параметр}суффикс` (параметр непустой).
fn match_part(part: &str, segment: &str) -> bool {
    let (Some(open), Some(close)) = (part.find('{'), part.rfind('}')) else {
        return part.eq_ignore_ascii_case(segment);
    };
    let prefix = &part[..open];
    let suffix = part.get(close + 1..).unwrap_or_default();
    segment.len() > prefix.len() + suffix.len()
        && segment
            .get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
        && segment
            .get(segment.len() - suffix.len()..)
            .is_some_and(|s| s.eq_ignore_ascii_case(suffix))
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::spec::find;

    use super::*;

    fn tool(route: &Route) -> Option<&'static str> {
        match route.matched {
            Some(RouteMatch::Operation(op)) => Some(op.tool_name),
            _ => None,
        }
    }

    #[test]
    fn matches_operations_with_path_parameters() {
        let found = route(HttpMethod::Get, "/api/reviews/v1/manage/dev/reviews");
        assert_eq!(found.access, AccessLevel::Read);
        assert_eq!(tool(&found), Some("list_reviews"));

        let found = route(HttpMethod::Get, "/api/reviews/v1/manage/dev/reviews/r-1");
        assert_eq!(tool(&found), Some("get_review"));

        let reply = route(HttpMethod::Post, "/api/reviews/v1/manage/dev/reviews");
        assert_eq!(tool(&reply), Some("reply_review"));
        assert_eq!(reply.access, AccessLevel::Destructive);

        let report = route(
            HttpMethod::Get,
            "/api/report/distribution-operation-quality/v1/IAPExport/100000001",
        );
        assert_eq!(tool(&report), Some("get_iap_report_url"));

        // Параметр пути не бывает пустым.
        assert!(
            route(HttpMethod::Get, "/api/project-service/v1/projects/")
                .matched
                .is_some_and(|m| m.path() == "/api/project-service/v1/projects")
        );
    }

    #[test]
    fn spelling_does_not_lower_the_access_level() {
        for path in [
            "/api/publish/v2/app-submit",
            "/api/publish/v2/APP-SUBMIT",
            "/api/publish/v2/app-submit/",
            "/api/publish/v2/app%2Dsubmit",
            "/api/%70ublish/v2/app-submit",
        ] {
            let found = route(HttpMethod::Post, path);
            assert_eq!(found.access, AccessLevel::Destructive, "{path}");
            assert_eq!(tool(&found), Some("submit_app"), "{path}");
        }
        // POST, который только читает, — чтение; PUT того же каталога — удаление.
        assert_eq!(
            route(
                HttpMethod::Post,
                "/api/pms/product-price-service/v1/manage/product/list"
            )
            .access,
            AccessLevel::Read
        );
        assert_eq!(
            route(
                HttpMethod::Put,
                "/api/pms/product-price-service/v1/manage/product"
            )
            .access,
            AccessLevel::Destructive
        );
        // Удаление POST-запросом — по таблице, а не по методу.
        assert_eq!(
            route(HttpMethod::Post, "/api/publish/v2/cert/delete").access,
            AccessLevel::Destructive
        );
    }

    #[test]
    fn unknown_paths_use_the_method() {
        let path = "/api/publish/v2/something-new";
        for (method, access) in [
            (HttpMethod::Get, AccessLevel::Read),
            (HttpMethod::Post, AccessLevel::Write),
            (HttpMethod::Put, AccessLevel::Write),
            (HttpMethod::Patch, AccessLevel::Write),
            (HttpMethod::Delete, AccessLevel::Destructive),
        ] {
            let found = route(method, path);
            assert_eq!(found.access, access, "{method:?}");
            assert!(found.matched.is_none());
        }
        // Известный путь с другим методом — тоже по методу.
        let found = route(HttpMethod::Delete, "/api/publish/v2/app-info");
        assert!(found.matched.is_none());
        assert_eq!(found.access, AccessLevel::Destructive);
        // Версия сравнивается точно.
        assert!(
            route(HttpMethod::Get, "/api/publish/v3/app-info")
                .matched
                .is_none()
        );
    }

    #[test]
    fn every_operation_matches_itself_and_is_not_blocked() {
        for op in operations() {
            let concrete = op.path.replace('{', "x").replace('}', "");
            let found = route(op.method, &concrete);
            assert!(found.access >= op.access, "{}", op.tool_name);
            assert!(found.matched.is_some(), "{} {concrete}", op.tool_name);
            assert!(blocked_operation(&concrete).is_none(), "{}", op.tool_name);
        }
        for op in excluded_operations().iter().filter(|op| op.callable) {
            assert!(blocked_operation(op.path).is_none(), "{}", op.operation_id);
        }
        assert!(find("list_reviews").is_some());
    }

    #[test]
    fn non_callable_operations_are_blocked_for_any_method() {
        for path in [
            "/api/oauth2/v1/token",
            "/api/publish/v2/upload-url/for-obs",
            "/API/publish/v2/Upload-URL/for-obs/",
            "/api/publish/v2/upload/multipart/init",
            "/api/publish/v2/upload/multipart/parts",
            "/api/publish/v2/upload/multipart/compose",
            // Соседние и старые пути закрытых каталогов.
            "/api/publish/v2/upload-url",
            "/api/publish/v2/upload/multipart/abort",
            "/api/oauth2/v1/other",
        ] {
            let op = blocked_operation(path).unwrap_or_else(|| panic!("{path}"));
            assert!(!op.callable, "{path}");
        }
        for path in [
            "/api/publish/v2/app-info",
            "/api/publish/v2/upload",
            "/api/publish/v2/package-list",
        ] {
            assert!(blocked_operation(path).is_none(), "{path}");
        }
        // Каталог с вызываемыми операциями целиком не закрывается.
        assert!(closed_directory("/api/publish/v2/app-submit").is_none());
    }

    #[test]
    fn decodes_percent_triplets() {
        assert_eq!(percent_decode("a%2Db%2fc"), "a-b/c");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(percent_decode("%D0%BF"), "п");
    }
}
