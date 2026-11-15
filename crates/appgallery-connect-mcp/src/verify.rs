//! Проверка учётных данных API-клиента: пробное получение access token через общий
//! исполнитель ([`ApiClient::authenticate`]), ничего больше не вызывая.
//!
//! Токен живёт в памяти только на время проверки; ни он, ни секрет не попадают в вывод:
//! тексты ошибок исполнителя уже без секрета и токена.

use anyhow::anyhow;
use appgallery_connect_api::{
    ApiClient, ApiClientCredentials, ApiError, ClientConfig, Region, RetryPolicy, TokenError,
};

/// Проверка через метод получения токена AppGallery Connect (или его подмену на loopback
/// в тестах).
#[derive(Debug, Clone, Default)]
pub struct Verifier {
    /// Подмена корня API (`wiremock`); в рабочей конфигурации — `None`.
    base_url: Option<String>,
}

impl Verifier {
    /// Проверка через настоящий AppGallery Connect на хосте региона профиля.
    pub fn new() -> Self {
        Self::default()
    }

    /// Проверка через локальный сервер (тесты). Клиент API принимает только loopback-адрес.
    #[cfg(test)]
    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            base_url: Some(base_url.to_owned()),
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
        config.retry = RetryPolicy {
            max_retries: 1,
            ..RetryPolicy::default()
        };
        let client = ApiClient::new(config).map_err(|err| anyhow!("{err}"))?;
        client
            .authenticate()
            .await
            .map_err(|err| describe_failure(&err, region))
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
