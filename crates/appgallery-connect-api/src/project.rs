//! Учётные данные API-клиентов уровня проекта.
//!
//! Сервер работает с API-клиентом уровня команды (в AppGallery Connect у него Project =
//! N/A, [`crate::credentials`]). Серверные API сервисов проекта (Remote Configuration,
//! App Linking, Auth Service) в режиме API-клиента принимают только клиента уровня
//! проекта: клиент команды получает отказ. Поэтому у профиля кроме основного клиента
//! могут быть клиенты проектов — по одному на проект ([`ProjectCredentials`]). Токен
//! такой клиент получает тем же `POST /api/oauth2/v1/token` на хосте региона профиля.
//!
//! Клиент проекта создаётся в AppGallery Connect (Project settings > Server SDK > API
//! client, или Users and permissions > API key > Connect API с выбранным проектом) и
//! скачивается файлом `agc-apiclient-*.json` ([`CredentialFile`]). Формат файла справочник
//! не описывает; поля взяты из разбора файла в Server SDK AppGallery Connect для Node.js
//! (`@agconnect/common-server`, `CredentialParser`): `type` (`project_client_id` или
//! `team_client_id`), `client_id`, `client_secret`, `project_id`, `region` (`CN`, `DE`,
//! `SG`, `RU`), `developer_id`, `configuration_version`. Из файла берутся только тип,
//! идентификатор и секрет клиента, проект и регион; остальные поля игнорируются.
//!
//! Тексты ошибок не содержат ни секрета, ни идентификатора клиента, ни фрагментов файла.

use std::fmt;

use serde_json::{Map, Value};
use zeroize::Zeroizing;

use crate::credentials::{ApiClientCredentials, CredentialsError};
use crate::region::Region;

/// Предел длины идентификатора проекта.
pub const MAX_PROJECT_ID_LEN: usize = 64;

/// Предел размера файла `agc-apiclient-*.json`.
pub const MAX_CREDENTIAL_FILE_BYTES: usize = 16 * 1024;

/// `type` файла клиента уровня проекта.
pub const PROJECT_CLIENT_TYPE: &str = "project_client_id";

/// `type` файла клиента уровня команды.
pub const TEAM_CLIENT_TYPE: &str = "team_client_id";

/// Проверяет идентификатор проекта (Project settings > General information > Project ID):
/// непустой, до [`MAX_PROJECT_ID_LEN`] символов из `A-Z`, `a-z`, `0-9`, `-`, `_`.
/// Идентификатор попадает в имя записи хранилища, в файл профилей и в команды-подсказки,
/// поэтому другие символы не допускаются. Пробелы по краям — ошибка вызывающего.
pub fn validate_project_id(value: &str) -> Result<(), CredentialsError> {
    if value.is_empty() {
        return Err(CredentialsError::Empty("project ID"));
    }
    if value.len() > MAX_PROJECT_ID_LEN {
        return Err(CredentialsError::TooLong {
            field: "project ID",
            limit: MAX_PROJECT_ID_LEN,
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(CredentialsError::InvalidProjectId);
    }
    Ok(())
}

/// API-клиент уровня проекта: идентификатор проекта и учётные данные клиента.
#[derive(Clone, PartialEq, Eq)]
pub struct ProjectCredentials {
    project_id: String,
    credentials: ApiClientCredentials,
}

impl ProjectCredentials {
    /// Проверяет идентификатор проекта ([`validate_project_id`]).
    pub fn new(
        project_id: impl Into<String>,
        credentials: ApiClientCredentials,
    ) -> Result<Self, CredentialsError> {
        let project_id = project_id.into();
        validate_project_id(&project_id)?;
        Ok(Self {
            project_id,
            credentials,
        })
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn credentials(&self) -> &ApiClientCredentials {
        &self.credentials
    }
}

impl fmt::Debug for ProjectCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProjectCredentials")
            .field("project_id", &self.project_id)
            .field("credentials", &self.credentials)
            .finish()
    }
}

/// Уровень API-клиента в файле `agc-apiclient-*.json` (поле `type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientLevel {
    /// `project_client_id`: клиент проекта.
    Project,
    /// `team_client_id`: клиент команды (Project = N/A).
    Team,
}

/// Разобранный файл `agc-apiclient-*.json`. `Debug` учётных данных скрыт.
#[derive(Debug, Clone)]
pub struct CredentialFile {
    pub level: ClientLevel,
    /// `project_id`; есть у файла клиента проекта.
    pub project_id: Option<String>,
    /// `region` — место обработки данных проекта; `None` — поля нет или оно пустое.
    pub region: Option<Region>,
    pub credentials: ApiClientCredentials,
}

/// Строковое значение поля (число — текстом). Значение из объекта забирается, чтобы
/// его память можно было затереть.
fn take_text(
    object: &mut Map<String, Value>,
    field: &'static str,
) -> Result<Option<Zeroizing<String>>, CredentialsError> {
    match object.remove(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(Zeroizing::new(text))),
        Some(Value::Number(number)) => Ok(Some(Zeroizing::new(number.to_string()))),
        Some(_) => Err(CredentialsError::Malformed(format!(
            "`{field}` is not a string"
        ))),
    }
}

impl CredentialFile {
    /// Разбирает текст файла. Неизвестные поля игнорируются; обязательные — `type`,
    /// `client_id`, `client_secret`, у клиента проекта ещё `project_id`. Пробелы по краям
    /// значений обрезаются.
    pub fn parse(text: &str) -> Result<Self, CredentialsError> {
        if text.len() > MAX_CREDENTIAL_FILE_BYTES {
            return Err(CredentialsError::Malformed(format!(
                "the file is larger than {MAX_CREDENTIAL_FILE_BYTES} bytes"
            )));
        }
        // Разбор в `Value`: ошибки типов serde_json повторяют значения, а синтаксическая
        // ошибка называет только строку и столбец.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let value: Value = serde_json::from_str(text).map_err(|err| {
            CredentialsError::Malformed(format!(
                "the file is not valid JSON (line {}, column {})",
                err.line(),
                err.column()
            ))
        })?;
        let Value::Object(mut object) = value else {
            return Err(CredentialsError::Malformed(
                "the file is not a JSON object".into(),
            ));
        };
        let result = Self::from_object(&mut object);
        // Секрет мог остаться в объекте, если разбор остановился раньше.
        if let Some(Value::String(secret)) = object.remove("client_secret") {
            drop(Zeroizing::new(secret));
        }
        result
    }

    fn from_object(object: &mut Map<String, Value>) -> Result<Self, CredentialsError> {
        let secret = take_text(object, "client_secret")?;
        let kind = take_text(object, "type")?;
        let level = match kind.as_deref().map(|kind| kind.trim()) {
            Some(PROJECT_CLIENT_TYPE) => ClientLevel::Project,
            Some(TEAM_CLIENT_TYPE) => ClientLevel::Team,
            Some(_) => {
                return Err(CredentialsError::Malformed(format!(
                    "`type` is neither `{PROJECT_CLIENT_TYPE}` nor `{TEAM_CLIENT_TYPE}`: this is \
                     not an AppGallery Connect API client credential file"
                )));
            }
            None => {
                return Err(CredentialsError::Malformed(
                    "there is no `type`: this is not an AppGallery Connect API client \
                     credential file (agc-apiclient-*.json)"
                        .into(),
                ));
            }
        };
        let client_id = take_text(object, "client_id")?
            .ok_or_else(|| CredentialsError::Malformed("there is no `client_id`".into()))?;
        let secret = secret
            .ok_or_else(|| CredentialsError::Malformed("there is no `client_secret`".into()))?;
        let credentials =
            ApiClientCredentials::from_parts(client_id.trim(), secret.trim().to_owned())?;

        let project_id = match take_text(object, "project_id")? {
            Some(id) if !id.trim().is_empty() => {
                let id = id.trim().to_owned();
                validate_project_id(&id)?;
                Some(id)
            }
            _ => None,
        };
        if level == ClientLevel::Project && project_id.is_none() {
            return Err(CredentialsError::Malformed(
                "the file of a project-level API client has no `project_id`".into(),
            ));
        }
        let region = match take_text(object, "region")? {
            Some(slug) if !slug.trim().is_empty() => {
                Some(Region::from_slug(slug.trim()).ok_or_else(|| {
                    CredentialsError::Malformed(format!(
                        "unknown `region` `{}`: expected CN, DE, SG or RU",
                        crate::path::shorten(slug.trim())
                    ))
                })?)
            }
            _ => None,
        };
        Ok(Self {
            level,
            project_id,
            region,
            credentials,
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn id() -> String {
        ["2640", "1234", "5678", "9012"].concat()
    }

    fn secret() -> String {
        ["9F8E", "7D6C", "5B4A", "3210"].concat().repeat(4)
    }

    fn file(fields: Value) -> String {
        fields.to_string()
    }

    #[test]
    fn validates_project_ids() {
        for good in ["736430079245000000", "abc-DEF_1"] {
            validate_project_id(good).unwrap();
        }
        for bad in ["", " 1", "1/2", "a b", "проект", &"1".repeat(65)] {
            assert!(validate_project_id(bad).is_err(), "{bad}");
        }
        let credentials = ApiClientCredentials::from_parts(id(), secret()).unwrap();
        assert!(ProjectCredentials::new("1/2", credentials.clone()).is_err());
        let project = ProjectCredentials::new("100", credentials).unwrap();
        let debug = format!("{project:?}");
        assert!(debug.contains("100"), "{debug}");
        assert!(
            !debug.contains(&secret()) && !debug.contains(&id()),
            "{debug}"
        );
    }

    #[test]
    fn parses_project_and_team_files() {
        let text = file(json!({
            "type": "project_client_id",
            "developer_id": "1000",
            "client_id": id(),
            "client_secret": format!(" {} ", secret()),
            "project_id": "736430079245000000",
            "configuration_version": "1.0",
            "region": "RU",
            "unknown": {"nested": true}
        }));
        let parsed = CredentialFile::parse(&format!("\u{feff}{text}\n")).unwrap();
        assert_eq!(parsed.level, ClientLevel::Project);
        assert_eq!(parsed.project_id.as_deref(), Some("736430079245000000"));
        assert_eq!(parsed.region, Some(Region::Ru));
        assert_eq!(parsed.credentials.client_id().expose(), id());
        assert_eq!(parsed.credentials.client_secret().expose(), secret());
        assert!(!format!("{parsed:?}").contains(&secret()));

        // Числа вместо строк и файл без региона.
        let text = file(json!({
            "type": "project_client_id",
            "client_id": 123456789012_u64,
            "client_secret": secret(),
            "project_id": 736430079245000000_u64,
        }));
        let parsed = CredentialFile::parse(&text).unwrap();
        assert_eq!(parsed.project_id.as_deref(), Some("736430079245000000"));
        assert_eq!(parsed.credentials.client_id().expose(), "123456789012");
        assert_eq!(parsed.region, None);

        let team = file(json!({
            "type": "team_client_id",
            "developer_id": "1000",
            "client_id": id(),
            "client_secret": secret(),
            "configuration_version": "1.0"
        }));
        let parsed = CredentialFile::parse(&team).unwrap();
        assert_eq!(parsed.level, ClientLevel::Team);
        assert_eq!(parsed.project_id, None);
    }

    #[test]
    fn rejects_broken_files_without_echoing_values() {
        let full = || {
            json!({
                "type": "project_client_id",
                "client_id": id(),
                "client_secret": secret(),
                "project_id": "100",
                "region": "CN",
            })
        };
        let without = |field: &str| {
            let mut value = full();
            value.as_object_mut().unwrap().remove(field);
            value.to_string()
        };
        let with = |field: &str, replacement: Value| {
            let mut value = full();
            value[field] = replacement;
            value.to_string()
        };
        let broken_json = full().to_string().replace("\"client_id\"", "client_id");
        let cases: Vec<(String, &str)> = vec![
            (String::new(), "not valid JSON"),
            (broken_json, "not valid JSON"),
            ("[1]".into(), "not a JSON object"),
            (without("type"), "no `type`"),
            (with("type", json!("service_account")), "neither"),
            (without("client_id"), "no `client_id`"),
            (without("client_secret"), "no `client_secret`"),
            (without("project_id"), "no `project_id`"),
            (with("project_id", json!("1/2")), "project ID"),
            (with("region", json!("US")), "unknown `region`"),
            (
                with("client_secret", json!({"x": secret()})),
                "not a string",
            ),
            (with("client_secret", json!("")), "client secret is empty"),
            (with("client_id", json!("a b")), "client ID contains"),
            ("x".repeat(MAX_CREDENTIAL_FILE_BYTES + 1), "larger than"),
        ];
        for (text, expected) in cases {
            let err = CredentialFile::parse(&text).unwrap_err().to_string();
            assert!(err.contains(expected), "{expected}: {err}");
            assert!(!err.contains(&secret()), "{err}");
            assert!(!err.contains(&id()), "{err}");
        }
        // Обрезанный файл: секрет в тексте ошибки не появляется.
        let truncated = full().to_string();
        let truncated = &truncated[..truncated.len() - 20];
        let err = CredentialFile::parse(truncated).unwrap_err().to_string();
        assert!(!err.contains(&secret()), "{err}");
    }
}
