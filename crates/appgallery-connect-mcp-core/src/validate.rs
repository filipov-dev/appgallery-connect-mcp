//! Проверка аргументов инструментов по их JSON Schema до отправки запроса.
//!
//! **Инструменты операций** ([`check_operation_arguments`]):
//!
//! - на верхнем уровне неизвестные аргументы отклоняются (`additionalProperties: false`),
//!   обязательные проверяются, значения параметров пути, строки запроса и заголовков —
//!   по [`ParamType`] и `enum` схемы. Скалярные типы приводятся мягко, как их всё равно
//!   передаст исполнитель (строкой): число допустимо вместо строки (`appId: 100000001`),
//!   строка из цифр — вместо целого;
//! - тело (`body`) — JSON-объект (или строка с ним: так тело передают некоторые клиенты).
//!   Внутри тела проверяются только типы известных полей, на любой глубине; неизвестные
//!   поля, вложенные `required`, `enum` и границы не проверяются: справочник Huawei местами
//!   ошибается в обязательности и составе полей, и решать это должен API;
//! - `null` в аргументе верхнего уровня и в полях тела — то же, что непереданное значение
//!   (так некоторые клиенты передают необязательные параметры).
//!
//! **Составные инструменты** ([`check_arguments`]) проверяются строго: их схемы пишем мы
//! сами. Поддерживается подмножество JSON Schema, которого хватает схемам инструментов:
//! `type` (строка или список), `enum`, `const`, `properties`, `required`,
//! `additionalProperties: false`, `items`, `minimum`/`maximum`, `minLength`/`maxLength`,
//! `minItems`/`maxItems`.

use appgallery_connect_api::spec::{OperationDef, ParamDef, ParamLocation, ParamType};
use rmcp::model::JsonObject;
use serde_json::Value;

use crate::error::ToolError;

/// Глубина вложенности, до которой проверяются значения.
const MAX_DEPTH: usize = 16;

/// Имя аргумента с JSON-телом запроса.
pub const BODY_ARG: &str = "body";

/// Проверяет аргументы инструмента операции `op` по его схеме `schema` (см. описание
/// модуля). Аргументы `null` верхнего уровня удаляются.
pub fn check_operation_arguments(
    op: &OperationDef,
    schema: &JsonObject,
    args: &mut JsonObject,
) -> Result<(), ToolError> {
    args.retain(|_, value| !value.is_null());
    reject_unknown(op, args)?;
    let missing: Vec<String> = op
        .params
        .iter()
        .filter(|param| param.required && !args.contains_key(param.name))
        .map(|param| format!("`{}`", param.name))
        .collect();
    if !missing.is_empty() {
        return Err(ToolError::new(format!(
            "missing required argument(s): {}",
            missing.join(", ")
        )));
    }
    let properties = schema.get("properties").and_then(Value::as_object);
    for param in op.params {
        let Some(value) = args.get(param.name) else {
            continue;
        };
        let property = properties.and_then(|p| p.get(param.name));
        match param.location {
            ParamLocation::Body => check_body(param, property, value)?,
            ParamLocation::Path | ParamLocation::Query | ParamLocation::Header => {
                check_param(param, property, value)?;
            }
        }
    }
    Ok(())
}

/// Проверяет один аргумент операции `op` так же, как [`check_operation_arguments`]: имя
/// известно, значение не `null`, тип и `enum` подходят. Нужна для фиксированных аргументов
/// пользовательских инструментов (`tools.toml`), которые проверяются при загрузке файла.
pub fn check_fixed_argument(
    op: &OperationDef,
    schema: &JsonObject,
    name: &str,
    value: &Value,
) -> Result<(), ToolError> {
    let Some(param) = op.param(name) else {
        let allowed: Vec<String> = op.params.iter().map(|p| format!("`{}`", p.name)).collect();
        return Err(ToolError::new(format!(
            "operation `{}` has no argument `{name}`; its arguments: {}",
            op.tool_name,
            if allowed.is_empty() {
                "none".to_owned()
            } else {
                allowed.join(", ")
            }
        )));
    };
    if value.is_null() {
        return Err(ToolError::new(format!(
            "argument `{name}` cannot be fixed to null"
        )));
    }
    let property = schema
        .get("properties")
        .and_then(Value::as_object)
        .and_then(|p| p.get(name));
    match param.location {
        ParamLocation::Body => check_body(param, property, value),
        ParamLocation::Path | ParamLocation::Query | ParamLocation::Header => {
            check_param(param, property, value)
        }
    }
}

fn reject_unknown(op: &OperationDef, args: &JsonObject) -> Result<(), ToolError> {
    let unknown: Vec<String> = args
        .keys()
        .filter(|key| op.param(key).is_none())
        .map(|key| format!("`{key}`"))
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    let allowed = if op.params.is_empty() {
        "none".to_owned()
    } else {
        op.params
            .iter()
            .map(|param| format!("`{}`", param.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    // Частая ошибка — поля тела на верхнем уровне аргументов вместо `body`.
    let hint = if op.param(BODY_ARG).is_some() {
        " (fields of the request body go inside the `body` object)"
    } else {
        ""
    };
    Err(ToolError::new(format!(
        "unknown argument(s) {}{hint}; allowed arguments: {allowed}",
        unknown.join(", ")
    )))
}

/// Параметр пути, строки запроса или заголовка: тип и допустимые значения.
fn check_param(param: &ParamDef, property: Option<&Value>, value: &Value) -> Result<(), ToolError> {
    let items: Vec<&Value> = match value {
        Value::Array(items) if param.repeated => items.iter().collect(),
        Value::Array(_) => {
            return Err(ToolError::new(format!(
                "argument `{}` must be a single value, got an array",
                param.name
            )));
        }
        other => vec![other],
    };
    // У повторяющегося параметра тип элементов — в `items` схемы.
    let item_type = if param.repeated {
        property
            .and_then(|p| p.get("items"))
            .and_then(|items| items.get("type"))
            .and_then(Value::as_str)
            .map(param_type_of)
            .unwrap_or(ParamType::String)
    } else {
        param.value_type
    };
    let allowed = property.and_then(|p| {
        p.get("enum")
            .or_else(|| p.get("items").and_then(|items| items.get("enum")))
            .and_then(Value::as_array)
    });
    for item in items {
        check_scalar(param.name, item_type, item)?;
        if let Some(allowed) = allowed {
            // Числа и строки сравниваются текстом: `enum` бывает и у чисел.
            let matches = allowed
                .iter()
                .any(|candidate| candidate == item || text(candidate) == text(item));
            if !matches {
                let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
                return Err(ToolError::new(format!(
                    "argument `{}`: {item} is not an allowed value; allowed values: {}",
                    param.name,
                    list.join(", ")
                )));
            }
        }
    }
    Ok(())
}

fn param_type_of(name: &str) -> ParamType {
    match name {
        "integer" => ParamType::Integer,
        "number" => ParamType::Number,
        "boolean" => ParamType::Boolean,
        _ => ParamType::String,
    }
}

/// Скалярное значение параметра: исполнитель передаст его строкой, поэтому допустима и
/// текстовая запись числа или логического значения.
fn check_scalar(name: &str, expected: ParamType, value: &Value) -> Result<(), ToolError> {
    let ok = match expected {
        ParamType::Integer => match value {
            Value::Number(n) => n.is_i64() || n.is_u64(),
            Value::String(s) => is_integer(s.trim()),
            _ => false,
        },
        ParamType::Number => match value {
            Value::Number(_) => true,
            Value::String(s) => s.trim().parse::<f64>().is_ok_and(f64::is_finite),
            _ => false,
        },
        ParamType::Boolean => match value {
            Value::Bool(_) => true,
            Value::String(s) => {
                s.trim().eq_ignore_ascii_case("true") || s.trim().eq_ignore_ascii_case("false")
            }
            _ => false,
        },
        // Строки: числа принимаются текстовой записью (`appId` — число, но в API строка).
        ParamType::String | ParamType::Array | ParamType::Object => {
            matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_))
        }
    };
    if ok {
        return Ok(());
    }
    let expected = match expected {
        ParamType::Integer => "an integer (a number or a string of digits)",
        ParamType::Number => "a number",
        ParamType::Boolean => "a boolean",
        _ => "a string",
    };
    Err(ToolError::new(format!(
        "argument `{name}` must be {expected}, got {}",
        kind(value)
    )))
}

/// Тело запроса: JSON-объект (или строка с ним) и типы известных полей.
fn check_body(param: &ParamDef, property: Option<&Value>, value: &Value) -> Result<(), ToolError> {
    let parsed;
    let body = match value {
        Value::String(text) => {
            parsed = serde_json::from_str::<Value>(text).ok();
            parsed.as_ref()
        }
        other => Some(other),
    };
    let Some(body @ Value::Object(_)) = body else {
        return Err(ToolError::new(format!(
            "argument `{}` must be a JSON object (the request body, see the tool schema), got {}",
            param.name,
            kind(value)
        )));
    };
    match property {
        Some(schema) => check_value(param.name, schema, body, 0, Mode::KnownFieldTypes),
        None => Ok(()),
    }
}

/// Проверяет аргументы составного инструмента `args` по его схеме `schema`
/// (`type: object`) строго. Аргументы `null`, которые схема не разрешает, удаляются.
pub fn check_arguments(schema: &Value, args: &mut JsonObject) -> Result<(), ToolError> {
    let properties = schema.get("properties").and_then(Value::as_object);
    args.retain(|name, value| {
        !value.is_null()
            || properties
                .and_then(|p| p.get(name))
                .is_some_and(|property| allows_type(property, "null"))
    });
    check_object("", schema, args, 0, Mode::Strict)
}

/// Насколько строго проверяется значение.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Вся поддерживаемая часть JSON Schema.
    Strict,
    /// Только типы известных полей (тело запроса операции); `null` допустим везде.
    KnownFieldTypes,
}

fn check_object(
    path: &str,
    schema: &Value,
    object: &JsonObject,
    depth: usize,
    mode: Mode,
) -> Result<(), ToolError> {
    let properties = schema.get("properties").and_then(Value::as_object);
    if mode == Mode::Strict {
        if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
            let unknown: Vec<String> = object
                .keys()
                .filter(|key| properties.is_none_or(|p| !p.contains_key(key.as_str())))
                .map(|key| format!("`{}`", join(path, key)))
                .collect();
            if !unknown.is_empty() {
                let allowed = properties
                    .map(|p| {
                        p.keys()
                            .map(|key| format!("`{}`", join(path, key)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .filter(|list| !list.is_empty())
                    .unwrap_or_else(|| "none".to_owned());
                return Err(ToolError::new(format!(
                    "unknown argument(s) {}; allowed arguments: {allowed}",
                    unknown.join(", ")
                )));
            }
        }
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            let missing: Vec<String> = required
                .iter()
                .filter_map(Value::as_str)
                .filter(|name| !object.contains_key(*name))
                .map(|name| format!("`{}`", join(path, name)))
                .collect();
            if !missing.is_empty() {
                return Err(ToolError::new(format!(
                    "missing required argument(s): {}",
                    missing.join(", ")
                )));
            }
        }
    }
    if let Some(properties) = properties {
        for (name, value) in object {
            if let Some(property) = properties.get(name) {
                check_value(&join(path, name), property, value, depth + 1, mode)?;
            }
        }
    }
    Ok(())
}

fn check_value(
    path: &str,
    schema: &Value,
    value: &Value,
    depth: usize,
    mode: Mode,
) -> Result<(), ToolError> {
    if depth > MAX_DEPTH || !schema.is_object() {
        return Ok(());
    }
    if mode == Mode::KnownFieldTypes && value.is_null() {
        return Ok(());
    }
    let invalid = |expected: String| {
        ToolError::new(format!(
            "argument `{path}` must be {expected}, got {}",
            kind(value)
        ))
    };
    if let Some(types) = schema_types(schema)
        && !types.iter().any(|t| matches_type(t, value))
    {
        let names: Vec<String> = types.iter().map(|t| article(t)).collect();
        return Err(invalid(names.join(" or ")));
    }
    if mode == Mode::Strict {
        check_constraints(path, schema, value)?;
    }
    match value {
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items") {
                for (index, item) in items.iter().enumerate() {
                    check_value(
                        &format!("{path}[{index}]"),
                        item_schema,
                        item,
                        depth + 1,
                        mode,
                    )?;
                }
            }
        }
        Value::Object(object) => check_object(path, schema, object, depth, mode)?,
        _ => {}
    }
    Ok(())
}

/// `const`, `enum` и границы (строгий режим).
fn check_constraints(path: &str, schema: &Value, value: &Value) -> Result<(), ToolError> {
    let invalid = |expected: String| {
        ToolError::new(format!(
            "argument `{path}` must be {expected}, got {}",
            kind(value)
        ))
    };
    if let Some(expected) = schema.get("const")
        && expected != value
    {
        return Err(invalid(format!("{expected}")));
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array)
        && !allowed.contains(value)
    {
        let list: Vec<String> = allowed.iter().map(Value::to_string).collect();
        return Err(ToolError::new(format!(
            "argument `{path}`: {value} is not an allowed value; allowed values: {}",
            list.join(", ")
        )));
    }
    let bounds = |len: u64, min_key: &str, max_key: &str, unit: &str| {
        if let Some(min) = schema.get(min_key).and_then(Value::as_u64)
            && len < min
        {
            return Err(ToolError::new(format!(
                "argument `{path}` must have at least {min} {unit}"
            )));
        }
        if let Some(max) = schema.get(max_key).and_then(Value::as_u64)
            && len > max
        {
            return Err(ToolError::new(format!(
                "argument `{path}` must have at most {max} {unit}"
            )));
        }
        Ok(())
    };
    match value {
        Value::Number(n) => {
            let number = n.as_f64().unwrap_or_default();
            if let Some(min) = schema.get("minimum").and_then(Value::as_f64)
                && number < min
            {
                return Err(invalid(format!("at least {min}")));
            }
            if let Some(max) = schema.get("maximum").and_then(Value::as_f64)
                && number > max
            {
                return Err(invalid(format!("at most {max}")));
            }
        }
        Value::String(s) => bounds(
            s.chars().count() as u64,
            "minLength",
            "maxLength",
            "character(s)",
        )?,
        Value::Array(items) => bounds(items.len() as u64, "minItems", "maxItems", "item(s)")?,
        _ => {}
    }
    Ok(())
}

fn schema_types(schema: &Value) -> Option<Vec<&str>> {
    match schema.get("type")? {
        Value::String(t) => Some(vec![t.as_str()]),
        Value::Array(types) => Some(types.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

fn allows_type(schema: &Value, name: &str) -> bool {
    schema_types(schema).is_some_and(|types| types.contains(&name))
}

fn matches_type(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => match value {
            Value::Number(n) => {
                n.is_i64() || n.is_u64() || n.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        },
        // Неизвестный тип не проверяем.
        _ => true,
    }
}

fn article(name: &str) -> String {
    match name {
        "integer" | "object" | "array" => format!("an {name}"),
        "null" => "null".to_owned(),
        other => format!("a {other}"),
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn is_integer(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// Вид JSON-значения для сообщений.
pub fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::spec::{find, operations};
    use serde_json::json;

    use super::*;

    fn op(name: &str) -> &'static OperationDef {
        find(name).unwrap_or_else(|| panic!("нет операции {name}"))
    }

    fn check_op(name: &str, value: Value) -> Result<JsonObject, ToolError> {
        let op = op(name);
        let schema: JsonObject = serde_json::from_str(op.input_schema).unwrap();
        let Value::Object(mut args) = value else {
            panic!("не объект");
        };
        check_operation_arguments(op, &schema, &mut args).map(|()| args)
    }

    #[test]
    fn operation_arguments_top_level() {
        // Число вместо строки и строка из цифр вместо целого допустимы.
        let args = check_op(
            "get_app_info",
            json!({"appId": 100000001, "releaseType": "3", "lang": null}),
        )
        .unwrap();
        assert!(!args.contains_key("lang"), "null — непереданный аргумент");
        check_op(
            "get_app_info",
            json!({"appId": "100000001", "releaseType": 1}),
        )
        .unwrap();

        let cases = [
            (json!({}), "missing required argument(s): `appId`"),
            (
                json!({"appId": null}),
                "missing required argument(s): `appId`",
            ),
            (
                json!({"appId": "1", "title": "x"}),
                "unknown argument(s) `title`; allowed arguments: `appId`",
            ),
            (
                json!({"appId": {"a": 1}}),
                "`appId` must be a string, got an object",
            ),
            (json!({"appId": ["1"]}), "must be a single value"),
            (
                json!({"appId": "1", "releaseType": "full"}),
                "`releaseType` must be an integer",
            ),
            (
                json!({"appId": "1", "releaseType": 1.5}),
                "`releaseType` must be an integer",
            ),
        ];
        for (args, expected) in cases {
            let err = check_op("get_app_info", args.clone()).unwrap_err();
            assert!(err.message().contains(expected), "{args}: {err}");
        }
    }

    #[test]
    fn body_fields_are_checked_by_type_only() {
        let op = op("reply_review");
        let schema: JsonObject = serde_json::from_str(op.input_schema).unwrap();
        let body = &schema["properties"]["body"];
        assert_eq!(body["type"], "object", "{body}");
        let field = body["properties"]
            .as_object()
            .and_then(|p| p.iter().find(|(_, s)| s["type"] == "string"))
            .map(|(name, _)| name.clone())
            .expect("строковое поле тела");
        let mut args = JsonObject::new();
        for param in op.params.iter().filter(|p| p.required && p.name != "body") {
            args.insert(param.name.to_owned(), json!("1"));
        }
        let call = |body: Value| {
            let mut args = args.clone();
            args.insert("body".into(), body);
            check_operation_arguments(op, &schema, &mut args)
        };
        // Неизвестные поля и отсутствие обязательных полей тела — решает API.
        call(json!({"unknownField": 1})).unwrap();
        call(json!({})).unwrap();
        call(json!({ field.as_str(): null })).unwrap();
        call(json!("{\"unknownField\": 1}")).unwrap();
        let err = call(json!({ field.as_str(): 5 })).unwrap_err();
        assert!(
            err.message()
                .contains(&format!("`body.{field}` must be a string, got a number")),
            "{err}"
        );
        for bad in [json!([1]), json!("not json"), json!(5)] {
            let err = call(bad.clone()).unwrap_err();
            assert!(
                err.message().contains("must be a JSON object"),
                "{bad}: {err}"
            );
        }
        // Поля тела на верхнем уровне — подсказка про `body`.
        let mut wrong = args.clone();
        wrong.insert(field.clone(), json!("x"));
        let err = check_operation_arguments(op, &schema, &mut wrong).unwrap_err();
        assert!(err.message().contains("inside the `body` object"), "{err}");
    }

    #[test]
    fn repeated_query_parameters_take_arrays() {
        let Some(op) = operations()
            .iter()
            .find(|op| op.params_in(ParamLocation::Query).any(|p| p.repeated))
        else {
            return;
        };
        let schema: JsonObject = serde_json::from_str(op.input_schema).unwrap();
        let param = op
            .params_in(ParamLocation::Query)
            .find(|p| p.repeated)
            .unwrap();
        let mut args = JsonObject::new();
        for required in op.params.iter().filter(|p| p.required) {
            let value = match required.location {
                ParamLocation::Body => json!({}),
                _ if required.repeated => json!(["1"]),
                _ => json!("1"),
            };
            args.insert(required.name.to_owned(), value);
        }
        args.insert(param.name.to_owned(), json!(["1", "2"]));
        check_operation_arguments(op, &schema, &mut args)
            .unwrap_or_else(|err| panic!("{}: {err}", op.tool_name));
        args.insert(param.name.to_owned(), json!([{"x": 1}]));
        assert!(check_operation_arguments(op, &schema, &mut args).is_err());
    }

    #[test]
    fn every_operation_accepts_its_required_arguments() {
        for op in operations() {
            let schema: JsonObject = serde_json::from_str(op.input_schema).unwrap();
            let mut args = JsonObject::new();
            for param in op.params.iter().filter(|p| p.required) {
                let property = &schema["properties"][param.name];
                let value = match param.location {
                    ParamLocation::Body => json!({}),
                    _ => property
                        .get("enum")
                        .or_else(|| property.get("items").and_then(|i| i.get("enum")))
                        .and_then(Value::as_array)
                        .and_then(|values| values.first().cloned())
                        .unwrap_or_else(|| json!("1")),
                };
                let value = if param.repeated && !value.is_array() {
                    json!([value])
                } else {
                    value
                };
                args.insert(param.name.to_owned(), value);
            }
            check_operation_arguments(op, &schema, &mut args)
                .unwrap_or_else(|err| panic!("{}: {err}", op.tool_name));
        }
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["app_id"],
            "properties": {
                "app_id": {"type": "string", "minLength": 1},
                "mode": {"type": "string", "enum": ["list", "download"]},
                "count": {"type": "integer", "minimum": 1, "maximum": 10},
                "dry_run": {"type": "boolean"},
                "langs": {"type": "array", "items": {"type": "string"}, "maxItems": 2},
                "options": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["kind"],
                    "properties": {"kind": {"type": "string"}}
                },
                "note": {"type": ["string", "null"]}
            }
        })
    }

    fn check(value: Value) -> Result<JsonObject, ToolError> {
        let Value::Object(mut args) = value else {
            panic!("не объект");
        };
        check_arguments(&schema(), &mut args).map(|()| args)
    }

    #[test]
    fn composite_arguments_are_strict() {
        let args = check(json!({
            "app_id": "1",
            "mode": "list",
            "count": 3.0,
            "dry_run": true,
            "langs": ["en-US"],
            "options": {"kind": "x"},
            "note": null,
            "extra": null
        }))
        .unwrap();
        assert!(args.contains_key("note"));
        assert!(!args.contains_key("extra"));

        let cases = [
            (json!({}), "missing required argument(s): `app_id`"),
            (json!({"app_id": "1", "x": 1}), "unknown argument(s) `x`"),
            (
                json!({"app_id": 1}),
                "`app_id` must be a string, got a number",
            ),
            (json!({"app_id": ""}), "at least 1 character(s)"),
            (
                json!({"app_id": "1", "mode": "all"}),
                "not an allowed value",
            ),
            (json!({"app_id": "1", "count": 1.5}), "must be an integer"),
            (json!({"app_id": "1", "count": 11}), "at most 10"),
            (
                json!({"app_id": "1", "langs": [1]}),
                "`langs[0]` must be a string",
            ),
            (
                json!({"app_id": "1", "langs": ["a", "b", "c"]}),
                "at most 2 item(s)",
            ),
            (json!({"app_id": "1", "options": {}}), "`options.kind`"),
            (
                json!({"app_id": "1", "options": {"kind": "x", "y": 1}}),
                "`options.y`",
            ),
            (json!({"app_id": "1", "note": 5}), "a string or null"),
        ];
        for (value, expected) in cases {
            let err = check(value.clone()).unwrap_err();
            assert!(err.message().contains(expected), "{value}: {err}");
        }
    }
}
