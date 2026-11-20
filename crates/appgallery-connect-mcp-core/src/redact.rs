//! Вырезание секретов из ответов перед отдачей агенту.
//!
//! В контекст агента не должны попадать значения, которые работают как учётные данные:
//! оттуда они легко уходят в логи, коммиты и документацию. Правила:
//!
//! 0. **Секретные поля операции по таблице спецификации** ([`hide_operation_secrets`],
//!    `OperationDef::secrets` из `[[secrets]]` дополнения): секрет приложения (`secret`
//!    у `get_app_fingerprints`), содержимое `agconnect-services.json` (`content` у
//!    `get_app_config_file`), временные ссылки на отчёты (`fileURL`), сертификаты и профили
//!    (`certDownloadUrl`, `provisionDownloadUrl`). Значение скрывается целиком, какого бы
//!    вида оно ни было (строка, объект, массив), путь проходит массивы на любом уровне:
//!    справочник Huawei местами путает объект и массив. Пометка называет поле, объясняет,
//!    почему оно скрыто, и как получить файл (`save_to`); секрет приложения не отдаётся
//!    никак.
//! 1. **Поля по имени** ([`is_credential_key`]): строковое значение поля, имя которого похоже
//!    на пароль, секрет, токен OAuth, приватный ключ, ключ API, код загрузки или заголовок
//!    авторизации ([`CREDENTIAL_KEYS`]), или совпадает с именем секретного поля любой
//!    операции таблицы (`fileURL`, `certDownloadUrl`, `provisionDownloadUrl`, `secret`:
//!    [`appgallery_connect_api::spec::secret_field_names`]), на любой глубине JSON. Так
//!    временная ссылка скрывается и там, где операция неизвестна. Имя приводится к snake_case
//!    (`appSecret` → `app_secret`, `authCode` → `auth_code`) и сравнивается целиком или по
//!    суффиксу после `_`. Признаки (`has_…`, `is_…`) и пустые строки не трогаются.
//! 2. **Значения по виду** в любой остальной строке — функцией вырезания исполнителя
//!    ([`appgallery_connect_api::ApiClient::scrub`]): известные клиенту токены, секрет и
//!    идентификатор клиента, значения `Bearer`, JWT, пары `ключ=значение` и `"ключ": "…"`
//!    с секретными именами внутри текста, PEM-блоки приватных ключей, подписанные адреса.
//!
//! Значения заменяются на [`MASK`], к результату добавляется пометка ([`Redactions::note`]):
//! сервер кладёт её в поле `note` обёртки ответа.
//!
//! Сохранение файлов из ответа (`save_to`) делает сервер ([`crate::server`]): значения для
//! сохранения он берёт до вырезания ([`savable_values`]), а агенту отдаёт ответ уже без них.

use std::collections::BTreeSet;

use appgallery_connect_api::spec::{OperationDef, SecretFieldDef, SecretKind, secret_field_names};
use serde_json::Value;

pub use appgallery_connect_api::scrub::MASK;

/// Имена полей с учётными данными (в snake_case). Совпадение — имя целиком или суффикс
/// после `_` (`client_secret`, `app_secret`, `upload_auth_code`) без учёта регистра;
/// camelCase, `-`, `.` и пробелы приводятся к `_`.
///
/// Голого `token` здесь нет намеренно: так называются и идентификаторы (токены страниц,
/// покупок), с которыми агент работает.
pub const CREDENTIAL_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "passphrase",
    "secret",
    "client_secret",
    "app_secret",
    "secret_key",
    "secret_access_key",
    "access_token",
    "refresh_token",
    "id_token",
    "oauth_token",
    "oauth2_token",
    "auth_token",
    "bearer_token",
    "session_token",
    "security_token",
    "private_key",
    "privatekey",
    "api_key",
    "apikey",
    "auth_code",
    "authorization_code",
    "credentials",
    "authorization",
    "cookie",
    "set_cookie",
    "assertion",
    "client_assertion",
    "code_verifier",
];

/// Префиксы флагов (`has_secret`, `is_password`): это признаки, а не значения.
const FLAG_PREFIXES: &[&str] = &["has_", "is_", "uses_"];

/// Секретное поле операции, скрытое в ответе (правило 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HiddenField {
    /// Инструмент, у которого есть `save_to` для этого поля.
    pub tool: &'static str,
    pub field: &'static SecretFieldDef,
    /// Сколько значений скрыто (поле в элементах массива).
    pub count: usize,
    /// Значение сохранено в файл (`save_to`), а не просто скрыто.
    pub saved: bool,
}

impl HiddenField {
    fn explanation(&self) -> String {
        let SecretFieldDef {
            path,
            kind,
            description,
            hint,
        } = *self.field;
        let mut text = if self.saved {
            format!(
                "`{path}` ({description}) is not returned: the server saved the file instead \
                 (see `saved`)."
            )
        } else {
            match kind {
                SecretKind::Credential => format!(
                    "`{path}` ({description}) was replaced with \"{MASK}\": it is a credential, \
                     and the server never returns it."
                ),
                SecretKind::DownloadUrl => format!(
                    "`{path}` ({description}) was replaced with \"{MASK}\": the link itself \
                     gives access to the file. To get the file, call `{tool}` with `save_to` set \
                     to the path of a new local file: the server downloads the file and returns \
                     only its path and size.",
                    tool = self.tool
                ),
                SecretKind::FileContent => format!(
                    "`{path}` ({description}) was replaced with \"{MASK}\". To get the file, \
                     call `{tool}` with `save_to` set to the path of a new local file: the server \
                     writes the file and returns only its path and size.",
                    tool = self.tool
                ),
            }
        };
        if !hint.is_empty() && !self.saved {
            text.push(' ');
            text.push_str(hint);
        }
        text
    }
}

/// Что было скрыто в одном ответе.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Redactions {
    /// Секретные поля операции по таблице спецификации (правило 0).
    pub operation_fields: Vec<HiddenField>,
    /// Имена полей, значения которых скрыты целиком (правило 1).
    pub credential_fields: BTreeSet<String>,
    /// Сколько строк изменило вырезание значений по виду (правило 2).
    pub secret_values: usize,
}

impl Redactions {
    pub fn is_empty(&self) -> bool {
        self.operation_fields.is_empty()
            && self.credential_fields.is_empty()
            && self.secret_values == 0
    }

    pub fn merge(&mut self, other: Redactions) {
        self.operation_fields.extend(other.operation_fields);
        self.credential_fields.extend(other.credential_fields);
        self.secret_values += other.secret_values;
    }

    /// Пояснение для агента; `None`, если ничего не скрыто.
    pub fn note(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = self
            .operation_fields
            .iter()
            .map(HiddenField::explanation)
            .collect();
        if !self.credential_fields.is_empty() {
            let fields: Vec<String> = self
                .credential_fields
                .iter()
                .map(|field| format!("`{field}`"))
                .collect();
            parts.push(format!(
                "Values of fields that look like credentials or temporary file links were \
                 replaced with \"{MASK}\": {}.",
                fields.join(", ")
            ));
        }
        if self.secret_values > 0 {
            parts.push(format!(
                "In {} text value(s), parts that look like access tokens, client secrets, \
                 private keys, authorization headers, upload credentials or signed URLs were \
                 replaced with \"{MASK}\".",
                self.secret_values
            ));
        }
        Some(format!(
            "[{} The server never shows hidden values; do not ask the user for them, and do \
             not try to recover them in other ways.]",
            parts.join(" ")
        ))
    }
}

/// Похоже ли имя поля на поле с учётными данными (см. [`CREDENTIAL_KEYS`]) или совпадает
/// ли оно с именем секретного поля операции из таблицы спецификации.
pub fn is_credential_key(key: &str) -> bool {
    if secret_field_names()
        .iter()
        .any(|name| name.eq_ignore_ascii_case(key.trim()))
    {
        return true;
    }
    let key = normalize_key(key);
    if FLAG_PREFIXES.iter().any(|prefix| key.starts_with(prefix)) {
        return false;
    }
    CREDENTIAL_KEYS.iter().any(|candidate| {
        key == *candidate
            || key
                .strip_suffix(candidate)
                .is_some_and(|rest| rest.ends_with('_'))
    })
}

/// `clientSecret`, `X-Api-Key`, `oauth2Token` → `client_secret`, `x_api_key`,
/// `oauth2_token`.
fn normalize_key(key: &str) -> String {
    let mut normalized = String::with_capacity(key.len() + 4);
    let mut previous: Option<char> = None;
    for c in key.trim().chars() {
        if c.is_uppercase() && previous.is_some_and(|p| p.is_lowercase() || p.is_ascii_digit()) {
            normalized.push('_');
        }
        if matches!(c, '-' | ' ' | '.') {
            normalized.push('_');
        } else {
            normalized.extend(c.to_lowercase());
        }
        previous = Some(c);
    }
    normalized
}

/// Скрывает секретные поля операции `op` по таблице спецификации (правило 0). `saved` —
/// путь поля, значение которого сервер сохранил в файл (`save_to`).
pub fn hide_operation_secrets(
    op: &'static OperationDef,
    value: &mut Value,
    saved: Option<&str>,
) -> Redactions {
    let mut redactions = Redactions::default();
    for field in op.secrets {
        let segments = segments(field.path);
        let count = hide_at(value, &segments);
        let saved = saved == Some(field.path);
        if count > 0 || saved {
            redactions.operation_fields.push(HiddenField {
                tool: op.tool_name,
                field,
                count,
                saved,
            });
        }
    }
    redactions
}

/// Вырезает секреты из ответа операции: её секретные поля по таблице (правило 0), затем
/// поля по имени и значения по виду ([`redact_json`]).
pub fn redact_operation_json(
    op: &'static OperationDef,
    value: &mut Value,
    saved: Option<&str>,
    scrub: &dyn Fn(&str) -> String,
) -> Redactions {
    let mut redactions = hide_operation_secrets(op, value, saved);
    redactions.merge(redact_json(value, scrub));
    redactions
}

/// Значения ответа, которые можно сохранить в файл (`save_to`): ссылки и содержимое файлов
/// из секретных полей операции, в порядке полей таблицы. Пустые строки и `null` пропущены.
pub fn savable_values<'a>(
    op: &'static OperationDef,
    value: &'a Value,
) -> Vec<(&'static SecretFieldDef, &'a Value)> {
    let mut found = Vec::new();
    for field in op.secrets.iter().filter(|field| field.kind.savable()) {
        let mut values = Vec::new();
        collect_at(value, &segments(field.path), &mut values);
        found.extend(values.into_iter().map(|value| (field, value)));
    }
    found
}

/// Сегменты пути `a.b[].c` без пометок массива: массивы проходятся на любом уровне.
fn segments(path: &str) -> Vec<&str> {
    path.split('.')
        .map(|segment| segment.strip_suffix("[]").unwrap_or(segment))
        .collect()
}

/// Скрывает значения по пути; возвращает их число. Массив на любом уровне проходится по
/// элементам; `null` и пустые строки не считаются.
fn hide_at(value: &mut Value, segments: &[&str]) -> usize {
    match value {
        Value::Array(items) => items.iter_mut().map(|item| hide_at(item, segments)).sum(),
        _ => match segments.split_first() {
            None => match value {
                Value::Null => 0,
                Value::String(text) if text.is_empty() || text == MASK => 0,
                other => {
                    *other = Value::String(MASK.to_owned());
                    1
                }
            },
            Some((name, rest)) => match value {
                Value::Object(map) => map.get_mut(*name).map_or(0, |child| hide_at(child, rest)),
                _ => 0,
            },
        },
    }
}

fn collect_at<'a>(value: &'a Value, segments: &[&str], found: &mut Vec<&'a Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_at(item, segments, found);
            }
        }
        _ => match segments.split_first() {
            None => match value {
                Value::Null => {}
                Value::String(text) if text.trim().is_empty() => {}
                other => found.push(other),
            },
            Some((name, rest)) => {
                if let Some(child) = value.get(*name) {
                    collect_at(child, rest, found);
                }
            }
        },
    }
}

/// Вырезает секреты из JSON (тела ответа API или результата составного инструмента).
/// `scrub` — вырезание значений по виду в строке (правило 2).
pub fn redact_json(value: &mut Value, scrub: &dyn Fn(&str) -> String) -> Redactions {
    let mut redactions = Redactions::default();
    walk(value, scrub, &mut redactions);
    redactions
}

/// Вырезает секреты по виду из текста (ответ не JSON).
pub fn redact_text(text: &str, scrub: &dyn Fn(&str) -> String) -> (String, Redactions) {
    let mut redactions = Redactions::default();
    let scrubbed = scrub(text);
    if scrubbed != text {
        redactions.secret_values += 1;
    }
    (scrubbed, redactions)
}

fn walk(value: &mut Value, scrub: &dyn Fn(&str) -> String, redactions: &mut Redactions) {
    match value {
        Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                match item {
                    Value::String(text) if text.is_empty() || text == MASK => {}
                    Value::String(text) if is_credential_key(key) => {
                        *text = MASK.to_owned();
                        redactions.credential_fields.insert(key.clone());
                    }
                    other => walk(other, scrub, redactions),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, scrub, redactions);
            }
        }
        Value::String(text) => {
            let scrubbed = scrub(text);
            if scrubbed != *text {
                *text = scrubbed;
                redactions.secret_values += 1;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::scrub::scrub_secrets;
    use serde_json::json;

    use super::*;

    fn secret() -> String {
        ["9F3A", "77C1", "D2E4", "B6A8"].concat()
    }

    fn scrub(text: &str) -> String {
        scrub_secrets(text)
    }

    #[test]
    fn credential_keys() {
        for key in [
            "client_secret",
            "clientSecret",
            "appSecret",
            "app_secret",
            "secret",
            "api_key",
            "apiKey",
            "X-Api-Key",
            "access_token",
            "accessToken",
            "refreshToken",
            "oauth2Token",
            "authCode",
            "uploadAuthCode",
            "Authorization",
            "private_key",
            "password",
        ] {
            assert!(is_credential_key(key), "{key}");
        }
        for key in [
            "token",
            "pageToken",
            "appId",
            "clientId",
            "client_id",
            "hasSecret",
            "is_password_set",
            "secretary",
            "keyword",
        ] {
            assert!(!is_credential_key(key), "{key}");
        }
    }

    #[test]
    fn hides_fields_by_name_at_any_depth() {
        let s = secret();
        let mut value = json!({
            "ret": {"code": 0, "msg": "success"},
            "appInfo": {"appSecret": s, "appName": "Demo", "hasSecret": true},
            "clients": [{"client_id": "1", "client_secret": s}, {"client_secret": ""}],
            "api_key": 42,
        });
        let redactions = redact_json(&mut value, &scrub);
        assert_eq!(value["appInfo"]["appSecret"], MASK);
        assert_eq!(value["appInfo"]["appName"], "Demo");
        assert_eq!(value["appInfo"]["hasSecret"], true);
        assert_eq!(value["clients"][0]["client_secret"], MASK);
        assert_eq!(value["clients"][0]["client_id"], "1");
        assert_eq!(value["clients"][1]["client_secret"], "");
        // Не строка — не секрет по имени.
        assert_eq!(value["api_key"], 42);
        assert!(!value.to_string().contains(&s));
        assert_eq!(
            redactions.credential_fields.iter().collect::<Vec<_>>(),
            ["appSecret", "client_secret"]
        );
        let note = redactions.note().unwrap();
        assert!(note.contains("`appSecret`, `client_secret`"), "{note}");
        assert!(note.contains("do not ask the user"), "{note}");
    }

    fn op(name: &str) -> &'static OperationDef {
        appgallery_connect_api::spec::find(name).unwrap()
    }

    #[test]
    fn hides_operation_secret_fields_with_explanations() {
        let s = secret();
        // Секрет приложения: скрыт и не сохраняется.
        let mut value = json!({
            "ret": {"code": 0, "msg": "success"},
            "secret": s,
            "certFingerprints": ["AA:BB"],
        });
        let redactions =
            redact_operation_json(op("get_app_fingerprints"), &mut value, None, &scrub);
        assert_eq!(value["secret"], MASK);
        assert_eq!(value["certFingerprints"][0], "AA:BB");
        let note = redactions.note().unwrap();
        assert!(note.contains("`secret` (the app secret"), "{note}");
        assert!(note.contains("never returns it"), "{note}");
        assert!(!note.contains("save_to"), "{note}");
        assert!(!value.to_string().contains(&s));

        // Файл конфигурации: содержимое скрыто целиком, даже если это объект.
        for content in [
            json!(format!("{{\"client\":{{\"api_key\":\"{s}\"}}}}")),
            json!({"k": s}),
        ] {
            let mut value = json!({"ret": {"code": 0}, "fileName": "agconnect-services.json", "content": content});
            let redactions =
                redact_operation_json(op("get_app_config_file"), &mut value, None, &scrub);
            assert_eq!(value["content"], MASK);
            assert_eq!(value["fileName"], "agconnect-services.json");
            let note = redactions.note().unwrap();
            assert!(
                note.contains("call `get_app_config_file` with `save_to`"),
                "{note}"
            );
        }

        // Ссылки в элементах массива (и объект вместо массива — тоже).
        let url = format!("https://cert.example.com/c.cer?k={s}");
        let mut value = json!({
            "certInfo": {"id": "1", "certDownloadUrl": url},
            "certList": [{"id": "2", "certDownloadUrl": url}, {"id": "3", "certDownloadUrl": null}],
        });
        let redactions = redact_operation_json(op("list_certificates"), &mut value, None, &scrub);
        assert_eq!(value["certInfo"]["certDownloadUrl"], MASK);
        assert_eq!(value["certList"][0]["certDownloadUrl"], MASK);
        assert_eq!(value["certList"][1]["certDownloadUrl"], Value::Null);
        assert_eq!(value["certList"][0]["id"], "2");
        assert_eq!(redactions.operation_fields.len(), 2);
        assert_eq!(redactions.operation_fields[1].count, 1);
        let note = redactions.note().unwrap();
        assert!(
            note.contains("narrow the list to one certificate"),
            "{note}"
        );
        assert!(!value.to_string().contains(&s));
    }

    #[test]
    fn savable_values_come_from_the_spec_fields() {
        let mut value = json!({"fileURL": "https://r.example.com/a.csv", "ret": {"code": 0}});
        let found = savable_values(op("get_iap_report_url"), &value);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].0.path, "fileURL");
        assert_eq!(found[0].1, "https://r.example.com/a.csv");
        let redactions = redact_operation_json(
            op("get_iap_report_url"),
            &mut value,
            Some("fileURL"),
            &scrub,
        );
        assert_eq!(value["fileURL"], MASK);
        let note = redactions.note().unwrap();
        assert!(note.contains("saved the file instead"), "{note}");
        assert!(!note.contains("valid only for a short time"), "{note}");

        let empty = json!({"fileURL": "", "ret": {"code": 0}});
        assert!(savable_values(op("get_iap_report_url"), &empty).is_empty());
        let many = json!({"provisionList": [
            {"provisionDownloadUrl": "https://a.example.com/1"},
            {"provisionDownloadUrl": "https://a.example.com/2"},
        ]});
        assert_eq!(savable_values(op("list_profiles"), &many).len(), 2);
        // Секрет приложения сохранить нельзя.
        let secret = json!({"secret": "x"});
        assert!(savable_values(op("get_app_fingerprints"), &secret).is_empty());
    }

    #[test]
    fn spec_secret_names_are_hidden_in_any_response() {
        // Ответ без известной операции: временная ссылка скрыта по имени поля.
        let mut value =
            json!({"data": {"fileURL": "https://r.example.com/a.csv"}, "content": "review"});
        let redactions = redact_json(&mut value, &scrub);
        assert_eq!(value["data"]["fileURL"], MASK);
        assert_eq!(value["content"], "review");
        assert!(redactions.credential_fields.contains("fileURL"));
        assert!(is_credential_key("certDownloadUrl"));
        assert!(!is_credential_key("content"));
    }

    #[test]
    fn scrubs_values_inside_strings() {
        let s = secret();
        let mut value = json!({
            "msg": format!("rejected Bearer {s}{s}"),
            "list": [format!("client_secret={s}&x=1"), "plain"],
        });
        let redactions = redact_json(&mut value, &scrub);
        assert_eq!(value["msg"], "rejected Bearer ***");
        assert_eq!(value["list"][0], "client_secret=***&x=1");
        assert_eq!(value["list"][1], "plain");
        assert_eq!(redactions.secret_values, 2);
        assert!(redactions.credential_fields.is_empty());
        assert!(redactions.note().unwrap().contains("2 text value(s)"));

        let mut clean = json!({"ret": {"code": 0}, "list": ["a", 1, null]});
        let redactions = redact_json(&mut clean, &scrub);
        assert!(redactions.is_empty());
        assert!(redactions.note().is_none());

        let (text, redactions) = redact_text(&format!("Bearer {s}{s}"), &scrub);
        assert_eq!(text, "Bearer ***");
        assert_eq!(redactions.secret_values, 1);
    }
}
