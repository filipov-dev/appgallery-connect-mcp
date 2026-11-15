//! Вырезание секретов из текстов, которые исполнитель отдаёт наружу (ошибки API и метода
//! получения токена, текстовые ответы).
//!
//! Это защита в глубину: известные исполнителю значения (access token, секрет и
//! идентификатор клиента) вырезаются точным совпадением
//! ([`crate::executor::ApiClient::scrub`]), а здесь — секреты по виду, даже если они
//! исполнителю не известны:
//! - значение после `Bearer `;
//! - JWT (`eyJ….eyJ….…`): так выглядит access token AppGallery Connect;
//! - значения полей с секретами в JSON, формах и строке запроса: `client_secret`,
//!   `access_token`, `authCode`, `appSecret`, `Authorization` и т. п. ([`SECRET_KEYS`]), а
//!   также секретные поля ответов из таблицы операций (секрет приложения `secret`,
//!   временные ссылки `fileURL`, `certDownloadUrl`, `provisionDownloadUrl`:
//!   [`crate::spec::secret_field_names`]). Литералы JSON `null`, `true` и `false` после
//!   имени поля не трогаются: иначе JSON ответа перестал бы разбираться;
//! - PEM-блоки приватных ключей, в том числе внутри JSON (с `\n` вместо переводов строк);
//! - подписанные адреса загрузки и скачивания (в query есть подпись, [`SIGNED_URL_MARKERS`]):
//!   адрес сам даёт право читать или записывать файл, он заменяется целиком.
//!
//! Полное вырезание секретов для ответов MCP (адреса загрузки, ссылки на отчёты, поля
//! ответов) — в ядре MCP-сервера.

/// Чем заменяется секрет.
pub const MASK: &str = "***";

/// Имена полей (без учёта регистра), значения которых вырезаются.
pub const SECRET_KEYS: &[&str] = &[
    "client_secret",
    "clientSecret",
    "access_token",
    "accessToken",
    "refresh_token",
    "oauth2Token",
    "authCode",
    "appSecret",
    "app_secret",
    "Authorization",
    "x-amz-security-token",
    "x-obs-security-token",
];

/// Параметры query (без учёта регистра), по которым адрес считается подписанным.
pub const SIGNED_URL_MARKERS: &[&str] = &[
    "x-amz-signature=",
    "x-amz-credential=",
    "x-obs-signature=",
    "signature=",
    "accesskeyid=",
    "awsaccesskeyid=",
    "authcode=",
    "token=",
    "sign=",
];

/// Заменяет секреты по виду на [`MASK`].
pub fn scrub_secrets(text: &str) -> String {
    let text = scrub_pem_blocks(text);
    let text = scrub_signed_urls(&text);
    let mut text = text;
    for key in SECRET_KEYS.iter().chain(crate::spec::secret_field_names()) {
        text = scrub_key_value(&text, key);
    }
    let text = scrub_after_prefix(&text, "Bearer ", 8);
    let text = scrub_after_prefix(&text, "bearer ", 8);
    scrub_jwts(&text)
}

/// Символы токенов и JWT (base64, base64url, точки).
fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | '+' | '/' | '=')
}

/// Символ адреса в тексте: не пробел, не кавычка и не скобка разметки.
fn is_url_char(c: char) -> bool {
    !(c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '\\'))
}

/// Поиск ASCII-подстроки без учёта регистра.
fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() || h.len() < n.len() {
        return None;
    }
    (0..=h.len() - n.len()).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Заменяет целиком адреса `http(s)://…`, в query которых есть подпись.
fn scrub_signed_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = find_ignore_case(rest, "http") {
        let candidate = &rest[pos..];
        let lower_scheme = candidate.get(..8).unwrap_or(candidate).to_ascii_lowercase();
        if !(lower_scheme.starts_with("https://") || lower_scheme.starts_with("http://")) {
            out.push_str(&rest[..pos + 4]);
            rest = &rest[pos + 4..];
            continue;
        }
        let len = candidate
            .char_indices()
            .find(|(_, c)| !is_url_char(*c))
            .map_or(candidate.len(), |(i, _)| i);
        let url = &candidate[..len];
        let signed = url.split_once('?').is_some_and(|(_, query)| {
            let query = query.to_ascii_lowercase();
            query.split(['&', ';']).any(|pair| {
                SIGNED_URL_MARKERS
                    .iter()
                    .any(|marker| pair.starts_with(marker))
            })
        });
        out.push_str(&rest[..pos]);
        out.push_str(if signed { MASK } else { url });
        rest = &candidate[len..];
    }
    out.push_str(rest);
    out
}

/// Заменяет значение поля `key` в JSON (`"key": "value"`, в том числе с экранированными
/// кавычками), формах и строке запроса (`key=value`) и заголовках (`key: value`).
fn scrub_key_value(text: &str, key: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = find_ignore_case(rest, key) {
        let key_end = pos + key.len();
        // Имя поля должно быть целым словом: `authCode` не должно совпасть в `myauthCodes`.
        let boundary_before = rest[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        let after_key = &rest[key_end..];
        let boundary_after = after_key
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        let value = if boundary_before && boundary_after {
            locate_value(after_key)
                .filter(|&(start, end)| !is_json_literal(&after_key[start..end], after_key, start))
        } else {
            None
        };
        match value {
            Some((start, end)) if end > start => {
                out.push_str(&rest[..key_end + start]);
                out.push_str(MASK);
                rest = &after_key[end..];
            }
            _ => {
                out.push_str(&rest[..key_end]);
                rest = after_key;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Значение без кавычек — литерал JSON `null`, `true` или `false`: секретом не бывает.
fn is_json_literal(value: &str, after_key: &str, start: usize) -> bool {
    let quoted = after_key[..start].ends_with(['"', '\'']);
    // Без кавычек значение тянется до `"`, `}` или конца строки: `null,"x":…` — тоже литерал.
    let token = value
        .split(|c: char| c == ',' || c == ']' || c.is_whitespace())
        .next()
        .unwrap_or(value);
    !quoted && matches!(token, "null" | "true" | "false")
}

/// Границы значения после имени поля: пропускает закрывающую кавычку имени, разделитель
/// (`:` или `=`) и открывающую кавычку значения. `None` — после имени нет разделителя.
fn locate_value(after_key: &str) -> Option<(usize, usize)> {
    let mut i = 0;
    let skip = |i: &mut usize, pattern: &str| {
        if after_key[*i..].starts_with(pattern) {
            *i += pattern.len();
            true
        } else {
            false
        }
    };
    let skip_spaces = |i: &mut usize| {
        while after_key[*i..].starts_with([' ', '\t']) {
            *i += 1;
        }
    };
    // Закрывающая кавычка имени: `"`, `'` или экранированная `\"`.
    let _ = skip(&mut i, "\\\"") || skip(&mut i, "\"") || skip(&mut i, "'");
    skip_spaces(&mut i);
    if !(skip(&mut i, ":") || skip(&mut i, "=")) {
        return None;
    }
    skip_spaces(&mut i);
    let quote = if skip(&mut i, "\\\"") {
        Some("\\\"")
    } else if skip(&mut i, "\"") {
        Some("\"")
    } else if skip(&mut i, "'") {
        Some("'")
    } else {
        None
    };
    let value = &after_key[i..];
    let len = match quote {
        // В кавычках — до закрывающей кавычки (или до конца текста, если её нет).
        Some(quote) => value.find(quote).unwrap_or(value.len()),
        // Без кавычек — до разделителя; значение заголовка `Authorization` состоит из двух
        // слов (`Bearer <token>`, `AWS4-HMAC-SHA256 Credential=…`), поэтому до конца строки.
        None => value
            .char_indices()
            .find(|(_, c)| matches!(c, '\r' | '\n' | '&' | '"' | '\'' | '}' | ';'))
            .map_or(value.len(), |(index, _)| index),
    };
    let trimmed = value[..len].trim_end().len();
    Some((i, i + trimmed))
}

/// Заменяет токен после `prefix` (не короче `min` символов).
fn scrub_after_prefix(text: &str, prefix: &str, min: usize) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(prefix) {
        let after = &rest[pos + prefix.len()..];
        let len = after
            .char_indices()
            .find(|(_, c)| !is_token_char(*c))
            .map_or(after.len(), |(i, _)| i);
        if len >= min {
            out.push_str(&rest[..pos]);
            out.push_str(prefix);
            out.push_str(MASK);
        } else {
            out.push_str(&rest[..pos + prefix.len() + len]);
        }
        rest = &after[len..];
    }
    out.push_str(rest);
    out
}

/// Заменяет JWT: три части base64url через точки, первые две начинаются с `eyJ`
/// (`{"` в base64).
fn scrub_jwts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find("eyJ") {
        let candidate = &rest[pos..];
        let len = candidate
            .char_indices()
            .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
            .map_or(candidate.len(), |(i, _)| i);
        let token = &candidate[..len];
        let parts: Vec<&str> = token.split('.').collect();
        let looks_like_jwt = parts.len() == 3
            && parts[1].starts_with("eyJ")
            && parts.iter().all(|part| !part.is_empty());
        out.push_str(&rest[..pos]);
        if looks_like_jwt {
            out.push_str(MASK);
        } else {
            out.push_str(token);
        }
        rest = &candidate[len..];
    }
    out.push_str(rest);
    out
}

/// Заменяет PEM-блоки приватных ключей (`BEGIN … PRIVATE KEY` … `END … PRIVATE KEY`).
/// Незакрытый блок заменяется до конца текста.
fn scrub_pem_blocks(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN ";
    const END: &str = "-----END ";
    const TAIL: &str = "PRIVATE KEY-----";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = rest.find(BEGIN) {
        let after = &rest[pos + BEGIN.len()..];
        // Заголовок блока: `BEGIN PRIVATE KEY-----`, `BEGIN RSA PRIVATE KEY-----` и т. п.
        let header_end = after.find("-----").map(|i| i + "-----".len());
        let is_private = header_end.is_some_and(|end| after[..end].ends_with(TAIL));
        let Some(header_end) = header_end.filter(|_| is_private) else {
            out.push_str(&rest[..pos + BEGIN.len()]);
            rest = after;
            continue;
        };
        out.push_str(&rest[..pos]);
        out.push_str(MASK);
        let body = &after[header_end..];
        rest = match body.find(END) {
            Some(end) => {
                let footer = &body[end + END.len()..];
                match footer.find(TAIL) {
                    Some(tail) => &footer[tail + TAIL.len()..],
                    None => "",
                }
            }
            None => "",
        };
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> String {
        ["B15B", "497B", "44E0", "80EB"].concat()
    }

    #[test]
    fn scrubs_bearer_values_and_jwts() {
        assert_eq!(
            scrub_secrets("sent Bearer abcdefgh12345678 to the API"),
            "sent Bearer *** to the API"
        );
        assert_eq!(scrub_secrets("Bearer abc"), "Bearer abc");
        // Собран из частей, чтобы сканер секретов не принял тестовую строку за токен.
        let jwt = ["eyJhbGciOiJIUzI1NiJ9", "eyJpc3MiOiJ4In0", "c2lnbmF0dXJl"].join(".");
        assert_eq!(
            scrub_secrets(&format!("token {jwt} rejected")),
            "token *** rejected"
        );
        assert_eq!(scrub_secrets("eyJ only"), "eyJ only");
        assert_eq!(scrub_secrets("eyJa.b.c"), "eyJa.b.c");
    }

    #[test]
    fn scrubs_secret_fields_in_json_forms_and_headers() {
        let s = secret();
        assert_eq!(
            scrub_secrets(&format!(
                "{{\"client_id\":\"123\",\"client_secret\":\"{s}\",\"x\":1}}"
            )),
            "{\"client_id\":\"123\",\"client_secret\":\"***\",\"x\":1}"
        );
        assert_eq!(
            scrub_secrets(&format!("{{\"authCode\" : \"{s}\"}}")),
            "{\"authCode\" : \"***\"}"
        );
        // JSON внутри JSON-строки: кавычки экранированы.
        assert_eq!(
            scrub_secrets(&format!("{{\\\"appSecret\\\":\\\"{s}\\\"}}")),
            "{\\\"appSecret\\\":\\\"***\\\"}"
        );
        assert_eq!(
            scrub_secrets(&format!("grant=x&client_secret={s}&client_id=1")),
            "grant=x&client_secret=***&client_id=1"
        );
        assert_eq!(
            scrub_secrets(&format!("ACCESS_TOKEN={s}")),
            "ACCESS_TOKEN=***"
        );
        assert_eq!(
            scrub_secrets(&format!(
                "Authorization: AWS4-HMAC-SHA256 Credential={s}/x, Signature={s}\nHost: h"
            )),
            "Authorization: ***\nHost: h"
        );
        assert_eq!(
            scrub_secrets(&format!("{{\"Authorization\":\"OBS {s}:{s}\"}}")),
            "{\"Authorization\":\"***\"}"
        );
        // Не поле: часть другого слова или нет разделителя.
        assert_eq!(scrub_secrets("myauthCodes=1"), "myauthCodes=1");
        assert_eq!(
            scrub_secrets("the authCode expired"),
            "the authCode expired"
        );
        assert_eq!(scrub_secrets("authCode="), "authCode=");
        // Литералы JSON остаются: ответ должен разбираться.
        assert_eq!(
            scrub_secrets("{\"authCode\":null,\"appSecret\": true}"),
            "{\"authCode\":null,\"appSecret\": true}"
        );
        assert_eq!(
            scrub_secrets("{\"authCode\":\"null\"}"),
            "{\"authCode\":\"***\"}"
        );
    }

    #[test]
    fn scrubs_secret_fields_of_the_spec() {
        let s = secret();
        assert!(crate::spec::secret_field_names().contains(&"fileURL"));
        assert_eq!(
            scrub_secrets(&format!(
                "{{\"fileURL\":\"https://r.example.com/a.csv\",\"secret\":\"{s}\"}}"
            )),
            "{\"fileURL\":\"***\",\"secret\":\"***\"}"
        );
        assert_eq!(
            scrub_secrets(&format!("certDownloadUrl={s}&x=1")),
            "certDownloadUrl=***&x=1"
        );
        assert_eq!(scrub_secrets("{\"fileURL\":null}"), "{\"fileURL\":null}");
        // Часть другого имени — не поле.
        assert_eq!(
            scrub_secrets("{\"client_secret_hint\":\"x\"}"),
            "{\"client_secret_hint\":\"x\"}"
        );
    }

    #[test]
    fn scrubs_signed_urls() {
        let s = secret();
        let url =
            format!("https://files.example.com/obj/1.apk?X-Amz-Algorithm=AWS4&X-Amz-Signature={s}");
        assert_eq!(
            scrub_secrets(&format!("upload to {url} failed")),
            "upload to *** failed"
        );
        assert_eq!(
            scrub_secrets(&format!(
                "{{\"fileURL\":\"https://r.example.com/a.csv?token={s}\"}}"
            )),
            "{\"fileURL\":\"***\"}"
        );
        assert_eq!(
            scrub_secrets(&format!(
                "HTTPS://r.example.com/a?AccessKeyId=AK&Signature={s}."
            )),
            "***"
        );
        // Обычные адреса не трогаются.
        let plain = "see https://developer.huawei.com/consumer/en/doc?id=1 and http";
        assert_eq!(scrub_secrets(plain), plain);
    }

    #[test]
    fn scrubs_private_key_blocks() {
        let begin = concat!("-----", "BEGIN PRIVATE KEY-----");
        let end = concat!("-----", "END PRIVATE KEY-----");
        let text = format!("key: {begin}\\nMIIEabc\\nxyz\\n{end}\\n tail");
        assert_eq!(scrub_secrets(&text), "key: ***\\n tail");
        assert_eq!(scrub_secrets(&format!("x {begin}\nAAAA")), "x ***");
        let cert = concat!(
            "-----",
            "BEGIN CERTIFICATE-----\nAAAA\n-----",
            "END CERTIFICATE-----"
        );
        assert_eq!(scrub_secrets(cert), cert);
    }

    #[test]
    fn keeps_non_ascii_text_intact() {
        let text = "ошибка: приложение не найдено — проверьте appId";
        assert_eq!(scrub_secrets(text), text);
        assert_eq!(
            scrub_secrets("пароль client_secret=значение-секрета&x"),
            "пароль client_secret=***&x"
        );
    }
}
