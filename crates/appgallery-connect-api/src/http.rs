//! Общие помощники HTTP для исполнителя, получения токена и передачи файлов.

use std::time::Duration;

use crate::region::Region;

/// `User-Agent` всех запросов.
pub const USER_AGENT: &str = concat!("appgallery-connect-mcp/", env!("CARGO_PKG_VERSION"));

/// Таймаут установки соединения по умолчанию.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Корень API: хост региона или подмена для тестов.
///
/// Подмена принимается только для loopback-адресов (`http://127.0.0.1:<порт>` у
/// `wiremock`): произвольный адрес сделал бы исполнитель HTTP-прокси, а учётные данные
/// ушли бы на чужой хост.
pub(crate) fn api_base(region: Region, base_override: Option<&str>) -> Result<url::Url, String> {
    let Some(text) = base_override else {
        return url::Url::parse(&region.base_url())
            .map_err(|err| format!("address of region `{}`: {err}", region.slug()));
    };
    let base = url::Url::parse(text).map_err(|err| format!("base URL override: {err}"))?;
    if !matches!(base.scheme(), "http" | "https") || !is_loopback(&base) {
        return Err(
            "the base URL override is accepted only for loopback addresses (tests); the API \
             host is chosen by the region of the profile"
                .into(),
        );
    }
    if !base.username().is_empty() || base.password().is_some() {
        return Err("the base URL must not contain a user name or password".into());
    }
    if base.query().is_some() || base.fragment().is_some() {
        return Err("the base URL must not contain a query or fragment".into());
    }
    if !matches!(base.path(), "" | "/") {
        return Err("the base URL must not contain a path".into());
    }
    Ok(base)
}

pub(crate) fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// Читает тело не больше `limit` байт. Возвращает прочитанное и признак обрезки.
pub(crate) async fn read_limited(
    response: &mut reqwest::Response,
    limit: usize,
) -> Result<(Vec<u8>, bool), reqwest::Error> {
    let mut buf = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let room = limit.saturating_sub(buf.len());
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            // Остаток не дочитываем: соединение закроется при drop ответа.
            return Ok((buf, true));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok((buf, false))
}

/// UTF-8 текст из байтов; незавершённый символ в конце (после обрезки) отбрасывается.
pub(crate) fn lossy_prefix(bytes: &[u8]) -> String {
    let end = match std::str::from_utf8(bytes) {
        Ok(_) => bytes.len(),
        Err(e) if e.error_len().is_none() => e.valid_up_to(),
        Err(_) => bytes.len(),
    };
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Обрезает текст до `max` символов, добавляя `…`.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((index, _)) => format!("{}…", &text[..index]),
        None => text.to_owned(),
    }
}

/// `Content-Type` ответа в нижнем регистре.
pub(crate) fn content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_ascii_lowercase())
}

/// Описание ошибки создания HTTPS-клиента. Без корневых сертификатов системы (чистые
/// образы Debian slim) rustls-native-certs не создаёт клиент: подсказываем, что поставить.
pub(crate) fn describe_client_error(err: &reqwest::Error) -> String {
    let mut text = format!(
        "cannot create the HTTPS client: {}",
        describe_reqwest_error(err)
    );
    if text.contains("CA certificates") {
        text.push_str(
            ". Install the system root certificates (the ca-certificates package) and try again",
        );
    }
    text
}

/// Безопасное описание ошибки reqwest: без URL (там может быть query, а у адресов
/// загрузки — подпись) и с причинами.
pub(crate) fn describe_reqwest_error(err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "could not connect"
    } else if err.is_redirect() {
        "redirect error"
    } else if err.is_body() || err.is_decode() {
        "could not read the response body"
    } else if err.is_builder() {
        "invalid request"
    } else {
        "request failed"
    };
    let mut text = kind.to_string();
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    crate::scrub::scrub_secrets(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_is_region_host_or_loopback_override() {
        assert_eq!(
            api_base(Region::Ru, None).unwrap().as_str(),
            "https://connect-api-drru.cloud.huawei.com/"
        );
        assert!(api_base(Region::Cn, Some("http://127.0.0.1:8080")).is_ok());
        assert!(api_base(Region::Cn, Some("http://localhost:1/")).is_ok());
        assert!(api_base(Region::Cn, Some("http://[::1]:9")).is_ok());
        for bad in [
            "https://evil.example",
            "https://connect-api.cloud.huawei.com.evil.example",
            "http://connect-api.cloud.huawei.com",
            "ftp://127.0.0.1",
            "http://user:pw@127.0.0.1:1",
            "http://127.0.0.1:1/prefix",
            "http://127.0.0.1:1/?x=1",
            "not a url",
        ] {
            assert!(api_base(Region::Cn, Some(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn truncates_by_characters() {
        assert_eq!(truncate("абвгд", 3), "абв…");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(lossy_prefix(&"аб".as_bytes()[..3]), "а");
    }
}
