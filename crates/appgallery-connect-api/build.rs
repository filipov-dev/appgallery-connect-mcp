//! Генерация таблиц операций из снимка спецификации AppGallery Connect API.
//!
//! Вход — снимок `spec/openapi.json` (OpenAPI 3.0, ведём сами по справочнику Huawei) и
//! файл-дополнение `spec/overlay.toml` (группы, наборы инструментов, имена, уровни
//! доступа, исключения). Результат — `$OUT_DIR/operations.rs` со статическими таблицами
//! `GROUPS`, `TOOLSETS`, `OPERATIONS` и `EXCLUDED`, которые подключает `src/spec.rs`.
//! Сеть не используется, результат детерминирован. Подробности:
//!
//! - `build/overlay.rs` — разбор файла-дополнения;
//! - `build/generate.rs` — разбор операций, параметров, тела и конверта результата,
//!   уровни доступа, исключения, имена, проверки, вывод кода;
//! - `build/schema.rs` — JSON Schema аргументов с раскрытыми `$ref`.

#[path = "build/generate.rs"]
mod generate;
#[path = "build/overlay.rs"]
mod overlay;
#[path = "build/schema.rs"]
mod schema;

use std::{env, fs, path::PathBuf};

const SPEC_PATH: &str = "spec/openapi.json";
const OVERLAY_PATH: &str = "spec/overlay.toml";

fn main() {
    println!("cargo:rerun-if-changed={SPEC_PATH}");
    println!("cargo:rerun-if-changed={OVERLAY_PATH}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=build");

    let spec_raw = fs::read_to_string(SPEC_PATH)
        .unwrap_or_else(|err| panic!("не удалось прочитать {SPEC_PATH}: {err}"));
    let spec: serde_json::Value = serde_json::from_str(&spec_raw)
        .unwrap_or_else(|err| panic!("{SPEC_PATH} не является корректным JSON: {err}"));
    let overlay_raw = fs::read_to_string(OVERLAY_PATH)
        .unwrap_or_else(|err| panic!("не удалось прочитать {OVERLAY_PATH}: {err}"));
    let overlay = overlay::parse(&overlay_raw).unwrap_or_else(|err| panic!("{err}"));

    let output = generate::generate(&spec, &overlay);
    if !output.errors.is_empty() {
        panic!(
            "{SPEC_PATH} и {OVERLAY_PATH} не согласованы ({} ошибок):\n- {}",
            output.errors.len(),
            output.errors.join("\n- ")
        );
    }

    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR не задан")).join("operations.rs");
    fs::write(out, output.code).expect("не удалось записать сгенерированные операции");
}
