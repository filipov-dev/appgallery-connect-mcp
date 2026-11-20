//! Инструмент `api_search`: поиск по всем операциям AppGallery Connect API без обращения к
//! API.
//!
//! Ищет среди инструментов операций (в том числе незарегистрированных на этом сервере: набор
//! не включён или уровень доступа выше), исключённых операций (получение токена, шаги
//! загрузки; вызываемые из них — через `api_request`) и составных инструментов. Каждый
//! результат говорит, можно ли вызвать его здесь, а если нет — почему и что его включает:
//! флаг уровня доступа, набор (`--toolsets`; текст причины даёт
//! [`ToolRegistry::unavailable_reason`]) или вызов через `api_request`. У исключённых
//! операций — `note` (почему исключена) и `alternative` (что вызвать вместо).
//!
//! Слова запроса сравниваются с именем инструмента, `operationId`, кратким описанием и
//! описанием, сегментами пути (camelCase и дефисы разбиваются на слова), набором и группой
//! API. Русские слова переводятся словарём [`ru::RU_EN`] («отзыв» → `review`, «выпуск» →
//! `release`, «товар» → `product`…), английские сравниваются по началу слова, поэтому
//! `review` находит и `reviews`, и `reply_review`.
//!
//! С аргументом `tool_name` возвращается полное описание одного инструмента со схемой
//! аргументов (или исключённой операции по `operationId`).

mod ru;

use std::sync::Arc;

use appgallery_connect_api::spec::{
    AccessLevel, ExcludedOperationDef, OperationDef, excluded_operations, find, find_excluded,
    find_group, find_toolset, operations, toolsets,
};
use rmcp::model::{JsonObject, Tool};
use serde_json::{Value, json};

use crate::composite::CompositeTool;
use crate::error::ToolError;
use crate::registry::{
    ToolRegistry, access_action, access_name, annotations, composite_tool, operation_tool,
};
use crate::request::API_REQUEST_TOOL;
use crate::validate::kind;

pub use ru::RU_EN;

/// Имя инструмента.
pub const API_SEARCH_TOOL: &str = "api_search";

/// Результатов по умолчанию и наибольшее число результатов.
const DEFAULT_LIMIT: usize = 15;
const MAX_LIMIT: usize = 50;

const ARGS: &[&str] = &["query", "tool_name", "toolset", "limit"];

/// Описание инструмента.
pub fn api_search_tool(http: bool) -> Tool {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "query": {
                "type": "string",
                "description": "Words to search for, in English or Russian, for example \
                    `phased release`, `reply review`, `subscription promotion`, `отзывы`, \
                    `поэтапный выпуск`, `цены товаров`, or a path such as \
                    `/api/publish/v2/app-info`."
            },
            "tool_name": {
                "type": "string",
                "description": "Return the full description and argument schema of this tool \
                    (registered here or not), or of an operation by its `operationId`, instead \
                    of searching."
            },
            "toolset": {
                "type": "string",
                "description": "Search only in this toolset."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_LIMIT,
                "description": "Maximum number of results (default 15)."
            }
        }
    });
    let schema = match schema {
        Value::Object(map) => map,
        _ => JsonObject::new(),
    };
    let enables = if http {
        "an access token with a higher level or other toolsets from the server operator, or a \
         call with `api_request`"
    } else {
        "the --allow-write or --allow-destructive flag, the --toolsets option, or a call with \
         `api_request`"
    };
    let description = format!(
        "Search all AppGallery Connect API operations by words in English or Russian (tool \
         names, operation IDs, summaries, paths, toolsets), including tools that are not \
         enabled on this server and operations without a tool. Each result gives the toolset, \
         the access level and whether it can be called here; if not, why and what enables it \
         ({enables}). Excluded operations come with a `note` and the `alternative` to use. \
         Pass `tool_name` to get the full description and argument schema of one tool. Does \
         not call the API."
    );
    let mut tool = Tool::new(API_SEARCH_TOOL, description, Arc::new(schema));
    tool.title = Some("Search AppGallery Connect API operations".to_owned());
    tool.annotations = Some(annotations(
        "Search AppGallery Connect API operations",
        AccessLevel::Read,
        None,
    ));
    tool
}

/// Запись поиска.
enum Entry<'a> {
    Operation(&'static OperationDef),
    Excluded(&'static ExcludedOperationDef),
    Composite(&'a CompositeTool),
}

impl Entry<'_> {
    /// Длина имени и пути: при равном весе первыми идут более общие записи.
    fn size(&self) -> usize {
        match self {
            Self::Operation(op) => op.tool_name.len() + op.path.len(),
            Self::Excluded(op) => op.operation_id.len() + op.path.len(),
            Self::Composite(tool) => tool.name.len(),
        }
    }

    fn toolset(&self) -> &str {
        match self {
            Self::Operation(op) => op.toolset,
            Self::Excluded(op) => op.toolset,
            Self::Composite(tool) => tool.toolset,
        }
    }

    /// Поля для поиска с весами: имя важнее пути, путь важнее описания.
    fn fields(&self) -> Vec<(String, u32)> {
        match self {
            Self::Operation(op) => vec![
                (op.tool_name.replace('_', " "), 4),
                (split_words(op.operation_id), 2),
                (split_words(op.path), 3),
                (op.summary.to_owned(), 2),
                (op.description.to_owned(), 1),
                (op.toolset.to_owned(), 1),
                (group_words(op.group), 1),
            ],
            Self::Excluded(op) => vec![
                (split_words(op.operation_id), 3),
                (split_words(op.path), 3),
                (op.summary.to_owned(), 2),
                (op.note.to_owned(), 1),
                (op.toolset.to_owned(), 1),
                (group_words(op.group), 1),
            ],
            Self::Composite(tool) => vec![
                (tool.name.replace('_', " "), 4),
                (first_sentence(tool.description).to_owned(), 2),
                (tool.description.to_owned(), 1),
                (tool.toolset.to_owned(), 1),
            ],
        }
    }
}

/// Slug и название группы API: `pms PMS API`.
fn group_words(slug: &str) -> String {
    match find_group(slug) {
        Some(group) => format!("{} {}", group.slug, group.title),
        None => slug.to_owned(),
    }
}

fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    match text.find(". ") {
        Some(end) => &text[..=end],
        None => text,
    }
}

/// Выполняет `api_search`.
pub fn search(args: &JsonObject, registry: &ToolRegistry) -> Result<String, ToolError> {
    for key in args.keys() {
        if !ARGS.contains(&key.as_str()) {
            return Err(ToolError::new(format!(
                "unknown argument `{key}`; allowed arguments: `query`, `tool_name`, `toolset`, \
                 `limit`"
            )));
        }
    }
    let text_arg = |name: &str| -> Result<Option<String>, ToolError> {
        match args.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.trim().to_owned())),
            Some(other) => Err(ToolError::new(format!(
                "argument `{name}` must be a string, got {}",
                kind(other)
            ))),
        }
    };
    if let Some(name) = text_arg("tool_name")? {
        return describe_tool(&name, registry);
    }
    let Some(query) = text_arg("query")? else {
        return Err(ToolError::new(
            "pass `query` (words to search for) or `tool_name` (a tool to describe)",
        ));
    };
    let toolset = text_arg("toolset")?;
    if let Some(slug) = toolset.as_deref()
        && find_toolset(slug).is_none()
    {
        return Err(ToolError::new(format!(
            "unknown toolset `{slug}`; available: {}",
            toolsets()
                .iter()
                .map(|t| t.slug)
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let limit = match args.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(value) => value
            .as_u64()
            .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
            .filter(|n| (1..=MAX_LIMIT as u64).contains(n))
            .map(|n| n as usize)
            .ok_or_else(|| {
                ToolError::new(format!(
                    "argument `limit` must be an integer from 1 to {MAX_LIMIT}"
                ))
            })?,
    };

    let terms = terms(&query);
    if terms.is_empty() {
        return Err(ToolError::new(
            "the query has no searchable words; use words such as `review`, `release`, \
             `product`, `report`, `certificate` (or Russian: `отзыв`, `выпуск`, `товар`)",
        ));
    }
    let mut entries: Vec<Entry> = operations().iter().map(Entry::Operation).collect();
    entries.extend(excluded_operations().iter().map(Entry::Excluded));
    entries.extend(registry.composite_tools().iter().map(Entry::Composite));

    let mut scored: Vec<(usize, u32, Entry)> = entries
        .into_iter()
        .filter(|entry| toolset.as_deref().is_none_or(|t| entry.toolset() == t))
        .filter_map(|entry| {
            let (matched, score) = score(&entry, &terms);
            (matched > 0).then_some((matched, score, entry))
        })
        .collect();
    // Сначала записи, где нашлось больше слов запроса, затем по весу.
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then(b.1.cmp(&a.1))
            .then(a.2.size().cmp(&b.2.size()))
    });
    let best = scored.first().map_or(0, |s| s.0);
    let total = scored.iter().filter(|s| s.0 == best).count();
    let results: Vec<Value> = scored
        .iter()
        .filter(|s| s.0 == best)
        .take(limit)
        .map(|(_, _, entry)| describe_entry(entry, registry))
        .collect();
    let mut out = json!({
        "query": query,
        "matches": total,
        "results": results,
    });
    if results.is_empty() {
        insert(
            &mut out,
            "note",
            json!("nothing found; try other words, English terms of the API or a path"),
        );
    } else if best < terms.len() {
        insert(
            &mut out,
            "note",
            json!("no result contains every word of the query; these match some of them"),
        );
    }
    if total > limit {
        insert(
            &mut out,
            "more",
            json!(format!(
                "{} more results; add words, pass `toolset` or raise `limit`",
                total - limit
            )),
        );
    }
    Ok(out.to_string())
}

/// Поисковые термины: русские слова — по словарю, английские — как есть, в нижнем регистре.
/// Каждый элемент — варианты одного слова запроса (достаточно совпадения любого).
fn terms(query: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for word in split_words(query)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let word = word.to_lowercase();
        if word.chars().any(|c| matches!(c, 'а'..='я' | 'ё')) {
            let word = word.replace('ё', "е");
            let stems: Vec<&(&str, &[&str])> = RU_EN
                .iter()
                .filter(|(stem, _)| word.starts_with(&stem.replace('ё', "е")))
                .collect();
            let longest = stems
                .iter()
                .map(|(stem, _)| stem.chars().count())
                .max()
                .unwrap_or(0);
            let mut translated: Vec<String> = Vec::new();
            for (stem, en) in stems {
                if stem.chars().count() == longest {
                    for term in en.iter() {
                        if !translated.iter().any(|t| t == term) {
                            translated.push((*term).to_owned());
                        }
                    }
                }
            }
            if !translated.is_empty() {
                out.push(translated);
            }
            continue;
        }
        if STOP_WORDS.contains(&word.as_str()) || (word.len() < 2 && !word.starts_with('v')) {
            continue;
        }
        out.push(vec![singular(&word)]);
    }
    out
}

/// Служебные слова английского запроса. `api` тоже: он есть в каждом пути.
const STOP_WORDS: &[&str] = &[
    "a", "an", "the", "of", "for", "to", "in", "on", "and", "or", "with", "by", "my", "all", "api",
];

/// Единственное число английского слова (грубо): `reviews` → `review`, `countries` →
/// `countr`.
fn singular(word: &str) -> String {
    if let Some(stem) = word.strip_suffix("ies").filter(|s| s.len() > 2) {
        return stem.to_owned();
    }
    if word.len() > 3 && word.ends_with('s') && !word.ends_with("ss") {
        return word[..word.len() - 1].to_owned();
    }
    word.to_owned()
}

/// Разбивает camelCase и разделители на слова: `appDownloadExport` → `app download export`,
/// `/api/publish/v2/app-info` → ` api publish v2 app info`.
fn split_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut prev_lower = false;
    for c in text.chars() {
        if c.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else {
            out.push(' ');
        }
    }
    out
}

/// Сколько слов запроса нашлось и суммарный вес.
fn score(entry: &Entry, terms: &[Vec<String>]) -> (usize, u32) {
    let fields: Vec<(Vec<String>, String, u32)> = entry
        .fields()
        .into_iter()
        .map(|(text, weight)| {
            let lower = split_words(&text);
            let words = lower.split_whitespace().map(str::to_owned).collect();
            // Слитная запись поля: `productgroup` находит `productGroup`.
            let joined = lower.split_whitespace().collect::<String>();
            (words, joined, weight)
        })
        .collect();
    let mut matched = 0;
    let mut total = 0;
    for variants in terms {
        // Целое слово весит вдвое больше начала слова.
        let best = fields
            .iter()
            .filter_map(|(words, joined, weight)| {
                let exact = variants
                    .iter()
                    .any(|term| words.iter().any(|w| w == term || singular(w) == *term));
                let partial = variants.iter().any(|term| {
                    words.iter().any(|w| w.starts_with(term.as_str()))
                        || (term.len() > 6 && joined.contains(term.as_str()))
                });
                if exact {
                    Some(weight * 2)
                } else if partial {
                    Some(*weight)
                } else {
                    None
                }
            })
            .max();
        if let Some(weight) = best {
            matched += 1;
            total += weight;
        }
    }
    (matched, total)
}

/// Состояние инструмента на этом сервере: `available` или почему не зарегистрирован и что
/// его включает.
fn tool_status(name: &str, registry: &ToolRegistry, api_request: Option<String>) -> String {
    if registry.get(name).is_some() {
        return "available".to_owned();
    }
    let Some(reason) = registry.unavailable_reason(name) else {
        return "not registered".to_owned();
    };
    // Набор не включён, а уровень позволяет: операцию можно вызвать сразу через
    // `api_request` (он наборами не ограничен). Выше уровня сервера `api_request` тоже не
    // поможет.
    match api_request {
        Some(call) if !reason.contains(API_REQUEST_TOOL) => {
            format!("not registered: {reason}; or call it now with `{API_REQUEST_TOOL}` ({call})")
        }
        _ => format!("not registered: {reason}"),
    }
}

/// Как вызвать операцию через `api_request`, если уровень сервера это позволяет.
/// Операции HMS IAP идут на другой хост и подписываются секретом приложения:
/// `api_request` их не вызывает.
fn api_request_call(op: &OperationDef, registry: &ToolRegistry) -> Option<String> {
    (registry.max_access().allows(op.access) && !op.service.is_iap())
        .then(|| format!("{} {}", op.method.as_str(), op.path))
}

fn describe_entry(entry: &Entry, registry: &ToolRegistry) -> Value {
    match entry {
        Entry::Operation(op) => json!({
            "tool": op.tool_name,
            "summary": op.summary,
            "method": op.method.as_str(),
            "path": op.path,
            "operation_id": op.operation_id,
            "group": op.group,
            "toolset": op.toolset,
            "access": op.access.as_str(),
            "status": tool_status(op.tool_name, registry, api_request_call(op, registry)),
        }),
        Entry::Excluded(op) => describe_excluded(op, registry),
        Entry::Composite(tool) => json!({
            "tool": tool.name,
            "summary": first_sentence(tool.description),
            "kind": "composite",
            "toolset": tool.toolset,
            "access": tool.access.as_str(),
            "status": tool_status(tool.name, registry, None),
        }),
    }
}

fn describe_excluded(op: &ExcludedOperationDef, registry: &ToolRegistry) -> Value {
    let status = if !op.callable {
        format!(
            "not callable: neither a tool nor `{API_REQUEST_TOOL}` can call it. {}",
            op.reason.explanation()
        )
    } else if registry.max_access().allows(op.access) {
        format!(
            "no tool: {} Call it with `{API_REQUEST_TOOL}` ({} {})",
            op.reason.explanation(),
            op.method.as_str(),
            op.path
        )
    } else {
        format!(
            "no tool: {} It needs {} access: {}, then call it with `{API_REQUEST_TOOL}`",
            op.reason.explanation(),
            access_name(op.access),
            access_action(op.access, registry.http())
        )
    };
    let mut value = json!({
        "operation_id": op.operation_id,
        "summary": op.summary,
        "method": op.method.as_str(),
        "path": op.path,
        "group": op.group,
        "access": op.access.as_str(),
        "excluded": op.reason.as_str(),
        "callable": op.callable,
        "status": status,
    });
    if !op.toolset.is_empty() {
        insert(&mut value, "toolset", json!(op.toolset));
    }
    if !op.note.is_empty() {
        insert(&mut value, "note", json!(op.note));
    }
    if !op.alternative.is_empty() {
        insert(&mut value, "alternative", json!(op.alternative));
    }
    value
}

fn insert(value: &mut Value, key: &str, item: Value) {
    if let Value::Object(map) = value {
        map.insert(key.to_owned(), item);
    }
}

/// Полное описание одного инструмента (`tool_name`): зарегистрированного, известного, но не
/// зарегистрированного, или операции по `operationId`.
fn describe_tool(name: &str, registry: &ToolRegistry) -> Result<String, ToolError> {
    // `operationId` инструмента операции — то же, что его имя.
    let op = find(name).or_else(|| operations().iter().find(|op| op.operation_id == name));
    let (tool, status) = if let Some(registered) = registry.get(name) {
        (registered.tool.clone(), "available".to_owned())
    } else if let Some(op) = op {
        let tool = operation_tool(op).map_err(ToolError::new)?;
        let status = tool_status(op.tool_name, registry, api_request_call(op, registry));
        (tool, status)
    } else if let Some(composite) = registry.composite_tools().iter().find(|t| t.name == name) {
        let tool = composite_tool(composite).map_err(ToolError::new)?;
        (tool, tool_status(name, registry, None))
    } else if let Some(excluded) = find_excluded(name) {
        return Ok(describe_excluded(excluded, registry).to_string());
    } else {
        return Err(ToolError::new(format!(
            "no tool named `{name}`; search with `query` to find the right name"
        )));
    };
    let mut value = json!({
        "tool": tool.name,
        "status": status,
        "description": tool.description,
        "input_schema": Value::Object((*tool.input_schema).clone()),
    });
    if let Some(op) = op {
        insert(&mut value, "method", json!(op.method.as_str()));
        insert(&mut value, "path", json!(op.path));
        insert(&mut value, "operation_id", json!(op.operation_id));
        insert(&mut value, "group", json!(op.group));
        insert(&mut value, "toolset", json!(op.toolset));
        insert(&mut value, "access", json!(op.access.as_str()));
    }
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ServerOptions;

    fn registry(access: AccessLevel, toolsets: Option<&[&str]>) -> ToolRegistry {
        ToolRegistry::new(&ServerOptions {
            max_access: access,
            toolsets: toolsets.map(|t| t.iter().map(|s| (*s).to_owned()).collect()),
            ..ServerOptions::default()
        })
        .unwrap()
    }

    fn run(registry: &ToolRegistry, args: Value) -> Value {
        let Value::Object(args) = args else {
            panic!("не объект");
        };
        serde_json::from_str(&search(&args, registry).unwrap()).unwrap()
    }

    fn tools(result: &Value) -> Vec<String> {
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["tool"].as_str().map(str::to_owned))
            .collect()
    }

    fn entry<'a>(result: &'a Value, tool: &str) -> &'a Value {
        result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["tool"] == tool)
            .unwrap_or_else(|| panic!("нет {tool}: {result}"))
    }

    #[test]
    fn finds_operations_by_english_words() {
        let registry = registry(AccessLevel::Read, None);
        for (query, expected) in [
            ("list reviews", "list_reviews"),
            ("reply review", "reply_review"),
            ("phased release", "update_phased_release"),
            ("release time", "update_release_time"),
            ("app ids package names", "list_app_ids"),
            ("compile status", "get_package_compile_status"),
            ("subscription groups", "list_product_groups"),
            ("promotions", "search_promotions"),
            ("certificates", "list_certificates"),
            ("devices", "list_devices"),
            (
                "installation failure report",
                "get_install_failure_report_url",
            ),
            ("IAPExport", "get_iap_report_url"),
            ("comments.review.get", "get_review"),
            ("PMS product", "get_product"),
        ] {
            let result = run(&registry, json!({"query": query, "limit": 50}));
            assert!(
                tools(&result).contains(&expected.to_owned()),
                "{query}: {result}"
            );
        }
    }

    #[test]
    fn finds_operations_by_paths() {
        let registry = registry(AccessLevel::Read, None);
        for (query, expected) in [
            ("/api/publish/v2/app-info", "get_app_info"),
            ("/api/reviews/v1/manage/dev/reviews", "list_reviews"),
            (
                "/api/pms/product-price-service/v1/manage/product/promotion",
                "get_promotion",
            ),
            ("on-shelf-time", "update_release_time"),
        ] {
            let result = run(&registry, json!({"query": query, "limit": 50}));
            assert!(
                tools(&result).contains(&expected.to_owned()),
                "{query}: {result}"
            );
            assert!(result.get("note").is_none(), "{query}: {result}");
        }
    }

    #[test]
    fn finds_operations_by_russian_words() {
        let registry = registry(AccessLevel::Read, None);
        for (query, expected) in [
            ("отзывы", "review"),
            ("список отзывов", "list_reviews"),
            ("ответить на отзыв", "reply_review"),
            ("оценки приложения", "list_ratings"),
            ("поэтапный выпуск", "update_phased_release"),
            ("время релиза", "update_release_time"),
            ("отправить на проверку", "submit_app"),
            ("идентификатор приложения по имени пакета", "list_app_ids"),
            ("статус компиляции пакета", "get_package_compile_status"),
            ("список пакетов", "list_packages"),
            ("страница приложения", "app_language_info"),
            ("перевод описания", "update_app_language_info"),
            ("удалить язык", "delete_app_language_info"),
            ("скриншоты и иконка", "update_app_file_info"),
            ("отчёт об установках", "get_download_report_url"),
            ("отчет о покупках", "get_iap_report_url"),
            ("товары", "product"),
            ("цены товаров", "update_product"),
            ("деактивировать товары", "deactivate_products"),
            ("активные товары", "check_active_products"),
            ("группы подписок", "list_product_groups"),
            ("акции", "promotion"),
            ("сертификаты", "certificate"),
            ("удалить сертификат", "delete_certificates"),
            ("профили", "profile"),
            ("устройства", "device"),
            ("проекты", "project"),
            ("команды", "list_teams"),
            ("файл конфигурации", "get_app_config_file"),
            ("отпечатки сертификата", "fingerprint"),
            ("зависимость от гугл", "update_gms_dependency"),
            ("удалённая конфигурация", "remote_config"),
            ("откатить конфигурацию", "rollback_remote_config"),
            ("история версий конфигурации", "list_remote_config_versions"),
            ("изменить параметр конфигурации", "update_remote_config"),
        ] {
            let result = run(&registry, json!({"query": query, "limit": 50}));
            assert!(
                tools(&result).iter().any(|t| t.contains(expected)),
                "{query}: {result}"
            );
        }
        // Самая длинная основа: «подписки» — не подпись, «деактивировать» — не активация.
        assert_eq!(terms("подписки"), [vec!["subscription".to_owned()]]);
        assert_eq!(terms("подписок"), [vec!["subscription".to_owned()]]);
        assert_eq!(terms("деактивировать"), [vec!["deactivate".to_owned()]]);
        assert_eq!(
            terms("подпись"),
            [vec![
                "certificate".to_owned(),
                "cert".to_owned(),
                "signing".to_owned()
            ]]
        );
        // «ё» и «е» равны.
        assert_eq!(terms("отчёты"), terms("отчеты"));
    }

    #[test]
    fn finds_purchase_operations_without_api_request() {
        let registry = registry(AccessLevel::Read, Some(&["apps"]));
        for (query, expected) in [
            ("проверить покупку", "verify_product_purchase"),
            ("подтвердить доставку покупки", "confirm_product_purchase"),
            ("возвраты покупок", "list_cancelled_purchases"),
            ("заказы покупок", "list_iap_orders"),
            ("отменить подписку", "cancel_subscription"),
            ("отложить подписку", "defer_subscription"),
            ("возврат за подписку", "refund_subscription"),
            ("отозвать подписку", "revoke_subscription"),
            ("verify purchase token", "verify_product_purchase"),
        ] {
            let result = run(&registry, json!({"query": query, "limit": 50}));
            assert!(
                tools(&result).contains(&expected.to_owned()),
                "{query}: {result}"
            );
        }
        // Набор не включён, но `api_request` операции HMS IAP не вызывает.
        let result = run(&registry, json!({"query": "verify purchase token"}));
        let found = entry(&result, "verify_product_purchase");
        assert_eq!(found["toolset"], "purchases");
        let status = found["status"].as_str().unwrap();
        assert!(status.contains("toolset `purchases`"), "{status}");
        assert!(!status.contains("api_request"), "{status}");

        // REST API IAP Kit (HarmonyOS 5+) находится, но не вызывается.
        for query in ["HarmonyOS order status", "статус подписки HarmonyOS"] {
            let result = run(&registry, json!({"query": query, "limit": 50}));
            let kit = result["results"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["group"] == "iapkit")
                .unwrap_or_else(|| panic!("{query}: {result}"));
            assert_eq!(kit["callable"], false, "{kit}");
            assert!(
                kit["status"].as_str().unwrap().starts_with("not callable"),
                "{kit}"
            );
            assert!(
                kit["alternative"]
                    .as_str()
                    .unwrap()
                    .contains("purchases toolset"),
                "{kit}"
            );
        }
    }

    #[test]
    fn explains_what_enables_a_result() {
        let registry = registry(AccessLevel::Read, Some(&["apps", "reviews"]));
        // Уровень доступа выше сервера: флаг, без `api_request`.
        let result = run(&registry, json!({"query": "reply review"}));
        let found = entry(&result, "reply_review");
        assert_eq!(found["toolset"], "reviews");
        assert_eq!(found["access"], "destructive");
        let status = found["status"].as_str().unwrap();
        assert!(status.contains("--allow-destructive"), "{status}");
        assert!(!status.contains("call it now"), "{status}");

        let result = run(&registry, json!({"query": "update app info"}));
        let status = entry(&result, "update_app_info")["status"]
            .as_str()
            .unwrap();
        assert!(status.contains("--allow-write"), "{status}");

        // Набор не включён: `--toolsets` или сразу `api_request`.
        let result = run(&registry, json!({"query": "list packages"}));
        let status = entry(&result, "list_packages")["status"].as_str().unwrap();
        assert!(
            status.contains("toolset `releases`")
                && status.contains("--toolsets")
                && status.contains("`api_request` (GET /api/publish/v2/package-list)"),
            "{status}"
        );

        let result = run(&registry, json!({"query": "list reviews"}));
        assert_eq!(entry(&result, "list_reviews")["status"], "available");
    }

    #[test]
    fn http_mode_asks_for_an_access_token_instead_of_flags() {
        use appgallery_connect_api::spec::{ExclusionReason, HttpMethod};

        let registry = ToolRegistry::new(&ServerOptions {
            max_access: AccessLevel::Read,
            toolsets: Some(vec!["apps".to_owned(), "reviews".to_owned()]),
            http: true,
            ..ServerOptions::default()
        })
        .unwrap();
        let result = run(&registry, json!({"query": "reply review"}));
        let status = entry(&result, "reply_review")["status"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            status.contains("access token with the destructive level"),
            "{status}"
        );
        assert!(!status.contains("--allow-"), "{status}");

        // Операция без инструмента, которую можно вызвать только через `api_request`.
        let above = ExcludedOperationDef {
            operation_id: "test.excluded.update",
            group: "publishing",
            toolset: "apps",
            summary: "Test operation",
            method: HttpMethod::Put,
            path: "/api/publish/v2/test",
            access: AccessLevel::Write,
            reason: ExclusionReason::Overlay,
            callable: true,
            note: "",
            alternative: "",
        };
        let status = describe_excluded(&above, &registry)["status"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(status.contains("server operator"), "{status}");
        assert!(!status.contains("--allow-"), "{status}");

        let description = api_search_tool(true).description.unwrap();
        assert!(description.contains("access token"), "{description}");
        assert!(!description.contains("--allow-"), "{description}");
        assert!(
            api_search_tool(false)
                .description
                .unwrap()
                .contains("--allow-write")
        );
    }

    #[test]
    fn finds_excluded_operations_with_note_and_alternative() {
        let registry = registry(AccessLevel::Destructive, Some(&["all"]));
        let result = run(&registry, json!({"query": "multipart upload", "limit": 50}));
        let excluded: Vec<&Value> = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["excluded"] == "overlay")
            .collect();
        assert!(!excluded.is_empty(), "{result}");
        for found in excluded {
            assert_eq!(found["callable"], false, "{found}");
            assert!(
                found["note"].as_str().unwrap().contains("one-time"),
                "{found}"
            );
            assert!(
                found["alternative"]
                    .as_str()
                    .unwrap()
                    .contains("package_upload"),
                "{found}"
            );
            assert!(
                found["status"]
                    .as_str()
                    .unwrap()
                    .starts_with("not callable"),
                "{found}"
            );
        }

        let result = run(&registry, json!({"query": "токен"}));
        let token = result["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["operation_id"] == "auth.token.create")
            .unwrap_or_else(|| panic!("{result}"));
        assert!(token.get("toolset").is_none(), "{token}");
        assert_eq!(token["callable"], false);

        let result = run(&registry, json!({"tool_name": "upload.uploadUrl.get"}));
        assert_eq!(result["operation_id"], "upload.uploadUrl.get");
        assert_eq!(result["toolset"], "releases");
        assert!(result["alternative"].as_str().is_some(), "{result}");
    }

    #[test]
    fn describes_one_tool() {
        let registry = registry(AccessLevel::Read, None);
        let result = run(&registry, json!({"tool_name": "update_app_language_info"}));
        assert!(result["status"].as_str().unwrap().contains("--allow-write"));
        assert!(result["input_schema"]["properties"].get("body").is_some());
        assert!(result["input_schema"]["properties"].get("appId").is_some());
        assert_eq!(result["method"], "PUT");
        assert_eq!(result["path"], "/api/publish/v2/app-language-info");
        assert_eq!(result["toolset"], "listing");
        assert_eq!(result["access"], "write");

        // По `operationId` — то же описание.
        let by_id = run(
            &registry,
            json!({"tool_name": "publishing.languageInfo.update"}),
        );
        assert_eq!(by_id["tool"], "update_app_language_info");

        let result = run(&registry, json!({"tool_name": "list_reviews"}));
        assert_eq!(result["status"], "available");
        let result = run(&registry, json!({"tool_name": "api_request"}));
        assert_eq!(result["status"], "available");
        assert!(
            result["input_schema"]["properties"]
                .get("headers")
                .is_some()
        );
    }

    #[test]
    fn filters_limits_and_rejects_bad_arguments() {
        let registry = registry(AccessLevel::Read, None);
        let result = run(
            &registry,
            json!({"query": "get", "toolset": "projects", "limit": 50}),
        );
        assert!(!tools(&result).is_empty());
        for found in result["results"].as_array().unwrap() {
            assert_eq!(found["toolset"], "projects", "{found}");
        }

        let result = run(&registry, json!({"query": "product", "limit": 2}));
        assert_eq!(result["results"].as_array().unwrap().len(), 2);
        assert!(result["matches"].as_u64().unwrap() > 2);
        assert!(result["more"].as_str().unwrap().contains("raise `limit`"));

        let result = run(&registry, json!({"query": "reviews nonexistentword"}));
        assert!(result["note"].as_str().unwrap().contains("some of them"));

        for bad in [
            json!({"query": "x", "nope": 1}),
            json!({}),
            json!({"query": 1}),
            json!({"tool_name": "no_such_tool"}),
            json!({"query": "review", "toolset": "nope"}),
            json!({"query": "review", "limit": 0}),
            json!({"query": "review", "limit": 51}),
            json!({"query": "!!!"}),
            json!({"query": "абракадабра"}),
        ] {
            let Value::Object(bad) = bad else {
                unreachable!()
            };
            assert!(search(&bad, &registry).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn splits_words() {
        assert_eq!(
            split_words("appDownloadExport")
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["app", "download", "export"]
        );
        assert_eq!(singular("reviews"), "review");
        assert_eq!(singular("countries"), "countr");
        assert_eq!(singular("access"), "access");
    }
}
