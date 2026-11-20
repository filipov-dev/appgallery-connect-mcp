//! Секрет приложения и токен уровня приложения (app-level access token).
//!
//! Серверные API покупок в приложении (HMS IAP, [`crate::service`]) не принимают токен
//! API-клиента AppGallery Connect: их запросы подписываются токеном приложения. Токен
//! выдаёт Huawei OAuth 2.0 (`POST https://oauth-login.cloud.huawei.com/oauth2/v3/token`,
//! тело `application/x-www-form-urlencoded`: `grant_type=client_credentials`,
//! `client_id=<OAuth client ID приложения>`, `client_secret=<секрет>`), ответ —
//! `{"access_token": …, "expires_in": …}`. Запрос к IAP несёт заголовок
//! `Authorization: Basic base64("APPAT:" + token)`.
//!
//! Секрет приложения — [`AppCredentials`]: идентификатор приложения (App ID), OAuth
//! client ID (по справочнику совпадает с App ID) и секрет (AppGallery Connect > My apps >
//! приложение > App information, или All services > In-App Purchases). Он хранится только
//! в системном хранилище, запись `<профиль>/app/<appId>` ([`crate::store::app_account`]),
//! и читается только этим крейтом; `Debug` скрывает секрет, `Display` нет вовсе.
//!
//! [`AppTokens`] держит токен в памяти процесса — свой у каждого приложения: заменяет
//! его заранее ([`REFRESH_MARGIN`]) и после ответа 401, параллельные запросы ждут одного
//! обновления. Справочник просит не получать токен перед каждым вызовом: частые запросы
//! токена Huawei отклоняет.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use reqwest::header;
use serde_json::Value;
use zeroize::Zeroizing;

use crate::credentials::{ApiClientCredentials, CredentialsError};
use crate::http::{
    CONNECT_TIMEOUT, USER_AGENT, api_base, describe_client_error, describe_reqwest_error,
    lossy_prefix, read_limited, truncate,
};
use crate::region::Region;
use crate::scrub::{MASK, scrub_secrets};
use crate::token::{
    AccessToken, BoxFuture, Clock, DEFAULT_TOKEN_LIFETIME, MAX_TOKEN_LIFETIME, REFRESH_MARGIN,
    SystemClock, TokenError, TokenSource,
};

/// Хост Huawei OAuth 2.0, который выдаёт токены уровня приложения.
pub const OAUTH_LOGIN_HOST: &str = "oauth-login.cloud.huawei.com";

/// Путь метода получения токена Huawei OAuth 2.0.
pub const APP_TOKEN_PATH: &str = "/oauth2/v3/token";

/// Предел длины идентификатора приложения (App ID — десятичное число).
pub const MAX_APP_ID_LEN: usize = 20;

/// Префикс значения `Authorization: Basic …` у запросов IAP: `APPAT:<token>`.
pub const APP_TOKEN_PREFIX: &str = "APPAT:";

/// Сколько прежних токенов помнить для вырезания из текстов.
const REMEMBERED_TOKENS: usize = 4;

/// Таймаут запроса токена.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Предел тела ответа метода получения токена.
const TOKEN_RESPONSE_LIMIT: usize = 64 * 1024;

/// Предел длины текста Huawei в ошибке.
const MAX_ERROR_MESSAGE: usize = 300;

/// Проверяет идентификатор приложения: непустой, до [`MAX_APP_ID_LEN`] десятичных цифр.
/// Идентификатор попадает в имя записи хранилища, файл профилей и команды-подсказки.
pub fn validate_app_id(value: &str) -> Result<(), CredentialsError> {
    if value.is_empty() {
        return Err(CredentialsError::Empty("app ID"));
    }
    if value.len() > MAX_APP_ID_LEN {
        return Err(CredentialsError::TooLong {
            field: "app ID",
            limit: MAX_APP_ID_LEN,
        });
    }
    if !value.chars().all(|c| c.is_ascii_digit()) {
        return Err(CredentialsError::InvalidAppId);
    }
    Ok(())
}

/// Секрет приложения: App ID, OAuth client ID и client secret. `Debug` скрывает client ID
/// и секрет; `Display` нет.
#[derive(Clone, PartialEq, Eq)]
pub struct AppCredentials {
    app_id: String,
    /// Client ID и секрет: та же запись хранилища, что у API-клиента.
    oauth: ApiClientCredentials,
}

impl AppCredentials {
    /// Проверяет App ID ([`validate_app_id`]). `client_id` — OAuth client ID приложения;
    /// `None` — App ID (так в справочнике IAP).
    pub fn new(
        app_id: impl Into<String>,
        client_id: Option<String>,
        client_secret: impl Into<String>,
    ) -> Result<Self, CredentialsError> {
        let client_secret = Zeroizing::new(client_secret.into());
        let app_id = app_id.into();
        validate_app_id(&app_id)?;
        let client_id = client_id.unwrap_or_else(|| app_id.clone());
        let oauth = ApiClientCredentials::from_parts(client_id, client_secret.as_str())?;
        Ok(Self { app_id, oauth })
    }

    /// Секрет приложения из записи хранилища (JSON `client_id` и `client_secret`).
    pub(crate) fn from_stored(
        app_id: &str,
        oauth: ApiClientCredentials,
    ) -> Result<Self, CredentialsError> {
        validate_app_id(app_id)?;
        Ok(Self {
            app_id: app_id.to_owned(),
            oauth,
        })
    }

    /// Запись хранилища: client ID и секрет.
    pub(crate) fn stored(&self) -> &ApiClientCredentials {
        &self.oauth
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    /// Совпадает ли OAuth client ID с App ID (для `auth status`; само значение не
    /// выводится).
    pub fn client_id_is_app_id(&self) -> bool {
        self.oauth.client_id().expose() == self.app_id
    }

    /// Значения, которые исполнитель вырезает точным совпадением: секрет и client ID, если
    /// он не совпадает с App ID (App ID — обычный аргумент инструментов).
    pub(crate) fn secret_values(&self) -> Vec<Zeroizing<String>> {
        let mut values = vec![Zeroizing::new(
            self.oauth.client_secret().expose().to_owned(),
        )];
        if !self.client_id_is_app_id() {
            values.push(Zeroizing::new(self.oauth.client_id().expose().to_owned()));
        }
        values
    }
}

impl fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppCredentials")
            .field("app_id", &self.app_id)
            .field("client_id", &MASK)
            .field("client_secret", &MASK)
            .finish()
    }
}

#[derive(Clone)]
struct Cached {
    token: AccessToken,
    obtained_at: SystemTime,
    expires_at: SystemTime,
}

/// Блокировка без паники: под ней нет кода, который паникует.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Токены уровня приложения: получение у Huawei OAuth 2.0 и кэш в памяти.
pub struct AppTokens {
    http: reqwest::Client,
    url: url::Url,
    credentials: AppCredentials,
    clock: Arc<dyn Clock>,
    refresh_margin: Duration,
    cache: tokio::sync::Mutex<Option<Cached>>,
    remembered: Mutex<Vec<AccessToken>>,
}

impl fmt::Debug for AppTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppTokens")
            .field("host", &self.url.host_str())
            .field("credentials", &self.credentials)
            .finish_non_exhaustive()
    }
}

impl AppTokens {
    /// Токены приложения у Huawei OAuth 2.0.
    pub fn new(credentials: AppCredentials) -> Result<Self, TokenError> {
        Self::with_base_url(None, credentials)
    }

    /// Как [`Self::new`], с подменой корня для тестов (только loopback-адрес, `wiremock`):
    /// токен запрашивается по пути [`APP_TOKEN_PATH`] этого адреса.
    pub fn with_base_url(
        base_url: Option<&str>,
        credentials: AppCredentials,
    ) -> Result<Self, TokenError> {
        let mut url = match base_url {
            // Регион для подмены не важен: проверяется только адрес.
            Some(base) => api_base(Region::Cn, Some(base)).map_err(TokenError::Config)?,
            None => url::Url::parse(&format!("https://{OAUTH_LOGIN_HOST}"))
                .map_err(|err| TokenError::Config(format!("OAuth address: {err}")))?,
        };
        url.set_path(APP_TOKEN_PATH);
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TOKEN_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // Секрет приложения не должен уйти по редиректу на другой адрес.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| TokenError::Config(describe_client_error(&err)))?;
        Ok(Self {
            http,
            url,
            credentials,
            clock: Arc::new(SystemClock),
            refresh_margin: REFRESH_MARGIN,
            cache: tokio::sync::Mutex::new(None),
            remembered: Mutex::new(Vec::new()),
        })
    }

    /// Заменяет часы (тесты).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn app_id(&self) -> &str {
        self.credentials.app_id()
    }

    fn fresh(&self, cached: &Cached, now: SystemTime) -> bool {
        now >= cached.obtained_at
            && now
                .checked_add(self.refresh_margin)
                .is_some_and(|deadline| deadline < cached.expires_at)
    }

    async fn current(&self) -> Result<AccessToken, TokenError> {
        let mut cache = self.cache.lock().await;
        let now = self.clock.now();
        if let Some(cached) = cache.as_ref()
            && self.fresh(cached, now)
        {
            return Ok(cached.token.clone());
        }
        let (token, expires_in) = self.fetch().await?;
        let lifetime = expires_in.min(MAX_TOKEN_LIFETIME);
        {
            let mut remembered = lock(&self.remembered);
            remembered.push(token.clone());
            if remembered.len() > REMEMBERED_TOKENS {
                remembered.remove(0);
            }
        }
        *cache = Some(Cached {
            token: token.clone(),
            obtained_at: now,
            expires_at: now.checked_add(lifetime).unwrap_or(now),
        });
        tracing::debug!(
            lifetime_secs = lifetime.as_secs(),
            "obtained an app-level access token"
        );
        Ok(token)
    }

    /// Запрашивает новый токен. Тело запроса с секретом затирается после отправки копии.
    async fn fetch(&self) -> Result<(AccessToken, Duration), TokenError> {
        let oauth = self.credentials.stored();
        let body = Zeroizing::new(format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            utf8_percent_encode(oauth.client_id().expose(), NON_ALPHANUMERIC),
            utf8_percent_encode(oauth.client_secret().expose(), NON_ALPHANUMERIC),
        ));
        let mut response = self
            .http
            .post(self.url.clone())
            .header(
                header::CONTENT_TYPE,
                "application/x-www-form-urlencoded; charset=UTF-8",
            )
            .header(header::ACCEPT, "application/json")
            .body(body.as_bytes().to_vec())
            .send()
            .await
            .map_err(|err| TokenError::Transport(describe_reqwest_error(&err.without_url())))?;
        let status = response.status();
        let (bytes, truncated) = read_limited(&mut response, TOKEN_RESPONSE_LIMIT)
            .await
            .map_err(|err| TokenError::Transport(describe_reqwest_error(&err.without_url())))?;
        let bytes = Zeroizing::new(bytes);
        let parsed = if truncated {
            None
        } else {
            serde_json::from_slice::<Value>(&bytes).ok()
        };

        if status.is_success()
            && let Some(value) = &parsed
            && let Some(token) = value
                .get("access_token")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|token| !token.is_empty())
        {
            if !token.chars().all(|c| c.is_ascii_graphic()) {
                return Err(TokenError::InvalidResponse(
                    "the app-level access token contains characters that cannot be sent in a \
                     header"
                        .into(),
                ));
            }
            let expires_in = match value.get("expires_in") {
                Some(Value::Number(number)) => number.as_u64(),
                Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
                _ => None,
            }
            .filter(|seconds| *seconds > 0)
            .map_or(DEFAULT_TOKEN_LIFETIME, Duration::from_secs);
            return Ok((AccessToken::new(token), expires_in));
        }

        // Отказ Huawei OAuth: `{"error": 1101, "sub_error": 20152, "error_description": …}`,
        // обычно со статусом 4xx.
        let code = parsed.as_ref().and_then(oauth_error_code);
        let message = parsed
            .as_ref()
            .and_then(|value| value.get("error_description"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if code.is_some() || !status.is_success() {
            let message = match message {
                Some(message) => message,
                None if code.is_some() => String::new(),
                None => lossy_prefix(&bytes).trim().to_owned(),
            };
            return Err(TokenError::AppRejected {
                app_id: self.credentials.app_id().to_owned(),
                status: status.as_u16(),
                code: code.map(|code| self.redact(&code)),
                message: self.redact(&message),
            });
        }
        Err(TokenError::InvalidResponse(
            "HTTP 200 from Huawei OAuth without `access_token` and without an error code".into(),
        ))
    }

    /// Текст Huawei для ошибки: без секрета приложения, без секретов по виду, обрезан.
    fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in self.credentials.secret_values() {
            if !secret.is_empty() && text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), MASK);
            }
        }
        truncate(&scrub_secrets(&text), MAX_ERROR_MESSAGE)
    }
}

/// Код ошибки Huawei OAuth: `error` и, если есть, `sub_error` (`1101/20152`).
fn oauth_error_code(value: &Value) -> Option<String> {
    let text = |key: &str| match value.get(key)? {
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
        _ => None,
    };
    let error = text("error")?;
    Some(match text("sub_error") {
        Some(sub) => format!("{error}/{sub}"),
        None => error,
    })
}

impl TokenSource for AppTokens {
    fn token(&self) -> BoxFuture<'_, Result<AccessToken, TokenError>> {
        Box::pin(self.current())
    }

    fn invalidate<'a>(&'a self, rejected: &'a AccessToken) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let mut cache = self.cache.lock().await;
            if cache
                .as_ref()
                .is_some_and(|cached| &cached.token == rejected)
            {
                *cache = None;
            }
        })
    }

    fn known_tokens(&self) -> Vec<AccessToken> {
        lock(&self.remembered).clone()
    }
}

/// Значение заголовка `Authorization` запроса IAP: `Basic base64("APPAT:" + token)`.
/// Строка затирается при освобождении.
pub fn app_authorization(token: &AccessToken) -> Zeroizing<String> {
    use base64::Engine as _;
    let raw = Zeroizing::new(format!("{APP_TOKEN_PREFIX}{}", token.expose()));
    Zeroizing::new(format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(raw.as_bytes())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret() -> String {
        ["a1b2", "c3d4", "e5f6", "0718"].concat().repeat(4)
    }

    #[test]
    fn app_ids_are_decimal() {
        assert!(validate_app_id("100000001").is_ok());
        for bad in ["", "12a", " 1", "1".repeat(21).as_str(), "-1"] {
            assert!(validate_app_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn credentials_hide_secret_and_client_id() {
        let creds = AppCredentials::new("100000001", None, secret()).unwrap();
        assert!(creds.client_id_is_app_id());
        let debug = format!("{creds:?}");
        assert!(debug.contains("100000001"), "{debug}");
        assert!(!debug.contains(&secret()), "{debug}");
        assert_eq!(creds.secret_values().len(), 1);

        let other = AppCredentials::new("100000001", Some("2000000002".into()), secret()).unwrap();
        assert!(!other.client_id_is_app_id());
        assert!(!format!("{other:?}").contains("2000000002"));
        assert_eq!(other.secret_values().len(), 2);

        let err = AppCredentials::new("app", None, secret()).unwrap_err();
        assert!(!err.to_string().contains(&secret()));
        assert!(AppCredentials::new("1", None, "with space").is_err());
    }

    #[test]
    fn authorization_is_basic_appat() {
        use base64::Engine as _;
        let token = AccessToken::new(["tok", "en", "-", "42"].concat());
        let value = app_authorization(&token);
        let encoded = value.strip_prefix("Basic ").unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap();
        assert_eq!(decoded, b"APPAT:token-42");
    }

    #[test]
    fn oauth_error_codes() {
        let value = serde_json::json!({"error": 1101, "sub_error": 20152});
        assert_eq!(oauth_error_code(&value).as_deref(), Some("1101/20152"));
        let value = serde_json::json!({"error": "invalid_client"});
        assert_eq!(oauth_error_code(&value).as_deref(), Some("invalid_client"));
        assert_eq!(oauth_error_code(&serde_json::json!({})), None);
    }
}
