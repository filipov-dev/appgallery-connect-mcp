//! Конверт результата в теле ответа AppGallery Connect API.
//!
//! API сообщает об ошибке не только кодом HTTP: ответ с HTTP 200 может нести ненулевой код
//! результата. Конверты у групп API разные ([`KNOWN_RESULTS`]):
//! - `{"ret": {"code": 0, "msg": "success"}}` — Publishing, Upload Management, Reports,
//!   получение токена;
//! - `{"ret": {"rtnCode": 0, "rtnDesc": "success"}}` — Comments;
//! - `{"error": {"errorCode": 0, "errorMsg": null}}` — PMS.
//!
//! Код бывает числом и строкой (`0` и `"0"` — успех). Часть методов (получение токена,
//! отчёты) по справочнику отдаёт `ret` не объектом, а строкой с JSON внутри — она тоже
//! разбирается.
//!
//! У операции из таблицы конверт задан ([`crate::spec::OperationDef::result`],
//! [`read_result`]); для произвольного запроса он определяется по телу
//! ([`detect_result`]).

use serde_json::Value;

use crate::spec::ResultDef;

/// Конверт Publishing, Upload Management и Reports API: `ret.code`, `ret.msg`.
pub const RESULT_RET_CODE: ResultDef = ResultDef {
    field: "ret",
    code_field: "code",
    message_field: "msg",
};

/// Конверт Comments API: `ret.rtnCode`, `ret.rtnDesc`.
pub const RESULT_RET_RTN_CODE: ResultDef = ResultDef {
    field: "ret",
    code_field: "rtnCode",
    message_field: "rtnDesc",
};

/// Конверт PMS API: `error.errorCode`, `error.errorMsg`.
pub const RESULT_ERROR_CODE: ResultDef = ResultDef {
    field: "error",
    code_field: "errorCode",
    message_field: "errorMsg",
};

/// Конверт HMS IAP: `responseCode` и `responseMessage` в корне тела, код строкой (`"0"` —
/// успех). В [`KNOWN_RESULTS`] не входит: у запросов AppGallery Connect API его нет.
pub const RESULT_RESPONSE_CODE: ResultDef = ResultDef {
    field: "",
    code_field: "responseCode",
    message_field: "responseMessage",
};

/// Известные конверты в порядке проверки.
pub const KNOWN_RESULTS: &[ResultDef] = &[RESULT_RET_CODE, RESULT_RET_RTN_CODE, RESULT_ERROR_CODE];

/// Результат из конверта ответа.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultStatus {
    /// Код результата текстом: число `0` и строка `"0"` дают `"0"`.
    pub code: String,
    /// Текст результата (`msg`, `rtnDesc`, `errorMsg`), если он есть и не пуст. Секреты из
    /// него не вырезаны: это делает тот, кто отдаёт текст наружу.
    pub message: Option<String>,
    /// Конверт, в котором найден код.
    pub envelope: ResultDef,
}

impl ResultStatus {
    /// Код означает успех: `0` (числом или строкой, в том числе `"00"` и `0.0`).
    pub fn is_success(&self) -> bool {
        let code = self.code.trim();
        code == "0" || code.parse::<f64>().is_ok_and(|number| number == 0.0)
    }
}

/// Читает конверт `def` из тела ответа. `None` — такого конверта в теле нет (нет поля,
/// оно не объект или в нём нет кода).
pub fn read_result(body: &Value, def: &ResultDef) -> Option<ResultStatus> {
    let parsed;
    // Пустое поле — код в корне тела.
    let holder = if def.field.is_empty() {
        body
    } else {
        body.get(def.field)?
    };
    let object = match holder {
        Value::Object(object) => object,
        // `ret` строкой с JSON внутри: `"{\"code\": 0, \"msg\": \"success\"}"`.
        Value::String(text) if text.trim_start().starts_with('{') => {
            parsed = serde_json::from_str::<Value>(text).ok()?;
            parsed.as_object()?
        }
        _ => return None,
    };
    let code = match object.get(def.code_field)? {
        Value::Number(number) => match number.as_f64() {
            Some(float) if float.fract() == 0.0 && float.abs() < 9.0e15 => {
                format!("{}", float as i64)
            }
            _ => number.to_string(),
        },
        Value::String(text) if !text.trim().is_empty() => text.trim().to_owned(),
        _ => return None,
    };
    let message = object
        .get(def.message_field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned);
    Some(ResultStatus {
        code,
        message,
        envelope: *def,
    })
}

/// Определяет конверт результата по телу ответа: `ret.code`, `ret.rtnCode`,
/// `error.errorCode` ([`KNOWN_RESULTS`]). `None` — конверта в теле нет.
pub fn detect_result(body: &Value) -> Option<ResultStatus> {
    KNOWN_RESULTS.iter().find_map(|def| read_result(body, def))
}

/// Конверт ответа операции: заданный в таблице, а если его в теле нет — любой известный
/// (справочник местами неточен, и ошибка не должна пройти как успех).
pub fn result_of(body: &Value, def: Option<&ResultDef>) -> Option<ResultStatus> {
    def.and_then(|def| read_result(body, def))
        .or_else(|| detect_result(body))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn detects_all_envelopes_with_numeric_and_string_codes() {
        let cases = [
            (
                json!({"ret": {"code": 0, "msg": "success"}}),
                "0",
                true,
                RESULT_RET_CODE,
            ),
            (
                json!({"ret": {"code": "0", "msg": "success"}}),
                "0",
                true,
                RESULT_RET_CODE,
            ),
            (
                json!({"ret": {"code": 204144660, "msg": "bad app"}}),
                "204144660",
                false,
                RESULT_RET_CODE,
            ),
            (
                json!({"ret": {"code": "204144660"}}),
                "204144660",
                false,
                RESULT_RET_CODE,
            ),
            (
                json!({"ret": {"rtnCode": 0, "rtnDesc": "success"}}),
                "0",
                true,
                RESULT_RET_RTN_CODE,
            ),
            (
                json!({"ret": {"rtnCode": "500", "rtnDesc": "failed"}}),
                "500",
                false,
                RESULT_RET_RTN_CODE,
            ),
            (
                json!({"error": {"errorCode": 0, "errorMsg": null}}),
                "0",
                true,
                RESULT_ERROR_CODE,
            ),
            (
                json!({"error": {"errorCode": "13", "errorMsg": "no product"}}),
                "13",
                false,
                RESULT_ERROR_CODE,
            ),
            (json!({"ret": {"code": 0.0}}), "0", true, RESULT_RET_CODE),
            (json!({"ret": {"code": -1}}), "-1", false, RESULT_RET_CODE),
            (
                json!({"ret": "{\"code\": 7, \"msg\": \"client rejected\"}"}),
                "7",
                false,
                RESULT_RET_CODE,
            ),
        ];
        for (body, code, success, envelope) in cases {
            let status = detect_result(&body).unwrap_or_else(|| panic!("{body}"));
            assert_eq!(status.code, code, "{body}");
            assert_eq!(status.is_success(), success, "{body}");
            assert_eq!(status.envelope, envelope, "{body}");
        }
        let status = detect_result(&json!({"ret": {"code": 5, "msg": " text "}})).unwrap();
        assert_eq!(status.message.as_deref(), Some("text"));
        let status = detect_result(&json!({"error": {"errorCode": 0, "errorMsg": null}})).unwrap();
        assert_eq!(status.message, None);
    }

    #[test]
    fn no_envelope_in_other_bodies() {
        for body in [
            json!({}),
            json!([1, 2]),
            json!("text"),
            json!({"ret": "ok"}),
            json!({"ret": 5}),
            json!({"ret": {}}),
            json!({"ret": {"code": null}}),
            json!({"ret": {"code": ""}}),
            json!({"error": "invalid_client"}),
            json!({"data": {"ret": {"code": 1}}}),
        ] {
            assert_eq!(detect_result(&body), None, "{body}");
        }
    }

    #[test]
    fn operation_envelope_wins_then_falls_back() {
        let body = json!({"ret": {"code": 0, "rtnCode": 9, "rtnDesc": "comments"}});
        assert_eq!(
            result_of(&body, Some(&RESULT_RET_RTN_CODE)).unwrap().code,
            "9"
        );
        assert_eq!(result_of(&body, None).unwrap().code, "0");
        // У операции задан конверт PMS, а пришёл конверт Publishing.
        let body = json!({"ret": {"code": 3, "msg": "x"}});
        assert_eq!(
            result_of(&body, Some(&RESULT_ERROR_CODE)).unwrap().code,
            "3"
        );
        assert_eq!(read_result(&body, &RESULT_ERROR_CODE), None);
    }

    #[test]
    fn root_envelope_of_iap() {
        let ok = json!({"responseCode": "0", "responseMessage": "success", "x": 1});
        let status = read_result(&ok, &RESULT_RESPONSE_CODE).unwrap();
        assert!(status.is_success());
        let failed = json!({"responseCode": "6", "responseMessage": "invalid token"});
        let status = result_of(&failed, Some(&RESULT_RESPONSE_CODE)).unwrap();
        assert_eq!(status.code, "6");
        assert_eq!(status.message.as_deref(), Some("invalid token"));
        assert!(!status.is_success());
        // Корневой конверт не определяется по телу запросов AppGallery Connect API.
        assert_eq!(detect_result(&failed), None);
        assert_eq!(read_result(&json!({"x": 1}), &RESULT_RESPONSE_CODE), None);
    }
}
