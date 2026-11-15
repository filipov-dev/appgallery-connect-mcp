//! Представление ответа API в результате инструмента.
//!
//! - JSON — компактный JSON ответа как есть (конверт результата `ret`/`error` остаётся:
//!   в нём бывают пояснения Huawei);
//! - если что-то вырезано — `{"result": <ответ>, "note": <пояснение>}`;
//! - если сервер сохранил файл из ответа (`save_to`) — ещё `"saved": {"path", "bytes",
//!   "field", "content_type"}`, а само поле со ссылкой или содержимым скрыто;
//! - текст — как есть;
//! - пустое тело — `{"status": <HTTP-статус>}`.

use rmcp::model::{CallToolResult, ContentBlock};
use serde_json::{Map, Value, json};

use crate::error::ToolError;

/// Поле обёртки с пояснением о вырезанных значениях.
pub const NOTE: &str = "note";

/// Поле обёртки со сведениями о файле, сохранённом из ответа (`save_to`).
pub const SAVED: &str = "saved";

/// Оборачивает JSON-результат в `{"result", ...extra}`, если есть что добавить.
pub fn wrap(result: Value, extra: Option<Map<String, Value>>) -> Value {
    match extra {
        None => result,
        Some(extra) => {
            let mut wrapped = Map::new();
            wrapped.insert("result".to_owned(), result);
            wrapped.extend(extra);
            Value::Object(wrapped)
        }
    }
}

/// Дополнение обёртки с пометкой `note`; `None` — пометки нет.
pub fn note_extra(note: Option<String>) -> Option<Map<String, Value>> {
    let note = note?;
    let mut extra = Map::new();
    extra.insert(NOTE.to_owned(), Value::String(note));
    Some(extra)
}

/// Текст пустого ответа.
pub fn empty_response(status: u16) -> String {
    json!({ "status": status }).to_string()
}

/// Успешный результат инструмента.
pub fn success(text: String) -> CallToolResult {
    CallToolResult::success(vec![ContentBlock::text(text)])
}

/// Результат инструмента с `isError: true`.
pub fn failure(err: &ToolError) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(err.message())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_only_when_needed() {
        assert_eq!(wrap(json!({"a": 1}), None), json!({"a": 1}));
        assert_eq!(
            wrap(json!({"a": 1}), note_extra(Some("hidden".into()))),
            json!({"result": {"a": 1}, "note": "hidden"})
        );
        assert!(note_extra(None).is_none());
        assert_eq!(empty_response(204), r#"{"status":204}"#);
    }

    #[test]
    fn error_result_is_flagged() {
        let result = failure(&ToolError::new("boom"));
        assert_eq!(result.is_error, Some(true));
        assert_eq!(success("ok".into()).is_error, Some(false));
    }
}
