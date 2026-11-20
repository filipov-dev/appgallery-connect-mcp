//! Учётные данные API-клиента AppGallery Connect.
//!
//! API-клиент создаётся в AppGallery Connect («Пользователи и разрешения» → Connect API) и
//! состоит из идентификатора ([`ClientId`]) и секрета ([`ClientSecret`]). Вместе они —
//! [`ApiClientCredentials`]: одна запись системного хранилища на профиль
//! ([`crate::store`]), из которой исполнитель получает access token ([`crate::token`]).
//!
//! Идентификатор не секрет, но относится к конкретному кабинету, поэтому `Debug` и
//! `Display` скрывают оба значения. Память секрета затирается при освобождении. Тексты
//! ошибок не содержат самих значений.

use std::fmt;

use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

/// Предел длины идентификатора клиента (в справочнике — `String(256)`).
pub const MAX_CLIENT_ID_LEN: usize = 256;

/// Предел длины секрета клиента (в справочнике — `String(2048)`).
pub const MAX_CLIENT_SECRET_LEN: usize = 2048;

/// Предел размера записи хранилища с учётными данными (JSON).
pub const MAX_STORED_BYTES: usize = 4096;

/// Ошибки проверки учётных данных. Тексты не содержат значений.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CredentialsError {
    #[error("the {0} is empty")]
    Empty(&'static str),
    #[error("the {field} is longer than {limit} characters")]
    TooLong { field: &'static str, limit: usize },
    /// Пробел, управляющий символ, кавычка, обратная косая или не-ASCII.
    #[error(
        "the {0} contains a character that is not allowed (spaces, control characters, \
         quotes, backslashes and non-ASCII characters); copy the value from AppGallery \
         Connect > Users and permissions > API key > Connect API"
    )]
    InvalidCharacter(&'static str),
    /// Запись хранилища или файл учётных данных не разбирается.
    #[error("not a usable API client credential: {0}")]
    Malformed(String),
    /// Идентификатор проекта с недопустимыми символами.
    #[error(
        "the project ID may contain only A-Z, a-z, 0-9, `-` and `_`; copy it from AppGallery \
         Connect > the project > Project settings > General information"
    )]
    InvalidProjectId,
    /// Идентификатор приложения не из десятичных цифр.
    #[error(
        "the app ID may contain only digits; copy it from AppGallery Connect > My apps > the \
         app > App information"
    )]
    InvalidAppId,
}

const CLIENT_ID: &str = "client ID";
const CLIENT_SECRET: &str = "client secret";

/// Проверка формы значения: непустое, без пробелов, управляющих и не-ASCII символов,
/// кавычек и обратных косых (значение целиком уходит в JSON и в заголовок HTTP).
fn check(value: &str, field: &'static str, limit: usize) -> Result<(), CredentialsError> {
    if value.is_empty() {
        return Err(CredentialsError::Empty(field));
    }
    if value.len() > limit {
        return Err(CredentialsError::TooLong { field, limit });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
    {
        return Err(CredentialsError::InvalidCharacter(field));
    }
    Ok(())
}

/// Идентификатор API-клиента (заголовок `client_id`). `Debug` и `Display` скрыты.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientId(String);

impl ClientId {
    /// Проверяет форму значения. Пробелы по краям не обрезаются: это ошибка ввода, её
    /// убирает вызывающий (CLI), если считает нужным.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialsError> {
        let value = value.into();
        check(&value, CLIENT_ID, MAX_CLIENT_ID_LEN)?;
        Ok(Self(value))
    }

    /// Значение идентификатора. Только для заголовка `client_id`, тела запроса токена и
    /// записи в хранилище; не логировать.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Идентификатор в маске для вывода пользователю в его терминале: первые и последние
    /// два символа (`12***89`); короткие значения — только `***`.
    pub fn masked(&self) -> String {
        let chars: Vec<char> = self.0.chars().collect();
        if chars.len() < 8 {
            return "***".to_owned();
        }
        let head: String = chars[..2].iter().collect();
        let tail: String = chars[chars.len() - 2..].iter().collect();
        format!("{head}***{tail}")
    }
}

impl fmt::Debug for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientId(***)")
    }
}

impl fmt::Display for ClientId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Секрет API-клиента. `Debug` и `Display` скрыты, память затирается при освобождении.
#[derive(Clone, PartialEq, Eq)]
pub struct ClientSecret(Zeroizing<String>);

impl ClientSecret {
    /// Проверяет форму значения; при ошибке переданная строка затирается.
    pub fn new(value: impl Into<String>) -> Result<Self, CredentialsError> {
        let value = Zeroizing::new(value.into());
        check(&value, CLIENT_SECRET, MAX_CLIENT_SECRET_LEN)?;
        Ok(Self(value))
    }

    /// Значение секрета. Только для тела запроса токена и записи в хранилище; не выводить
    /// и не логировать.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ClientSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientSecret(***)")
    }
}

impl fmt::Display for ClientSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Учётные данные API-клиента: идентификатор и секрет.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiClientCredentials {
    client_id: ClientId,
    client_secret: ClientSecret,
}

/// Запись хранилища: `{"client_id": "...", "client_secret": "..."}`.
#[derive(Serialize, Deserialize)]
struct Stored {
    client_id: String,
    client_secret: String,
}

impl Drop for Stored {
    fn drop(&mut self) {
        self.client_secret.zeroize();
    }
}

impl ApiClientCredentials {
    pub fn new(client_id: ClientId, client_secret: ClientSecret) -> Self {
        Self {
            client_id,
            client_secret,
        }
    }

    /// Учётные данные из двух строк с проверкой формы.
    pub fn from_parts(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Result<Self, CredentialsError> {
        // Секрет оборачивается первым: при ошибке в идентификаторе его память затрётся.
        let client_secret = ClientSecret::new(client_secret);
        let client_id = ClientId::new(client_id)?;
        Ok(Self::new(client_id, client_secret?))
    }

    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    pub fn client_secret(&self) -> &ClientSecret {
        &self.client_secret
    }

    /// JSON-текст записи хранилища. Только для записи в системное хранилище.
    pub fn to_stored_json(&self) -> Zeroizing<String> {
        let stored = Stored {
            client_id: self.client_id.expose().to_owned(),
            client_secret: self.client_secret.expose().to_owned(),
        };
        // Сериализация структуры из двух строк не может завершиться ошибкой.
        Zeroizing::new(serde_json::to_string(&stored).unwrap_or_default())
    }

    /// Разбирает запись хранилища. Ошибка не содержит фрагментов текста.
    pub fn from_stored_json(text: &str) -> Result<Self, CredentialsError> {
        if text.len() > MAX_STORED_BYTES {
            return Err(CredentialsError::Malformed(format!(
                "the stored value is larger than {MAX_STORED_BYTES} bytes"
            )));
        }
        let mut stored: Stored = serde_json::from_str(text.trim()).map_err(|err| {
            CredentialsError::Malformed(format!(
                "the stored value is not a JSON object with `client_id` and `client_secret` \
                 (line {}, column {})",
                err.line(),
                err.column()
            ))
        })?;
        Self::from_parts(
            std::mem::take(&mut stored.client_id),
            std::mem::take(&mut stored.client_secret),
        )
    }
}

impl fmt::Debug for ApiClientCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClientCredentials")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> String {
        ["1234", "5678", "9012"].concat()
    }

    fn secret() -> String {
        ["A1B2", "C3D4", "E5F6", "0718"].concat().repeat(4)
    }

    #[test]
    fn accepts_plain_values_and_hides_them() {
        let credentials = ApiClientCredentials::from_parts(id(), secret()).unwrap();
        assert_eq!(credentials.client_id().expose(), id());
        assert_eq!(credentials.client_secret().expose(), secret());
        let debug = format!("{credentials:?}");
        assert!(!debug.contains(&id()), "{debug}");
        assert!(!debug.contains(&secret()), "{debug}");
        assert_eq!(credentials.client_id().to_string(), "***");
        assert_eq!(credentials.client_secret().to_string(), "***");
        assert_eq!(credentials.client_id().masked(), "12***12");
        assert_eq!(ClientId::new("1234567").unwrap().masked(), "***");
    }

    #[test]
    fn rejects_bad_shapes_without_echoing_values() {
        let cases: Vec<(String, String, &str)> = vec![
            (String::new(), secret(), "client ID is empty"),
            (id(), String::new(), "client secret is empty"),
            (format!(" {}", id()), secret(), "client ID contains"),
            (id(), format!("{}\n", secret()), "client secret contains"),
            (
                id(),
                format!("{}\u{7f}", secret()),
                "client secret contains",
            ),
            (format!("{}\"", id()), secret(), "client ID contains"),
            (format!("{}é", id()), secret(), "client ID contains"),
            ("1".repeat(MAX_CLIENT_ID_LEN + 1), secret(), "longer than"),
            (id(), "a".repeat(MAX_CLIENT_SECRET_LEN + 1), "longer than"),
        ];
        for (client_id, client_secret, expected) in cases {
            let err = ApiClientCredentials::from_parts(client_id, client_secret)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{expected}: {err}");
            assert!(!err.contains(&id()), "{err}");
            assert!(!err.contains(&secret()), "{err}");
        }
    }

    #[test]
    fn stored_json_round_trip() {
        let credentials = ApiClientCredentials::from_parts(id(), secret()).unwrap();
        let json = credentials.to_stored_json();
        assert_eq!(
            ApiClientCredentials::from_stored_json(&json).unwrap(),
            credentials
        );
        for (text, expected) in [
            ("", "not a JSON object"),
            ("[1]", "not a JSON object"),
            ("{\"client_id\": \"1\"}", "not a JSON object"),
            ("{\"client_id\": \"\", \"client_secret\": \"x\"}", "empty"),
        ] {
            let err = ApiClientCredentials::from_stored_json(text)
                .unwrap_err()
                .to_string();
            assert!(err.contains(expected), "{expected}: {err}");
        }
        let broken = format!(
            "{{\"client_id\": \"{}\", \"client_secret\": \"{}",
            id(),
            secret()
        );
        let err = ApiClientCredentials::from_stored_json(&broken)
            .unwrap_err()
            .to_string();
        assert!(!err.contains(&secret()), "{err}");
    }
}
