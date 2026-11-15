//! Разбор CSV-файла отчёта и краткая сводка для агента.
//!
//! Справочник не говорит, в какой кодировке и с каким разделителем Huawei пишет CSV, поэтому
//! разбор терпим к вариантам:
//!
//! - кодировка: UTF-8 (с BOM и без), UTF-16 LE/BE (по BOM или по нулевым байтам ASCII);
//!   невалидный UTF-8 читается с заменой символов и пометкой;
//! - разделитель: запятая, точка с запятой или табуляция — какой чаще встречается в первой
//!   непустой строке вне кавычек;
//! - поля в кавычках по RFC 4180: `""` внутри — кавычка, перевод строки внутри не разрывает
//!   запись;
//! - числа: `1234`, `-1.5`, `1 234,5` (пробелы и неразрывные пробелы — разделители
//!   разрядов), `1,234.5` (запятые разрядов), `12,5` (десятичная запятая, если разделитель
//!   полей не запятая); значение с `%` — доля, такие колонки не суммируются.
//!
//! Итоги считаются только по колонкам основных метрик вида отчёта (имена из справочника на
//! английском и русском, сравнение по началу имени без регистра). Если ни одно имя не
//! совпало (другой язык отчёта или Huawei назвал колонки иначе), суммируются все числовые
//! колонки, кроме первой (по справочнику первая колонка — измерение группировки), долей и
//! колонок, похожих на коды и идентификаторы; пометка говорит об этом. Строки итогов в
//! самом файле («Total», «Итого») в итоги не входят.

use serde_json::{Map, Value, json};

/// Сколько колонок попадает в сводку, не больше.
pub const MAX_COLUMNS: usize = 200;
/// Длина значения ячейки в первых строках, символов.
const MAX_CELL_CHARS: usize = 200;

/// Метрика отчёта: начала имён колонки на английском и русском (без регистра).
#[derive(Debug, Clone, Copy)]
pub struct Metric {
    pub en: &'static str,
    pub ru: &'static str,
}

/// Как считать итоги по виду отчёта.
#[derive(Debug, Clone, Copy)]
pub struct Totals {
    /// Колонки, которые суммируются.
    pub metrics: &'static [Metric],
    /// Суммы денег — отдельно по валюте из этой колонки (если она есть).
    pub currency: Option<Metric>,
}

/// Подписи строк итогов в самом файле: они не суммируются ещё раз.
const TOTAL_LABELS: &[&str] = &[
    "total",
    "totals",
    "summary",
    "итого",
    "всего",
    "合计",
    "总计",
];

/// Признаки долей и средних в имени колонки: их сумма не имеет смысла.
const RATE_WORDS: &[&str] = &[
    "rate",
    "ratio",
    "ctr",
    "cvr",
    "arppu",
    "arpu",
    "proportion",
    "percent",
    "average",
    "доля",
    "соотношение",
    "отношение",
    "средн",
    "%",
];

/// Признаки кодов, номеров и идентификаторов в имени колонки.
const ID_WORDS: &[&str] = &[
    "code",
    "id",
    "no.",
    "version",
    "date",
    "time",
    "код",
    "№",
    "номер",
    "версия",
    "время",
    "дата",
];

/// Кодировка файла.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf8Lossy,
    Utf16Le,
    Utf16Be,
}

impl Encoding {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf8Lossy => "utf-8 (invalid bytes replaced)",
            Self::Utf16Le => "utf-16le",
            Self::Utf16Be => "utf-16be",
        }
    }
}

/// Текст файла и его кодировка.
pub fn decode(bytes: &[u8]) -> (String, Encoding) {
    let utf16 = |rest: &[u8], little: bool| -> String {
        let units = rest.as_chunks::<2>().0.iter().map(|pair| {
            if little {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        });
        char::decode_utf16(units)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect()
    };
    match bytes {
        [0xFF, 0xFE, rest @ ..] => (utf16(rest, true), Encoding::Utf16Le),
        [0xFE, 0xFF, rest @ ..] => (utf16(rest, false), Encoding::Utf16Be),
        [0xEF, 0xBB, 0xBF, rest @ ..] => utf8(rest),
        [a, 0, ..] if *a != 0 => (utf16(bytes, true), Encoding::Utf16Le),
        [0, b, ..] if *b != 0 => (utf16(bytes, false), Encoding::Utf16Be),
        _ => utf8(bytes),
    }
}

fn utf8(bytes: &[u8]) -> (String, Encoding) {
    match std::str::from_utf8(bytes) {
        Ok(text) => (text.to_owned(), Encoding::Utf8),
        Err(_) => (
            String::from_utf8_lossy(bytes).into_owned(),
            Encoding::Utf8Lossy,
        ),
    }
}

/// Разделитель полей по первой непустой строке.
pub fn detect_delimiter(text: &str) -> char {
    let line = first_line(text);
    let mut counts = [(',', 0usize), (';', 0), ('\t', 0)];
    let mut quoted = false;
    for c in line.chars() {
        if c == '"' {
            quoted = !quoted;
        } else if !quoted && let Some(slot) = counts.iter_mut().find(|(d, _)| *d == c) {
            slot.1 += 1;
        }
    }
    // При равенстве — запятая (первая в списке).
    counts
        .iter()
        .fold(
            (',', 0),
            |best, &(d, n)| if n > best.1 { (d, n) } else { best },
        )
        .0
}

/// Первая непустая строка (без учёта кавычек: только для выбора разделителя).
fn first_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

/// Записи CSV: поля по RFC 4180, пустые строки пропускаются.
pub fn records(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut records = Vec::new();
    let mut record: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    let finish = |record: &mut Vec<String>, field: &mut String, records: &mut Vec<Vec<String>>| {
        record.push(std::mem::take(field));
        let fields = std::mem::take(record);
        if !(fields.len() == 1 && fields.first().is_some_and(|f| f.trim().is_empty())) {
            records.push(fields);
        }
    };
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
        } else if c == '"' && field.trim().is_empty() {
            field.clear();
            quoted = true;
        } else if c == delimiter {
            record.push(std::mem::take(&mut field));
        } else if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            finish(&mut record, &mut field, &mut records);
        } else if c == '\n' {
            finish(&mut record, &mut field, &mut records);
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !record.is_empty() {
        finish(&mut record, &mut field, &mut records);
    }
    records
}

/// Значение ячейки как число.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cell {
    Empty,
    Number(f64),
    Percent,
    Text,
}

/// Разбор числа ячейки (см. описание модуля).
pub fn cell(text: &str, delimiter: char) -> Cell {
    let text = text.trim();
    if text.is_empty() || text == "-" || text == "--" {
        return Cell::Empty;
    }
    if text.ends_with('%') {
        return match parse_number(text.trim_end_matches('%'), delimiter) {
            Some(_) => Cell::Percent,
            None => Cell::Text,
        };
    }
    parse_number(text, delimiter).map_or(Cell::Text, Cell::Number)
}

fn parse_number(text: &str, delimiter: char) -> Option<f64> {
    let compact: String = text
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '\u{a0}' | '\u{202f}'))
        .collect();
    let plain = |s: &str| -> Option<f64> {
        // `f64::from_str` принимает `inf` и `NaN`: числа отчёта — только цифры, знак, точка и
        // экспонента.
        let valid = !s.is_empty()
            && s.chars().any(|c| c.is_ascii_digit())
            && s.chars()
                .all(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | '.' | 'e' | 'E'));
        if !valid {
            return None;
        }
        s.parse::<f64>().ok().filter(|f| f.is_finite())
    };
    if let Some(number) = plain(&compact) {
        return Some(number);
    }
    let commas = compact.matches(',').count();
    if commas == 0 {
        return None;
    }
    // Десятичная запятая: `12,5`, если поля разделены не запятой.
    if delimiter != ',' && commas == 1 && !compact.contains('.') {
        return plain(&compact.replace(',', "."));
    }
    // Запятые разрядов: `1,234` и `1,234,567.89`.
    let (int, frac) = compact
        .split_once('.')
        .map_or((compact.as_str(), None), |(i, f)| (i, Some(f)));
    let digits = int.trim_start_matches(['-', '+']);
    let mut groups = digits.split(',');
    let head_ok = groups
        .next()
        .is_some_and(|g| (1..=3).contains(&g.len()) && g.chars().all(|c| c.is_ascii_digit()));
    let rest_ok = groups.all(|g| g.len() == 3 && g.chars().all(|c| c.is_ascii_digit()));
    if !head_ok || !rest_ok {
        return None;
    }
    let mut joined = int.replace(',', "");
    if let Some(frac) = frac {
        joined.push('.');
        joined.push_str(frac);
    }
    plain(&joined)
}

/// Сводка таблицы.
#[derive(Debug, Clone)]
pub struct Summary {
    pub value: Map<String, Value>,
    pub notes: Vec<String>,
}

/// Состояние колонки при обходе.
#[derive(Debug, Clone, Default)]
struct Column {
    numbers: u64,
    texts: u64,
    percents: u64,
    sum: f64,
}

/// Сводка CSV: число строк, колонки, итоги, первые `max_rows` строк.
pub fn summarize(bytes: &[u8], totals: Totals, max_rows: usize) -> Summary {
    let mut notes = Vec::new();
    let mut value = Map::new();
    let (text, encoding) = decode(bytes);
    if encoding == Encoding::Utf8Lossy {
        notes.push(
            "the report is not valid UTF-8: invalid bytes were replaced; save it with \
             `save_to` to read it in its own encoding"
                .to_owned(),
        );
    }
    let delimiter = detect_delimiter(&text);
    let mut all = records(&text, delimiter).into_iter();
    let Some(header) = all.next() else {
        value.insert("rows".into(), json!(0));
        value.insert("columns".into(), json!([]));
        value.insert("empty".into(), json!(true));
        notes.push(
            "the report file is empty: there is no data for this period and these filters"
                .to_owned(),
        );
        return Summary { value, notes };
    };
    let mut columns: Vec<String> = header.iter().map(|name| name.trim().to_owned()).collect();
    if columns.len() > MAX_COLUMNS {
        notes.push(format!(
            "the report has {} columns; only the first {MAX_COLUMNS} are summarized",
            columns.len()
        ));
        columns.truncate(MAX_COLUMNS);
    }
    let currency = totals
        .currency
        .and_then(|metric| columns.iter().position(|name| matches_metric(name, metric)));
    let mut stats = vec![Column::default(); columns.len()];
    // Суммы по валютам: (колонка, валюта) → сумма.
    let mut by_currency: Vec<(usize, String, f64)> = Vec::new();
    let mut rows = 0u64;
    let mut total_rows = 0u64;
    let mut ragged = 0u64;
    let mut first: Vec<Value> = Vec::new();
    for record in all {
        let label = record
            .first()
            .map(|cell| cell.trim().to_lowercase())
            .unwrap_or_default();
        if TOTAL_LABELS.contains(&label.as_str()) {
            total_rows += 1;
            continue;
        }
        rows += 1;
        if record.len() != header.len() {
            ragged += 1;
        }
        if first.len() < max_rows {
            let cells: Vec<Value> = record
                .iter()
                .take(columns.len())
                .map(|cell| json!(shorten(cell.trim())))
                .collect();
            first.push(Value::Array(cells));
        }
        let row_currency = currency
            .and_then(|index| record.get(index))
            .map(|code| code.trim().to_owned())
            .filter(|code| !code.is_empty());
        for (index, (stat, text)) in stats.iter_mut().zip(record.iter()).enumerate() {
            match cell(text, delimiter) {
                Cell::Empty => {}
                Cell::Number(number) => {
                    stat.numbers += 1;
                    stat.sum += number;
                    if let Some(code) = &row_currency {
                        match by_currency
                            .iter_mut()
                            .find(|(column, c, _)| *column == index && c == code)
                        {
                            Some(slot) => slot.2 += number,
                            None => by_currency.push((index, code.clone(), number)),
                        }
                    }
                }
                Cell::Percent => stat.percents += 1,
                Cell::Text => stat.texts += 1,
            }
        }
    }
    value.insert("rows".into(), json!(rows));
    value.insert("columns".into(), json!(columns));
    if rows == 0 {
        value.insert("empty".into(), json!(true));
        notes.push(
            "the report has only the header row: there is no data for this period and these \
             filters"
                .to_owned(),
        );
    }

    // Колонки для итогов: метрики вида отчёта, иначе все подходящие числовые.
    let numeric = |stat: &Column| stat.numbers > 0 && stat.texts == 0 && stat.percents == 0;
    let mut chosen: Vec<usize> = columns
        .iter()
        .enumerate()
        .filter(|(index, name)| {
            totals.metrics.iter().any(|m| matches_metric(name, *m))
                && stats.get(*index).is_some_and(numeric)
        })
        .map(|(index, _)| index)
        .collect();
    let known = !chosen.is_empty() || totals.metrics.is_empty();
    if !known && rows > 0 {
        chosen = columns
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(index, name)| {
                Some(*index) != currency
                    && !looks_like(name, RATE_WORDS)
                    && !looks_like(name, ID_WORDS)
                    && stats.get(*index).is_some_and(numeric)
            })
            .map(|(index, _)| index)
            .collect();
        if !chosen.is_empty() {
            notes.push(
                "the column names do not match the AppGallery Connect reference (another report \
                 language?), so `totals` sums every numeric column except the first one and \
                 rates; check that the sums make sense"
                    .to_owned(),
            );
        }
    }
    let mut sums = Map::new();
    let mut money = Map::new();
    for index in chosen {
        let Some(name) = columns.get(index) else {
            continue;
        };
        match currency {
            Some(_) => {
                let mut per = Map::new();
                for (_, code, sum) in by_currency.iter().filter(|(c, _, _)| *c == index) {
                    per.insert(code.clone(), number_json(*sum));
                }
                money.insert(name.clone(), Value::Object(per));
            }
            None => {
                if let Some(stat) = stats.get(index) {
                    sums.insert(name.clone(), number_json(stat.sum));
                }
            }
        }
    }
    if totals.currency.is_some() && currency.is_none() && rows > 0 {
        notes.push(
            "the report has no currency column, so amounts are summed without regard to \
             currency"
                .to_owned(),
        );
    }
    value.insert("totals".into(), Value::Object(sums));
    if currency.is_some() {
        value.insert("totalsByCurrency".into(), Value::Object(money));
    }
    value.insert("firstRows".into(), Value::Array(first));
    value.insert("truncated".into(), json!(rows > max_rows as u64));
    value.insert("encoding".into(), json!(encoding.as_str()));
    value.insert(
        "delimiter".into(),
        json!(match delimiter {
            '\t' => "tab".to_owned(),
            other => other.to_string(),
        }),
    );
    if total_rows > 0 {
        notes.push(format!(
            "{total_rows} total row(s) of the file were left out of `rows` and `totals`"
        ));
    }
    if ragged > 0 {
        notes.push(format!(
            "{ragged} row(s) have a different number of cells than the header"
        ));
    }
    Summary { value, notes }
}

/// Имя колонки начинается с имени метрики (без регистра, на английском или русском).
fn matches_metric(name: &str, metric: Metric) -> bool {
    let name = name.trim().to_lowercase();
    [metric.en, metric.ru]
        .iter()
        .any(|prefix| !prefix.is_empty() && name.starts_with(&prefix.to_lowercase()))
}

/// В имени колонки есть одно из слов.
fn looks_like(name: &str, words: &[&str]) -> bool {
    let name = name.to_lowercase();
    words.iter().any(|word| {
        if word.chars().all(|c| c.is_ascii_alphabetic()) {
            // Латинское слово — целиком: `id` не должен находить `paid`.
            name.split(|c: char| !c.is_alphanumeric())
                .any(|part| part == *word)
        } else {
            name.contains(word)
        }
    })
}

/// Число для JSON: целое без дробной части, иначе с округлением до 6 знаков.
fn number_json(value: f64) -> Value {
    let rounded = (value * 1e6).round() / 1e6;
    if rounded.fract() == 0.0 && rounded.abs() < 9.0e15 {
        json!(rounded as i64)
    } else {
        json!(rounded)
    }
}

/// Значение ячейки не длиннее [`MAX_CELL_CHARS`] символов.
fn shorten(text: &str) -> String {
    if text.chars().count() <= MAX_CELL_CHARS {
        return text.to_owned();
    }
    let mut short: String = text.chars().take(MAX_CELL_CHARS - 1).collect();
    short.push('…');
    short
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Totals = Totals {
        metrics: &[],
        currency: None,
    };

    #[test]
    fn encodings_are_detected() {
        assert_eq!(decode(b"a,b").1, Encoding::Utf8);
        let (text, enc) = decode(&[0xEF, 0xBB, 0xBF, b'a']);
        assert_eq!((text.as_str(), enc), ("a", Encoding::Utf8));
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("Время,1".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode(&le), ("Время,1".to_owned(), Encoding::Utf16Le));
        let be: Vec<u8> = "ab".encode_utf16().flat_map(u16::to_be_bytes).collect();
        assert_eq!(decode(&be), ("ab".to_owned(), Encoding::Utf16Be));
        assert_eq!(decode(&[b'a', 0xFF, b'b']).1, Encoding::Utf8Lossy);
    }

    #[test]
    fn delimiters_and_quotes() {
        assert_eq!(detect_delimiter("a;b;c\n1;2;3"), ';');
        assert_eq!(detect_delimiter("\n\na\tb\tc"), '\t');
        assert_eq!(detect_delimiter("\"a;b\",c"), ',');
        assert_eq!(detect_delimiter("one"), ',');
        let parsed = records(
            "name,text\r\n\"a, b\",\"line 1\nline \"\"2\"\"\"\r\n\r\nc,\n",
            ',',
        );
        assert_eq!(
            parsed,
            vec![
                vec!["name".to_owned(), "text".to_owned()],
                vec!["a, b".to_owned(), "line 1\nline \"2\"".to_owned()],
                vec!["c".to_owned(), String::new()],
            ]
        );
    }

    #[test]
    fn numbers_are_parsed_loosely() {
        assert_eq!(cell("12", ','), Cell::Number(12.0));
        assert_eq!(cell(" -1.5 ", ','), Cell::Number(-1.5));
        assert_eq!(cell("1,234", ','), Cell::Number(1234.0));
        assert_eq!(cell("1,234,567.25", ','), Cell::Number(1_234_567.25));
        assert_eq!(cell("12,5", ';'), Cell::Number(12.5));
        assert_eq!(cell("1 234,5", ';'), Cell::Number(1234.5));
        assert_eq!(cell("12.5%", ','), Cell::Percent);
        assert_eq!(cell("", ','), Cell::Empty);
        assert_eq!(cell("-", ','), Cell::Empty);
        for text in ["inf", "NaN", "RU", "2026-01-01", "1,23", "1.0.0"] {
            assert_eq!(cell(text, ','), Cell::Text, "{text}");
        }
    }

    #[test]
    fn totals_use_known_metrics() {
        const METRICS: &[Metric] = &[
            Metric {
                en: "New downloads",
                ru: "Новые загрузки",
            },
            Metric {
                en: "Uninstalls",
                ru: "Удаления",
            },
        ];
        let csv = "Время,Новые загрузки,Удаления,CTR\n\
                   20260101,10,1,5%\n\
                   20260102,\"1,000\",2,6%\n\
                   Итого,1010,3,5.5%\n";
        let totals = Totals {
            metrics: METRICS,
            currency: None,
        };
        let summary = summarize(csv.as_bytes(), totals, 1);
        let value = Value::Object(summary.value);
        assert_eq!(value["rows"], 2);
        assert_eq!(
            value["totals"],
            json!({"Новые загрузки": 1010, "Удаления": 3})
        );
        assert_eq!(value["firstRows"], json!([["20260101", "10", "1", "5%"]]));
        assert_eq!(value["truncated"], true);
        assert!(summary.notes.iter().any(|n| n.contains("1 total row")));
    }

    #[test]
    fn unknown_columns_fall_back_to_numeric_ones() {
        let csv = "日期,下载量,转化率,Result code\n20260101,5,1.5%,-110\n20260102,7.5,2%,-110\n";
        let summary = summarize(
            csv.as_bytes(),
            Totals {
                metrics: &[Metric {
                    en: "Impressions",
                    ru: "Впечатления",
                }],
                currency: None,
            },
            10,
        );
        let value = Value::Object(summary.value);
        assert_eq!(value["totals"], json!({"下载量": 12.5}));
        assert!(summary.notes.iter().any(|n| n.contains("do not match")));
    }

    #[test]
    fn money_is_summed_per_currency() {
        let csv = "Purchased(UTC);App name;Purchase price;Currency\n\
                   2026-01-01;App;1,5;EUR\n\
                   2026-01-02;App;2;USD\n\
                   2026-01-03;App;3;EUR\n";
        let summary = summarize(
            csv.as_bytes(),
            Totals {
                metrics: &[Metric {
                    en: "Purchase price",
                    ru: "Цена покупки",
                }],
                currency: Some(Metric {
                    en: "Currency",
                    ru: "Валюта",
                }),
            },
            10,
        );
        let value = Value::Object(summary.value);
        assert_eq!(
            value["totalsByCurrency"],
            json!({"Purchase price": {"EUR": 4.5, "USD": 2}})
        );
        assert_eq!(value["delimiter"], ";");
    }

    #[test]
    fn empty_reports() {
        let summary = summarize(b"", NONE, 5);
        assert_eq!(summary.value["rows"], 0);
        assert_eq!(summary.value["empty"], true);
        assert!(summary.notes[0].contains("empty"));
        let summary = summarize(b"\xEF\xBB\xBFa,b\r\n", NONE, 5);
        assert_eq!(summary.value["columns"], json!(["a", "b"]));
        assert_eq!(summary.value["empty"], true);
        assert!(summary.notes[0].contains("only the header"));
    }

    #[test]
    fn names_are_matched_by_words() {
        assert!(looks_like("Order ID", ID_WORDS));
        assert!(!looks_like("Paying users", ID_WORDS));
        assert!(looks_like("Доля платящих пользователей", RATE_WORDS));
        assert!(looks_like("CTR", RATE_WORDS));
        assert_eq!(shorten(&"я".repeat(300)).chars().count(), MAX_CELL_CHARS);
        assert_eq!(number_json(2.0), json!(2));
        assert_eq!(number_json(0.1 + 0.2), json!(0.3));
    }
}
