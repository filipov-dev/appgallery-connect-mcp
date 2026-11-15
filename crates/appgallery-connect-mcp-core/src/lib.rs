//! Ядро MCP-сервера AppGallery Connect: регистрация инструментов, уровни доступа, проверка
//! аргументов, постобработка ответов и инструкции для агентов. Не зависит от транспорта.
//!
//! - [`server`] — обработчик `rmcp` ([`AppGalleryServer`]) и инструкции для агента;
//! - [`registry`] — какие инструменты регистрируются при заданных [`ServerOptions`];
//! - [`validate`] — проверка аргументов по JSON Schema инструмента;
//! - [`redact`] — вырезание секретов из ответов (поля операций по таблице спецификации,
//!   поля по имени и значения по виду);
//! - [`files`] — политика локальных файлов ([`files::FilePolicy`]) и `save_to`;
//! - [`response`] — результат инструмента из ответа API;
//! - [`error`] — ошибки вызова с текстом для агента;
//! - [`toolsets`] — наборы инструментов (`--toolsets`), наборы по умолчанию и инструменты
//!   динамических наборов (`list_toolsets`, `enable_toolset`, `disable_toolset`);
//! - [`jq`] — фильтр ответа `jq` (jaq) и его выполнение в отдельном процессе;
//! - [`custom`] — пользовательские инструменты из `tools.toml`;
//! - [`composite`] — контракт составных инструментов: сервер регистрирует
//!   [`ServerOptions::composite_tools`] наравне с инструментами операций;
//! - [`search`] — `api_search`: поиск по всем операциям (со словарём русских слов);
//! - [`request`] — `api_request`: запрос к любому пути API с уровнем доступа по таблице
//!   операций.
//!
//! Транспорт (stdio) и CLI живут в крейте `appgallery-connect-mcp`, клиент API —
//! в `appgallery-connect-api`.

pub mod composite;
pub mod custom;
pub mod error;
pub mod files;
pub mod jq;
pub mod redact;
pub mod registry;
pub mod request;
pub mod response;
pub mod search;
pub mod server;
pub mod toolsets;
pub mod validate;

pub use appgallery_connect_api;
pub use composite::{CompositeTool, LocalFiles, MemoryVault, NoVault, SecretVault};
pub use custom::{CustomTool, CustomToolDef, CustomToolsFile};
pub use error::ToolError;
pub use files::FilePolicy;
pub use jq::{JqFilter, JqRunner};
pub use registry::{RegisteredTool, ToolKind, ToolRegistry, access_name};
pub use server::{AppGalleryServer, Services};
pub use toolsets::{
    ALL_TOOLSETS, Toolset, available_toolsets, default_toolsets, effective_toolsets,
    normalize_toolsets,
};

use std::path::PathBuf;

use appgallery_connect_api::AccessLevel;

/// Профиль, для которого MCP-сервер называется [`DEFAULT_SERVER_NAME`].
pub const DEFAULT_PROFILE_NAME: &str = "default";

/// Имя MCP-сервера профиля по умолчанию.
pub const DEFAULT_SERVER_NAME: &str = "appgallery";

/// Префикс имени MCP-сервера остальных профилей: `agc-<profile>`.
pub const SERVER_NAME_PREFIX: &str = "agc-";

/// Имя MCP-сервера профиля: `appgallery` для `default`, `agc-<profile>` для остальных.
///
/// Это же имя `connect` регистрирует в MCP-клиенте, поэтому имена инструментов у агента
/// выглядят как `mcp__appgallery__get_app_info`. Клиенты ограничивают
/// `mcp__<server>__<tool>` 64 символами: при именах инструментов до 40 символов имя сервера
/// должно быть не длиннее 17, то есть профиль — не длиннее 13 символов.
pub fn server_name(profile: &str) -> String {
    if profile == DEFAULT_PROFILE_NAME {
        DEFAULT_SERVER_NAME.to_owned()
    } else {
        format!("{SERVER_NAME_PREFIX}{profile}")
    }
}

/// Настройки MCP-сервера.
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// Максимальный разрешённый уровень доступа. Инструменты выше него не регистрируются.
    pub max_access: AccessLevel,
    /// Включённые наборы инструментов (slug, проверяются [`normalize_toolsets`]).
    /// `None` или пустой список — наборы по умолчанию ([`default_toolsets`]).
    pub toolsets: Option<Vec<String>>,
    /// Профиль (API-клиент), с которым запущен сервер: имя сервера ([`server_name`]) и
    /// подсказки в инструкциях (`appgallery-connect-mcp auth login --profile …`).
    pub profile: Option<String>,
    /// `appId` приложений по умолчанию из профиля: подсказка агенту в инструкциях.
    pub apps: Vec<String>,
    /// Имена пакетов по умолчанию из профиля: подсказка агенту в инструкциях.
    pub packages: Vec<String>,
    /// Составные инструменты, из которых регистрируются подходящие по уровню доступа и
    /// набору. По умолчанию — [`composite::all`].
    pub composite_tools: Vec<CompositeTool>,
    /// Режим динамических наборов (`serve --dynamic-toolsets` или `dynamic_toolsets = true`
    /// в профиле, [`toolsets`]): при запуске регистрируются `list_toolsets`,
    /// `enable_toolset`, `disable_toolset` и наборы из [`Self::toolsets`] (без них — ни
    /// одного), остальные агент включает сам. Уровень доступа от этого не меняется.
    pub dynamic_toolsets: bool,
    /// Пользовательские инструменты из `tools.toml` ([`custom`]): наследуют уровень доступа
    /// и набор своей операции.
    pub custom_tools: Vec<CustomTool>,
    /// Как выполняются фильтры `jq`. По умолчанию — в потоке этого процесса; `serve`
    /// использует отдельный процесс ([`JqRunner::subprocess`]).
    pub jq: JqRunner,
    /// Дополнительные каталоги для файловых аргументов инструментов (`serve --allow-path`).
    /// Вместе с рабочим и временным каталогом — корни [`files::FilePolicy`]; каталог должен
    /// существовать, иначе сервер не создаётся.
    pub allowed_paths: Vec<PathBuf>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            max_access: AccessLevel::Read,
            toolsets: None,
            profile: None,
            apps: Vec::new(),
            packages: Vec::new(),
            composite_tools: composite::all(),
            dynamic_toolsets: false,
            custom_tools: Vec::new(),
            jq: JqRunner::default(),
            allowed_paths: Vec::new(),
        }
    }
}

/// Уровень доступа по флагам `--allow-write` и `--allow-destructive`: удаление подразумевает
/// запись. Единое правило для `serve`, `connect` и профилей.
pub fn access_from_flags(allow_write: bool, allow_destructive: bool) -> AccessLevel {
    if allow_destructive {
        AccessLevel::Destructive
    } else if allow_write {
        AccessLevel::Write
    } else {
        AccessLevel::Read
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_implies_write() {
        assert_eq!(access_from_flags(false, false), AccessLevel::Read);
        assert_eq!(access_from_flags(true, false), AccessLevel::Write);
        assert_eq!(access_from_flags(false, true), AccessLevel::Destructive);
        assert_eq!(access_from_flags(true, true), AccessLevel::Destructive);
    }

    #[test]
    fn server_names_follow_the_profile() {
        assert_eq!(server_name("default"), "appgallery");
        assert_eq!(server_name("work"), "agc-work");
    }
}
