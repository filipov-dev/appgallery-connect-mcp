//! Пользовательские инструменты: операции AppGallery Connect API с фиксированными
//! аргументами и фильтром `jq` из `tools.toml`.
//!
//! Файл читает бинарник (`serve`, `tools check`) и передаёт разобранные [`CustomToolDef`]
//! сюда: [`CustomTool::compile`] проверяет определение. Инструмент — это одна операция из
//! таблицы операций (`operation` — имя инструмента операции, например `list_reviews`, или её
//! `operationId`, например `comments.review.list`), поэтому у него те же границы, что у
//! инструмента этой операции:
//!
//! - уровень доступа и набор — уровень и набор операции: инструмент выше уровня сервера
//!   или вне включённых наборов не регистрируется (вызов объясняет, какой флаг или набор
//!   нужен), в динамическом режиме он включается вместе со своим набором;
//! - аргументы проверяются так же, как у инструмента операции
//!   ([`crate::validate::check_operation_arguments`]), ответ проходит ту же постобработку;
//! - фиксированные аргументы (`args`) проверяются при загрузке файла, из схемы инструмента
//!   убираются, а переданный агентом фиксированный аргумент — ошибка: агент не может
//!   подменить то, что задал пользователь;
//! - фильтр `jq` определения применяется к ответу; фильтр из вызова агента применяется
//!   к его результату (`(фильтр файла) | (фильтр вызова)`).
//!
//! Исключённые операции (их нет среди инструментов) пользовательскими инструментами не
//! бывают. Инструмента над произвольным путём API (без операции) тоже нет: его место —
//! будущий `api_request`.
//!
//! Имя не должно совпадать со встроенными инструментами (всеми операциями, а не только
//! зарегистрированными, служебными инструментами ядра и именами из `reserved` — составными
//! инструментами) и с другими пользовательскими инструментами: смысл файла не зависит от
//! флагов сервера.

use std::collections::HashSet;
use std::sync::Arc;

use appgallery_connect_api::spec::{AccessLevel, OperationDef, find, find_excluded, operations};
use rmcp::model::{JsonObject, Tool};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::ToolError;
use crate::jq::{JQ_ARG, JqFilter, take_jq, takes_jq, truncate_text};
use crate::registry::{MAX_TOOL_NAME_LEN, RESERVED_TOOL_NAMES, access_description, operation_tool};
use crate::validate::check_fixed_argument;

/// Наибольшая длина значения фиксированного аргумента в описании инструмента.
const MAX_SHOWN_VALUE: usize = 200;

/// Содержимое файла `tools.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomToolsFile {
    #[serde(default)]
    pub tools: Vec<CustomToolDef>,
}

/// Определение инструмента в файле.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomToolDef {
    /// Имя MCP-инструмента.
    pub name: String,
    /// Описание для агента: что делает инструмент и когда его вызывать.
    pub description: String,
    /// Операция: имя инструмента операции (`list_reviews`) или `operationId`
    /// (`comments.review.list`).
    pub operation: String,
    /// Фиксированные аргументы операции: те же имена, что у инструмента операции
    /// (`appId`, `lang`, `body`, ...).
    #[serde(default)]
    pub args: Map<String, Value>,
    /// Фильтр ответа (см. [`crate::jq`]).
    #[serde(default)]
    pub jq: Option<String>,
}

/// Проверенный пользовательский инструмент.
#[derive(Debug, Clone)]
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub operation: &'static OperationDef,
    fixed: JsonObject,
    jq: Option<JqFilter>,
    /// Откуда определение (путь к файлу) — для сообщений об ошибках и `tools list`.
    pub source: String,
}

/// Операция по имени инструмента или `operationId`.
fn find_operation(name: &str) -> Option<&'static OperationDef> {
    find(name).or_else(|| operations().iter().find(|op| op.operation_id == name))
}

impl CustomTool {
    /// Проверяет определение. Ошибка называет файл и инструмент.
    pub fn compile(def: CustomToolDef, source: &str) -> Result<Self, String> {
        let name = def.name.trim().to_owned();
        check_identifier(&name).map_err(|err| format!("{source}: {err}"))?;
        let fail = |message: String| format!("{source}: tool `{name}`: {message}");
        let description = def.description.trim().to_owned();
        if description.is_empty() {
            return Err(fail("`description` is empty".into()));
        }
        let operation_name = def.operation.trim();
        let Some(operation) = find_operation(operation_name) else {
            if let Some(excluded) = find_excluded(operation_name) {
                let mut why = format!(
                    "operation `{operation_name}` is not available as a tool, so it cannot be \
                     a custom tool"
                );
                if !excluded.note.is_empty() {
                    why.push_str(": ");
                    why.push_str(excluded.note.trim_end_matches('.'));
                }
                if !excluded.alternative.is_empty() {
                    why.push_str("; ");
                    why.push_str(excluded.alternative.trim_end_matches('.'));
                }
                return Err(fail(why));
            }
            return Err(fail(format!(
                "unknown operation `{operation_name}`; use the tool name of an AppGallery \
                 Connect API operation (e.g. `list_reviews`) or its operationId (e.g. \
                 `comments.review.list`), see `appgallery-connect-mcp tools list --toolsets all \
                 --allow-destructive`"
            )));
        };
        let schema: JsonObject = serde_json::from_str(operation.input_schema)
            .map_err(|err| fail(format!("invalid operation schema: {err}")))?;
        for (arg, value) in &def.args {
            if arg == JQ_ARG {
                return Err(fail(format!(
                    "`{JQ_ARG}` is not an operation argument; set the filter with the `jq` key \
                     of the tool"
                )));
            }
            check_fixed_argument(operation, &schema, arg, value)
                .map_err(|err| fail(format!("`args`: {}", err.message())))?;
        }
        let jq = match def.jq.as_deref() {
            Some(_) if !takes_jq(operation) => {
                return Err(fail(format!(
                    "`jq` is not supported for `{}`: its response is empty",
                    operation.tool_name
                )));
            }
            Some(code) => Some(
                JqFilter::parse(code).map_err(|err| fail(format!("`jq`: {}", err.message())))?,
            ),
            None => None,
        };
        Ok(Self {
            name,
            description,
            operation,
            fixed: def.args,
            jq,
            source: source.to_owned(),
        })
    }

    /// Уровень доступа инструмента — уровень его операции.
    pub fn access(&self) -> AccessLevel {
        self.operation.access
    }

    /// Набор инструмента — набор его операции.
    pub fn toolset(&self) -> &'static str {
        self.operation.toolset
    }

    /// Фиксированные аргументы.
    pub fn fixed_args(&self) -> &JsonObject {
        &self.fixed
    }

    /// Фильтр ответа из определения.
    pub fn jq(&self) -> Option<&JqFilter> {
        self.jq.as_ref()
    }

    /// Описание MCP-инструмента: схема инструмента операции (с `jq`) без фиксированных
    /// аргументов, аннотации — по уровню операции.
    pub fn tool(&self) -> Result<Tool, String> {
        let op = self.operation;
        let base = operation_tool(op)?;
        let mut schema = (*base.input_schema).clone();
        if let Some(Value::Object(properties)) = schema.get_mut("properties") {
            for arg in self.fixed.keys() {
                properties.remove(arg);
            }
            // Свой фильтр определения не сочетается с `save_to`: файл сохраняется как есть.
            if self.jq.is_some() {
                properties.remove(crate::files::SAVE_TO_ARG);
            }
        }
        let mut drop_required = false;
        if let Some(Value::Array(required)) = schema.get_mut("required") {
            required.retain(|item| {
                item.as_str()
                    .is_none_or(|name| !self.fixed.contains_key(name))
            });
            drop_required = required.is_empty();
        }
        if drop_required {
            schema.remove("required");
        }

        let mut description = format!(
            "{}\n\nCustom tool defined by the user: calls `{}` ({} {})",
            self.description,
            op.tool_name,
            op.method.as_str(),
            op.path
        );
        if !self.fixed.is_empty() {
            let fixed: Vec<String> = self
                .fixed
                .iter()
                .map(|(name, value)| format!("`{name}` = {}", shown_value(value)))
                .collect();
            description.push_str(&format!(
                " with fixed arguments {} (they cannot be passed)",
                fixed.join(", ")
            ));
        }
        if let Some(jq) = &self.jq {
            description.push_str(&format!(
                "; the response is filtered with `{}` before your own `{JQ_ARG}` filter",
                jq.as_str()
            ));
        }
        description.push('.');
        description.push_str("\n\nAccess: ");
        description.push_str(access_description(op.access));

        let mut tool = Tool::new(self.name.clone(), description, Arc::new(schema));
        tool.annotations = base.annotations.map(|mut annotations| {
            annotations.title = None;
            annotations
        });
        Ok(tool)
    }

    /// Аргументы операции для вызова (аргументы агента плюс фиксированные) и итоговый
    /// фильтр: фильтр определения, затем фильтр из вызова.
    pub fn build_call(
        &self,
        mut args: JsonObject,
    ) -> Result<(JsonObject, Option<JqFilter>), ToolError> {
        // У операций с пустым ответом аргумента `jq` нет: его отклонит проверка аргументов.
        let call_jq = if takes_jq(self.operation) {
            take_jq(&mut args)?
        } else {
            None
        };
        let jq = match (&self.jq, call_jq) {
            (Some(own), Some(extra)) => Some(JqFilter::parse(&format!(
                "({}) | ({})",
                own.as_str(),
                extra.as_str()
            ))?),
            (own, extra) => extra.or_else(|| own.clone()),
        };
        let overridden: Vec<String> = args
            .iter()
            .filter(|(key, value)| !value.is_null() && self.fixed.contains_key(key.as_str()))
            .map(|(key, _)| format!("`{key}`"))
            .collect();
        if !overridden.is_empty() {
            return Err(ToolError::new(format!(
                "argument(s) {} are fixed by the definition of custom tool `{}` and cannot be \
                 passed; use the `{}` tool to set them yourself",
                overridden.join(", "),
                self.name,
                self.operation.tool_name
            )));
        }
        for (key, value) in &self.fixed {
            args.insert(key.clone(), value.clone());
        }
        Ok((args, jq))
    }
}

/// Проверяет набор пользовательских инструментов: имена уникальны и не совпадают со
/// встроенными и с `reserved` (составными инструментами). Встроенные имена берутся из всей
/// таблицы операций, а не только из зарегистрированных, чтобы смысл файла не зависел от
/// флагов доступа и наборов.
pub fn check_names(tools: &[CustomTool], reserved: &[&str]) -> Result<(), String> {
    let builtin: HashSet<&str> = operations()
        .iter()
        .map(|op| op.tool_name)
        .chain(RESERVED_TOOL_NAMES.iter().copied())
        .chain(reserved.iter().copied())
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    for tool in tools {
        if builtin.contains(tool.name.as_str()) {
            return Err(format!(
                "{}: tool `{}` has the same name as a built-in tool; rename it",
                tool.source, tool.name
            ));
        }
        if !seen.insert(tool.name.as_str()) {
            return Err(format!(
                "{}: tool `{}` is defined more than once",
                tool.source, tool.name
            ));
        }
    }
    Ok(())
}

fn check_identifier(name: &str) -> Result<(), String> {
    let valid = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && name.len() <= MAX_TOOL_NAME_LEN;
    if valid {
        Ok(())
    } else {
        Err(format!(
            "invalid tool name `{name}`: use lowercase letters, digits and `_`, starting with a \
             letter, at most {MAX_TOOL_NAME_LEN} characters"
        ))
    }
}

/// Значение фиксированного аргумента для описания: компактный JSON, длинное — с многоточием.
fn shown_value(value: &Value) -> String {
    let mut text = value.to_string();
    if truncate_text(&mut text, MAX_SHOWN_VALUE) {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::spec::{ParamLocation, excluded_operations};
    use serde_json::json;

    use super::*;

    fn compile(value: Value) -> Result<CustomTool, String> {
        CustomTool::compile(serde_json::from_value(value).unwrap(), "tools.toml")
    }

    fn latest_reviews() -> Value {
        json!({
            "name": "latest_reviews",
            "description": "Latest reviews of the main app",
            "operation": "list_reviews",
            "args": {
                "appId": "100000001",
                "countries": "RU",
                "limit": 10
            },
            "jq": "[.data.reviewList[]? | {reviewId, rating}]"
        })
    }

    fn args(value: Value) -> JsonObject {
        match value {
            Value::Object(map) => map,
            _ => panic!("не объект"),
        }
    }

    #[test]
    fn compiles_and_describes() {
        let tool = compile(latest_reviews()).unwrap();
        assert_eq!(tool.access(), AccessLevel::Read);
        assert_eq!(tool.toolset(), "reviews");
        let mcp = tool.tool().unwrap();
        assert_eq!(mcp.name, "latest_reviews");
        let properties = mcp.input_schema["properties"].as_object().unwrap();
        // Фиксированные аргументы убраны, остальные аргументы операции и `jq` остались.
        assert!(!properties.contains_key("appId"));
        assert!(!properties.contains_key("limit"));
        assert!(properties.contains_key("page"));
        assert!(properties.contains_key(JQ_ARG));
        assert_eq!(mcp.input_schema["additionalProperties"], false);
        let required: Vec<&str> = mcp.input_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(required.contains(&"beginTime"), "{required:?}");
        assert!(!required.contains(&"appId"), "{required:?}");
        let description = mcp.description.as_deref().unwrap();
        assert!(description.starts_with("Latest reviews of the main app"));
        assert!(
            description.contains("`list_reviews` (GET /api/reviews/v1/manage/dev/reviews)"),
            "{description}"
        );
        assert!(description.contains("`limit` = 10"), "{description}");
        assert!(description.contains("Access: read-only."));
        let annotations = mcp.annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.title, None);
    }

    #[test]
    fn operation_id_is_accepted() {
        let mut def = latest_reviews();
        def["operation"] = json!("comments.review.list");
        let tool = compile(def).unwrap();
        assert_eq!(tool.operation.tool_name, "list_reviews");
    }

    #[test]
    fn builds_calls() {
        let tool = compile(latest_reviews()).unwrap();
        let (request, jq) = tool.build_call(args(json!({"page": 2}))).unwrap();
        assert_eq!(request["limit"], 10);
        assert_eq!(request["appId"], "100000001");
        assert_eq!(request["page"], 2);
        assert_eq!(
            jq.unwrap().as_str(),
            "[.data.reviewList[]? | {reviewId, rating}]"
        );

        let (_, jq) = tool.build_call(args(json!({"jq": ".[0]"}))).unwrap();
        assert_eq!(
            jq.unwrap().as_str(),
            "([.data.reviewList[]? | {reviewId, rating}]) | (.[0])"
        );

        let err = tool.build_call(args(json!({"limit": 100}))).unwrap_err();
        assert!(err.message().contains("`limit` are fixed"), "{err}");
        assert!(tool.build_call(args(json!({"limit": null}))).is_ok());
        let err = tool.build_call(args(json!({"jq": ".a |"}))).unwrap_err();
        assert!(err.message().contains("argument `jq`"), "{err}");
    }

    #[test]
    fn access_and_toolset_come_from_the_operation() {
        let update = compile(json!({
            "name": "set_main_app_info",
            "description": "Change information of the main app",
            "operation": "update_app_info",
            "args": {"appId": "100000001"}
        }))
        .unwrap();
        assert_eq!(update.access(), AccessLevel::Write);
        assert_eq!(update.toolset(), "listing");
        let annotations = update.tool().unwrap().annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(annotations.destructive_hint, Some(false));
        let submit = compile(json!({
            "name": "submit_main_app",
            "description": "Submit",
            "operation": "submit_app"
        }))
        .unwrap();
        assert_eq!(submit.access(), AccessLevel::Destructive);
        assert_eq!(
            submit.tool().unwrap().annotations.unwrap().destructive_hint,
            Some(true)
        );
    }

    #[test]
    fn rejects_invalid_definitions() {
        let cases = [
            (json!({"name": "Bad-Name"}), "invalid tool name"),
            (json!({"description": " "}), "`description` is empty"),
            (
                json!({"operation": "no_such_op"}),
                "unknown operation `no_such_op`",
            ),
            (json!({"args": {"nope": 1}}), "has no argument `nope`"),
            (json!({"args": {"limit": "many"}}), "must be an integer"),
            (json!({"args": {"limit": null}}), "cannot be fixed to null"),
            (json!({"args": {"jq": "."}}), "not an operation argument"),
            (json!({"jq": ".a |"}), "`jq`: argument `jq`: syntax error"),
            (json!({"jq": "env"}), "undefined filter `env/0`"),
        ];
        for (patch, needle) in cases {
            let mut value = latest_reviews();
            for (key, item) in patch.as_object().unwrap() {
                value[key] = item.clone();
            }
            let err = compile(value).unwrap_err();
            assert!(err.contains(needle), "{needle}: {err}");
            assert!(err.starts_with("tools.toml: "), "{err}");
        }

        // Исключённая операция (например, шаг загрузки или получение токена) — не инструмент.
        let excluded = excluded_operations().first().unwrap();
        let err = compile(json!({
            "name": "x",
            "description": "d",
            "operation": excluded.operation_id
        }))
        .unwrap_err();
        assert!(err.contains("not available as a tool"), "{err}");

        // Тело — объект.
        if let Some(op) = operations().iter().find(|op| op.param("body").is_some()) {
            let err = compile(json!({
                "name": "with_body",
                "description": "d",
                "operation": op.tool_name,
                "args": {"body": 1}
            }))
            .unwrap_err();
            assert!(err.contains("must be a JSON object"), "{err}");
        }

        // Значение вне `enum` параметра отклоняется при загрузке.
        let schema_enum = operations().iter().find_map(|op| {
            let schema: Value = serde_json::from_str(op.input_schema).ok()?;
            op.params
                .iter()
                .filter(|p| p.location == ParamLocation::Query && !p.repeated)
                .find(|p| schema["properties"][p.name].get("enum").is_some())
                .map(|p| (op, p.name))
        });
        if let Some((op, param)) = schema_enum {
            let err = compile(json!({
                "name": "with_enum",
                "description": "d",
                "operation": op.tool_name,
                "args": {param: "NOT_A_VALUE"}
            }))
            .unwrap_err();
            assert!(err.contains("not an allowed value"), "{err}");
        }

        let unknown_key = serde_json::from_value::<CustomToolDef>(json!({
            "name": "x", "description": "d", "operation": "list_reviews", "method": "GET"
        }));
        assert!(unknown_key.is_err());
    }

    #[test]
    fn names_must_not_clash() {
        let mut builtin = latest_reviews();
        builtin["name"] = json!("list_reviews");
        let err = check_names(&[compile(builtin).unwrap()], &[]).unwrap_err();
        assert!(err.contains("built-in"), "{err}");
        for name in RESERVED_TOOL_NAMES {
            let mut builtin = latest_reviews();
            builtin["name"] = json!(name);
            assert!(
                check_names(&[compile(builtin).unwrap()], &[]).is_err(),
                "{name}"
            );
        }
        let twice = [
            compile(latest_reviews()).unwrap(),
            compile(latest_reviews()).unwrap(),
        ];
        assert!(
            check_names(&twice, &[])
                .unwrap_err()
                .contains("more than once")
        );
        assert!(check_names(&[compile(latest_reviews()).unwrap()], &[]).is_ok());
        // Имена составных инструментов передаёт сервер.
        let err =
            check_names(&[compile(latest_reviews()).unwrap()], &["latest_reviews"]).unwrap_err();
        assert!(err.contains("built-in"), "{err}");
    }
}
