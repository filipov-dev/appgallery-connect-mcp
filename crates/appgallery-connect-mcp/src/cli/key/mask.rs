//! Маскирование учётных данных API-клиента и токенов в потоковом выводе дочерней команды
//! (`key exec`).
//!
//! Образцы для поиска ([`needles_for_credentials`]):
//!
//! - секрет клиента и его base64;
//! - идентификатор клиента и его base64 (не секрет, но относится к конкретному кабинету;
//!   короткие значения не маскируются, чтобы не портить вывод);
//! - содержимое файла целиком (с завершающим переводом строки и без) и его base64.
//!
//! Так `cat "$APPGALLERY_CONNECT_CREDENTIALS_FILE"`, `grep client_secret …` и `base64 < …`
//! показывают `***`.
//!
//! Кроме образцов маскируются access token AppGallery Connect: это JWT, его заголовок в
//! base64url начинается с `eyJhbGci`, `eyJ0eXAi` или `eyJraWQi` (`{"alg"`, `{"typ"`,
//! `{"kid"`); от префикса до конца символов токена (буквы, цифры, `-`, `_`, `.`) — дочерняя
//! команда получает токены по учётным данным сама.
//!
//! Алгоритм ([`Masker`]):
//!
//! - Ищутся все вхождения всех образцов, в том числе перекрывающиеся. Пересекающиеся и
//!   соседние вхождения сливаются в одну область, и вся область заменяется одним [`MASK`]:
//!   так при перекрытии двух образцов не остаётся видимого «хвоста» ни одного из них.
//! - Вывод приходит кусками. Позиция в буфере «решена», если любой образец (или префикс
//!   токена), начинающийся в ней, либо целиком помещается в буфер, либо уже не совпадает.
//!   Нерешённый хвост (короче самого длинного образца и совпадает с началом какого-то
//!   образца) переносится в следующий кусок, поэтому значение, разрезанное границей кусков,
//!   всё равно маскируется. Хвост, который не может быть началом значения, выводится сразу:
//!   приглашения без перевода строки не задерживаются.
//! - Открытая область маски и «внутри токена» тоже переносятся между кусками: [`MASK`]
//!   выводится один раз, а токен, разрезанный границей кусков, скрывается целиком.
//!
//! Маскирование — страховка от случайной утечки, а не граница безопасности: значение,
//! преобразованное дочерней командой (hex, разбивка на части), не распознаётся.

use appgallery_connect_api::ApiClientCredentials;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use zeroize::Zeroizing;

/// Чем заменяется секрет в выводе.
pub const MASK: &[u8] = b"***";

/// Начала access token AppGallery Connect (заголовок JWT): после префикса скрываются все
/// символы токена.
const TOKEN_PREFIXES: [&[u8]; 3] = [b"eyJhbGci", b"eyJ0eXAi", b"eyJraWQi"];

/// Образцы короче этого не маскируются: короткая строка совпадёт с чем угодно и испортит
/// вывод.
const MIN_MASK_LEN: usize = 4;

/// Идентификатор клиента короче этого не маскируется (как в `ClientId::masked`).
const MIN_CLIENT_ID_LEN: usize = 8;

/// Размер буфера чтения вывода дочерней команды.
const READ_BUF: usize = 8 * 1024;

type Needle = Zeroizing<Vec<u8>>;

/// Значение и его base64 (стандартный и URL-safe алфавиты без `=`, так base64 находится и
/// внутри более длинной строки, и с паддингом).
fn with_base64(needles: &mut Vec<Needle>, value: &[u8]) {
    needles.push(Zeroizing::new(value.to_vec()));
    needles.push(Zeroizing::new(STANDARD_NO_PAD.encode(value).into_bytes()));
    needles.push(Zeroizing::new(URL_SAFE_NO_PAD.encode(value).into_bytes()));
}

/// Текст файла: как есть, без завершающих пробельных символов и с одним переводом строки в
/// конце (так `cat` файла скрывается вместе с его последним переводом строки), и base64
/// каждой формы.
fn file_forms(needles: &mut Vec<Needle>, text: &str) {
    let text = text.as_bytes();
    let trimmed = text.trim_ascii_end();
    let mut line = Zeroizing::new(trimmed.to_vec());
    line.push(b'\n');
    for form in [text, trimmed, line.as_slice()] {
        with_base64(needles, form);
    }
}

/// Строки, которые нужно маскировать для учётных данных и файла с ними (см. описание
/// модуля).
pub fn needles_for_credentials(
    credentials: &ApiClientCredentials,
    file_contents: &str,
) -> Vec<Needle> {
    let mut needles = Vec::new();
    file_forms(&mut needles, file_contents);
    with_base64(
        &mut needles,
        credentials.client_secret().expose().as_bytes(),
    );
    let client_id = credentials.client_id().expose();
    if client_id.len() >= MIN_CLIENT_ID_LEN {
        with_base64(&mut needles, client_id.as_bytes());
    }
    needles.retain(|needle| needle.len() >= MIN_MASK_LEN);
    needles.sort_unstable_by(|a, b| a.as_slice().cmp(b.as_slice()));
    needles.dedup_by(|a, b| a.as_slice() == b.as_slice());
    needles
}

/// Символ access token (base64url и точки JWT).
fn is_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
}

/// Потоковый маскировщик. Не `Clone`/`Debug`: внутри учётные данные.
pub struct Masker {
    needles: Vec<Needle>,
    /// Байты, с которых начинается хотя бы один образец или префикс токена (быстрый пропуск
    /// остальных).
    starts: [bool; 256],
    max_len: usize,
    /// Нерешённый хвост предыдущего куска.
    carry: Zeroizing<Vec<u8>>,
    /// До какой позиции (относительно начала `carry`) байты уже скрыты открытой областью.
    masked_until: usize,
    /// Область маски открыта: [`MASK`] для неё уже выведен.
    region_open: bool,
    /// Идёт токен: следующие символы токена тоже скрываются.
    in_token: bool,
}

impl Masker {
    pub fn new<I, T>(needles: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: AsRef<[u8]>,
    {
        let needles: Vec<Needle> = needles
            .into_iter()
            .filter(|n| !n.as_ref().is_empty())
            .map(|n| Zeroizing::new(n.as_ref().to_vec()))
            .collect();
        let mut starts = [false; 256];
        for needle in needles.iter().map(|n| n.as_slice()).chain(TOKEN_PREFIXES) {
            if let Some(&first) = needle.first() {
                starts[usize::from(first)] = true;
            }
        }
        let max_len = needles
            .iter()
            .map(|n| n.len())
            .chain(TOKEN_PREFIXES.iter().map(|p| p.len()))
            .max()
            .unwrap_or(0);
        Self {
            needles,
            starts,
            max_len,
            carry: Zeroizing::new(Vec::new()),
            masked_until: 0,
            region_open: false,
            in_token: false,
        }
    }

    /// Обрабатывает очередной кусок; возвращает то, что уже можно вывести.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.process(chunk, false)
    }

    /// Конец потока: выводит остаток.
    pub fn finish(&mut self) -> Vec<u8> {
        self.process(&[], true)
    }

    fn open_region(&mut self, out: &mut Vec<u8>, until: usize) {
        if !self.region_open {
            out.extend_from_slice(MASK);
            self.region_open = true;
        }
        self.masked_until = self.masked_until.max(until);
    }

    fn process(&mut self, chunk: &[u8], eof: bool) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let limit = if eof {
            buf.len()
        } else {
            self.decided_len(&buf)
        };

        let mut out = Vec::with_capacity(limit + MASK.len());
        for pos in 0..limit {
            let byte = buf[pos];
            if self.starts[usize::from(byte)] {
                let rest = &buf[pos..];
                let mut until = None;
                for needle in &self.needles {
                    if rest.starts_with(needle) {
                        until = until.max(Some(pos + needle.len()));
                    }
                }
                for prefix in TOKEN_PREFIXES {
                    if rest.starts_with(prefix) {
                        until = until.max(Some(pos + prefix.len()));
                        self.in_token = true;
                    }
                }
                if let Some(until) = until {
                    self.open_region(&mut out, until);
                }
            }
            if pos < self.masked_until {
                continue;
            }
            if self.in_token && is_token_byte(byte) {
                // Область маски остаётся открытой: токен скрывается одним `***`.
                continue;
            }
            self.in_token = false;
            self.region_open = false;
            out.push(byte);
        }

        if eof {
            self.masked_until = 0;
            self.region_open = false;
            self.in_token = false;
        } else {
            self.masked_until = self.masked_until.saturating_sub(limit);
            self.carry = Zeroizing::new(buf.split_off(limit));
        }
        out
    }

    /// Длина решённой части буфера: первая позиция, с которой хвост буфера — собственный
    /// префикс какого-то образца или префикса токена (дальше без следующего куска решить
    /// нельзя).
    fn decided_len(&self, buf: &[u8]) -> usize {
        let from = buf.len().saturating_sub(self.max_len.saturating_sub(1));
        (from..buf.len())
            .find(|&pos| {
                let tail = &buf[pos..];
                self.needles
                    .iter()
                    .map(|n| n.as_slice())
                    .chain(TOKEN_PREFIXES)
                    .any(|needle| needle.len() > tail.len() && needle.starts_with(tail))
            })
            .unwrap_or(buf.len())
    }
}

/// Копирует поток `reader` в `writer`, маскируя значения. Ошибка записи (например, закрытый
/// канал у `| head`) прекращает копирование: `reader` закрывается, как в обычном конвейере.
pub async fn pump<R, W>(mut reader: R, mut writer: W, mut masker: Masker) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = Zeroizing::new(vec![0u8; READ_BUF]);
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let out = masker.push(&buf[..read]);
        if !out.is_empty() {
            writer.write_all(&out).await?;
            writer.flush().await?;
        }
    }
    let out = masker.finish();
    writer.write_all(&out).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::key::FileFormat;
    use crate::testing::credentials;

    fn masker(values: &[&str]) -> Masker {
        Masker::new(values.iter().map(|v| v.as_bytes()))
    }

    /// Прогоняет текст кусками заданного размера.
    fn run(masker: &mut Masker, text: &str, chunk: usize) -> String {
        let mut out = Vec::new();
        for part in text.as_bytes().chunks(chunk) {
            out.extend(masker.push(part));
        }
        out.extend(masker.finish());
        String::from_utf8(out).unwrap()
    }

    /// Результат должен совпадать при любом разбиении на куски.
    fn all_chunkings(make: impl Fn() -> Masker, text: &str) -> String {
        let whole = run(&mut make(), text, text.len().max(1));
        for size in 1..=text.len().max(1) {
            assert_eq!(run(&mut make(), text, size), whole, "chunk size {size}");
        }
        whole
    }

    #[test]
    fn masks_single_value() {
        assert_eq!(
            all_chunkings(
                || masker(&["s3cret-value"]),
                "token=s3cret-value; again s3cret-value\n"
            ),
            "token=***; again ***\n"
        );
    }

    #[test]
    fn masks_value_split_across_chunks() {
        let mut m = masker(&["s3cret-value"]);
        let mut out = m.push(b"before s3cr");
        // Начало значения задержано, остальное выведено сразу.
        assert_eq!(out, b"before ");
        out.extend(m.push(b"et-val"));
        out.extend(m.push(b"ue after"));
        out.extend(m.finish());
        assert_eq!(String::from_utf8(out).unwrap(), "before *** after");
    }

    #[test]
    fn does_not_hold_unrelated_tail() {
        let mut m = masker(&["s3cret-value"]);
        assert_eq!(m.push(b"Password: "), b"Password: ");
        assert_eq!(m.push(b"xyz"), b"xyz");
        // Хвост, похожий на начало значения, ждёт; на конце потока выводится как есть.
        assert_eq!(m.push(b" s3c"), b" ");
        assert_eq!(m.finish(), b"s3c");
    }

    #[test]
    fn masks_overlapping_values_as_one_region() {
        assert_eq!(
            all_chunkings(|| masker(&["abcdefgh", "efghijkl"]), "x abcdefghijkl y"),
            "x *** y"
        );
        assert_eq!(
            all_chunkings(
                || masker(&["pass", "password-long"]),
                "1 password-long 2 pass 3 passw"
            ),
            "1 *** 2 *** 3 ***w"
        );
        assert_eq!(all_chunkings(|| masker(&["abcd"]), "abcdabcd!"), "***!");
    }

    #[test]
    fn masks_access_tokens() {
        let token = ["eyJhbGciOiJSUzI1NiJ9", "eyJzdWIiOiIxIn0", "c2ln-_x"].join(".");
        assert_eq!(
            all_chunkings(
                || masker(&[]),
                &format!("Authorization: Bearer {token}\n{{\"access_token\":\"{token}\"}}\n")
            ),
            "Authorization: Bearer ***\n{\"access_token\":\"***\"}\n"
        );
        // Base64 другого JSON и похожее начало — не токен.
        assert_eq!(
            all_chunkings(|| masker(&[]), "eyJuYW1lIjoiYSJ9 eyJhbG"),
            "eyJuYW1lIjoiYSJ9 eyJhbG"
        );
    }

    #[test]
    fn masks_every_form_of_the_credentials() {
        for format in [FileFormat::Json, FileFormat::Dotenv] {
            let creds = credentials("3");
            let contents = format.render(&creds).unwrap();
            let id = creds.client_id().expose();
            let secret = creds.client_secret().expose();
            let masker = || Masker::new(needles_for_credentials(&creds, &contents));

            // `cat` файла.
            assert_eq!(run(&mut masker(), &contents, 7), "***");
            // Отдельные значения и их base64.
            assert_eq!(
                run(&mut masker(), &format!("id={id} secret={secret}\n"), 5),
                "id=*** secret=***\n"
            );
            let encoded = base64::engine::general_purpose::STANDARD.encode(secret);
            assert_eq!(run(&mut masker(), &format!("{encoded}\n"), 3), "***\n");
            // base64 всего файла (`base64 < credentials.json`), с паддингом.
            let encoded = base64::engine::general_purpose::STANDARD.encode(contents.as_bytes());
            let out = run(&mut masker(), &encoded, 11);
            assert!(!out.contains(encoded.trim_end_matches('=')), "{out}");
            assert!(out.starts_with("***"), "{out}");
            // Имена полей без значений остаются.
            assert_eq!(
                run(&mut masker(), "client_secret HUAWEI_CLIENT_ID\n", 4),
                "client_secret HUAWEI_CLIENT_ID\n"
            );
        }
    }

    #[test]
    fn short_client_id_is_not_masked() {
        let creds =
            ApiClientCredentials::from_parts("1234", credentials("1").client_secret().expose())
                .unwrap();
        let needles = needles_for_credentials(&creds, "x");
        assert!(needles.iter().all(|n| n.as_slice() != b"1234"));
    }

    #[test]
    fn handles_binary_output() {
        let value = [0xffu8, 0x00, 0x10, 0x80, 0x7f];
        let mut m = Masker::new([value]);
        let mut input = vec![1u8, 2];
        input.extend_from_slice(&value);
        input.push(3);
        let mut out = Vec::new();
        for byte in &input {
            out.extend(m.push(std::slice::from_ref(byte)));
        }
        out.extend(m.finish());
        assert_eq!(out, [1, 2, b'*', b'*', b'*', 3]);
    }

    #[tokio::test]
    async fn pump_masks_stream() {
        let input: &[u8] = b"line one s3cret-value\nline two\n";
        let mut output = Vec::new();
        pump(input, &mut output, masker(&["s3cret-value"]))
            .await
            .unwrap();
        assert_eq!(output, b"line one ***\nline two\n");
    }
}
