//! Генератор операций: детерминизм (тот же снимок и дополнение дают тот же код, и он
//! совпадает с тем, что собрал `build.rs`) и ошибки сборки на испорченном дополнении
//! и снимке.
//!
//! Тест подключает модули генератора из `build/` напрямую.

#[path = "../build/generate.rs"]
mod generate;
#[path = "../build/overlay.rs"]
mod overlay;
#[path = "../build/schema.rs"]
mod schema;

use std::sync::OnceLock;

use serde_json::{Value, json};

const OVERLAY: &str = include_str!("../spec/overlay.toml");
const BUILT: &str = include_str!(concat!(env!("OUT_DIR"), "/operations.rs"));

fn spec() -> &'static Value {
    static SPEC: OnceLock<Value> = OnceLock::new();
    SPEC.get_or_init(|| {
        let raw = include_str!("../spec/openapi.json");
        serde_json::from_str(raw).expect("снимок — JSON")
    })
}

fn run(overlay_raw: &str) -> generate::Output {
    let overlay = overlay::parse(overlay_raw).expect("дополнение разбирается");
    generate::generate(spec(), &overlay)
}

#[test]
fn generation_is_deterministic_and_matches_build() {
    let first = run(OVERLAY);
    let second = run(OVERLAY);
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(
        first.code, second.code,
        "два прогона генератора различаются"
    );
    assert_eq!(
        first.code, BUILT,
        "код build.rs отличается от прогона генератора"
    );
}

#[test]
fn generation_does_not_depend_on_key_order() {
    // Ключи с переставленным порядком дают тот же код (заметно, если у `serde_json`
    // включена фича `preserve_order`; без неё объекты и так упорядочены).
    fn reversed(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.reverse();
                Value::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| (k.clone(), reversed(v)))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(reversed).collect()),
            other => other.clone(),
        }
    }
    let shuffled = reversed(spec());
    let output = generate::generate(&shuffled, &overlay::parse(OVERLAY).expect("дополнение"));
    assert!(output.errors.is_empty(), "{:?}", output.errors);
    assert_eq!(output.code, BUILT);
}

#[test]
fn tool_name_rules() {
    use generate::is_valid_tool_name;
    assert!(is_valid_tool_name("get_app_info"));
    assert!(is_valid_tool_name("get_paid_app_details_report_url"));
    assert!(!is_valid_tool_name("Get_app"));
    assert!(!is_valid_tool_name("get__app"));
    assert!(!is_valid_tool_name("get_app_"));
    assert!(!is_valid_tool_name("get-app"));
    assert!(!is_valid_tool_name(&"a".repeat(41)));
    assert!(is_valid_tool_name(&"a".repeat(40)));
}

/// Дополнение с заменой: `from` должен встречаться в нём ровно один раз.
fn patched(from: &str, to: &str) -> String {
    assert_eq!(OVERLAY.matches(from).count(), 1, "{from}");
    OVERLAY.replacen(from, to, 1)
}

fn errors_of(overlay_raw: &str) -> Vec<String> {
    let output = run(overlay_raw);
    assert!(output.code.is_empty(), "при ошибках код не выводится");
    assert!(!output.errors.is_empty(), "ожидались ошибки генератора");
    output.errors
}

fn assert_error(errors: &[String], parts: &[&str]) {
    assert!(
        errors.iter().any(|e| parts.iter().all(|p| e.contains(p))),
        "нет ошибки с {parts:?}: {errors:?}"
    );
}

#[test]
fn operation_without_toolset_is_an_error() {
    let errors = errors_of(&patched("    \"publishing.appId.list\",\n", ""));
    assert_error(
        &errors,
        &["publishing.appId.list", "не входит ни в один набор"],
    );

    // Исключённая операция, которую можно вызвать через `api_request`, тоже нуждается
    // в наборе: пустой набор допустим только при `callable = false`.
    let callable = patched(
        "    \"upload.multipart.compose\",\n]\ncallable = false\n",
        "    \"upload.multipart.compose\",\n]\ncallable = true\n",
    );
    let errors = errors_of(&callable.replace(
        "    \"upload.uploadUrl.get\",\n    \"upload.file.put\",",
        "    \"upload.file.put\",",
    ));
    assert_error(
        &errors,
        &["upload.uploadUrl.get", "не входит ни в один набор"],
    );
}

#[test]
fn operation_in_two_toolsets_is_an_error() {
    let errors = errors_of(&patched(
        "    \"publishing.appInfo.update\",\n",
        "    \"publishing.appInfo.update\",\n    \"publishing.appId.list\",\n",
    ));
    assert_error(
        &errors,
        &["publishing.appId.list", "в наборах apps и listing"],
    );
}

#[test]
fn duplicate_tool_name_is_an_error() {
    let errors = errors_of(&patched(
        "\"publishing.appInfo.get\" = \"get_app_info\"",
        "\"publishing.appInfo.get\" = \"list_app_ids\"",
    ));
    assert_error(&errors, &["имя инструмента list_app_ids повторяется"]);
}

#[test]
fn long_tool_name_is_an_error() {
    let long = "get_app_info_with_a_name_that_is_far_too_long";
    let errors = errors_of(&patched(
        "\"publishing.appInfo.get\" = \"get_app_info\"",
        &format!("\"publishing.appInfo.get\" = \"{long}\""),
    ));
    assert_error(&errors, &[long, "длиннее 40"]);
}

#[test]
fn operation_without_name_is_an_error() {
    let errors = errors_of(&patched(
        "\"publishing.appId.list\" = \"list_app_ids\"\n",
        "",
    ));
    assert_error(&errors, &["publishing.appId.list", "нет имени инструмента"]);
}

#[test]
fn excluded_and_external_operations_take_no_names() {
    let errors = errors_of(&patched(
        "\"publishing.appId.list\" = \"list_app_ids\"",
        "\"publishing.appId.list\" = \"list_app_ids\"\n\
         \"upload.file.put\" = \"put_upload_file\"\n\
         \"auth.token.create\" = \"create_token\"",
    ));
    assert_error(&errors, &["[names]", "upload.file.put", "внешняя операция"]);
    assert_error(&errors, &["[names]", "auth.token.create", "исключена"]);
}

#[test]
fn iap_kit_operations_must_not_be_callable() {
    // REST API IAP Kit: подписи JWT ES256 в сервере нет, вызывать такие операции нельзя.
    let errors = errors_of(&patched(
        "    \"iapkit.refundResult.send\",\n]\ncallable = false\n",
        "    \"iapkit.refundResult.send\",\n]\n",
    ));
    assert_error(
        &errors,
        &["iapkit.orderStatus.query", "IAP Kit", "callable = false"],
    );
}

#[test]
fn external_operations_must_not_be_callable() {
    let errors = errors_of(&patched(
        "operations = [\"upload.file.put\", \"upload.chunk.put\"]\ncallable = false\n",
        "operations = [\"upload.file.put\", \"upload.chunk.put\"]\n",
    ));
    assert_error(&errors, &["upload.file.put", "callable = false"]);
}

#[test]
fn unknown_overlay_references_are_errors() {
    let errors = errors_of(&patched(
        "\"publishing.appId.list\" = \"list_app_ids\"",
        "\"publishing.appId.list\" = \"list_app_ids\"\n\"publishing.missing.get\" = \"get_missing\"",
    ));
    assert_error(
        &errors,
        &["[names]", "publishing.missing.get", "нет в снимке"],
    );

    let errors = errors_of(&patched(
        "operations = [\"comments.review.reply\"]",
        "operations = [\"comments.review.reply\", \"comments.review.delete\"]",
    ));
    assert_error(
        &errors,
        &["[[access]]", "comments.review.delete", "нет в снимке"],
    );

    let errors = errors_of(&patched(
        "    \"comments.rating.list\",\n",
        "    \"comments.rating.list\",\n    \"comments.rating.reply\",\n",
    ));
    assert_error(
        &errors,
        &["[[toolsets]]", "comments.rating.reply", "нет в снимке"],
    );

    let errors = errors_of(&patched(
        "operations = [\"auth.token.create\"]",
        "operations = [\"auth.token.create\", \"auth.token.revoke\"]",
    ));
    assert_error(
        &errors,
        &["[[exclude]]", "auth.token.revoke", "нет в снимке"],
    );

    let errors = errors_of(&format!(
        "{OVERLAY}\n[[groups]]\nslug = \"push\"\ntitle = \"Push Kit\"\n\
         reference = \"https://developer.huawei.com/consumer/en/doc/push\"\n"
    ));
    assert_error(&errors, &["[[groups]]", "push", "нет в снимке"]);
}

#[test]
fn access_exception_equal_to_default_is_an_error() {
    let errors = errors_of(&format!(
        "{OVERLAY}\n[[access]]\nlevel = \"read\"\nreason = \"only reads.\"\n\
         operations = [\"publishing.appInfo.get\"]\n"
    ));
    assert_error(
        &errors,
        &["publishing.appInfo.get", "совпадает с уровнем по умолчанию"],
    );
}

#[test]
fn unknown_overlay_keys_are_rejected() {
    let broken = format!("{OVERLAY}\n[unknown_section]\nkey = 1\n");
    assert!(overlay::parse(&broken).is_err());

    let broken = patched("slug = \"apps\"\n", "slug = \"apps\"\nhidden = true\n");
    let error = overlay::parse(&broken).expect_err("неизвестный ключ набора");
    assert!(error.contains("hidden"), "{error}");

    let broken = patched(
        "operations = [\"auth.token.create\"]\ncallable = false\n",
        "operations = [\"auth.token.create\"]\ncallable = false\nreplacement = \"none\"\n",
    );
    assert!(overlay::parse(&broken).is_err());
}

fn spec_errors(mutate: impl FnOnce(&mut Value)) -> Vec<String> {
    let mut spec = spec().clone();
    mutate(&mut spec);
    let output = generate::generate(&spec, &overlay::parse(OVERLAY).expect("дополнение"));
    assert!(output.code.is_empty());
    output.errors
}

#[test]
fn malformed_spec_is_an_error() {
    let errors = spec_errors(|spec| {
        spec["paths"]["/api/publish/v2/app-info"]["get"]["x-agc-unknown"] = json!(true);
    });
    assert_error(&errors, &["publishing.appInfo.get", "x-agc-unknown"]);

    let errors = spec_errors(|spec| {
        spec["paths"]["/api/publish/v2/app-info"]["get"]["x-agc-auth-modes"] = json!(["team"]);
    });
    assert_error(&errors, &["publishing.appInfo.get", "режим авторизации"]);

    let errors = spec_errors(|spec| {
        let op = &mut spec["paths"]["/api/publish/v2/app-info"]["get"];
        op["x-agc-result"] = json!({"field": "ret", "codeField": "code"});
    });
    assert_error(&errors, &["publishing.appInfo.get", "x-agc-result"]);

    // Заголовки авторизации добавляет исполнитель.
    let errors = spec_errors(|spec| {
        let op = &mut spec["paths"]["/api/publish/v2/app-info"]["get"];
        op["parameters"]
            .as_array_mut()
            .expect("parameters")
            .push(json!({
                "name": "Authorization", "in": "header", "required": false,
                "description": "Token.", "schema": {"type": "string"}
            }));
    });
    assert_error(&errors, &["Authorization", "исполнитель"]);

    // Массив только повтором имени в строке запроса.
    let errors = spec_errors(|spec| {
        let op = &mut spec["paths"]["/api/publish/v2/app-info"]["get"];
        op["parameters"]
            .as_array_mut()
            .expect("parameters")
            .push(json!({
                "name": "langs", "in": "query", "required": false, "style": "form",
                "explode": false, "description": "Languages.",
                "schema": {"type": "array", "items": {"type": "string"}}
            }));
    });
    assert_error(&errors, &["langs", "explode: true"]);

    // Ссылка на несуществующую модель и лишний ключ рядом с `$ref`.
    let errors = spec_errors(|spec| {
        let schema = &mut spec["paths"]["/api/publish/v2/app-info"]["put"]["requestBody"]["content"]
            ["application/json"]["schema"];
        schema["properties"]["broken"] =
            json!({"$ref": "#/components/schemas/Missing", "nullable": true});
    });
    assert_error(&errors, &["publishing.appInfo.update", "Missing"]);
    assert_error(&errors, &["publishing.appInfo.update", "рядом с $ref"]);

    // Внешняя операция без источника адреса.
    let errors = spec_errors(|spec| {
        spec["x-agc-external-operations"]["upload.file.put"]["x-agc-url-source"] =
            json!("upload.missing.get");
    });
    assert_error(&errors, &["upload.file.put", "x-agc-url-source"]);
}

#[test]
fn deprecated_operation_is_excluded_by_itself() {
    let mut spec = spec().clone();
    spec["paths"]["/api/publish/v2/properties/gms"]["put"]["deprecated"] = json!(true);
    let output = generate::generate(&spec, &overlay::parse(OVERLAY).expect("дополнение"));
    // Устаревшая операция не получает имени: имя в дополнении — ошибка.
    assert_error(
        &output.errors,
        &["[names]", "publishing.gmsDependency.update", "исключена"],
    );

    let overlay = patched(
        "\"publishing.gmsDependency.update\" = \"update_gms_dependency\"\n",
        "",
    );
    let output = generate::generate(&spec, &overlay::parse(&overlay).expect("дополнение"));
    assert!(output.errors.is_empty(), "{:?}", output.errors);
    assert!(!output.code.contains("update_gms_dependency"));
    assert!(output.code.contains(
        "operation_id: \"publishing.gmsDependency.update\", group: \"publishing\", toolset: \"releases\""
    ));
    assert!(output.code.contains("reason: ExclusionReason::Deprecated"));
}

/// Раздел `[[secrets]]` в конец дополнения.
fn with_secret(kind: &str, operations: &str, fields: &str) -> String {
    format!(
        "{OVERLAY}\n[[secrets]]\nkind = \"{kind}\"\ndescription = \"test value\"\n\
         operations = [{operations}]\nfields = [{fields}]\n"
    )
}

#[test]
fn secret_fields_are_checked_against_the_response_schema() {
    // Поля нет в схеме ответа.
    let errors = errors_of(&with_secret(
        "credential",
        "\"publishing.appInfo.get\"",
        "\"appInfo.noSuchField\"",
    ));
    assert_error(
        &errors,
        &[
            "[[secrets]] publishing.appInfo.get",
            "appInfo.noSuchField",
            "нет поля noSuchField",
        ],
    );
    // Объект вместо массива и массив вместо строки.
    let errors = errors_of(&with_secret(
        "download-url",
        "\"provisioning.profile.create\"",
        "\"provisionInfo[].provisionDownloadUrl\"",
    ));
    assert_error(
        &errors,
        &["provisionInfo[].provisionDownloadUrl", "не массив"],
    );
    let errors = errors_of(&with_secret(
        "download-url",
        "\"provisioning.cert.list\"",
        "\"certList\"",
    ));
    assert_error(&errors, &["certList", "должно быть строкой"]);
    // Неизвестная и исключённая операции.
    let errors = errors_of(&with_secret(
        "credential",
        "\"project.missing.get\"",
        "\"x\"",
    ));
    assert_error(
        &errors,
        &["[[secrets]]", "project.missing.get", "нет в снимке"],
    );
    let errors = errors_of(&with_secret(
        "credential",
        "\"auth.token.create\"",
        "\"access_token\"",
    ));
    assert_error(&errors, &["[[secrets]]", "auth.token.create", "исключена"]);
}

#[test]
fn secret_rules_are_consistent() {
    // Поле указано дважды.
    let errors = errors_of(&with_secret(
        "credential",
        "\"project.appExtraInfo.get\"",
        "\"secret\"",
    ));
    assert_error(&errors, &["project.appExtraInfo.get", "secret", "дважды"]);
    // Ссылка и содержимое у одной операции: save_to сохраняет что-то одно.
    let errors = errors_of(&with_secret(
        "download-url",
        "\"project.configFile.get\"",
        "\"fileName\"",
    ));
    assert_error(&errors, &["project.configFile.get", "save_to"]);
    // Неизвестный вид и неизвестный ключ — ошибка разбора.
    let bad = with_secret("token", "\"project.configFile.get\"", "\"fileName\"");
    assert!(overlay::parse(&bad).is_err());
    let bad = format!(
        "{OVERLAY}\n[[secrets]]\nkind = \"credential\"\ndescription = \"x\"\noperations = []\nfields = []\nextra = 1\n"
    );
    assert!(overlay::parse(&bad).is_err());
    // Пустые списки.
    let errors = errors_of(&format!(
        "{OVERLAY}\n[[secrets]]\nkind = \"credential\"\ndescription = \" \"\noperations = []\nfields = []\n"
    ));
    assert_error(&errors, &["[[secrets]]", "нет description"]);
    assert_error(&errors, &["[[secrets]]", "нужны operations и fields"]);
}

#[test]
fn renamed_secret_field_in_the_spec_is_an_error() {
    // Поле переименовали в снимке, а дополнение осталось: секрет не должен пройти молча.
    let errors = spec_errors(|spec| {
        let properties = spec
            .pointer_mut(
                "/paths/~1api~1report~1distribution-operation-quality~1v1~1IAPExport~1{appId}/get/responses/200/content/application~1json/schema/properties",
            )
            .and_then(Value::as_object_mut)
            .expect("схема ответа отчёта");
        let field = properties.remove("fileURL").expect("fileURL");
        properties.insert("fileUrl".into(), field);
    });
    assert_error(
        &errors,
        &["reports.iap.export", "fileURL", "нет поля fileURL"],
    );
}

/// Раздел `[[project_credentials]]` в конец дополнения.
fn with_project_rule(body: &str) -> String {
    format!("{OVERLAY}\n[[project_credentials]]\n{body}\n")
}

/// Снимок, в котором операции проектов описаны и для режима API-клиента (в настоящем
/// снимке у них только OAuth-клиент и Service Account).
fn spec_with_api_client_projects() -> Value {
    let mut spec = spec().clone();
    let paths = spec
        .get_mut("paths")
        .and_then(Value::as_object_mut)
        .expect("paths");
    for item in paths.values_mut() {
        for op in item.as_object_mut().expect("путь").values_mut() {
            if op.get("x-agc-group") == Some(&json!("project")) {
                op["x-agc-auth-modes"] = json!(["api-client"]);
            }
        }
    }
    spec
}

#[test]
fn project_credentials_mark_operations_with_the_project_parameter() {
    // В дополнении клиент проекта нужен четырём операциям Remote Configuration и двум
    // операциям App Linking (заголовок `productId`).
    assert_eq!(BUILT.matches("project_param: Some(").count(), 6);
    assert_eq!(
        BUILT.matches("project_param: Some(\"productId\")").count(),
        6
    );
    let spec = spec_with_api_client_projects();
    let overlay =
        with_project_rule("operations = [\"project.project.get\", \"project.service.list\"]");
    let output = generate::generate(&spec, &overlay::parse(&overlay).expect("дополнение"));
    assert!(output.errors.is_empty(), "{:?}", output.errors);
    // Параметр пути и параметр запроса `projectId`; остальные операции без пометки.
    assert_eq!(
        output
            .code
            .matches("project_param: Some(\"projectId\")")
            .count(),
        2
    );
    assert!(output.code.contains("Project-level API client"));
    let explicit =
        with_project_rule("operations = [\"project.project.get\"]\nparam = \"projectId\"");
    let output = generate::generate(&spec, &overlay::parse(&explicit).expect("дополнение"));
    assert!(output.errors.is_empty(), "{:?}", output.errors);
}

#[test]
fn project_credentials_rules_are_checked() {
    let spec = spec_with_api_client_projects();
    let errors_with = |body: &str| {
        let output = generate::generate(
            &spec,
            &overlay::parse(&with_project_rule(body)).expect("дополнение"),
        );
        assert!(output.code.is_empty());
        output.errors
    };
    // В группе есть операции без параметра проекта.
    let errors = errors_with("groups = [\"project\"]");
    assert_error(
        &errors,
        &["project.team.list", "нет параметра productId или projectId"],
    );
    // Справочник не описывает операцию для режима API-клиента.
    let errors = errors_of(&with_project_rule("operations = [\"project.project.get\"]"));
    assert_error(&errors, &["project.project.get", "без api-client"]);
    // Необязательный параметр, тело, параметр, которого нет.
    let errors = errors_with("operations = [\"project.project.get\"]\nparam = \"queryFlag\"");
    assert_error(&errors, &["queryFlag", "обязательным"]);
    let errors = errors_with("operations = [\"project.project.get\"]\nparam = \"nope\"");
    assert_error(&errors, &["нет параметра nope"]);
    // Неизвестные группа и операция, исключённая операция, повтор, пустое правило.
    let errors = errors_with("groups = [\"nope\"]");
    assert_error(&errors, &["группы nope нет"]);
    let errors = errors_with("operations = [\"project.missing.get\"]");
    assert_error(&errors, &["project.missing.get", "нет в снимке"]);
    let errors = errors_with("operations = [\"auth.token.create\"]");
    assert_error(&errors, &["auth.token.create", "исключена"]);
    let errors = errors_with(
        "operations = [\"project.project.get\"]\n\n[[project_credentials]]\noperations = [\"project.project.get\"]",
    );
    assert_error(&errors, &["project.project.get", "дважды"]);
    let errors = errors_with("param = \"projectId\"");
    assert_error(&errors, &["нужны groups или operations"]);
    // Неизвестный ключ — ошибка разбора.
    assert!(overlay::parse(&with_project_rule("operations = []\nextra = 1")).is_err());
}

#[test]
fn iap_service_operations_are_checked() {
    // Неизвестный сервис.
    let errors = spec_errors(|spec| {
        spec["paths"]["/api/publish/v2/app-info"]["get"]["x-agc-service"] = json!("iap");
    });
    assert_error(&errors, &["publishing.appInfo.get", "x-agc-service"]);
    // У операции IAP путь без `/api/`.
    let errors = spec_errors(|spec| {
        spec["paths"]["/api/publish/v2/app-info"]["get"]["x-agc-service"] = json!("iap-orders");
        spec["paths"]["/api/publish/v2/app-info"]["put"]["x-agc-service"] = json!("iap-orders");
    });
    assert_error(&errors, &["/api/publish/v2/app-info", "HMS IAP"]);
    // У пути Connect API — `/api/`; разные сервисы у одного пути.
    let errors = spec_errors(|spec| {
        let item = spec["paths"]["/api/publish/v2/app-info"].clone();
        spec["paths"]["/applications/v2/x"] = item;
    });
    assert_error(&errors, &["/applications/v2/x", "/api/"]);
    let errors = spec_errors(|spec| {
        spec["paths"]["/api/publish/v2/app-info"]["get"]["x-agc-service"] = json!("iap-orders");
    });
    assert_error(
        &errors,
        &["/api/publish/v2/app-info", "разные x-agc-service"],
    );
    // Свой параметр `appId` у операции IAP: его добавляет генератор.
    let errors = spec_errors(|spec| {
        let mut item = spec["paths"]["/api/publish/v2/app-info"].clone();
        if let Some(map) = item.as_object_mut() {
            map.retain(|method, _| method == "get");
        }
        item["get"]["x-agc-service"] = json!("iap-orders");
        item["get"]["operationId"] = json!("publishing.iapThing.get");
        spec["paths"]["/applications/v2/x"] = item;
    });
    assert_error(&errors, &["publishing.iapThing.get", "appId"]);
    // Корневой конверт результата: пустым может быть только `field`.
    let errors = spec_errors(|spec| {
        let op = &mut spec["paths"]["/api/publish/v2/app-info"]["get"];
        op["x-agc-result"] = json!({"field": "", "codeField": "", "messageField": "m"});
    });
    assert_error(&errors, &["publishing.appInfo.get", "x-agc-result"]);
}

/// Раздел `[[omit]]` в конец дополнения.
fn with_omit(operations: &str, fields: &str) -> String {
    format!(
        "{OVERLAY}\n[[omit]]\nreason = \"test reason.\"\noperations = {operations}\nfields = {fields}\n"
    )
}

#[test]
fn json_strings_come_from_the_spec() {
    // Пути полей с `x-agc-json` попадают в таблицу.
    assert!(BUILT.contains("json_strings: &[\"purchaseTokenData\"],"));
    assert!(BUILT.contains("json_strings: &[\"cancelledPurchaseList\"],"));
    // `x-agc-json` только у строки и только со схемой-объектом.
    let path = "/applications/v2/purchases/cancelledList";
    let errors = spec_errors(|spec| {
        let field = &mut spec["paths"][path]["post"]["responses"]["200"]["content"]["application/json"]
            ["schema"]["properties"]["continuationToken"];
        field["type"] = json!("integer");
        field["x-agc-json"] = json!({"type": "object"});
    });
    assert_error(
        &errors,
        &[
            "orders.cancelledPurchase.list",
            "x-agc-json",
            "continuationToken",
            "строкой",
        ],
    );
    let errors = spec_errors(|spec| {
        spec["paths"][path]["post"]["responses"]["200"]["content"]["application/json"]["schema"]
            ["properties"]["cancelledPurchaseList"]["x-agc-json"] = json!("array");
    });
    assert_error(&errors, &["cancelledPurchaseList", "схемой-объектом"]);
}

#[test]
fn omitted_fields_are_checked() {
    let errors = errors_of(&with_omit(
        "[\"orders.purchase.confirm\"]",
        "[\"dataSignature\"]",
    ));
    assert_error(
        &errors,
        &[
            "[[omit]] orders.purchase.confirm",
            "dataSignature",
            "нет поля",
        ],
    );
    // Поле — уже разбираемая строка.
    let errors = errors_of(&with_omit(
        "[\"orders.purchaseToken.verify\"]",
        "[\"purchaseTokenData\"]",
    ));
    assert_error(
        &errors,
        &["[[omit]] orders.purchaseToken.verify", "уже указано"],
    );
    // Повтор того же поля другим правилом.
    let errors = errors_of(&with_omit(
        "[\"subscriptions.purchase.get\"]",
        "[\"dataSignature\"]",
    ));
    assert_error(
        &errors,
        &["subscriptions.purchase.get", "dataSignature", "уже указано"],
    );
    let errors = errors_of(&with_omit("[\"orders.missing.get\"]", "[\"x\"]"));
    assert_error(&errors, &["[[omit]]", "orders.missing.get", "нет в снимке"]);
    let errors = errors_of(&format!(
        "{OVERLAY}\n[[omit]]\nreason = \" \"\noperations = []\nfields = []\n"
    ));
    assert_error(&errors, &["[[omit]]", "нет reason"]);
    assert_error(&errors, &["[[omit]]", "нужны operations и fields"]);
}
