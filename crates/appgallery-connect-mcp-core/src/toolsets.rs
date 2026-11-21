//! Наборы инструментов (`--toolsets`) и динамические наборы (`serve --dynamic-toolsets`).
//!
//! Набор — несколько связанных операций под коротким английским slug из
//! `appgallery_connect_api::spec::toolsets()`. Набор выбирается точным совпадением slug;
//! `all` включает все наборы. Неизвестный набор — ошибка запуска со списком допустимых.
//!
//! Без выбора включаются наборы, отмеченные в файле-дополнении спецификации как наборы по
//! умолчанию ([`default_toolsets`]): приложения, выпуски, отзывы — то, с чем работают
//! каждый день. Остальные (страница приложения, отчёты, товары, проекты, подпись HarmonyOS)
//! нужны реже, а описания их инструментов заметно увеличивают список инструментов в
//! контексте агента.
//!
//! **Динамические наборы.** Клиенты без отложенной загрузки инструментов (например, Codex)
//! кладут описания всех инструментов в контекст сразу. В динамическом режиме при запуске
//! регистрируются только `list_toolsets`, `enable_toolset`, `disable_toolset` и наборы из
//! `--toolsets` или профиля (без них — ни одного). Агент включает нужный набор, сервер
//! пересобирает реестр ([`ToolRegistry::with_toolsets`]) и сообщает клиенту
//! `notifications/tools/list_changed`. Составные и пользовательские инструменты входят в
//! свои наборы и включаются вместе с ними.
//!
//! Уровень доступа действует как обычно: включение набора не регистрирует инструменты выше
//! `max_access`. Управление наборами меняет только список инструментов этого сервера и
//! данных AppGallery Connect не касается, поэтому эти инструменты — уровня чтения.
//!
//! Здесь только описание этих инструментов, разбор аргументов и тексты ответов ([`list`],
//! [`toggle`]); состояние (текущий реестр сессии) и уведомления — в [`crate::server`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use appgallery_connect_api::spec::{AccessLevel, find_toolset, operations, toolsets};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use serde_json::{Map, Value, json};

use crate::error::ToolError;
use crate::registry::{ToolKind, ToolRegistry, access_action};

/// Значение `--toolsets`, которое включает все наборы.
pub const ALL_TOOLSETS: &str = "all";

/// Набор инструментов: slug, описание и число операций в спецификации.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolset {
    pub slug: &'static str,
    pub description: &'static str,
    /// Число инструментов операций набора (без исключённых операций).
    pub operations: usize,
    /// Включён без `--toolsets`.
    pub default: bool,
}

/// Наборы, включённые без `--toolsets` и без настройки профиля, в порядке дополнения.
pub fn default_toolsets() -> Vec<&'static str> {
    toolsets()
        .iter()
        .filter(|toolset| toolset.default_enabled)
        .map(|toolset| toolset.slug)
        .collect()
}

/// Все наборы в порядке файла-дополнения.
pub fn available_toolsets() -> Vec<Toolset> {
    toolsets()
        .iter()
        .map(|toolset| Toolset {
            slug: toolset.slug,
            description: toolset.description,
            operations: operations()
                .iter()
                .filter(|op| op.toolset == toolset.slug)
                .count(),
            default: toolset.default_enabled,
        })
        .collect()
}

/// Проверяет имена наборов: пробелы по краям и пустые элементы отбрасываются, повторы
/// убираются, `all` раскрывается во все наборы. Неизвестный набор — ошибка.
pub fn normalize_toolsets<S: AsRef<str>>(names: &[S]) -> Result<Vec<String>, String> {
    let mut result: Vec<String> = Vec::new();
    for name in names {
        let name = name.as_ref().trim();
        if name.is_empty() {
            continue;
        }
        if name == ALL_TOOLSETS {
            return Ok(toolsets().iter().map(|t| t.slug.to_owned()).collect());
        }
        if find_toolset(name).is_none() {
            let list: Vec<&str> = toolsets().iter().map(|t| t.slug).collect();
            return Err(format!(
                "unknown toolset `{name}`; available toolsets: {}, or `{ALL_TOOLSETS}`",
                list.join(", ")
            ));
        }
        if !result.iter().any(|known| known == name) {
            result.push(name.to_owned());
        }
    }
    Ok(result)
}

/// Наборы, которые действуют при выборе `selected` (`None` или пусто — по умолчанию).
pub fn effective_toolsets(selected: Option<&[String]>) -> Vec<String> {
    match selected {
        Some(names) if !names.is_empty() => names.to_vec(),
        _ => default_toolsets().into_iter().map(str::to_owned).collect(),
    }
}

pub const LIST_TOOLSETS_TOOL: &str = "list_toolsets";
pub const ENABLE_TOOLSET_TOOL: &str = "enable_toolset";
pub const DISABLE_TOOLSET_TOOL: &str = "disable_toolset";

/// Сколько имён инструментов показывать в `list_toolsets` как пример содержимого набора.
const EXAMPLES: usize = 4;

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    }
}

fn toolset_schema(verb: &str) -> JsonObject {
    object(json!({
        "type": "object",
        "properties": {
            "toolset": {
                "type": "string",
                "description": format!(
                    "Toolset to {verb} (slug from `{LIST_TOOLSETS_TOOL}`, e.g. `listing`). \
                     Several toolsets can be given separated by commas."
                )
            }
        },
        "required": ["toolset"],
        "additionalProperties": false
    }))
}

fn annotated(name: &'static str, title: &str, description: String, schema: JsonObject) -> Tool {
    let mut tool = Tool::new(name, description, Arc::new(schema));
    tool.title = Some(title.to_owned());
    tool.annotations = Some(
        ToolAnnotations::with_title(title)
            .read_only(true)
            .open_world(false),
    );
    tool
}

/// Описание `list_toolsets`.
pub fn list_toolsets_tool() -> Tool {
    annotated(
        LIST_TOOLSETS_TOOL,
        "List toolsets",
        format!(
            "List the toolsets (groups of AppGallery Connect API tools: apps, releases, store \
             listing, reviews, reports, products, projects, HarmonyOS signing) of this server: \
             slug, description, number of tools available at the server's access level, \
             example tool names and whether the toolset is enabled. Only the tools of enabled \
             toolsets are loaded; enable a toolset with `{ENABLE_TOOLSET_TOOL}` before using \
             its tools.\n\n\
             Access: read-only (does not call the API)."
        ),
        object(json!({"type": "object", "properties": {}, "additionalProperties": false})),
    )
}

/// Описание `enable_toolset`.
pub fn enable_toolset_tool() -> Tool {
    annotated(
        ENABLE_TOOLSET_TOOL,
        "Enable a toolset",
        format!(
            "Load the tools of a toolset (see `{LIST_TOOLSETS_TOOL}`). The server then notifies \
             the client that its tool list changed, and the new tools become available. Tools \
             above the server's access level stay unavailable.\n\n\
             Access: read-only (changes only the tool list of this server)."
        ),
        toolset_schema("enable"),
    )
}

/// Описание `disable_toolset`.
pub fn disable_toolset_tool() -> Tool {
    annotated(
        DISABLE_TOOLSET_TOOL,
        "Disable a toolset",
        format!(
            "Unload the tools of a toolset enabled earlier with `{ENABLE_TOOLSET_TOOL}`, to keep \
             the tool list short.\n\n\
             Access: read-only (changes only the tool list of this server)."
        ),
        toolset_schema("disable"),
    )
}

/// Имена инструментов каждого набора (операций, составных и пользовательских), которые этот
/// сервер может зарегистрировать на своём уровне доступа, без учёта того, включён ли набор.
fn tools_by_toolset(registry: &ToolRegistry) -> BTreeMap<&str, Vec<String>> {
    let mut map: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for op in operations() {
        if registry.max_access().allows(op.access) {
            map.entry(op.toolset)
                .or_default()
                .push(op.tool_name.to_owned());
        }
    }
    for tool in registry.composite_tools() {
        if registry.max_access().allows(tool.access) {
            map.entry(tool.toolset)
                .or_default()
                .push(tool.name.to_owned());
        }
    }
    for (tool, toolset) in registry.custom_tools() {
        map.entry(toolset)
            .or_default()
            .push(tool.tool.name.to_string());
    }
    map
}

/// Ответ `list_toolsets`: компактный JSON.
pub fn list(args: &JsonObject, registry: &ToolRegistry) -> Result<String, ToolError> {
    if let Some(key) = args.keys().next() {
        return Err(ToolError::new(format!(
            "unknown argument `{key}`; `{LIST_TOOLSETS_TOOL}` takes no arguments"
        )));
    }
    let available = tools_by_toolset(registry);
    let mut hidden_total = 0;
    let toolsets: Vec<Value> = available_toolsets()
        .into_iter()
        .map(|toolset| {
            let names = available.get(toolset.slug).cloned().unwrap_or_default();
            let mut entry = Map::new();
            entry.insert("toolset".into(), json!(toolset.slug));
            entry.insert("description".into(), json!(toolset.description));
            entry.insert("tools".into(), json!(names.len()));
            entry.insert(
                "examples".into(),
                json!(names.iter().take(EXAMPLES).collect::<Vec<_>>()),
            );
            // Инструменты выше уровня доступа: сколько их (включает их только пользователь).
            let hidden = operations()
                .iter()
                .filter(|op| op.toolset == toolset.slug && !registry.max_access().allows(op.access))
                .count()
                + registry
                    .composite_tools()
                    .iter()
                    .filter(|tool| {
                        tool.toolset == toolset.slug && !registry.max_access().allows(tool.access)
                    })
                    .count();
            if hidden > 0 {
                hidden_total += hidden;
                entry.insert("above_access_level".into(), json!(hidden));
            }
            entry.insert(
                "enabled".into(),
                json!(registry.toolset_enabled(toolset.slug)),
            );
            Value::Object(entry)
        })
        .collect();
    let mut out = Map::new();
    out.insert("toolsets".into(), Value::Array(toolsets));
    let mut note = format!("Enable a toolset with `{ENABLE_TOOLSET_TOOL}` to load its tools.");
    if hidden_total > 0 {
        let next = match registry.max_access() {
            AccessLevel::Read => AccessLevel::Write,
            _ => AccessLevel::Destructive,
        };
        note.push_str(&format!(
            " `above_access_level` counts tools this server does not allow at its access \
             level; only the user can allow them (for example, {}).",
            access_action(next, registry.http())
        ));
    }
    out.insert("note".into(), json!(note));
    Ok(Value::Object(out).to_string())
}

/// Наборы из аргумента `toolset` (через запятую), проверенные.
fn parse_toolsets(args: &JsonObject) -> Result<Vec<String>, ToolError> {
    if let Some(key) = args.keys().find(|key| key.as_str() != "toolset") {
        return Err(ToolError::new(format!(
            "unknown argument `{key}`; the only argument is `toolset`"
        )));
    }
    let raw = match args.get("toolset") {
        Some(Value::String(text)) => text,
        None | Some(Value::Null) => {
            return Err(ToolError::new(format!(
                "missing required argument `toolset`; see `{LIST_TOOLSETS_TOOL}` for the slugs"
            )));
        }
        Some(_) => return Err(ToolError::new("argument `toolset` must be a string")),
    };
    let names: Vec<&str> = raw.split(',').collect();
    let toolsets = normalize_toolsets(&names).map_err(ToolError::new)?;
    if toolsets.is_empty() {
        return Err(ToolError::new(format!(
            "argument `toolset` is empty; see `{LIST_TOOLSETS_TOOL}` for the slugs"
        )));
    }
    Ok(toolsets)
}

/// Имена зарегистрированных инструментов наборов: всё, кроме управления наборами.
fn toolset_tools(registry: &ToolRegistry) -> BTreeSet<String> {
    registry
        .iter()
        .filter(|tool| {
            matches!(
                tool.kind,
                ToolKind::Operation(_) | ToolKind::Composite(_) | ToolKind::Custom(_)
            )
        })
        .map(|tool| tool.tool.name.to_string())
        .collect()
}

/// Результат `enable_toolset`/`disable_toolset`: новый реестр (`None` — список инструментов
/// не изменился) и текст ответа.
#[derive(Debug)]
pub struct Toggle {
    pub registry: Option<ToolRegistry>,
    pub text: String,
}

/// Включает (`enable = true`) или выключает наборы из аргументов относительно реестра
/// `current`. Уровень доступа реестра не меняется.
pub fn toggle(
    current: &ToolRegistry,
    args: &JsonObject,
    enable: bool,
) -> Result<Toggle, ToolError> {
    let requested = parse_toolsets(args)?;
    let mut enabled: Vec<String> = current.enabled_toolsets().to_vec();
    let mut changed_sets = Vec::new();
    let mut unchanged_sets = Vec::new();
    for slug in requested {
        let present = enabled.contains(&slug);
        if present == enable {
            unchanged_sets.push(slug);
        } else {
            if enable {
                enabled.push(slug.clone());
            } else {
                enabled.retain(|name| name != &slug);
            }
            changed_sets.push(slug);
        }
    }

    let next = if changed_sets.is_empty() {
        None
    } else {
        Some(current.with_toolsets(&enabled).map_err(ToolError::new)?)
    };
    let before = toolset_tools(current);
    let after = next
        .as_ref()
        .map(toolset_tools)
        .unwrap_or_else(|| before.clone());
    let (diff_key, diff): (&str, Vec<&String>) = if enable {
        ("added_tools", after.difference(&before).collect())
    } else {
        ("removed_tools", before.difference(&after).collect())
    };

    let mut out = Map::new();
    let (changed_key, unchanged_key) = if enable {
        ("enabled", "already_enabled")
    } else {
        ("disabled", "not_enabled")
    };
    out.insert(changed_key.into(), json!(changed_sets));
    if !unchanged_sets.is_empty() {
        out.insert(unchanged_key.into(), json!(unchanged_sets));
    }
    out.insert(diff_key.into(), json!(diff));
    out.insert("enabled_toolsets".into(), json!(enabled));
    let note = if enable && !changed_sets.is_empty() && diff.is_empty() {
        Some(
            "No tools were added: all tools of this toolset are above the server's access \
             level. Only the user can allow them by restarting the server."
                .to_owned(),
        )
    } else if enable && !diff.is_empty() {
        Some(
            "The client was notified that the tool list changed. If these tools do not appear \
             in your tool list, the client does not reload it: call the same operations with \
             `api_request` (toolsets do not limit it), or ask the user to restart the server \
             with these toolsets in --toolsets."
                .to_owned(),
        )
    } else {
        None
    };
    if let Some(note) = note {
        out.insert("note".into(), json!(note));
    }
    Ok(Toggle {
        registry: next,
        text: Value::Object(out).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry(max_access: AccessLevel, toolsets: Option<&[&str]>) -> ToolRegistry {
        ToolRegistry::new(&crate::ServerOptions {
            max_access,
            toolsets: toolsets.map(|t| t.iter().map(|s| s.to_string()).collect()),
            dynamic_toolsets: true,
            ..crate::ServerOptions::default()
        })
        .unwrap()
    }

    fn args(value: Value) -> JsonObject {
        object(value)
    }

    fn parse(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn initial_registry_has_only_built_in_tools() {
        // Без `--toolsets` в динамическом режиме — ни одного набора, а не наборы по умолчанию:
        // остаются поиск, запрос и управление наборами.
        let reg = registry(AccessLevel::Destructive, None);
        let mut names: Vec<String> = reg.iter().map(|t| t.tool.name.to_string()).collect();
        names.sort();
        assert_eq!(
            names,
            [
                "api_request",
                "api_search",
                DISABLE_TOOLSET_TOOL,
                ENABLE_TOOLSET_TOOL,
                LIST_TOOLSETS_TOOL
            ]
        );
        assert!(reg.enabled_toolsets().is_empty());
        assert!(reg.is_dynamic());
        // `api_request` — по уровню сервера, остальные служебные — чтение.
        for tool in reg.iter().filter(|t| t.tool.name != "api_request") {
            assert_eq!(tool.access, AccessLevel::Read);
            let annotations = tool.tool.annotations.as_ref().unwrap();
            assert_eq!(annotations.read_only_hint, Some(true), "{}", tool.tool.name);
        }

        let started = registry(AccessLevel::Read, Some(&["reviews"]));
        assert_eq!(started.enabled_toolsets(), ["reviews"]);
        assert!(started.get("list_reviews").is_some());
        assert!(started.get("get_app_info").is_none());
    }

    #[test]
    fn list_shows_counts_and_state() {
        let reg = registry(AccessLevel::Read, Some(&["reviews"]));
        let out = parse(&list(&JsonObject::new(), &reg).unwrap());
        let toolsets = out["toolsets"].as_array().unwrap();
        assert_eq!(toolsets.len(), available_toolsets().len());
        let reviews = toolsets.iter().find(|t| t["toolset"] == "reviews").unwrap();
        assert_eq!(reviews["enabled"], true);
        assert!(reviews["tools"].as_u64().unwrap() > 0);
        assert!(reviews["above_access_level"].as_u64().unwrap() >= 1);
        let apps = toolsets.iter().find(|t| t["toolset"] == "apps").unwrap();
        assert_eq!(apps["enabled"], false);
        assert!(out["note"].as_str().unwrap().contains("write access"));

        let err = list(&args(json!({"x": 1})), &reg).unwrap_err();
        assert!(err.message().contains("no arguments"));
    }

    #[test]
    fn enable_and_disable_keep_the_access_level() {
        let reg = registry(AccessLevel::Read, None);
        let toggled = toggle(&reg, &args(json!({"toolset": "reviews"})), true).unwrap();
        let next = toggled.registry.unwrap();
        assert_eq!(next.max_access(), AccessLevel::Read);
        assert!(next.get("list_reviews").is_some());
        assert!(next.get("reply_review").is_none(), "выше уровня доступа");
        assert!(
            next.unavailable_reason("reply_review")
                .unwrap()
                .contains("destructive access")
        );
        let out = parse(&toggled.text);
        assert_eq!(out["enabled"], json!(["reviews"]));
        let added = out["added_tools"].as_array().unwrap();
        assert!(added.contains(&json!("list_reviews")));
        assert!(!added.contains(&json!("reply_review")));

        // Повторное включение ничего не меняет.
        let again = toggle(&next, &args(json!({"toolset": "reviews"})), true).unwrap();
        assert!(again.registry.is_none());
        assert_eq!(parse(&again.text)["already_enabled"], json!(["reviews"]));

        let off = toggle(&next, &args(json!({"toolset": " reviews ,apps"})), false).unwrap();
        let after = off.registry.unwrap();
        assert!(after.get("list_reviews").is_none());
        let out = parse(&off.text);
        assert_eq!(out["disabled"], json!(["reviews"]));
        assert_eq!(out["not_enabled"], json!(["apps"]));
        assert_eq!(out["enabled_toolsets"], json!([]));

        // Из набора на уровне чтения добавляются только инструменты чтения.
        let listing = toggle(&reg, &args(json!({"toolset": "listing"})), true).unwrap();
        let out = parse(&listing.text);
        assert_eq!(out["added_tools"], json!(["listing_export"]));
        let registry = listing.registry.unwrap();
        assert!(registry.get("listing_apply").is_none());
        assert!(registry.get("update_app_language_info").is_none());
        assert_eq!(registry.max_access(), AccessLevel::Read);
    }

    #[test]
    fn rejects_bad_arguments() {
        let reg = registry(AccessLevel::Read, None);
        for (input, expected) in [
            (json!({}), "missing required argument"),
            (json!({"toolset": "repos"}), "unknown toolset `repos`"),
            (json!({"toolset": " , "}), "is empty"),
            (json!({"toolset": 1}), "must be a string"),
            (json!({"toolset": "apps", "x": 1}), "unknown argument `x`"),
        ] {
            let err = toggle(&reg, &args(input.clone()), true).unwrap_err();
            assert!(err.message().contains(expected), "{input}: {err}");
        }
    }

    #[test]
    fn disabled_reason_points_to_enable_toolset() {
        let reg = registry(AccessLevel::Read, None);
        let reason = reg.unavailable_reason("list_reviews").unwrap();
        assert!(reason.contains("`enable_toolset`"), "{reason}");
        assert!(reason.contains("toolset: \"reviews\""), "{reason}");
        let reason = reg.unavailable_reason("update_app_info").unwrap();
        assert!(reason.contains("write access"), "{reason}");
    }

    #[test]
    fn defaults_come_from_the_spec() {
        assert_eq!(default_toolsets(), ["apps", "releases", "reviews"]);
        let all = available_toolsets();
        assert_eq!(
            all.iter().map(|t| t.operations).sum::<usize>(),
            operations().len()
        );
        assert!(all.iter().all(|t| t.operations > 0), "{all:?}");
        assert_eq!(
            all.iter().filter(|t| t.default).count(),
            default_toolsets().len()
        );
    }

    #[test]
    fn normalize_validates_names() {
        assert_eq!(
            normalize_toolsets(&[" apps ", "", "reviews", "apps"]).unwrap(),
            ["apps", "reviews"]
        );
        let err = normalize_toolsets(&["Выпуски"]).unwrap_err();
        assert!(err.contains("unknown toolset `Выпуски`"), "{err}");
        assert!(
            err.contains("provisioning") && err.contains("`all`"),
            "{err}"
        );
        let all = normalize_toolsets(&["apps", "all"]).unwrap();
        assert_eq!(all.len(), toolsets().len());
        assert!(normalize_toolsets::<&str>(&[]).unwrap().is_empty());
    }

    #[test]
    fn effective_falls_back_to_defaults() {
        assert_eq!(effective_toolsets(None), default_toolsets());
        assert_eq!(effective_toolsets(Some(&[])), default_toolsets());
        let chosen = vec!["listing".to_owned()];
        assert_eq!(effective_toolsets(Some(&chosen)), ["listing"]);
    }
}
