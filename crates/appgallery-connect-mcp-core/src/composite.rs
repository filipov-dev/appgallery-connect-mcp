//! Составные инструменты: несколько запросов к API за один вызов MCP-инструмента.
//!
//! Модуль задаёт контракт между составными инструментами и ядром сервера (как в
//! `play-console-mcp-core`):
//!
//! - каждый инструмент — отдельный модуль `composite/<имя>.rs` с функцией
//!   `tools() -> Vec<CompositeTool>`, [`all`] собирает их в один список;
//! - сервер регистрирует их так же, как инструменты операций: по уровню доступа
//!   ([`CompositeTool::access`]) и набору ([`CompositeTool::toolset`]); аргументы
//!   проверяются по схеме строго ([`crate::validate::check_arguments`]);
//! - ответ инструмента проходит ту же постобработку, что и ответы операций (вырезание
//!   секретов [`crate::redact`], пометка `note`, лимит размера). Инструмент сам не кладёт в
//!   ответ то, чего агенту видеть не нужно: адреса и коды загрузки, временные ссылки,
//!   содержимое файлов, секреты.
//!
//! Инструменты обращаются к API только через [`Context::api`] (скачивание по ссылке из
//! ответа — [`ApiClient::download_to_file`], только с хостов Huawei), к локальным файлам —
//! только через [`Context::files`] ([`crate::files::FilePolicy`]), к секретам, которые
//! нужно сохранить, не показывая агенту, — только через [`Context::vault`]. Пути операций
//! берутся из таблицы спецификации по имени инструмента или `operationId`, а не пишутся
//! строками.

pub mod app_overview;
pub mod certificate_request;
pub mod config_download;
pub mod image_sync;
pub mod listing;
pub mod listing_apply;
pub mod listing_export;
pub mod package_upload;
pub mod products;
pub mod products_apply;
pub mod products_export;
pub mod release;
pub mod release_manage;
pub mod release_status;
pub mod release_submit;
pub mod remote_config_set;
pub mod report_download;
pub mod reviews_summary;
pub mod search_apps;
pub mod upload;

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use appgallery_connect_api::{AccessLevel, ApiClient, KeyringVault};
use serde_json::Value;

use crate::error::ToolError;

/// Результат вызова составного инструмента: JSON для агента.
pub type ToolResult = Result<Value, ToolError>;

/// Будущее значение результата; инструменты асинхронные.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Описание составного инструмента.
#[derive(Clone, Copy)]
pub struct CompositeTool {
    /// Имя MCP-инструмента, не длиннее 40 символов, не совпадает с инструментами операций.
    pub name: &'static str,
    /// Slug набора инструментов (один из наборов спецификации).
    pub toolset: &'static str,
    /// Уровень доступа, задаётся явно.
    pub access: AccessLevel,
    /// Описание для агента (английский): что делает, когда использовать, что вернёт.
    pub description: &'static str,
    /// JSON Schema аргументов: `type: object`, `additionalProperties: false`.
    pub input_schema: fn() -> Value,
    /// Выполнение. Аргументы уже проверены сервером по `input_schema`.
    pub run: for<'a> fn(&'a Context, Value) -> ToolFuture<'a>,
}

impl std::fmt::Debug for CompositeTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompositeTool")
            .field("name", &self.name)
            .field("toolset", &self.toolset)
            .field("access", &self.access)
            .finish_non_exhaustive()
    }
}

/// Окружение вызова: клиент API, локальные файлы, хранилище секретов, данные профиля и
/// уровень доступа сервера.
pub struct Context {
    /// Клиент AppGallery Connect API (токен и `client_id` добавляет он сам).
    pub api: Arc<ApiClient>,
    /// Доступ к локальным файлам по правилам сервера ([`crate::files::FilePolicy`]).
    pub files: Arc<dyn LocalFiles>,
    /// Хранилище секретов, которые инструмент должен сохранить, не показывая агенту.
    pub vault: Arc<dyn SecretVault>,
    /// Профиль сервера: имя и настройки, нужные инструментам.
    pub profile: ProfileInfo,
    /// Уровень доступа сервера. Инструмент проверяет его сам, если отдельный режим вызова
    /// требует уровня выше, чем у самого инструмента.
    pub access: AccessLevel,
    /// HTTP-режим (`serve --http`): уровень доступа задаёт токен клиента, поэтому подсказки
    /// об уровне называют токен и оператора сервера, а не флаги
    /// ([`crate::registry::access_hint`]).
    pub http: bool,
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("api", &self.api)
            .field("profile", &self.profile)
            .field("access", &self.access)
            .field("http", &self.http)
            .field("http", &self.http)
            .finish_non_exhaustive()
    }
}

/// Правила локальных файлов сервера (рабочий каталог, временный каталог, `--allow-path`).
///
/// Реализация ядра ([`crate::files::FilePolicy`]) проверяет путь, разворачивает
/// символические ссылки и возвращает канонический путь; открывать файл нужно по нему.
/// Пути агента бывают относительными (от рабочего каталога сервера) и с `/` на Windows.
pub trait LocalFiles: Send + Sync {
    /// Существующий файл для чтения.
    fn resolve_read(&self, path: &str) -> Result<PathBuf, ToolError>;
    /// Новый файл для записи: родительский каталог существует, сам файл — нет
    /// (существующие файлы не перезаписываются; создавать его нужно с `create_new`, как
    /// [`crate::files::write_new_file`] и [`ApiClient::download_to_file`]).
    fn resolve_write(&self, path: &str) -> Result<PathBuf, ToolError>;
    /// Каталог для записи нескольких новых файлов (создаётся, если его нет).
    fn resolve_write_dir(&self, path: &str) -> Result<PathBuf, ToolError>;
    /// Существующий каталог для чтения. Файлы внутри открываются только после
    /// [`Self::resolve_read`]: символическая ссылка внутри каталога может вести наружу.
    fn resolve_read_dir(&self, path: &str) -> Result<PathBuf, ToolError>;
}

/// Хранилище секретов, которые получают или создают сами инструменты (например, секрет
/// приложения, который нужно сохранить для пользователя, не показывая агенту). Значения
/// наружу не отдаются: метода чтения нет, агент получает только имя записи.
pub trait SecretVault: Send + Sync {
    /// Сохранить значение под именем записи (`account`), заменив прежнее.
    fn store(&self, account: &str, value: &str) -> Result<(), ToolError>;
    /// Есть ли запись.
    fn contains(&self, account: &str) -> Result<bool, ToolError>;
}

/// Секреты инструментов в системном хранилище (отдельный сервис, записи профиля).
impl SecretVault for KeyringVault {
    fn store(&self, account: &str, value: &str) -> Result<(), ToolError> {
        KeyringVault::store(self, account, value).map_err(|err| {
            ToolError::new(format!(
                "cannot store the secret in the system keychain: {err}"
            ))
        })
    }

    fn contains(&self, account: &str) -> Result<bool, ToolError> {
        KeyringVault::contains(self, account)
            .map_err(|err| ToolError::new(format!("cannot read the system keychain: {err}")))
    }
}

/// Хранилища секретов нет: сохранить секрет нельзя (сервер без системного хранилища).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoVault;

impl SecretVault for NoVault {
    fn store(&self, _account: &str, _value: &str) -> Result<(), ToolError> {
        Err(ToolError::new(
            "secrets cannot be stored: this server has no secret storage",
        ))
    }

    fn contains(&self, _account: &str) -> Result<bool, ToolError> {
        Ok(false)
    }
}

/// Хранилища секретов нет, потому что сервер работает в HTTP-режиме (`serve --http`): он
/// удалённый и общий для клиентов, системного хранилища у него нет (часто это контейнер), а
/// хранить секреты клиентов он не должен. Сообщение объясняет именно это, без советов про
/// хранилище: у оператора сервера и у агента его здесь нет.
#[derive(Debug, Default, Clone, Copy)]
pub struct HttpNoVault;

impl SecretVault for HttpNoVault {
    fn store(&self, _account: &str, _value: &str) -> Result<(), ToolError> {
        Err(ToolError::new(
            "secrets cannot be stored in HTTP mode: this server runs remotely for several \
             clients and does not keep secrets for them, so this feature is not available here; \
             nothing was stored. Do not show the secret in the conversation",
        ))
    }

    fn contains(&self, _account: &str) -> Result<bool, ToolError> {
        Ok(false)
    }
}

/// Хранилище секретов в памяти процесса — для тестов (системный keychain в автотестах не
/// используется).
#[derive(Debug, Default)]
pub struct MemoryVault {
    items: Mutex<HashMap<String, String>>,
}

impl MemoryVault {
    /// Значение записи — только для проверок в тестах.
    pub fn value(&self, account: &str) -> Option<String> {
        self.items.lock().ok()?.get(account).cloned()
    }
}

impl SecretVault for MemoryVault {
    fn store(&self, account: &str, value: &str) -> Result<(), ToolError> {
        self.items
            .lock()
            .map_err(|_| ToolError::new("the in-memory secret storage is poisoned"))?
            .insert(account.to_owned(), value.to_owned());
        Ok(())
    }

    fn contains(&self, account: &str) -> Result<bool, ToolError> {
        Ok(self
            .items
            .lock()
            .map_err(|_| ToolError::new("the in-memory secret storage is poisoned"))?
            .contains_key(account))
    }
}

/// Данные профиля, нужные составным инструментам.
#[derive(Debug, Clone, Default)]
pub struct ProfileInfo {
    /// Имя профиля (один профиль — один API-клиент).
    pub name: String,
    /// `appId` приложений по умолчанию.
    pub apps: Vec<String>,
    /// Имена пакетов по умолчанию.
    pub packages: Vec<String>,
}

/// Все составные инструменты сервера. Новый модуль добавляет сюда свой `tools()`.
pub fn all() -> Vec<CompositeTool> {
    let mut tools = Vec::new();
    tools.extend(package_upload::tools());
    tools.extend(release_submit::tools());
    tools.extend(release_manage::tools());
    tools.extend(release_status::tools());
    tools.extend(app_overview::tools());
    tools.extend(report_download::tools());
    tools.extend(reviews_summary::tools());
    tools.extend(search_apps::tools());
    tools.extend(config_download::tools());
    tools.extend(certificate_request::tools());
    tools.extend(listing_export::tools());
    tools.extend(listing_apply::tools());
    tools.extend(image_sync::tools());
    tools.extend(products_export::tools());
    tools.extend(products_apply::tools());
    tools.extend(remote_config_set::tools());
    tools
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vaults_never_return_values() {
        let vault = MemoryVault::default();
        assert!(!vault.contains("app-secret").unwrap());
        vault.store("app-secret", "value-1").unwrap();
        vault.store("app-secret", "value-2").unwrap();
        assert!(vault.contains("app-secret").unwrap());
        assert_eq!(vault.value("app-secret").as_deref(), Some("value-2"));

        let none: Arc<dyn SecretVault> = Arc::new(NoVault);
        assert!(none.store("x", "y").is_err());
        assert!(!none.contains("x").unwrap());
    }
}
