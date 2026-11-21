//! Реестр MCP-инструментов сервера.
//!
//! Строится при запуске из `appgallery_connect_api::spec::operations()` и составных
//! инструментов ([`ServerOptions::composite_tools`]) с учётом [`ServerOptions`]:
//! инструменты выше `max_access` и вне включённых наборов не регистрируются, поэтому не
//! видны в `tools/list` и не вызываются. Для незарегистрированных, но известных
//! инструментов [`ToolRegistry::unavailable_reason`] объясняет, какой флаг или набор их
//! включает.
//!
//! Схема аргументов инструмента операции — `input_schema` операции и аргумент ядра
//! `save_to` у операций, в ответе которых временная ссылка на файл или содержимое файла
//! ([`crate::files::add_save_to`]); описание —
//! `summary`, `description` (с пояснением уровня доступа из дополнения), ссылка на
//! справочник и уровень доступа; аннотации `readOnlyHint`/`destructiveHint` — по уровню,
//! `idempotentHint` — по методу у изменяющих инструментов, `openWorldHint` — всегда.
//!
//! Инструменты `api_search` ([`crate::search`]) и `api_request` ([`crate::request`])
//! регистрируются всегда и в наборы не входят: первый ничего не меняет, второй ограничен
//! уровнем доступа сервера (его аннотации — по этому уровню).
//!
//! У инструментов операций с JSON-ответом и у составных инструментов есть необязательный
//! аргумент `jq` ([`crate::jq`]).
//!
//! Пользовательские инструменты ([`crate::custom`], `tools.toml`) наследуют уровень доступа
//! и набор своей операции и регистрируются по тем же правилам, что и она.
//!
//! В режиме динамических наборов ([`ServerOptions::dynamic_toolsets`]) регистрируются ещё
//! `list_toolsets`, `enable_toolset` и `disable_toolset` ([`crate::toolsets`]), а реестр —
//! снимок для текущего списка включённых наборов: сервер пересобирает его через
//! [`ToolRegistry::with_toolsets`]. Пустой список наборов в этом режиме означает «ни
//! одного», а не наборы по умолчанию.
//!
//! Имена служебных инструментов заранее заняты ([`RESERVED_TOOL_NAMES`]), чтобы с ними не
//! совпали операции, составные и пользовательские инструменты.

use std::collections::HashMap;
use std::sync::Arc;

use appgallery_connect_api::spec::{
    AccessLevel, HttpMethod, OperationDef, find, find_toolset, operations,
};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::Value;

use crate::ServerOptions;
use crate::composite::CompositeTool;
use crate::custom::check_names;
use crate::jq::{add_jq_argument, takes_jq};
use crate::request::api_request_tool;
use crate::search::api_search_tool;
use crate::toolsets::{
    ENABLE_TOOLSET_TOOL, disable_toolset_tool, effective_toolsets, enable_toolset_tool,
    list_toolsets_tool, normalize_toolsets,
};

/// Максимальная длина имени инструмента: клиенты добавляют к имени префикс сервера
/// (`mcp__<server>__`) и ограничивают итог 64 символами.
pub const MAX_TOOL_NAME_LEN: usize = 40;

/// Имена служебных инструментов ядра (часть из них появится позже): с ними не должны
/// совпадать операции спецификации и составные инструменты.
pub const RESERVED_TOOL_NAMES: &[&str] = &[
    "api_search",
    "api_request",
    "list_toolsets",
    "enable_toolset",
    "disable_toolset",
];

/// Что выполняет инструмент.
#[derive(Debug, Clone, Copy)]
pub enum ToolKind {
    /// Сгенерированная операция AppGallery Connect API.
    Operation(&'static OperationDef),
    /// Составной инструмент: индекс в `ServerOptions::composite_tools`.
    Composite(usize),
    /// Поиск по операциям ([`crate::search`]).
    ApiSearch,
    /// Запрос к любому пути API ([`crate::request`]).
    ApiRequest,
    /// Пользовательский инструмент: индекс в `ServerOptions::custom_tools`.
    Custom(usize),
    /// `list_toolsets` (динамические наборы).
    ListToolsets,
    /// `enable_toolset` (динамические наборы).
    EnableToolset,
    /// `disable_toolset` (динамические наборы).
    DisableToolset,
}

/// Зарегистрированный инструмент: описание для `tools/list` и обработчик.
#[derive(Debug, Clone)]
pub struct RegisteredTool {
    pub tool: Tool,
    pub kind: ToolKind,
    /// Уровень доступа инструмента; у `api_request` — максимальный уровень сервера.
    pub access: AccessLevel,
}

/// Набор инструментов, доступных при заданных [`ServerOptions`].
#[derive(Debug, Clone)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    index: HashMap<String, usize>,
    /// Включённые наборы (slug).
    toolsets: Vec<String>,
    max_access: AccessLevel,
    /// Составные инструменты сервера (зарегистрированные и нет).
    composite: Vec<CompositeTool>,
    /// Режим динамических наборов ([`ServerOptions::dynamic_toolsets`]).
    dynamic: bool,
    /// Пользовательские инструменты, доступные при уровне доступа сервера, с набором своей
    /// операции; регистрируются, когда набор включён.
    custom: Vec<(RegisteredTool, &'static str)>,
    /// Пользовательские инструменты выше уровня доступа сервера и почему они недоступны.
    disabled_custom: HashMap<String, String>,
    /// Локальные файлы доступны ([`ServerOptions::local_files`]); без них составные
    /// инструменты с обязательным файловым аргументом не регистрируются.
    local_files: bool,
    /// HTTP-режим ([`ServerOptions::http`]): уровень доступа задаёт токен клиента, и
    /// подсказки говорят об операторе сервера, а не о флагах запуска.
    http: bool,
}

impl ToolRegistry {
    /// Строит реестр. Ошибка — неизвестный набор инструментов, конфликт имён составных или
    /// пользовательских инструментов или некорректная схема (последнее — ошибка генератора
    /// или составного инструмента).
    pub fn new(options: &ServerOptions) -> Result<Self, String> {
        let selected = match &options.toolsets {
            Some(names) => Some(normalize_toolsets(names)?),
            None => None,
        };
        // В динамическом режиме без выбора при запуске не включён ни один набор.
        let toolsets = if options.dynamic_toolsets {
            selected.unwrap_or_default()
        } else {
            effective_toolsets(selected.as_deref())
        };
        check_reserved_names()?;
        check_composite(&options.composite_tools)?;
        let composite_names: Vec<&str> = options.composite_tools.iter().map(|t| t.name).collect();
        check_names(&options.custom_tools, &composite_names)?;
        let mut custom = Vec::new();
        let mut disabled_custom = HashMap::new();
        for (index, tool) in options.custom_tools.iter().enumerate() {
            let access = tool.access();
            if options.max_access.allows(access) {
                let registered = RegisteredTool {
                    tool: tool.tool()?,
                    kind: ToolKind::Custom(index),
                    access,
                };
                custom.push((registered, tool.toolset()));
            } else {
                disabled_custom.insert(
                    tool.name.clone(),
                    format!(
                        "custom tool `{}` is disabled: it {} data in AppGallery Connect and the \
                         server runs with {} access. {}",
                        tool.name,
                        access_verb(access),
                        access_name(options.max_access),
                        access_hint(access, options.http),
                    ),
                );
            }
        }
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets,
            max_access: options.max_access,
            composite: options.composite_tools.clone(),
            dynamic: options.dynamic_toolsets,
            custom,
            disabled_custom,
            local_files: options.local_files(),
            http: options.http,
        }
        .populate()
    }

    /// Тот же реестр с другим списком включённых наборов (динамический режим). Ошибка —
    /// неизвестный набор.
    pub fn with_toolsets(&self, toolsets: &[String]) -> Result<Self, String> {
        let toolsets = normalize_toolsets(toolsets)?;
        Self {
            tools: Vec::new(),
            index: HashMap::new(),
            toolsets: if self.dynamic {
                toolsets
            } else {
                effective_toolsets(Some(&toolsets))
            },
            max_access: self.max_access,
            composite: self.composite.clone(),
            dynamic: self.dynamic,
            custom: self.custom.clone(),
            disabled_custom: self.disabled_custom.clone(),
            local_files: self.local_files,
            http: self.http,
        }
        .populate()
    }

    /// Заполняет пустой реестр инструментами по его настройкам: `api_search`,
    /// `api_request`, управление наборами (в динамическом режиме), операции по имени,
    /// составные, пользовательские инструменты.
    fn populate(mut self) -> Result<Self, String> {
        self.push(RegisteredTool {
            tool: api_search_tool(self.http),
            kind: ToolKind::ApiSearch,
            access: AccessLevel::Read,
        });
        self.push(RegisteredTool {
            tool: api_request_tool(self.max_access, self.http),
            kind: ToolKind::ApiRequest,
            access: self.max_access,
        });
        if self.dynamic {
            // Управление наборами меняет только список инструментов этого сервера, данных
            // AppGallery Connect не касается — уровень чтения.
            for (tool, kind) in [
                (list_toolsets_tool(), ToolKind::ListToolsets),
                (enable_toolset_tool(), ToolKind::EnableToolset),
                (disable_toolset_tool(), ToolKind::DisableToolset),
            ] {
                self.push(RegisteredTool {
                    tool,
                    kind,
                    access: AccessLevel::Read,
                });
            }
        }
        for op in operations() {
            if self.max_access.allows(op.access) && self.toolset_enabled(op.toolset) {
                let tool = operation_tool(op)?;
                self.push(RegisteredTool {
                    tool,
                    kind: ToolKind::Operation(op),
                    access: op.access,
                });
            }
        }
        for (index, composite) in self.composite.clone().iter().enumerate() {
            if self.max_access.allows(composite.access)
                && self.toolset_enabled(composite.toolset)
                && (self.local_files || !requires_local_files(composite))
            {
                self.push(RegisteredTool {
                    tool: composite_tool(composite)?,
                    kind: ToolKind::Composite(index),
                    access: composite.access,
                });
            }
        }
        for (tool, toolset) in self.custom.clone() {
            if self.toolset_enabled(toolset) {
                self.push(tool);
            }
        }
        Ok(self)
    }

    fn push(&mut self, tool: RegisteredTool) {
        self.index
            .insert(tool.tool.name.to_string(), self.tools.len());
        self.tools.push(tool);
    }

    /// Составные инструменты сервера (зарегистрированные и нет).
    pub fn composite_tools(&self) -> &[CompositeTool] {
        &self.composite
    }

    /// Включён ли набор `slug`.
    pub fn toolset_enabled(&self, slug: &str) -> bool {
        self.toolsets.iter().any(|name| name == slug)
    }

    /// Включённые наборы (slug).
    pub fn enabled_toolsets(&self) -> &[String] {
        &self.toolsets
    }

    /// Максимальный уровень доступа сервера.
    pub fn max_access(&self) -> AccessLevel {
        self.max_access
    }

    /// HTTP-режим: уровень доступа задаёт токен клиента, а не флаги перезапуска.
    pub fn http(&self) -> bool {
        self.http
    }

    /// Режим динамических наборов: наборы включаются инструментом `enable_toolset`.
    pub fn is_dynamic(&self) -> bool {
        self.dynamic
    }

    /// Пользовательские инструменты, доступные при уровне доступа сервера (зарегистрированные
    /// и нет), с наборами их операций.
    pub fn custom_tools(&self) -> impl Iterator<Item = (&RegisteredTool, &'static str)> {
        self.custom.iter().map(|(tool, toolset)| (tool, *toolset))
    }

    /// Зарегистрированные инструменты в порядке регистрации: `api_search`, `api_request`,
    /// управление наборами, операции по имени, составные, пользовательские.
    pub fn iter(&self) -> impl Iterator<Item = &RegisteredTool> {
        self.tools.iter()
    }

    /// Описания инструментов для `tools/list`.
    pub fn tools(&self) -> Vec<Tool> {
        self.tools.iter().map(|t| t.tool.clone()).collect()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.index.get(name).and_then(|&i| self.tools.get(i))
    }

    /// Почему известный инструмент не зарегистрирован; `None` — инструмент неизвестен или
    /// зарегистрирован.
    pub fn unavailable_reason(&self, name: &str) -> Option<String> {
        if self.get(name).is_some() {
            return None;
        }
        if let Some(reason) = self.disabled_custom.get(name) {
            return Some(reason.clone());
        }
        let custom = self
            .custom
            .iter()
            .find(|(tool, _)| tool.tool.name == name)
            .map(|(tool, toolset)| (tool.access, *toolset));
        let (access, toolset) = match find(name) {
            Some(op) => (op.access, op.toolset),
            None => match custom {
                Some(found) => found,
                None => {
                    let tool = self.composite.iter().find(|tool| tool.name == name)?;
                    (tool.access, tool.toolset)
                }
            },
        };
        if !self.max_access.allows(access) {
            return Some(format!(
                "tool `{name}` is disabled: it {} data in AppGallery Connect and the server runs \
                 with {} access. {}",
                access_verb(access),
                access_name(self.max_access),
                access_hint(access, self.http),
            ));
        }
        if !self.local_files
            && self
                .composite
                .iter()
                .any(|tool| tool.name == name && requires_local_files(tool))
        {
            return Some(format!(
                "tool `{name}` is disabled: it reads or writes files, and this server runs \
                 remotely over HTTP without a file directory. Ask the server operator to start \
                 it with --allow-path <dir> if files are needed"
            ));
        }
        if !self.toolset_enabled(toolset) {
            if self.dynamic {
                return Some(format!(
                    "tool `{name}` belongs to toolset `{toolset}`, which is not enabled: call \
                     `{ENABLE_TOOLSET_TOOL}` with `toolset: \"{toolset}\"` first"
                ));
            }
            return Some(format!(
                "tool `{name}` belongs to toolset `{toolset}`, which is not enabled; ask the user \
                 to restart the server with `{toolset}` added to --toolsets (enabled: {})",
                self.toolsets.join(", "),
            ));
        }
        None
    }
}

/// Имена служебных инструментов не совпадают с операциями спецификации (ошибка
/// генератора или дополнения).
fn check_reserved_names() -> Result<(), String> {
    for name in RESERVED_TOOL_NAMES {
        if find(name).is_some() {
            return Err(format!(
                "operation `{name}` clashes with the built-in tool of the same name"
            ));
        }
    }
    Ok(())
}

/// Проверяет составные инструменты: имена уникальны, не совпадают с операциями и
/// служебными инструментами, не длиннее [`MAX_TOOL_NAME_LEN`], набор существует.
fn check_composite(tools: &[CompositeTool]) -> Result<(), String> {
    let mut seen: Vec<&str> = RESERVED_TOOL_NAMES.to_vec();
    for tool in tools {
        if seen.contains(&tool.name) {
            return Err(format!(
                "composite tool `{}` is defined twice or clashes with a built-in tool",
                tool.name
            ));
        }
        if find(tool.name).is_some() {
            return Err(format!(
                "composite tool `{}` clashes with the operation tool of the same name",
                tool.name
            ));
        }
        if tool.name.len() > MAX_TOOL_NAME_LEN || tool.name.is_empty() {
            return Err(format!(
                "composite tool name `{}` must be 1-{MAX_TOOL_NAME_LEN} characters long",
                tool.name
            ));
        }
        if find_toolset(tool.toolset).is_none() {
            return Err(format!(
                "composite tool `{}` belongs to an unknown toolset `{}`",
                tool.name, tool.toolset
            ));
        }
        seen.push(tool.name);
    }
    Ok(())
}

/// Описание MCP-инструмента для операции: схема — `input_schema` операции, `jq` у
/// операций с JSON-ответом и `save_to` у операций, которые сохраняют файл; описание — `summary`, `description`, ссылка на справочник и
/// уровень доступа.
pub fn operation_tool(op: &'static OperationDef) -> Result<Tool, String> {
    let mut schema: JsonObject = serde_json::from_str(op.input_schema)
        .map_err(|err| format!("{}: invalid input schema: {err}", op.tool_name))?;
    if takes_jq(op) {
        add_jq_argument(op.tool_name, &mut schema)?;
    }
    if op.saves_file() {
        crate::files::add_save_to(op.tool_name, &mut schema)?;
    }
    let summary = op.summary.trim();
    let mut description = summary.to_owned();
    let details = op.description.trim();
    if !details.is_empty() && details != summary {
        if !description.is_empty() {
            description.push_str("\n\n");
        }
        description.push_str(details);
    }
    if op.saves_file() {
        description.push_str("\n\n");
        description.push_str(crate::files::SAVE_TO_DESCRIPTION);
    }
    if !op.reference.is_empty() {
        description.push_str("\n\nAPI reference: ");
        description.push_str(op.reference);
    }
    description.push_str("\n\nAccess: ");
    description.push_str(access_description(op.access));

    let mut tool = Tool::new(op.tool_name, description, Arc::new(schema));
    if !summary.is_empty() {
        tool.title = Some(summary.to_owned());
    }
    let idempotent = matches!(op.method, HttpMethod::Put | HttpMethod::Delete);
    tool.annotations = Some(annotations(summary, op.access, Some(idempotent)));
    Ok(tool)
}

/// Описание MCP-инструмента для составного инструмента: его схема с аргументом `jq`,
/// описание с уровнем доступа и аннотации по уровню.
pub fn composite_tool(composite: &CompositeTool) -> Result<Tool, String> {
    let Value::Object(mut schema) = (composite.input_schema)() else {
        return Err(format!(
            "composite tool `{}`: the input schema is not a JSON object",
            composite.name
        ));
    };
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err(format!(
            "composite tool `{}`: the input schema must have `type: object`",
            composite.name
        ));
    }
    add_jq_argument(composite.name, &mut schema)?;
    let mut description = composite.description.trim().to_owned();
    description.push_str("\n\nAccess: ");
    description.push_str(access_description(composite.access));
    let mut tool = Tool::new(composite.name, description, Arc::new(schema));
    tool.annotations = Some(annotations("", composite.access, None));
    Ok(tool)
}

/// Текст уровня доступа в описании инструмента.
pub fn access_description(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read-only.",
        AccessLevel::Write => "write (creates or changes data).",
        AccessLevel::Destructive => {
            "destructive (deletes data, releases to users or affects money)."
        }
    }
}

/// Аннотации по уровню доступа. `idempotentHint` имеет смысл только для изменяющих
/// инструментов: `PUT` и `DELETE` идемпотентны, `POST` и `PATCH` — нет.
pub fn annotations(title: &str, access: AccessLevel, idempotent: Option<bool>) -> ToolAnnotations {
    let base = if title.is_empty() {
        ToolAnnotations::new()
    } else {
        ToolAnnotations::with_title(title)
    }
    .open_world(true);
    let changing = |base: ToolAnnotations, destructive: bool| {
        let base = base.read_only(false).destructive(destructive);
        match idempotent {
            Some(idempotent) => base.idempotent(idempotent),
            None => base,
        }
    };
    match access {
        AccessLevel::Read => base.read_only(true),
        AccessLevel::Write => changing(base, false),
        AccessLevel::Destructive => changing(base, true),
    }
}

pub(crate) fn access_verb(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "reads",
        AccessLevel::Write => "creates or changes",
        AccessLevel::Destructive => "deletes, releases to users or irreversibly changes",
    }
}

/// Название уровня доступа для сообщений и инструкций.
pub fn access_name(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read-only",
        AccessLevel::Write => "write",
        AccessLevel::Destructive => "destructive",
    }
}

/// Как пользователь включает уровень `access` в stdio: права — настройка профиля (флаги
/// регистрации их не расширяют), на macOS повышение ещё и одобряется Touch ID. Фраза
/// продолжает «ask the user to …».
fn profile_access_step(access: AccessLevel) -> String {
    match access {
        AccessLevel::Read => "keep the read-only access".to_owned(),
        level => format!(
            "allow {} access for the server's profile with `appgallery-connect-mcp ui` and \
             restart the MCP client",
            access_name(level)
        ),
    }
}

/// Что даёт уровень доступа `access`, для вставки в сообщение («needs …»): в stdio — права
/// профиля сервера, в HTTP-режиме — токен доступа с этим уровнем.
pub fn access_requirement(access: AccessLevel, http: bool) -> String {
    if http {
        format!("an access token with the {} level", access_name(access))
    } else {
        format!("{} access for the server's profile", access_name(access))
    }
}

/// Как получить уровень доступа `access`: в stdio — права профиля (`ui`) и перезапуск
/// клиента, в HTTP-режиме — токен доступа с этим уровнем от оператора сервера.
pub fn access_hint(access: AccessLevel, http: bool) -> String {
    if http {
        format!(
            "This server runs remotely over HTTP: {} if this change is intended",
            access_action(access, http)
        )
    } else {
        format!(
            "Ask the user to {} if this change is intended",
            profile_access_step(access)
        )
    }
}

/// Что попросить у пользователя ради уровня `access`, со строчной буквы: «ask the user …».
pub fn access_action(access: AccessLevel, http: bool) -> String {
    if http {
        format!(
            "ask the user for an access token with the {} level from the server operator",
            access_name(access)
        )
    } else {
        format!("ask the user to {}", profile_access_step(access))
    }
}

/// Имена файловых аргументов составных инструментов: путь к файлу или каталогу.
pub const FILE_ARGUMENTS: &[&str] = &["file", "dir", "save_to"];

/// Нужны ли составному инструменту локальные файлы: среди обязательных аргументов схемы
/// есть файловый ([`FILE_ARGUMENTS`]). Без локальных файлов (HTTP-режим без `--allow-path`)
/// такой инструмент не регистрируется: вызвать его всё равно нельзя.
pub fn requires_local_files(tool: &CompositeTool) -> bool {
    (tool.input_schema)()
        .get("required")
        .and_then(Value::as_array)
        .is_some_and(|required| {
            required
                .iter()
                .filter_map(Value::as_str)
                .any(|name| FILE_ARGUMENTS.contains(&name))
        })
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::spec::toolsets;
    use serde_json::json;

    use super::*;
    use crate::composite::{Context, ToolFuture};
    use crate::toolsets::default_toolsets;

    fn options(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ServerOptions {
        ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            composite_tools: Vec::new(),
            ..ServerOptions::default()
        }
    }

    fn all() -> Option<&'static [&'static str]> {
        Some(&["all"])
    }

    /// Число инструментов уровня и наборов: операции и `api_search` с `api_request`.
    fn count(access: AccessLevel, toolsets: &[&str]) -> usize {
        operations()
            .iter()
            .filter(|op| access.allows(op.access) && toolsets.contains(&op.toolset))
            .count()
            + 2
    }

    fn all_slugs() -> Vec<&'static str> {
        toolsets().iter().map(|t| t.slug).collect()
    }

    fn run_nothing(_ctx: &Context, _args: Value) -> ToolFuture<'_> {
        Box::pin(async { Ok(json!({})) })
    }

    fn composite(name: &'static str, access: AccessLevel) -> CompositeTool {
        CompositeTool {
            name,
            toolset: "releases",
            access,
            description: "Test tool.",
            input_schema: || json!({"type": "object", "additionalProperties": false, "properties": {}}),
            run: run_nothing,
        }
    }

    #[test]
    fn schemas_and_descriptions_match_operations() {
        assert!(!operations().is_empty());
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            assert_eq!(tool.name, op.tool_name);
            assert!(op.tool_name.len() <= MAX_TOOL_NAME_LEN, "{}", op.tool_name);
            let mut expected: Value = serde_json::from_str(op.input_schema).unwrap();
            let mut actual = Value::Object((*tool.input_schema).clone());
            // Ядро добавляет `jq` операциям с JSON-ответом и `save_to` операциям, которые
            // сохраняют файл; остальное — схема операции.
            let jq = actual["properties"]
                .as_object_mut()
                .and_then(|p| p.remove(crate::jq::JQ_ARG));
            assert_eq!(jq.is_some(), takes_jq(op), "{}", op.tool_name);
            let save_to = actual["properties"]
                .as_object_mut()
                .and_then(|p| p.remove(crate::files::SAVE_TO_ARG));
            assert_eq!(save_to.is_some(), op.saves_file(), "{}", op.tool_name);
            if expected.get("properties").is_none() && actual["properties"] == json!({}) {
                expected["properties"] = json!({});
            }
            assert_eq!(actual, expected, "{}", op.tool_name);
            assert_eq!(actual["type"], "object", "{}", op.tool_name);
            assert_eq!(actual["additionalProperties"], false, "{}", op.tool_name);
            let description = tool.description.as_deref().unwrap();
            assert!(description.starts_with(op.summary), "{}", op.tool_name);
            assert!(
                description.ends_with(access_description(op.access)),
                "{}",
                op.tool_name
            );
            if !op.reference.is_empty() {
                assert!(
                    description.contains(&format!("API reference: {}", op.reference)),
                    "{}",
                    op.tool_name
                );
            }
            assert_eq!(tool.title.as_deref(), Some(op.summary), "{}", op.tool_name);
        }
    }

    #[test]
    fn annotations_follow_access_level() {
        for op in operations() {
            let tool = operation_tool(op).unwrap();
            let a = tool.annotations.as_ref().unwrap();
            assert_eq!(a.open_world_hint, Some(true));
            match op.access {
                AccessLevel::Read => {
                    assert_eq!(a.read_only_hint, Some(true), "{}", op.tool_name);
                    assert_eq!(a.destructive_hint, None, "{}", op.tool_name);
                    assert_eq!(a.idempotent_hint, None, "{}", op.tool_name);
                }
                AccessLevel::Write | AccessLevel::Destructive => {
                    assert_eq!(a.read_only_hint, Some(false), "{}", op.tool_name);
                    assert_eq!(
                        a.destructive_hint,
                        Some(op.access == AccessLevel::Destructive),
                        "{}",
                        op.tool_name
                    );
                    assert_eq!(
                        a.idempotent_hint,
                        Some(matches!(op.method, HttpMethod::Put | HttpMethod::Delete)),
                        "{}",
                        op.tool_name
                    );
                }
            }
        }
    }

    #[test]
    fn access_filtering() {
        let slugs = all_slugs();
        let read = ToolRegistry::new(&options(AccessLevel::Read, all())).unwrap();
        assert_eq!(read.len(), count(AccessLevel::Read, &slugs));
        assert!(read.iter().all(|t| t.access == AccessLevel::Read));
        assert!(read.get("list_reviews").is_some());
        assert!(read.get("update_app_info").is_none());
        assert!(read.get("submit_app").is_none());

        let write = ToolRegistry::new(&options(AccessLevel::Write, all())).unwrap();
        assert_eq!(write.len(), count(AccessLevel::Write, &slugs));
        assert!(write.get("update_app_info").is_some());
        assert!(write.get("submit_app").is_none());
        assert!(write.iter().all(|t| t.access != AccessLevel::Destructive));

        let destructive = ToolRegistry::new(&options(AccessLevel::Destructive, all())).unwrap();
        assert_eq!(destructive.len(), operations().len() + 2);
        assert!(destructive.get("submit_app").is_some());
        assert!(read.len() < write.len() && write.len() < destructive.len());
    }

    #[test]
    fn search_and_request_are_always_registered() {
        for access in [
            AccessLevel::Read,
            AccessLevel::Write,
            AccessLevel::Destructive,
        ] {
            for chosen in [None, Some(&["products"][..])] {
                let registry = ToolRegistry::new(&options(access, chosen)).unwrap();
                let search = registry.get("api_search").unwrap();
                assert!(matches!(search.kind, ToolKind::ApiSearch));
                assert_eq!(search.access, AccessLevel::Read);
                let request = registry.get("api_request").unwrap();
                assert!(matches!(request.kind, ToolKind::ApiRequest));
                assert_eq!(request.access, access);
                let hints = request.tool.annotations.as_ref().unwrap();
                assert_eq!(hints.read_only_hint, Some(access == AccessLevel::Read));
                assert!(registry.unavailable_reason("api_request").is_none());
            }
        }
    }

    #[test]
    fn toolset_filtering() {
        let default = ToolRegistry::new(&options(AccessLevel::Destructive, None)).unwrap();
        assert_eq!(default.enabled_toolsets(), default_toolsets());
        assert_eq!(
            default.len(),
            count(AccessLevel::Destructive, &default_toolsets())
        );
        assert!(default.get("list_certificates").is_none());
        let empty = ToolRegistry::new(&options(AccessLevel::Destructive, Some(&[]))).unwrap();
        assert_eq!(empty.len(), default.len());

        let chosen = ToolRegistry::new(&options(AccessLevel::Read, Some(&["products"]))).unwrap();
        assert!(!chosen.is_empty());
        assert!(chosen.iter().all(|t| match t.kind {
            ToolKind::Operation(op) => op.toolset == "products",
            _ => true,
        }));
        assert!(chosen.get("list_reviews").is_none());

        let err = ToolRegistry::new(&options(AccessLevel::Read, Some(&["Отзывы"]))).unwrap_err();
        assert!(err.contains("unknown toolset `Отзывы`"), "{err}");
    }

    #[test]
    fn unavailable_reasons_name_the_flag_or_toolset() {
        let read = ToolRegistry::new(&options(AccessLevel::Read, all())).unwrap();
        let reason = read.unavailable_reason("update_app_info").unwrap();
        assert!(reason.contains("write access"), "{reason}");
        let reason = read.unavailable_reason("submit_app").unwrap();
        assert!(reason.contains("destructive access"), "{reason}");
        assert!(read.unavailable_reason("list_reviews").is_none());
        assert!(read.unavailable_reason("no_such_tool").is_none());

        let products = ToolRegistry::new(&options(AccessLevel::Read, Some(&["products"]))).unwrap();
        let reason = products.unavailable_reason("list_reviews").unwrap();
        assert!(reason.contains("toolset `reviews`"), "{reason}");
        assert!(
            reason.contains("--toolsets (enabled: products)"),
            "{reason}"
        );
    }

    #[test]
    fn http_mode_hides_file_tools_without_allowed_paths() {
        let names = |options: &ServerOptions| -> Vec<String> {
            ToolRegistry::new(options)
                .unwrap()
                .iter()
                .map(|tool| tool.tool.name.to_string())
                .collect()
        };
        let base = ServerOptions {
            max_access: AccessLevel::Destructive,
            toolsets: Some(vec!["all".into()]),
            ..ServerOptions::default()
        };
        let stdio = names(&base);
        let http = ServerOptions {
            http: true,
            ..base.clone()
        };
        assert!(!http.local_files());
        let remote = names(&http);
        for tool in [
            "package_upload",
            "listing_export",
            "listing_apply",
            "image_sync",
        ] {
            assert!(stdio.iter().any(|n| n == tool), "{tool}");
            assert!(!remote.iter().any(|n| n == tool), "{tool}");
        }
        // Без обязательного файла инструмент полезен и без каталога: остаётся.
        assert!(remote.iter().any(|n| n == "release_status"));
        let registry = ToolRegistry::new(&http).unwrap();
        let reason = registry.unavailable_reason("package_upload").unwrap();
        assert!(reason.contains("--allow-path"), "{reason}");

        let with_dir = ServerOptions {
            allowed_paths: vec![std::env::temp_dir()],
            ..http.clone()
        };
        assert!(with_dir.local_files());
        assert!(names(&with_dir).iter().any(|n| n == "package_upload"));

        // Уровень доступа в HTTP-режиме даёт токен оператора, а не флаг запуска.
        let read = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Read,
            ..http
        })
        .unwrap();
        let reason = read.unavailable_reason("update_app_info").unwrap();
        assert!(reason.contains("server operator"), "{reason}");
        assert!(!reason.contains("--allow-write"), "{reason}");
    }

    #[test]
    fn composite_tools_follow_access_and_toolsets() {
        let tools = vec![
            composite("test_read", AccessLevel::Read),
            composite("test_write", AccessLevel::Write),
        ];
        let read = ToolRegistry::new(&ServerOptions {
            composite_tools: tools.clone(),
            ..options(AccessLevel::Read, None)
        })
        .unwrap();
        let tool = read.get("test_read").unwrap();
        assert!(matches!(tool.kind, ToolKind::Composite(0)));
        assert!(
            tool.tool
                .description
                .as_deref()
                .unwrap()
                .ends_with("Access: read-only.")
        );
        assert_eq!(
            tool.tool.annotations.as_ref().unwrap().read_only_hint,
            Some(true)
        );
        assert!(read.get("test_write").is_none());
        assert!(
            read.unavailable_reason("test_write")
                .unwrap()
                .contains("write access")
        );

        let other = ToolRegistry::new(&ServerOptions {
            composite_tools: tools,
            ..options(AccessLevel::Read, Some(&["apps"]))
        })
        .unwrap();
        let reason = other.unavailable_reason("test_read").unwrap();
        assert!(reason.contains("toolset `releases`"), "{reason}");

        for bad in [
            vec![composite("list_reviews", AccessLevel::Read)],
            vec![composite("api_request", AccessLevel::Read)],
            vec![
                composite("twice", AccessLevel::Read),
                composite("twice", AccessLevel::Read),
            ],
            vec![CompositeTool {
                toolset: "nope",
                ..composite("x", AccessLevel::Read)
            }],
        ] {
            assert!(
                ToolRegistry::new(&ServerOptions {
                    composite_tools: bad,
                    ..options(AccessLevel::Read, None)
                })
                .is_err()
            );
        }
    }
}
