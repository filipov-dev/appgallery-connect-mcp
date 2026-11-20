//! Вход от имени пользователя: режим OAuth-клиента Connect API.
//!
//! В этом режиме запросы идут от имени пользователя HUAWEI ID с правами его роли в команде,
//! а не от API-клиента команды. Так работают в чужом кабинете по приглашению, где владелец
//! не создаёт API-клиент. По справочнику (Connect API > Getting Started > OAuth Client Mode)
//! режим доступен «платформенным разработчикам»: области Connect API выдаются по заявке на
//! `agconnect@huawei.com` и проходят проверку Huawei, а сам вход — Account Kit (HUAWEI ID).
//!
//! OAuth-клиент — приложение с HUAWEI ID (Account Kit): его client ID и client secret
//! (App information > OAuth 2.0 client ID). Вход — authorization code flow с PKCE (S256):
//! браузер открывает [`AUTHORIZE_URL`] с `access_type=offline`, HUAWEI ID возвращает код на
//! адрес возврата, код меняется на access и refresh token в `POST` [`TOKEN_URL`] (форма,
//! ответ `access_token`, `expires_in` = 3600, `refresh_token`, `scope`). Access token живёт
//! час, refresh token — 180 дней; дальше access token выпускается по refresh token
//! ([`UserTokens`]). Отзыв — `POST` [`REVOKE_URL`] с `token=<refresh token>`: отзывается
//! пара токенов. Ошибка сервера входа — HTTP 400 с `error`, `sub_error` и
//! `error_description` (числа и текст).
//!
//! Запрос Connect API в этом режиме несёт заголовки `teamId` (команда пользователя) и
//! `oauth2Token` (access token как есть, без `Bearer`) вместо `client_id` и
//! `Authorization` ([`crate::executor`]). Команды пользователя отдаёт `GET
//! /api/ups/user-permission-service/v1/user-team-list` с одним `oauth2Token`
//! ([`list_teams`]).
//!
//! Секреты — client secret и refresh token — хранятся только в системном хранилище
//! ([`crate::store`], запись `<профиль>/oauth`) и в памяти в обёртках, которые не
//! раскрываются в `Debug` и затираются при освобождении. Адреса HUAWEI ID фиксированы;
//! подмена ([`OAuthEndpoints::with_token_url`] и соседние) принимается только для
//! loopback-адресов (тесты с `wiremock`).

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::credentials::{ClientId, ClientSecret, CredentialsError};
use crate::executor::{ApiClient, ApiError};
use crate::http::{
    CONNECT_TIMEOUT, USER_AGENT, describe_client_error, describe_reqwest_error, is_loopback,
    read_limited, truncate,
};
use crate::region::Region;
use crate::scrub::{MASK, scrub_secrets};
use crate::token::{AccessToken, BoxFuture, TokenCache, TokenError, TokenSource, lock};

/// Страница входа и согласия HUAWEI ID.
pub const AUTHORIZE_URL: &str = "https://oauth-login.cloud.huawei.com/oauth2/v3/authorize";

/// Обмен кода и обновление access token.
pub const TOKEN_URL: &str = "https://oauth-login.cloud.huawei.com/oauth2/v3/token";

/// Отзыв токенов (`token=<access или refresh token>`): отзывается вся пара.
pub const REVOKE_URL: &str = "https://oauth-login.cloud.huawei.com/oauth2/v3/revoke";

/// Область OpenID: по справочнику обязательна в каждом запросе входа.
pub const OPENID_SCOPE: &str = "openid";

/// Общий префикс областей Connect API.
pub const AGC_SCOPE_PREFIX: &str = "https://www.huawei.com/auth/agc/";

/// Область Connect API: короткое имя (суффикс адреса), адрес и группы API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgcScope {
    pub name: &'static str,
    pub url: &'static str,
    pub apis: &'static str,
}

/// Области Connect API из справочника (Getting Started > OAuth Client Mode > Applying for
/// the API scope). Все они «чувствительные»: OAuth-клиент получает их только после заявки
/// и проверки Huawei.
pub const AGC_SCOPES: &[AgcScope] = &[
    AgcScope {
        name: "publish",
        url: "https://www.huawei.com/auth/agc/publish",
        apis: "Publishing API, Upload Management API",
    },
    AgcScope {
        name: "develop",
        url: "https://www.huawei.com/auth/agc/develop",
        apis: "Provisioning API",
    },
    AgcScope {
        name: "report/read",
        url: "https://www.huawei.com/auth/agc/report/read",
        apis: "Reports API",
    },
    AgcScope {
        name: "project",
        url: "https://www.huawei.com/auth/agc/project",
        apis: "Project Management API (also the team list)",
    },
    AgcScope {
        name: "product",
        url: "https://www.huawei.com/auth/agc/product",
        apis: "PMS API (changes)",
    },
    AgcScope {
        name: "product/read",
        url: "https://www.huawei.com/auth/agc/product/read",
        apis: "PMS API (reading)",
    },
];

/// Области Connect API, которые вход просит без `--oauth-scope`: публикация и загрузка
/// (основная работа) и Project Management API (список команд при входе).
pub const DEFAULT_AGC_SCOPES: &[&str] = &["publish", "project"];

/// Срок жизни access token пользователя: справочник называет час, больше не принимаем.
pub const USER_TOKEN_LIFETIME: Duration = Duration::from_secs(3600);

/// Операция снимка со списком команд пользователя.
pub const TEAM_LIST_OPERATION: &str = "project.team.list";

/// Предел длины refresh token и идентификатора команды.
const MAX_REFRESH_TOKEN_LEN: usize = 4096;
const MAX_TEAM_ID_LEN: usize = 64;

/// Предел записи хранилища со входом пользователя.
pub const MAX_STORED_OAUTH_BYTES: usize = 8192;

/// Таймаут запросов к серверу входа.
const OAUTH_TIMEOUT: Duration = Duration::from_secs(30);

/// Предел ответа сервера входа.
const OAUTH_RESPONSE_LIMIT: usize = 64 * 1024;

/// Предел текста Huawei в ошибке.
const MAX_ERROR_TEXT: usize = 300;

/// `type` записи хранилища со входом пользователя.
const STORED_TYPE: &str = "oauth";

/// Адрес области по короткому имени (`publish`, `report/read`) или полному адресу.
pub fn scope_url(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value == OPENID_SCOPE {
        return Ok(value.to_owned());
    }
    if let Some(scope) = AGC_SCOPES
        .iter()
        .find(|scope| scope.name == value || scope.url == value)
    {
        return Ok(scope.url.to_owned());
    }
    let valid = value.strip_prefix(AGC_SCOPE_PREFIX).is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= 100
            && rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-'))
    });
    if valid {
        return Ok(value.to_owned());
    }
    let names: Vec<&str> = AGC_SCOPES.iter().map(|scope| scope.name).collect();
    Err(format!(
        "unknown Connect API scope `{value}`: use one of {} or a full scope URL that starts \
         with {AGC_SCOPE_PREFIX}",
        names.join(", ")
    ))
}

/// Области входа: `openid`, затем запрошенные (или [`DEFAULT_AGC_SCOPES`]), без повторов.
pub fn requested_scopes(extra: &[String]) -> Result<Vec<String>, String> {
    let mut scopes = vec![OPENID_SCOPE.to_owned()];
    let defaults: Vec<String> = DEFAULT_AGC_SCOPES.iter().map(|s| (*s).to_owned()).collect();
    let source = if extra.is_empty() { &defaults } else { extra };
    for value in source {
        let url = scope_url(value)?;
        if !scopes.contains(&url) {
            scopes.push(url);
        }
    }
    Ok(scopes)
}

/// Область без общего префикса — для вывода.
pub fn short_scope(scope: &str) -> &str {
    scope.strip_prefix(AGC_SCOPE_PREFIX).unwrap_or(scope)
}

/// Проверяет идентификатор команды (`teamId`): 1–64 символа `A-Za-z0-9_-`. Значение
/// уходит в заголовок, в файл профилей и в подсказки.
pub fn validate_team_id(value: &str) -> Result<(), CredentialsError> {
    if value.is_empty() {
        return Err(CredentialsError::Empty("team ID"));
    }
    if value.len() > MAX_TEAM_ID_LEN {
        return Err(CredentialsError::TooLong {
            field: "team ID",
            limit: MAX_TEAM_ID_LEN,
        });
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err(CredentialsError::Malformed(
            "the team ID may contain only A-Z, a-z, 0-9, `-` and `_`".into(),
        ));
    }
    Ok(())
}

/// OAuth-клиент: client ID и, если есть, client secret приложения с HUAWEI ID. `Debug`
/// скрывает оба.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
}

impl fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .field("has_secret", &self.client_secret.is_some())
            .finish()
    }
}

impl OAuthClient {
    pub fn new(client_id: ClientId, client_secret: Option<ClientSecret>) -> Self {
        Self {
            client_id,
            client_secret,
        }
    }

    pub fn client_id(&self) -> &ClientId {
        &self.client_id
    }

    pub fn client_secret(&self) -> Option<&ClientSecret> {
        self.client_secret.as_ref()
    }
}

/// Refresh token. `Debug` и `Display` скрыты, память затирается.
#[derive(Clone, PartialEq, Eq)]
struct RefreshToken(Zeroizing<String>);

impl RefreshToken {
    fn new(value: &str) -> Result<Self, CredentialsError> {
        if value.is_empty() {
            return Err(CredentialsError::Empty("refresh token"));
        }
        if value.len() > MAX_REFRESH_TOKEN_LEN {
            return Err(CredentialsError::TooLong {
                field: "refresh token",
                limit: MAX_REFRESH_TOKEN_LEN,
            });
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_graphic() && c != '"' && c != '\\')
        {
            return Err(CredentialsError::Malformed(
                "the refresh token has unexpected characters".into(),
            ));
        }
        Ok(Self(Zeroizing::new(value.to_owned())))
    }
}

/// Вход пользователя: OAuth-клиент, refresh token и выданные области. Одна запись
/// хранилища; `Debug` показывает только области.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthCredentials {
    client: OAuthClient,
    refresh_token: RefreshToken,
    scopes: Vec<String>,
}

impl fmt::Debug for OAuthCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthCredentials")
            .field("client", &self.client)
            .field("scopes", &self.scopes)
            .finish_non_exhaustive()
    }
}

/// Запись хранилища (JSON).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    #[serde(rename = "type")]
    kind: String,
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    refresh_token: String,
    #[serde(default)]
    scopes: Vec<String>,
}

impl Drop for Stored {
    fn drop(&mut self) {
        if let Some(secret) = &mut self.client_secret {
            secret.zeroize();
        }
        self.refresh_token.zeroize();
    }
}

impl OAuthCredentials {
    pub fn new(
        client: OAuthClient,
        refresh_token: &str,
        scopes: Vec<String>,
    ) -> Result<Self, CredentialsError> {
        let refresh_token = RefreshToken::new(refresh_token)?;
        if !scopes
            .iter()
            .all(|scope| !scope.is_empty() && scope.len() <= 200 && scope.is_ascii())
        {
            return Err(CredentialsError::Malformed(
                "the list of granted scopes is not valid".into(),
            ));
        }
        Ok(Self {
            client,
            refresh_token,
            scopes,
        })
    }

    pub fn client(&self) -> &OAuthClient {
        &self.client
    }

    /// Области, которые выдал HUAWEI ID при входе (поле `scope` ответа), или запрошенные,
    /// если ответ их не назвал.
    pub fn scopes(&self) -> &[String] {
        &self.scopes
    }

    /// Области из `requested`, которых нет среди выданных.
    pub fn missing_scopes<'a>(&self, requested: &'a [String]) -> Vec<&'a str> {
        requested
            .iter()
            .filter(|scope| !self.scopes.contains(scope))
            .map(String::as_str)
            .collect()
    }

    /// Значение refresh token. Только для запросов к серверу входа и вырезания из текстов.
    pub fn expose_refresh_token(&self) -> &str {
        &self.refresh_token.0
    }

    /// Те же клиент и области с новым refresh token (сервер входа выдал новый).
    fn with_refresh_token(&self, refresh_token: RefreshToken) -> Self {
        Self {
            client: self.client.clone(),
            refresh_token,
            scopes: self.scopes.clone(),
        }
    }

    /// JSON записи хранилища. Значение — секрет: только в системное хранилище.
    pub fn to_stored_json(&self) -> Zeroizing<String> {
        let stored = Stored {
            kind: STORED_TYPE.to_owned(),
            client_id: self.client.client_id.expose().to_owned(),
            client_secret: self
                .client
                .client_secret
                .as_ref()
                .map(|secret| secret.expose().to_owned()),
            refresh_token: self.refresh_token.0.as_str().to_owned(),
            scopes: self.scopes.clone(),
        };
        // Сериализация структуры из строк не может завершиться ошибкой.
        Zeroizing::new(serde_json::to_string(&stored).unwrap_or_default())
    }

    /// Разбирает запись хранилища. Ошибка не содержит фрагментов текста.
    pub fn from_stored_json(text: &str) -> Result<Self, CredentialsError> {
        if text.len() > MAX_STORED_OAUTH_BYTES {
            return Err(CredentialsError::Malformed(format!(
                "the stored value is larger than {MAX_STORED_OAUTH_BYTES} bytes"
            )));
        }
        let mut stored: Stored = serde_json::from_str(text.trim()).map_err(|err| {
            CredentialsError::Malformed(format!(
                "the stored value is not a user sign-in (line {}, column {})",
                err.line(),
                err.column()
            ))
        })?;
        if stored.kind != STORED_TYPE {
            return Err(CredentialsError::Malformed(
                "the stored value is not a user sign-in".into(),
            ));
        }
        let secret = stored.client_secret.take().map(ClientSecret::new);
        let client_id = ClientId::new(std::mem::take(&mut stored.client_id))?;
        let client = OAuthClient::new(client_id, secret.transpose()?);
        Self::new(
            client,
            &Zeroizing::new(std::mem::take(&mut stored.refresh_token)),
            std::mem::take(&mut stored.scopes),
        )
    }
}

/// Сохраняет новый refresh token, если сервер входа выдал его при обновлении (справочник
/// этого не описывает, но и не исключает). Ошибка — текст для предупреждения в лог.
pub type RefreshTokenSink = Arc<dyn Fn(&OAuthCredentials) -> Result<(), String> + Send + Sync>;

/// Адреса HUAWEI ID. В тестах подменяются на loopback-адрес `wiremock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthEndpoints {
    authorize: String,
    token: String,
    revoke: String,
}

fn loopback_override(url: &str) -> Result<String, TokenError> {
    let parsed = url::Url::parse(url)
        .map_err(|err| TokenError::Config(format!("sign-in URL override: {err}")))?;
    if !(matches!(parsed.scheme(), "http" | "https") && is_loopback(&parsed)) {
        return Err(TokenError::Config(
            "only a loopback URL can replace a HUAWEI ID endpoint (tests)".into(),
        ));
    }
    Ok(parsed.into())
}

impl Default for OAuthEndpoints {
    fn default() -> Self {
        Self::huawei()
    }
}

impl OAuthEndpoints {
    /// Настоящие адреса HUAWEI ID.
    pub fn huawei() -> Self {
        Self {
            authorize: AUTHORIZE_URL.to_owned(),
            token: TOKEN_URL.to_owned(),
            revoke: REVOKE_URL.to_owned(),
        }
    }

    /// Другой адрес страницы входа (тесты): только loopback.
    pub fn with_authorize_url(mut self, url: &str) -> Result<Self, TokenError> {
        self.authorize = loopback_override(url)?;
        Ok(self)
    }

    /// Другой адрес обмена и обновления (тесты): только loopback.
    pub fn with_token_url(mut self, url: &str) -> Result<Self, TokenError> {
        self.token = loopback_override(url)?;
        Ok(self)
    }

    /// Другой адрес отзыва (тесты): только loopback.
    pub fn with_revoke_url(mut self, url: &str) -> Result<Self, TokenError> {
        self.revoke = loopback_override(url)?;
        Ok(self)
    }

    pub fn authorize_url(&self) -> &str {
        &self.authorize
    }
}

/// Настройки входа пользователя для исполнителя ([`crate::ClientConfig::oauth`]).
#[derive(Clone)]
pub struct OAuthSession {
    pub credentials: OAuthCredentials,
    /// Команда (`teamId`), от имени которой идут запросы. `None` — только для списка команд
    /// при входе: остальные операции без команды не имеют смысла.
    pub team_id: Option<String>,
    pub endpoints: OAuthEndpoints,
    /// Куда сохранить новый refresh token, если сервер входа его выдаст.
    pub sink: Option<RefreshTokenSink>,
}

impl fmt::Debug for OAuthSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthSession")
            .field("credentials", &self.credentials)
            .field("team_id", &self.team_id)
            .field("endpoints", &self.endpoints)
            .field("sink", &self.sink.is_some())
            .finish()
    }
}

impl OAuthSession {
    /// Вход с командой и настоящими адресами HUAWEI ID.
    pub fn new(credentials: OAuthCredentials, team_id: Option<String>) -> Self {
        Self {
            credentials,
            team_id,
            endpoints: OAuthEndpoints::huawei(),
            sink: None,
        }
    }
}

/// Случайные байты.
fn random_bytes(len: usize) -> Result<Zeroizing<Vec<u8>>, TokenError> {
    let mut buf = Zeroizing::new(vec![0u8; len]);
    getrandom::fill(&mut buf)
        .map_err(|err| TokenError::Config(format!("no system random number generator: {err}")))?;
    Ok(buf)
}

/// Параметр `state`: защита адреса возврата от чужих перенаправлений. Только цифры и
/// латинские буквы: справочник требует `^[0-9a-zA-Z:/\.]+$` до 100 символов.
pub fn random_state() -> Result<String, TokenError> {
    let bytes = random_bytes(24)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// PKCE (RFC 7636, метод S256): `code_verifier` остаётся в процессе, в браузер уходит только
/// `code_challenge`.
pub struct Pkce {
    verifier: Zeroizing<String>,
    challenge: String,
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Pkce(***)")
    }
}

impl Pkce {
    pub fn generate() -> Result<Self, TokenError> {
        // 32 байта — 43 символа base64url: минимум, который допускает RFC 7636.
        let verifier = Zeroizing::new(URL_SAFE_NO_PAD.encode(random_bytes(32)?.as_slice()));
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
        })
    }

    pub fn challenge(&self) -> &str {
        &self.challenge
    }

    /// `code_verifier`: только для обмена кода и вырезания из текстов.
    pub fn expose_verifier(&self) -> &str {
        &self.verifier
    }
}

/// Адрес страницы входа: код авторизации с PKCE, `access_type=offline` (иначе refresh
/// token не выдаётся) и `state`.
pub fn authorization_url(
    client: &OAuthClient,
    endpoints: &OAuthEndpoints,
    redirect_uri: &str,
    scopes: &[String],
    pkce: &Pkce,
    state: &str,
) -> Result<url::Url, TokenError> {
    let mut url = url::Url::parse(&endpoints.authorize)
        .map_err(|err| TokenError::Config(format!("sign-in URL: {err}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("access_type", "offline")
        .append_pair("client_id", client.client_id().expose())
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("scope", &scopes.join(" "))
        .append_pair("code_challenge", pkce.challenge())
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state);
    Ok(url)
}

/// HTTP-клиент сервера входа: без редиректов (секреты не уходят на другой адрес).
fn oauth_http_client() -> Result<reqwest::Client, TokenError> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(OAUTH_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| TokenError::Config(describe_client_error(&err)))
}

const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

/// Тело формы `application/x-www-form-urlencoded`. Содержит секреты: затирается.
fn encode_form(form: &[(&str, &str)]) -> Zeroizing<String> {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in form {
        serializer.append_pair(name, value);
    }
    Zeroizing::new(serializer.finish())
}

/// Ответ сервера входа с токенами.
struct Grant {
    token: AccessToken,
    expires_in: Duration,
    refresh_token: Option<Zeroizing<String>>,
    scope: Option<String>,
}

/// Отказ сервера входа: коды Huawei (`error`, `sub_error`) и текст без секретов.
struct Rejection {
    status: u16,
    error: Option<String>,
    sub_error: Option<String>,
    description: String,
}

/// Код ошибки Huawei текстом: число или короткий идентификатор.
fn error_code(value: Option<&Value>) -> Option<String> {
    let text = match value? {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.trim().to_owned(),
        _ => return None,
    };
    (!text.is_empty()
        && text.len() <= 64
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
    .then_some(text)
}

/// Вырезает `secrets` точным совпадением и секреты по виду.
fn scrub_with(text: &str, secrets: &[&str]) -> String {
    let mut text = text.to_owned();
    for secret in secrets.iter().filter(|secret| secret.len() >= 4) {
        if text.contains(secret) {
            text = text.replace(secret, MASK);
        }
    }
    truncate(&scrub_secrets(&text), MAX_ERROR_TEXT)
}

/// `POST` формы на сервер входа. Значения `secrets` (код, verifier, refresh token, client
/// secret) вырезаются из текстов ошибок.
async fn post_form(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
    secrets: &[&str],
) -> Result<Result<Grant, Rejection>, TokenError> {
    let mut response = http
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, FORM_CONTENT_TYPE)
        .body(encode_form(form).to_string())
        .send()
        .await
        .map_err(|err| TokenError::Transport(describe_reqwest_error(&err.without_url())))?;
    let status = response.status();
    let (bytes, truncated) = read_limited(&mut response, OAUTH_RESPONSE_LIMIT)
        .await
        .map_err(|err| TokenError::Transport(describe_reqwest_error(&err.without_url())))?;
    let bytes = Zeroizing::new(bytes);
    let parsed = if truncated {
        None
    } else {
        serde_json::from_slice::<Value>(&bytes).ok()
    };
    let token = parsed
        .as_ref()
        .and_then(|value| value.get("access_token"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty());
    if !status.is_success() || token.is_none() {
        let object = parsed.as_ref().and_then(Value::as_object);
        let description = object
            .and_then(|object| object.get("error_description"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if object.is_some() {
                    String::new()
                } else {
                    String::from_utf8_lossy(&bytes).trim().to_owned()
                }
            });
        if status.is_success() && object.is_some_and(|o| !o.contains_key("error")) {
            return Err(TokenError::InvalidResponse(
                "the HUAWEI ID token endpoint returned no access_token".into(),
            ));
        }
        return Ok(Err(Rejection {
            status: status.as_u16(),
            error: error_code(object.and_then(|o| o.get("error"))),
            sub_error: error_code(object.and_then(|o| o.get("sub_error"))),
            description: scrub_with(description.trim(), secrets),
        }));
    }
    let Some(value) = parsed.as_ref() else {
        return Err(TokenError::InvalidResponse(
            "not a JSON token response".into(),
        ));
    };
    let token = token.unwrap_or_default();
    // Токен уходит в заголовок `oauth2Token`: только видимые ASCII-символы без пробелов.
    if !token.chars().all(|c| c.is_ascii_graphic()) {
        return Err(TokenError::InvalidResponse(
            "the access token contains characters that cannot be sent in a header".into(),
        ));
    }
    if let Some(kind) = value.get("token_type").and_then(Value::as_str)
        && !kind.eq_ignore_ascii_case("bearer")
    {
        return Err(TokenError::InvalidResponse(
            "the token type is not Bearer".into(),
        ));
    }
    let expires_in = match value.get("expires_in") {
        Some(Value::Number(number)) => number.as_u64(),
        Some(Value::String(text)) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
    .filter(|seconds| *seconds > 0)
    .map_or(USER_TOKEN_LIFETIME, Duration::from_secs)
    .min(USER_TOKEN_LIFETIME);
    Ok(Ok(Grant {
        token: AccessToken::new(token),
        expires_in,
        refresh_token: value
            .get("refresh_token")
            .and_then(Value::as_str)
            .map(|token| Zeroizing::new(token.trim().to_owned()))
            .filter(|token| !token.is_empty()),
        scope: value
            .get("scope")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }))
}

/// Этап, на котором отказал сервер входа: от него зависит подсказка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Code,
    Refresh,
    Revoke,
}

/// Понятная ошибка из отказа сервера входа: коды Huawei и подсказка по `sub_error`
/// (Account Kit > REST > Error Codes).
fn describe_rejection(rejection: Rejection, stage: Stage) -> TokenError {
    let during = match stage {
        Stage::Code => "the authorization code",
        Stage::Refresh => "the saved sign-in",
        Stage::Revoke => "revoking the sign-in",
    };
    let mut text = format!("HUAWEI ID rejected {during} (HTTP {}", rejection.status);
    if let Some(error) = &rejection.error {
        text.push_str(", error ");
        text.push_str(error);
    }
    if let Some(sub) = &rejection.sub_error {
        text.push_str(", sub_error ");
        text.push_str(sub);
    }
    text.push(')');
    if !rejection.description.is_empty() {
        text.push_str(": ");
        text.push_str(&rejection.description);
    }
    let relogin = "Sign in again with `appgallery-connect-mcp auth login --oauth --profile \
                   <profile>`.";
    let hint = match rejection.sub_error.as_deref() {
        Some("11205" | "31204" | "31202" | "31218" | "20192") => format!(
            " The refresh token has expired or was revoked (it lives 180 days and is revoked \
             when the password changes or the access is removed in HUAWEI ID). {relogin}"
        ),
        Some("12304" | "20172") => " The client secret of the OAuth client is wrong: copy it \
             again from AppGallery Connect (App information > OAuth 2.0 client ID)."
            .to_owned(),
        Some("12303" | "20002" | "20154") => " The client ID is wrong or the OAuth client was \
             deleted: copy the client ID of the app from AppGallery Connect (App information > \
             OAuth 2.0 client ID)."
            .to_owned(),
        Some("20022" | "20023" | "20024" | "22023") => " The redirect URI is not the one set \
             for the OAuth client: set http://127.0.0.1:<port>/ as the redirect URL of the app \
             (AppGallery Connect > the project > the app > Account Kit > redirect_uri) and pass \
             the same address with --redirect-uri."
            .to_owned(),
        Some("20153" | "20155" | "20156" | "20152") => {
            " The authorization code is invalid, expired (5 minutes) or already used. Sign in \
             again."
                .to_owned()
        }
        _ if stage == Stage::Refresh => format!(" {relogin}"),
        _ => String::new(),
    };
    text.push_str(&hint);
    TokenError::SignIn(text)
}

/// Результат обмена кода: запись для хранилища и первый access token.
#[derive(Debug)]
pub struct UserGrant {
    pub credentials: OAuthCredentials,
    pub token: AccessToken,
    pub expires_in: Duration,
}

/// Меняет код авторизации на токены. Области записи — выданные HUAWEI ID (поле `scope`
/// ответа), а без него — запрошенные.
pub async fn exchange_code(
    client: &OAuthClient,
    endpoints: &OAuthEndpoints,
    code: &str,
    pkce: &Pkce,
    redirect_uri: &str,
    requested_scopes: &[String],
) -> Result<UserGrant, TokenError> {
    let http = oauth_http_client()?;
    let secret = client.client_secret().map(ClientSecret::expose);
    let mut form = vec![
        ("grant_type", "authorization_code"),
        ("code", code),
        ("client_id", client.client_id().expose()),
        ("code_verifier", pkce.expose_verifier()),
        ("redirect_uri", redirect_uri),
    ];
    if let Some(secret) = secret {
        form.push(("client_secret", secret));
    }
    tracing::debug!("exchanging the HUAWEI ID authorization code");
    let secrets = [code, pkce.expose_verifier(), secret.unwrap_or_default()];
    let grant = post_form(&http, &endpoints.token, &form, &secrets)
        .await?
        .map_err(|rejection| describe_rejection(rejection, Stage::Code))?;
    let Some(refresh_token) = grant.refresh_token else {
        return Err(TokenError::InvalidResponse(
            "HUAWEI ID returned no refresh token (the request had access_type=offline). Remove \
             the access of the app in HUAWEI ID (Privacy center > Control account access) and \
             sign in again"
                .into(),
        ));
    };
    let scopes: Vec<String> = match grant.scope.as_deref() {
        Some(granted) if !granted.trim().is_empty() => {
            granted.split_whitespace().map(str::to_owned).collect()
        }
        _ => requested_scopes.to_vec(),
    };
    let credentials = OAuthCredentials::new(client.clone(), &refresh_token, scopes)
        .map_err(|err| TokenError::InvalidResponse(err.to_string()))?;
    Ok(UserGrant {
        credentials,
        token: grant.token,
        expires_in: grant.expires_in,
    })
}

/// Отзывает refresh token (и выданные по нему access token) на сервере входа.
pub async fn revoke(
    credentials: &OAuthCredentials,
    endpoints: &OAuthEndpoints,
) -> Result<(), TokenError> {
    let http = oauth_http_client()?;
    let refresh = credentials.expose_refresh_token();
    let mut response = http
        .post(&endpoints.revoke)
        .header(reqwest::header::ACCEPT, "application/json")
        .header(reqwest::header::CONTENT_TYPE, FORM_CONTENT_TYPE)
        .body(encode_form(&[("token", refresh)]).to_string())
        .send()
        .await
        .map_err(|err| TokenError::Transport(describe_reqwest_error(&err.without_url())))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let (bytes, _) = read_limited(&mut response, OAUTH_RESPONSE_LIMIT)
        .await
        .map_err(|err| TokenError::Transport(describe_reqwest_error(&err.without_url())))?;
    let parsed = serde_json::from_slice::<Value>(&bytes).ok();
    let object = parsed.as_ref().and_then(Value::as_object);
    let secret = credentials
        .client()
        .client_secret()
        .map(ClientSecret::expose)
        .unwrap_or_default();
    let description = object
        .and_then(|o| o.get("error_description"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| String::from_utf8_lossy(&bytes).trim().to_owned());
    let rejection = Rejection {
        status: status.as_u16(),
        error: error_code(object.and_then(|o| o.get("error"))),
        sub_error: error_code(object.and_then(|o| o.get("sub_error"))),
        description: scrub_with(&description, &[refresh, secret]),
    };
    let TokenError::SignIn(text) = describe_rejection(rejection, Stage::Revoke) else {
        return Err(TokenError::SignIn(
            "HUAWEI ID did not revoke the sign-in".into(),
        ));
    };
    Err(TokenError::SignIn(format!(
        "{text}. Remove the access manually in HUAWEI ID (Privacy center > Control account \
         access) if it is no longer needed"
    )))
}

/// Access token пользователя по refresh token: кэш в памяти ([`TokenCache`]), обновление
/// заранее, одно обновление на параллельные запросы.
pub struct UserTokens {
    http: reqwest::Client,
    token_url: String,
    credentials: Mutex<OAuthCredentials>,
    /// Прежние refresh token (после замены): вырезаются из текстов, как и текущий.
    old_refresh_tokens: Mutex<Vec<AccessToken>>,
    sink: Option<RefreshTokenSink>,
    cache: TokenCache,
}

impl fmt::Debug for UserTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserTokens")
            .field("token_url", &self.token_url)
            .field("credentials", &*lock(&self.credentials))
            .finish_non_exhaustive()
    }
}

impl UserTokens {
    pub fn new(session: &OAuthSession) -> Result<Self, TokenError> {
        Ok(Self {
            http: oauth_http_client()?,
            token_url: session.endpoints.token.clone(),
            credentials: Mutex::new(session.credentials.clone()),
            old_refresh_tokens: Mutex::new(Vec::new()),
            sink: session.sink.clone(),
            cache: TokenCache::new(),
        })
    }

    /// Заменяет часы (тесты).
    pub fn with_clock(mut self, clock: Arc<dyn crate::token::Clock>) -> Self {
        self.cache.set_clock(clock);
        self
    }

    /// Текущий вход (с новым refresh token, если сервер входа его выдал).
    pub fn credentials(&self) -> OAuthCredentials {
        lock(&self.credentials).clone()
    }

    async fn refresh(&self) -> Result<(AccessToken, Duration), TokenError> {
        let credentials = self.credentials();
        let client = credentials.client();
        let refresh = credentials.expose_refresh_token();
        let secret = client.client_secret().map(ClientSecret::expose);
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client.client_id().expose()),
        ];
        if let Some(secret) = secret {
            form.push(("client_secret", secret));
        }
        tracing::debug!("refreshing the access token of the HUAWEI ID sign-in");
        let grant = post_form(
            &self.http,
            &self.token_url,
            &form,
            &[refresh, secret.unwrap_or_default()],
        )
        .await?
        .map_err(|rejection| describe_rejection(rejection, Stage::Refresh))?;
        if let Some(new) = grant.refresh_token.as_deref()
            && new != refresh
        {
            self.rotate(&credentials, new);
        }
        Ok((grant.token, grant.expires_in))
    }

    /// Сервер входа выдал новый refresh token: дальше используется он, прежний помнится для
    /// вырезания из текстов, новый сохраняется в хранилище через `sink`.
    fn rotate(&self, current: &OAuthCredentials, new: &str) {
        let Ok(token) = RefreshToken::new(new) else {
            tracing::warn!("HUAWEI ID returned a refresh token of an unexpected form; ignored");
            return;
        };
        let updated = current.with_refresh_token(token);
        lock(&self.old_refresh_tokens).push(AccessToken::new(current.expose_refresh_token()));
        *lock(&self.credentials) = updated.clone();
        match &self.sink {
            Some(sink) => {
                if let Err(err) = sink(&updated) {
                    tracing::warn!(
                        "HUAWEI ID issued a new refresh token, but it could not be saved in the \
                         system keychain ({err}); sign in again if the server cannot start next \
                         time"
                    );
                }
            }
            None => tracing::debug!("HUAWEI ID issued a new refresh token (kept in memory)"),
        }
    }
}

impl TokenSource for UserTokens {
    fn token(&self) -> BoxFuture<'_, Result<AccessToken, TokenError>> {
        Box::pin(self.cache.get(|| self.refresh()))
    }

    fn invalidate<'a>(&'a self, rejected: &'a AccessToken) -> BoxFuture<'a, ()> {
        Box::pin(self.cache.invalidate(rejected))
    }

    /// Access token, а также текущий и прежние refresh token и client secret: всё это
    /// вырезается из текстов ошибок и ответов.
    fn known_tokens(&self) -> Vec<AccessToken> {
        let mut known = self.cache.remembered();
        let credentials = self.credentials();
        known.push(AccessToken::new(credentials.expose_refresh_token()));
        if let Some(secret) = credentials.client().client_secret() {
            known.push(AccessToken::new(secret.expose()));
        }
        known.extend(lock(&self.old_refresh_tokens).iter().cloned());
        known
    }
}

/// Команда пользователя из списка команд (`project.team.list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Team {
    pub id: String,
    pub name: String,
    /// Сайт команды (`siteId`): регион профиля должен с ним совпадать.
    pub site: Option<Region>,
    /// Тип аккаунта (`userType`): 1 — частный разработчик, 2 — компания.
    pub user_type: Option<i64>,
}

/// Разбирает ответ списка команд. Команда без пригодного `id` пропускается.
pub fn parse_teams(body: &Value) -> Vec<Team> {
    let Some(teams) = body.get("teams").and_then(Value::as_array) else {
        return Vec::new();
    };
    teams
        .iter()
        .filter_map(|team| {
            let id = match team.get("id")? {
                Value::String(text) => text.trim().to_owned(),
                Value::Number(number) => number.to_string(),
                _ => return None,
            };
            validate_team_id(&id).ok()?;
            let name = team
                .get("name")
                .and_then(Value::as_str)
                .map(|name| name.chars().filter(|c| !c.is_control()).take(100).collect())
                .unwrap_or_default();
            Some(Team {
                id,
                name,
                site: team
                    .get("siteId")
                    .and_then(Value::as_i64)
                    .and_then(Region::from_site_id),
                user_type: team.get("userType").and_then(Value::as_i64),
            })
        })
        .collect()
}

/// Команды пользователя: `GET /api/ups/user-permission-service/v1/user-team-list` клиентом
/// в режиме OAuth-клиента (команда для этого запроса не нужна).
pub async fn list_teams(client: &ApiClient) -> Result<Vec<Team>, ApiError> {
    let op = crate::spec::operations()
        .iter()
        .find(|op| op.operation_id == TEAM_LIST_OPERATION)
        .ok_or_else(|| {
            ApiError::InvalidConfig("the team list operation is missing from the spec".into())
        })?;
    let response = client.call(op, &Value::Null).await?;
    Ok(response.body.json().map(parse_teams).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(secret: bool) -> OAuthClient {
        OAuthClient::new(
            ClientId::new(["1088", "0000", "42"].concat()).unwrap(),
            secret.then(|| ClientSecret::new(["Ab12", "Cd34"].concat().repeat(8)).unwrap()),
        )
    }

    fn refresh() -> String {
        ["Rt", "Value", "-", "0123456789"].concat()
    }

    #[test]
    fn endpoints_are_fixed() {
        let endpoints = OAuthEndpoints::huawei();
        assert_eq!(endpoints.authorize, AUTHORIZE_URL);
        assert_eq!(endpoints.token, TOKEN_URL);
        assert_eq!(endpoints.revoke, REVOKE_URL);
        assert!(
            endpoints
                .clone()
                .with_token_url("https://example.com/token")
                .is_err()
        );
        assert!(
            endpoints
                .with_revoke_url("http://127.0.0.1:1/revoke")
                .is_ok()
        );
    }

    #[test]
    fn scopes_accept_short_names_and_urls() {
        let scopes = requested_scopes(&[]).unwrap();
        assert_eq!(
            scopes,
            [
                "openid",
                "https://www.huawei.com/auth/agc/publish",
                "https://www.huawei.com/auth/agc/project"
            ]
        );
        let scopes = requested_scopes(&[
            "report/read".into(),
            "https://www.huawei.com/auth/agc/product/read".into(),
            "report/read".into(),
        ])
        .unwrap();
        assert_eq!(scopes.len(), 3);
        assert_eq!(short_scope(&scopes[1]), "report/read");
        assert!(requested_scopes(&["https://example.com/x".into()]).is_err());
        assert!(requested_scopes(&["everything".into()]).is_err());
    }

    #[test]
    fn credentials_round_trip_and_hide_values() {
        for with_secret in [true, false] {
            let credentials =
                OAuthCredentials::new(client(with_secret), &refresh(), vec!["openid".into()])
                    .unwrap();
            let json = credentials.to_stored_json();
            assert!(json.contains("\"type\":\"oauth\""), "{}", json.as_str());
            assert_eq!(
                OAuthCredentials::from_stored_json(&json).unwrap(),
                credentials
            );
            let debug = format!("{credentials:?}");
            assert!(!debug.contains(&refresh()), "{debug}");
            assert!(!debug.contains("Ab12Cd34"), "{debug}");
            assert!(!debug.contains("1088000042"), "{debug}");
        }
        // Запись API-клиента — не вход пользователя.
        let api = r#"{"client_id":"1","client_secret":"x"}"#;
        assert!(OAuthCredentials::from_stored_json(api).is_err());
        let err = OAuthCredentials::from_stored_json(&format!(
            "{{\"type\":\"oauth\",\"client_id\":\"1\",\"refresh_token\":\"{}",
            refresh()
        ))
        .unwrap_err()
        .to_string();
        assert!(!err.contains(&refresh()), "{err}");
        assert!(OAuthCredentials::new(client(false), "has space", vec![]).is_err());
    }

    #[test]
    fn authorization_url_uses_pkce_offline_access_and_alphanumeric_state() {
        let pkce = Pkce::generate().unwrap();
        assert_eq!(pkce.expose_verifier().len(), 43);
        assert_eq!(
            pkce.challenge(),
            URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.expose_verifier().as_bytes()))
        );
        let state = random_state().unwrap();
        assert_eq!(state.len(), 48);
        assert!(state.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_ne!(state, random_state().unwrap());
        let scopes = requested_scopes(&[]).unwrap();
        let url = authorization_url(
            &client(true),
            &OAuthEndpoints::huawei(),
            "http://127.0.0.1:5555/",
            &scopes,
            &pkce,
            &state,
        )
        .unwrap();
        assert!(url.as_str().starts_with(AUTHORIZE_URL), "{url}");
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs["response_type"], "code");
        assert_eq!(pairs["access_type"], "offline");
        assert_eq!(pairs["client_id"], "1088000042");
        assert_eq!(pairs["redirect_uri"], "http://127.0.0.1:5555/");
        assert_eq!(pairs["scope"], scopes.join(" "));
        assert_eq!(pairs["code_challenge_method"], "S256");
        assert_eq!(pairs["code_challenge"], pkce.challenge());
        assert_eq!(pairs["state"], state);
        assert!(!url.as_str().contains("Ab12Cd34"));
        assert!(!url.as_str().contains(pkce.expose_verifier()));
    }

    #[test]
    fn team_ids_and_teams() {
        validate_team_id("1234567890").unwrap();
        for bad in ["", "a b", "x/y", &"1".repeat(65)] {
            assert!(validate_team_id(bad).is_err(), "{bad}");
        }
        let body = serde_json::json!({
            "ret": {"code": 0, "msg": "success"},
            "teams": [
                {"id": "100", "name": "Team\u{7}One", "siteId": 8, "userType": 2},
                {"id": 200, "name": "Two", "siteId": 1},
                {"id": "bad id", "name": "Broken"},
                {"name": "No id"}
            ]
        });
        let teams = parse_teams(&body);
        assert_eq!(teams.len(), 2);
        assert_eq!(teams[0].id, "100");
        assert_eq!(teams[0].name, "TeamOne");
        assert_eq!(teams[0].site, Some(Region::Ru));
        assert_eq!(teams[1].id, "200");
        assert_eq!(teams[1].site, Some(Region::Cn));
        assert!(parse_teams(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn rejections_explain_the_sub_error_without_secrets() {
        let rejection = |sub: &str, description: &str| Rejection {
            status: 400,
            error: Some("1203".into()),
            sub_error: Some(sub.into()),
            description: scrub_with(description, &[&refresh()]),
        };
        let text = describe_rejection(
            rejection("31204", &format!("expired {}", refresh())),
            Stage::Refresh,
        )
        .to_string();
        assert!(text.contains("sub_error 31204"), "{text}");
        assert!(text.contains("auth login --oauth"), "{text}");
        assert!(!text.contains(&refresh()), "{text}");
        let text = describe_rejection(rejection("20023", "redirect"), Stage::Code).to_string();
        assert!(text.contains("--redirect-uri"), "{text}");
        let text = describe_rejection(rejection("12304", "secret"), Stage::Code).to_string();
        assert!(text.contains("client secret"), "{text}");
    }
}
