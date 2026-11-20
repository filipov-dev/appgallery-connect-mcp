//! Проверка учётных данных: пробное получение access token через общий исполнитель
//! ([`ApiClient::authenticate`]), ничего больше не вызывая; для входа пользователя — ещё
//! список его команд.
//!
//! Токен живёт в памяти только на время проверки; ни он, ни секрет не попадают в вывод:
//! тексты ошибок исполнителя уже без секрета и токена.

use std::sync::Arc;

use anyhow::anyhow;
use appgallery_connect_api::user_oauth::{self, OAuthEndpoints, Team, UserTokens};
use appgallery_connect_api::{
    ApiClient, ApiClientCredentials, ApiError, AppCredentials, ClientConfig, OAuthCredentials,
    OAuthSession, Region, RetryPolicy, TokenError,
};

/// Проверка через AppGallery Connect и HUAWEI ID (или их подмену на loopback в тестах).
#[derive(Debug, Clone, Default)]
pub struct Verifier {
    /// Подмена корня API (`wiremock`); в рабочей конфигурации — `None`.
    base_url: Option<String>,
    /// Адреса HUAWEI ID: вход, обновление и отзыв токенов пользователя.
    endpoints: OAuthEndpoints,
}

impl Verifier {
    /// Проверка через настоящий AppGallery Connect на хосте региона профиля.
    pub fn new() -> Self {
        Self::default()
    }

    /// Проверка через локальный сервер (тесты): и API, и HUAWEI ID (`/oauth2/v3/...`).
    /// Клиент API принимает только loopback-адрес.
    #[cfg(test)]
    pub fn with_base_url(base_url: &str) -> Self {
        let endpoint = |name: &str| format!("{base_url}/oauth2/v3/{name}");
        let endpoints = OAuthEndpoints::huawei()
            .with_authorize_url(&endpoint("authorize"))
            .and_then(|e| e.with_token_url(&endpoint("token")))
            .and_then(|e| e.with_revoke_url(&endpoint("revoke")))
            .unwrap_or_default();
        Self {
            base_url: Some(base_url.to_owned()),
            endpoints,
        }
    }

    /// Адреса HUAWEI ID.
    pub fn endpoints(&self) -> &OAuthEndpoints {
        &self.endpoints
    }

    fn retry() -> RetryPolicy {
        RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        }
    }

    /// Получает access token для `credentials` в регионе `region`.
    pub async fn check(
        &self,
        region: Region,
        credentials: &ApiClientCredentials,
    ) -> anyhow::Result<()> {
        let mut config = ClientConfig::new(region, Some(credentials.clone()));
        config.base_url = self.base_url.clone();
        config.retry = Self::retry();
        let client = ApiClient::new(config).map_err(|err| anyhow!("{err}"))?;
        client
            .authenticate()
            .await
            .map_err(|err| describe_failure(&err, region))
    }

    /// Обновляет access token входа пользователя и читает список его команд. Возвращает
    /// команды и вход (с новым refresh token, если HUAWEI ID его выдал), либо ошибку и, если
    /// токен получен, — тоже вход: ошибка списка команд не означает, что вход не работает.
    pub async fn teams(
        &self,
        region: Region,
        credentials: &OAuthCredentials,
    ) -> Result<(Vec<Team>, OAuthCredentials), TeamsError> {
        let session = OAuthSession {
            endpoints: self.endpoints.clone(),
            ..OAuthSession::new(credentials.clone(), None)
        };
        let tokens = Arc::new(UserTokens::new(&session).map_err(|err| TeamsError::SignIn {
            error: anyhow!("{err}"),
        })?);
        let mut config = ClientConfig::with_oauth(region, session);
        config.base_url = self.base_url.clone();
        config.retry = Self::retry();
        let client = ApiClient::with_token_source(config, tokens.clone()).map_err(|err| {
            TeamsError::SignIn {
                error: anyhow!("{err}"),
            }
        })?;
        if let Err(err) = client.authenticate().await {
            return Err(TeamsError::SignIn {
                error: anyhow!("could not refresh the HUAWEI ID sign-in: {err}"),
            });
        }
        match user_oauth::list_teams(&client).await {
            Ok(teams) => Ok((teams, tokens.credentials())),
            Err(err) => Err(TeamsError::List {
                error: anyhow!(
                    "the sign-in works, but the team list (Project Management API, region {}) \
                     failed: {err}",
                    region.slug()
                ),
                credentials: tokens.credentials(),
            }),
        }
    }

    /// Проверка входа пользователя для `auth status --check` и `connect`: access token
    /// и команда `team_id` в списке команд пользователя. Возвращает и вход на момент после
    /// проверки: если HUAWEI ID выдал новый refresh token, вызывающий его сохраняет.
    pub async fn check_user(
        &self,
        region: Region,
        credentials: &OAuthCredentials,
        team_id: &str,
    ) -> (OAuthCredentials, anyhow::Result<()>) {
        match self.teams(region, credentials).await {
            Ok((teams, latest)) if teams.iter().any(|team| team.id == team_id) => (latest, Ok(())),
            Ok((teams, latest)) => (
                latest,
                Err(anyhow!(
                    "the signed-in user is not a member of team `{team_id}` (teams: {}); sign \
                     in again and choose the team",
                    describe_teams(&teams)
                )),
            ),
            Err(TeamsError::SignIn { error }) => (credentials.clone(), Err(error)),
            Err(TeamsError::List { error, credentials }) => (credentials, Err(error)),
        }
    }
}

/// Ошибка [`Verifier::teams`].
#[derive(Debug)]
pub enum TeamsError {
    /// Access token получить не удалось: вход не работает.
    SignIn { error: anyhow::Error },
    /// Вход работает, список команд не получен.
    List {
        error: anyhow::Error,
        credentials: OAuthCredentials,
    },
}

/// Команды для вывода: `id (name)` через запятую.
pub fn describe_teams(teams: &[Team]) -> String {
    if teams.is_empty() {
        return "none".to_owned();
    }
    teams
        .iter()
        .map(|team| {
            if team.name.is_empty() {
                team.id.clone()
            } else {
                format!("{} ({})", team.id, team.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl Verifier {
    /// Получает токен уровня приложения для секрета приложения `app` у Huawei OAuth (или
    /// его подмены на loopback в тестах). Регион здесь не важен: хост OAuth один.
    pub async fn check_app(&self, app: &AppCredentials) -> anyhow::Result<()> {
        let mut config = ClientConfig::new(Region::default(), None);
        config.base_url = self.base_url.clone();
        config.apps = vec![app.clone()];
        let client = ApiClient::new(config).map_err(|err| anyhow!("{err}"))?;
        client
            .authenticate_app(app.app_id())
            .await
            .map_err(|err| match err {
                ApiError::Token(TokenError::AppRejected { .. }) => anyhow!("{err}"),
                ApiError::Token(TokenError::Transport(_)) => {
                    anyhow!("could not reach Huawei OAuth (oauth-login.cloud.huawei.com): {err}")
                }
                _ => anyhow!("could not get an app-level access token: {err}"),
            })
    }
}

/// Понятная ошибка проверки. Отказ Huawei уже содержит HTTP-статус, код результата и
/// подсказку (см. [`TokenError::Rejected`]).
fn describe_failure(err: &ApiError, region: Region) -> anyhow::Error {
    match err {
        ApiError::Token(TokenError::Rejected { .. }) => anyhow!(
            "could not get an access token in region {} ({}): {err}",
            region.slug(),
            region.host()
        ),
        ApiError::Token(TokenError::Transport(_)) | ApiError::Transport(_) => anyhow!("{err}"),
        _ => anyhow!("could not get an access token: {err}"),
    }
}
