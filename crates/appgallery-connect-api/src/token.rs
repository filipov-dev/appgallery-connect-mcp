//! Access token API-клиента AppGallery Connect.
//!
//! Токен выдаёт `POST /api/oauth2/v1/token` на хосте региона: JSON-тело
//! `{"grant_type": "client_credentials", "client_id": …, "client_secret": …}`, ответ —
//! `{"access_token": …, "expires_in": 172800}`. Отказ может прийти с HTTP 200:
//! `{"ret": {"code": <число>, "msg": "…"}}` — это [`TokenError::Rejected`] с кодом и
//! текстом Huawei.
//!
//! Источник токенов для исполнителя — трейт [`TokenSource`]: исполнитель берёт токен перед
//! каждой попыткой, после ответа 401 просит сбросить отвергнутый
//! ([`TokenSource::invalidate`]) и вырезает известные токены из текстов
//! ([`TokenSource::known_tokens`]). [`ApiClientTokens`] держит токен в памяти процесса и
//! заменяет его, когда до истечения остаётся меньше [`REFRESH_MARGIN`]; параллельные
//! запросы ждут одного обновления. [`StaticToken`] — заранее известный токен (тесты).
//!
//! Токен хранится в [`Zeroizing`], его `Debug` и `Display` не раскрывают значение; секрет
//! клиента и токен не попадают в тексты ошибок и логи. Время берётся из [`Clock`]: тесты
//! подставляют [`ManualClock`].

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use reqwest::header;
use serde_json::{Value, json};
use zeroize::Zeroizing;

use crate::credentials::ApiClientCredentials;
use crate::envelope::detect_result;
use crate::http::{
    CONNECT_TIMEOUT, USER_AGENT, api_base, describe_client_error, describe_reqwest_error,
    lossy_prefix, read_limited, truncate,
};
use crate::path::TOKEN_PATH;
use crate::region::Region;
use crate::scrub::{MASK, scrub_secrets};

/// `grant_type` запроса токена API-клиента.
pub const CLIENT_CREDENTIALS_GRANT: &str = "client_credentials";

/// Наибольший срок жизни токена, который мы принимаем: Huawei выдаёт токен на 48 часов.
pub const MAX_TOKEN_LIFETIME: Duration = Duration::from_secs(48 * 3600);

/// Срок жизни токена, если в ответе нет `expires_in`.
pub const DEFAULT_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);

/// Токен заменяется новым, когда до истечения остаётся меньше этого времени.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(10 * 60);

/// Сколько прежних токенов помнить для вырезания из текстов.
const REMEMBERED_TOKENS: usize = 4;

/// Таймаут запроса токена.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);

/// Предел тела ответа метода получения токена.
const TOKEN_RESPONSE_LIMIT: usize = 64 * 1024;

/// Предел длины текста Huawei в ошибке.
const MAX_ERROR_MESSAGE: usize = 300;

/// Будущее, которое возвращают методы [`TokenSource`] (трейт должен оставаться
/// dyn-совместимым: исполнитель хранит `Arc<dyn TokenSource>`).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Источник текущего времени.
pub trait Clock: Send + Sync + fmt::Debug {
    fn now(&self) -> SystemTime;
}

/// Системные часы.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// Управляемые часы для тестов: время меняется только вызовами [`ManualClock::advance`]
/// и [`ManualClock::set`].
#[derive(Debug)]
pub struct ManualClock(Mutex<SystemTime>);

impl ManualClock {
    pub fn new(start: SystemTime) -> Self {
        Self(Mutex::new(start))
    }

    pub fn set(&self, time: SystemTime) {
        *lock(&self.0) = time;
    }

    pub fn advance(&self, by: Duration) {
        let mut now = lock(&self.0);
        *now += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> SystemTime {
        *lock(&self.0)
    }
}

/// Блокировка без паники: под блокировками этого модуля нет кода, который паникует, поэтому
/// отравление не означает испорченных данных.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Access token. `Debug` и `Display` не раскрывают значение, память затирается при
/// освобождении.
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(Zeroizing<String>);

impl AccessToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    /// Значение токена. Использовать только для заголовка `Authorization`.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccessToken(***)")
    }
}

impl fmt::Display for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(MASK)
    }
}

/// Ошибки получения токена. Тексты на английском и не содержат ни секрета, ни токена.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    /// AppGallery Connect отказал: неверные идентификатор или секрет, клиент удалён и т. п.
    /// Приходит и с HTTP 200 (код в `ret.code`).
    #[error("{}", describe_rejection(*.status, .code.as_deref(), .message))]
    Rejected {
        /// HTTP-статус ответа (может быть 200).
        status: u16,
        /// Код результата Huawei (`ret.code`), если он есть.
        code: Option<String>,
        /// Текст Huawei, обрезанный и без секретов.
        message: String,
    },
    /// Ответ не похож на ответ метода получения токена.
    #[error("the AppGallery Connect token endpoint returned an unexpected response: {0}")]
    InvalidResponse(String),
    /// Сеть, TLS, таймаут.
    #[error("could not reach the AppGallery Connect token endpoint: {0}")]
    Transport(String),
    /// Некорректная настройка источника токенов.
    #[error("invalid token source configuration: {0}")]
    Config(String),
}

fn describe_rejection(status: u16, code: Option<&str>, message: &str) -> String {
    let mut text = format!("AppGallery Connect rejected the API client credentials (HTTP {status}");
    if let Some(code) = code {
        text.push_str(", code ");
        text.push_str(code);
    }
    text.push(')');
    if !message.is_empty() {
        text.push_str(": ");
        text.push_str(message);
    }
    text.push_str(
        ". Check the client ID and secret of the API client (AppGallery Connect > Users and \
         permissions > API key > Connect API), that the client has not been deleted and that \
         the region of the profile matches the account.",
    );
    text
}

/// Источник токенов для исполнителя.
pub trait TokenSource: Send + Sync + fmt::Debug {
    /// Действующий токен: из памяти или новый.
    fn token(&self) -> BoxFuture<'_, Result<AccessToken, TokenError>>;

    /// API отверг токен (401): следующий [`Self::token`] должен получить новый. Если токен
    /// уже заменён (параллельный запрос успел обновить), ничего не делает.
    fn invalidate<'a>(&'a self, rejected: &'a AccessToken) -> BoxFuture<'a, ()>;

    /// Текущий и недавние прежние токены: исполнитель вырезает их из текстов ошибок и
    /// ответов на случай, если сервер отразил заголовок авторизации.
    fn known_tokens(&self) -> Vec<AccessToken>;
}

#[derive(Clone)]
struct Cached {
    token: AccessToken,
    obtained_at: SystemTime,
    expires_at: SystemTime,
}

/// Токены API-клиента: получение у AppGallery Connect и кэш в памяти.
///
/// Токен заменяется, когда до истечения остаётся меньше запаса ([`REFRESH_MARGIN`]) или
/// часы отвели назад раньше времени получения. Обновление идёт под асинхронной
/// блокировкой: параллельные запросы ждут его и получают тот же новый токен.
pub struct ApiClientTokens {
    http: reqwest::Client,
    url: url::Url,
    credentials: ApiClientCredentials,
    clock: Arc<dyn Clock>,
    refresh_margin: Duration,
    cache: tokio::sync::Mutex<Option<Cached>>,
    /// Недавние токены (новые в конце), для [`TokenSource::known_tokens`] без асинхронной
    /// блокировки.
    remembered: Mutex<Vec<AccessToken>>,
}

impl fmt::Debug for ApiClientTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiClientTokens")
            .field("host", &self.url.host_str())
            .field("credentials", &self.credentials)
            .field("refresh_margin", &self.refresh_margin)
            .finish_non_exhaustive()
    }
}

impl ApiClientTokens {
    /// Токены API-клиента на хосте региона.
    pub fn new(region: Region, credentials: ApiClientCredentials) -> Result<Self, TokenError> {
        Self::with_base_url(region, None, credentials)
    }

    /// Как [`Self::new`], с подменой корня API для тестов (только loopback-адрес,
    /// `wiremock`).
    pub fn with_base_url(
        region: Region,
        base_url: Option<&str>,
        credentials: ApiClientCredentials,
    ) -> Result<Self, TokenError> {
        let mut url = api_base(region, base_url).map_err(TokenError::Config)?;
        url.set_path(TOKEN_PATH);
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(TOKEN_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // Секрет клиента не должен уйти по редиректу на другой адрес.
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

    /// Заменяет запас времени, за который токен обновляется до истечения.
    pub fn with_refresh_margin(mut self, margin: Duration) -> Self {
        self.refresh_margin = margin;
        self
    }

    /// Учётные данные, по которым выпускаются токены.
    pub fn credentials(&self) -> &ApiClientCredentials {
        &self.credentials
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
            "obtained an AppGallery Connect access token"
        );
        Ok(token)
    }

    /// Запрашивает новый токен. Тело запроса с секретом затирается после отправки копии.
    async fn fetch(&self) -> Result<(AccessToken, Duration), TokenError> {
        let body = Zeroizing::new(
            json!({
                "grant_type": CLIENT_CREDENTIALS_GRANT,
                "client_id": self.credentials.client_id().expose(),
                "client_secret": self.credentials.client_secret().expose(),
            })
            .to_string(),
        );
        let mut response = self
            .http
            .post(self.url.clone())
            .header(header::CONTENT_TYPE, "application/json")
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
                    "the access token contains characters that cannot be sent in a header".into(),
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

        // Отказ: конверт `ret` (в том числе при HTTP 200) или статус не 2xx.
        if let Some(result) = parsed.as_ref().and_then(detect_result) {
            return Err(TokenError::Rejected {
                status: status.as_u16(),
                code: Some(self.redact(&result.code)),
                message: self.redact(result.message.as_deref().unwrap_or_default()),
            });
        }
        if !status.is_success() {
            return Err(TokenError::Rejected {
                status: status.as_u16(),
                code: None,
                message: self.redact(lossy_prefix(&bytes).trim()),
            });
        }
        Err(TokenError::InvalidResponse(
            "HTTP 200 without `access_token` and without a result code".into(),
        ))
    }

    /// Текст Huawei для ошибки: без учётных данных клиента, без секретов по виду, обрезан.
    fn redact(&self, text: &str) -> String {
        let mut text = text.to_owned();
        for secret in [
            self.credentials.client_secret().expose(),
            self.credentials.client_id().expose(),
        ] {
            if text.contains(secret) {
                text = text.replace(secret, MASK);
            }
        }
        truncate(&scrub_secrets(&text), MAX_ERROR_MESSAGE)
    }
}

impl TokenSource for ApiClientTokens {
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

/// Заранее известный токен без обновления (тесты).
#[derive(Debug, Clone)]
pub struct StaticToken(AccessToken);

impl StaticToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(AccessToken::new(token))
    }
}

impl TokenSource for StaticToken {
    fn token(&self) -> BoxFuture<'_, Result<AccessToken, TokenError>> {
        let token = self.0.clone();
        Box::pin(async move { Ok(token) })
    }

    fn invalidate<'a>(&'a self, _rejected: &'a AccessToken) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }

    fn known_tokens(&self) -> Vec<AccessToken> {
        vec![self.0.clone()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_and_errors_hide_values() {
        let value = ["tok", "en-", "value-", "0123456789"].concat();
        let token = AccessToken::new(value.clone());
        assert_eq!(format!("{token:?}"), "AccessToken(***)");
        assert_eq!(token.to_string(), "***");
        assert_eq!(token.expose(), value);
        assert!(!format!("{:?}", StaticToken::new(value.clone())).contains(&value));
    }

    #[test]
    fn rejection_text_names_code_and_hint() {
        let err = TokenError::Rejected {
            status: 200,
            code: Some("205524993".into()),
            message: "client auth failed".into(),
        };
        let text = err.to_string();
        assert!(text.contains("HTTP 200, code 205524993"), "{text}");
        assert!(text.contains("client auth failed"), "{text}");
        assert!(text.contains("Connect API"), "{text}");
    }
}
