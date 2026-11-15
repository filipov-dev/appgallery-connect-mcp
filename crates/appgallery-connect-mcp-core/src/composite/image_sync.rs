//! `image_sync`: иконка, скриншоты и видео страницы приложения из локального каталога
//! (формат — [`super::listing`]).
//!
//! Слоты — язык и вид файлов: иконка (`<lang>/images/icon.png`, fileType 0), скриншоты
//! (`<lang>/images/screenshots/`, по именам файлов: `2.png` раньше `10.png`, fileType 2) и
//! видео с обложками (`<lang>/videos/1.mp4` и `1.jpg`, fileType 1). AppGallery Connect
//! заменяет файлы слота целиком (`publishing.fileInfo.update` с полным списком), поэтому
//! единица изменения — слот: изменился один скриншот — загружаются и прикрепляются все
//! скриншоты этого языка, остальные слоты не трогаются. Слот без локальных файлов не
//! трогается.
//!
//! Что изменилось. API не отдаёт ни хешей, ни размеров файлов: в `get_app_info` только
//! адреса картинок на CDN, и картинки там перекодированы. Поэтому сравнение идёт с
//! манифестом `<dir>/.appgallery-images.json`, который пишут `listing_export` (по скачанным
//! файлам) и `image_sync` (после успешной загрузки): по каждому слоту — `appId`, имена,
//! SHA-256 и размеры файлов и сколько файлов слота AppGallery Connect показал после
//! синхронизации. Слот загружается, если:
//! - в манифесте его нет (или манифест от другого приложения);
//! - имена, порядок или SHA-256 локальных файлов отличаются от манифеста;
//! - число файлов слота в AppGallery Connect отличается от записанного (файлы поменяли в
//!   консоли или удалили);
//! - `force`.
//!
//! Замену картинки в консоли без изменения числа файлов манифест не видит: тогда нужен
//! `force`. Манифест — единственный файл, который инструмент заменяет: он пишется во
//! временный новый файл и переименовывается поверх прежнего. Пользовательские файлы не
//! перезаписываются.
//!
//! До загрузки: каталог (лишние файлы, число файлов, пары видео и обложек, повторы) — до
//! любых запросов; содержимое слотов, которые будут загружены (формат по первым байтам и
//! расширению, размер файла, разрешение) — после чтения состояния, до первой загрузки.
//! Правила — по приложению справочника «App File Restrictions» для приложений Android на
//! телефонах: иконка PNG 216 x 216 до 2 МБ; скриншоты JPG/PNG до 2 МБ, от 3 до 5, одной
//! ориентации; видео MP4/MOV до 500 МБ, до трёх, с обложкой JPG/PNG до 2 МБ. Справочник
//! называет разрешение скриншотов и обложек 450 x 800 и 800 x 450; инструмент принимает
//! любое 9:16 или 16:9 с короткой стороной от 450 пикселей. Длина видео не проверяется.
//!
//! Загрузка одного файла — общий помощник [`super::upload`]: целиком, адрес из
//! `upload.uploadUrl.get` (операция исключена из инструментов и `api_request`), отправка
//! файла `ApiClient::upload_file` (только на хосты Huawei) с повтором сбойной отправки,
//! идентификатор объекта — `fileDestUrl` для `publishing.fileInfo.update`. Адрес, заголовки
//! и идентификатор объекта — секреты: их нет в ответе, ошибках и логах.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use appgallery_connect_api::AccessLevel;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::listing::{
    self, Remote, TOOLSET, app_args, app_id_schema, bool_arg, dir_entries, is_language_dir,
    path_text, release_type, release_type_schema, required_str, string_list,
};
use super::upload::{self, UploadSpec, UploadTuning, UploadedFile};
use super::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};

/// Имя инструмента.
pub const NAME: &str = "image_sync";

/// Манифест последней синхронизации в корне каталога.
pub(crate) const MANIFEST: &str = ".appgallery-images.json";
const MANIFEST_VERSION: u32 = 1;

const FILE_INFO_UPDATE: &str = "publishing.fileInfo.update";

const MIB: u64 = 1024 * 1024;
/// Наибольший размер картинки (иконка, скриншот, обложка видео).
const MAX_IMAGE_BYTES: u64 = 2 * MIB;
/// Наибольший размер видео.
const MAX_VIDEO_BYTES: u64 = 500 * MIB;
/// Сторона иконки.
const ICON_SIDE: u32 = 216;
/// Короткая сторона скриншота и обложки, не меньше.
const MIN_SHORT_SIDE: u32 = 450;
/// Скриншотов одного языка.
const MIN_SCREENSHOTS: usize = 3;
const MAX_SCREENSHOTS: usize = 5;
/// Видео одного языка.
const MAX_VIDEOS: usize = 3;

const DESCRIPTION: &str = "Upload the app icon, screenshots and introduction videos of the \
AppGallery store listing from a local directory (as written by listing_export): \
<lang>/images/icon.png, <lang>/images/screenshots/1.png ... (ordered by file name) and \
<lang>/videos/1.mp4 with its poster 1.jpg (up to three pairs). AppGallery Connect replaces \
all files of a kind in a language at once, so the unit of change is one kind in one language: \
only kinds whose local files changed since the last sync (by SHA-256, recorded in \
<dir>/.appgallery-images.json) or whose file count in AppGallery Connect changed are uploaded; \
kinds without local files are left alone. A replacement made in the AppGallery Connect \
console that keeps the number of files is not detected: pass `force` then. Checks before any \
upload (rules for Android apps on phones): icon PNG 216 x 216 up to 2 MB; 3 to 5 screenshots, \
JPG or PNG up to 2 MB, 9:16 or 16:9 with the short side at least 450 px, all of one \
orientation; videos MP4 or MOV up to 500 MB with JPG or PNG posters like the screenshots. \
`dry_run` returns the plan only. The changes apply to the draft version and reach users only \
after the app is submitted and approved (submit_app). Local paths are allowed only inside the \
server's allowed directories. Upload URLs and object IDs are never returned.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: TOOLSET,
        access: AccessLevel::Write,
        description: DESCRIPTION,
        input_schema,
        run,
    }]
}

fn input_schema() -> Value {
    let kinds: Vec<&str> = MediaKind::ALL.iter().map(|kind| kind.name()).collect();
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "appId": app_id_schema(),
            "dir": {
                "type": "string",
                "description": "Listing directory (inside the allowed directories), as written \
                    by listing_export."
            },
            "languages": {
                "type": "array",
                "items": {"type": "string", "minLength": 2, "maxLength": 16},
                "minItems": 1,
                "description": "Only these languages (directory names such as en-US). Default: \
                    every language directory."
            },
            "kinds": {
                "type": "array",
                "items": {"type": "string", "enum": kinds},
                "minItems": 1,
                "description": "Only these kinds of files. Default: all."
            },
            "force": {
                "type": "boolean",
                "description": "Upload every selected kind that has local files, even when \
                    nothing changed since the last sync. Default: false."
            },
            "dry_run": {
                "type": "boolean",
                "description": "Only return the plan, upload nothing. Default: false."
            },
            "chineseMainland": {
                "type": "boolean",
                "description": "Whether the app is distributed in the Chinese mainland (the \
                    upload URL request needs it for accounts registered outside the Chinese \
                    mainland). Default: true when the release countries of the app include CN."
            },
            "releaseType": release_type_schema()
        },
        "required": ["dir"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(sync(ctx, args, UploadTuning::default()))
}

/// Вид файлов страницы приложения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MediaKind {
    Icon,
    Screenshots,
    Videos,
}

impl MediaKind {
    pub(crate) const ALL: [MediaKind; 3] = [Self::Icon, Self::Screenshots, Self::Videos];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Icon => "icon",
            Self::Screenshots => "screenshots",
            Self::Videos => "videos",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }

    /// `fileType` операции `publishing.fileInfo.update`.
    fn file_type(self) -> u8 {
        match self {
            Self::Icon => 0,
            Self::Screenshots => 2,
            Self::Videos => 1,
        }
    }
}

/// Файлы страницы приложения одного языка в AppGallery Connect (адреса на CDN).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RemoteMedia {
    pub icon: Option<String>,
    pub screenshots: Vec<String>,
    /// Пары (видео, обложка); `None` — поле не разобралось.
    pub videos: Option<Vec<(String, String)>>,
}

impl RemoteMedia {
    /// Сколько файлов слота в AppGallery Connect (у видео — пар); `None` — неизвестно.
    pub(crate) fn count(&self, kind: MediaKind) -> Option<usize> {
        match kind {
            MediaKind::Icon => Some(usize::from(self.icon.is_some())),
            MediaKind::Screenshots => Some(self.screenshots.len()),
            MediaKind::Videos => self.videos.as_ref().map(Vec::len),
        }
    }
}

/// Файлы одного языка из `languages[]` ответа `get_app_info`. Поля телефона берутся из
/// корня языка, а если их нет — из `deviceMaterials` с `deviceType` 4 (телефон).
pub(crate) fn remote_media(language: &Value) -> RemoteMedia {
    let phone = language
        .get("deviceMaterials")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("deviceType").and_then(Value::as_u64) == Some(4))
        });
    let field = |own: &str, device: &str| {
        let value = language.get(own).filter(|v| !is_empty_value(v));
        value.or_else(|| {
            phone
                .and_then(|p| p.get(device))
                .filter(|v| !is_empty_value(v))
        })
    };
    let icon = field("icon", "appIcon")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|url| !url.is_empty())
        .map(str::to_owned);
    let screenshots = field("introPic", "screenShots")
        .map(url_list)
        .unwrap_or_default();
    let videos = match field("introVideo", "introVedios") {
        None => Some(Vec::new()),
        Some(value) => parse_videos(value),
    };
    RemoteMedia {
        icon,
        screenshots,
        videos,
    }
}

fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.trim().is_empty() || text.trim() == "[]",
        Value::Array(items) => items.is_empty(),
        _ => false,
    }
}

/// Адреса через запятую или массивом.
fn url_list(value: &Value) -> Vec<String> {
    let items: Vec<String> = match value {
        Value::String(text) => text.split(',').map(str::to_owned).collect(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    };
    items
        .into_iter()
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
        .collect()
}

/// Видео: строка с JSON-массивом объектов `videoUrl`/`frameUrl` (в `deviceMaterials` —
/// `vedioUrl`, так в справочнике) или массив таких объектов либо строк с ними.
fn parse_videos(value: &Value) -> Option<Vec<(String, String)>> {
    let items: Vec<Value> = match value {
        Value::String(text) => match serde_json::from_str::<Value>(text).ok()? {
            Value::Array(items) => items,
            object @ Value::Object(_) => vec![object],
            _ => return None,
        },
        Value::Array(items) => items.clone(),
        _ => return None,
    };
    let mut videos = Vec::new();
    for item in items {
        let item = match item {
            Value::String(text) => serde_json::from_str::<Value>(&text).ok()?,
            other => other,
        };
        let url = |names: &[&str]| {
            names
                .iter()
                .find_map(|name| item.get(*name).and_then(Value::as_str))
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
        };
        let video = url(&["videoUrl", "vedioUrl"])?;
        let frame = url(&["frameUrl"]).unwrap_or_default();
        videos.push((video, frame));
    }
    Some(videos)
}

/// Формат файла по первым байтам.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileFormat {
    Png,
    Jpeg,
    Webp,
    Mp4,
    Mov,
}

impl FileFormat {
    pub(crate) fn sniff(head: &[u8]) -> Option<Self> {
        if head.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(Self::Png);
        }
        if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(Self::Jpeg);
        }
        if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
            return Some(Self::Webp);
        }
        let atom = head.get(4..8)?;
        if atom == b"ftyp" {
            return Some(if head.get(8..12) == Some(b"qt  ") {
                Self::Mov
            } else {
                Self::Mp4
            });
        }
        // QuickTime без `ftyp`: файл начинается с другого атома.
        if [b"moov", b"wide", b"mdat", b"free"]
            .iter()
            .any(|a| atom == *a)
        {
            return Some(Self::Mov);
        }
        None
    }

    pub(crate) fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
        }
    }

    fn is_video(self) -> bool {
        matches!(self, Self::Mp4 | Self::Mov)
    }

    /// Подходит ли расширение файла к формату.
    fn matches_extension(self, ext: &str) -> bool {
        match self {
            Self::Png => ext == "png",
            Self::Jpeg => matches!(ext, "jpg" | "jpeg"),
            Self::Webp => ext == "webp",
            Self::Mp4 => matches!(ext, "mp4" | "mov"),
            Self::Mov => matches!(ext, "mov" | "mp4"),
        }
    }
}

fn extension(name: &str) -> Option<String> {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
}

fn is_image_name(name: &str) -> bool {
    extension(name).is_some_and(|e| matches!(e.as_str(), "png" | "jpg" | "jpeg" | "webp"))
}

fn is_video_name(name: &str) -> bool {
    extension(name).is_some_and(|e| matches!(e.as_str(), "mp4" | "mov"))
}

/// Размеры картинки в пикселях из заголовка PNG (`IHDR`) или JPEG (маркер `SOFn`).
pub(crate) fn image_size(format: FileFormat, bytes: &[u8]) -> Option<(u32, u32)> {
    match format {
        FileFormat::Png => {
            let ihdr = bytes.get(12..24)?;
            if ihdr.get(..4)? != b"IHDR" {
                return None;
            }
            let width = u32::from_be_bytes(ihdr.get(4..8)?.try_into().ok()?);
            let height = u32::from_be_bytes(ihdr.get(8..12)?.try_into().ok()?);
            Some((width, height))
        }
        FileFormat::Jpeg => {
            let mut pos = 2usize;
            loop {
                let header = bytes.get(pos..pos + 4)?;
                if header[0] != 0xFF {
                    return None;
                }
                let marker = header[1];
                if marker == 0xFF {
                    pos += 1;
                    continue;
                }
                // Маркеры без длины.
                if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
                    pos += 2;
                    continue;
                }
                let len = usize::from(u16::from_be_bytes([header[2], header[3]]));
                let is_sof = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
                if is_sof {
                    let segment = bytes.get(pos + 4..pos + 9)?;
                    let height = u32::from(u16::from_be_bytes([segment[1], segment[2]]));
                    let width = u32::from(u16::from_be_bytes([segment[3], segment[4]]));
                    return Some((width, height));
                }
                pos += 2 + len;
            }
        }
        _ => None,
    }
}

/// SHA-256 (hex) и размер файла; файл читается потоком.
pub(crate) fn sha256_file(path: &Path) -> Result<(String, u64), ToolError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| ToolError::new(format!("cannot read `{}`: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| ToolError::new(format!("cannot read `{}`: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        total += read as u64;
        hasher.update(&buffer[..read]);
    }
    Ok((hex(&hasher.finalize()), total))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

/// Первые байты файла.
fn read_head(path: &Path, len: usize) -> Result<Vec<u8>, ToolError> {
    let file = std::fs::File::open(path)
        .map_err(|e| ToolError::new(format!("cannot read `{}`: {e}", path.display())))?;
    let mut head = Vec::with_capacity(len);
    file.take(len as u64)
        .read_to_end(&mut head)
        .map_err(|e| ToolError::new(format!("cannot read `{}`: {e}", path.display())))?;
    Ok(head)
}

// ---------------------------------------------------------------------------
// Манифест.

/// Манифест последней синхронизации картинок.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Manifest {
    pub version: u32,
    #[serde(rename = "appId")]
    pub app_id: String,
    pub slots: Vec<ManifestSlot>,
}

/// Слот манифеста: язык, вид, файлы и сколько файлов показал AppGallery Connect.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct ManifestSlot {
    pub language: String,
    pub kind: String,
    pub files: Vec<ManifestFile>,
    /// Число файлов слота (у видео — пар) в AppGallery Connect после синхронизации;
    /// `None` — неизвестно.
    #[serde(default)]
    pub remote_count: Option<usize>,
}

/// Файл слота: путь от каталога языка, SHA-256 и размер.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ManifestFile {
    pub name: String,
    pub sha256: String,
    pub bytes: u64,
}

impl Manifest {
    pub(crate) fn new(app_id: &str) -> Self {
        Self {
            version: MANIFEST_VERSION,
            app_id: app_id.to_owned(),
            slots: Vec::new(),
        }
    }

    fn slot(&self, language: &str, kind: MediaKind) -> Option<&ManifestSlot> {
        self.slots
            .iter()
            .find(|slot| slot.language == language && slot.kind == kind.name())
    }

    /// Заменяет или добавляет слот; слоты упорядочены по языку и виду.
    pub(crate) fn put(&mut self, slot: ManifestSlot) {
        self.slots
            .retain(|old| !(old.language == slot.language && old.kind == slot.kind));
        self.slots.push(slot);
        self.slots.sort_by(|a, b| {
            let rank = |s: &ManifestSlot| MediaKind::from_name(&s.kind).map(MediaKind::file_type);
            (a.language.as_str(), rank(a)).cmp(&(b.language.as_str(), rank(b)))
        });
    }
}

/// Манифест каталога. `Ok(None)` — манифеста нет; непонятный манифест — `None` и пометка.
fn read_manifest(
    ctx: &Context,
    root: &Path,
) -> Result<(Option<Manifest>, Option<String>), ToolError> {
    let path = root.join(MANIFEST);
    if path.symlink_metadata().is_err() {
        return Ok((None, None));
    }
    let text = listing::read_text(ctx, &path)?;
    match serde_json::from_str::<Manifest>(&text) {
        Ok(manifest) if manifest.version == MANIFEST_VERSION => Ok((Some(manifest), None)),
        _ => Ok((
            None,
            Some(format!(
                "{MANIFEST} is not a manifest of this server's version; every kind with local \
                 files is treated as changed and the manifest is written anew"
            )),
        )),
    }
}

/// Записывает манифест: новый временный файл (`create_new`) и переименование поверх
/// прежнего манифеста. Прежний манифест должен быть обычным файлом в разрешённых каталогах.
pub(crate) fn write_manifest(
    ctx: &Context,
    root: &Path,
    manifest: &Manifest,
) -> Result<(), ToolError> {
    let mut text = serde_json::to_string_pretty(manifest)
        .map_err(|e| ToolError::new(format!("cannot serialize the manifest: {e}")))?;
    text.push('\n');
    let target = root.join(MANIFEST);
    if target.symlink_metadata().is_ok() {
        ctx.files.resolve_read(path_text(&target)?)?;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let temp = root.join(format!("{MANIFEST}.{}-{nanos}.tmp", std::process::id()));
    let temp = ctx.files.resolve_write(path_text(&temp)?)?;
    crate::files::write_new_file(&temp, text.as_bytes())?;
    std::fs::rename(&temp, &target).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        ToolError::new(format!("cannot write `{MANIFEST}`: {e}"))
    })
}

// ---------------------------------------------------------------------------
// Локальные файлы.

/// Локальный файл слота.
#[derive(Debug, Clone)]
struct LocalFile {
    /// Путь от каталога языка (`images/icon.png`) — имя в манифесте.
    name: String,
    /// Канонический путь.
    path: PathBuf,
    sha256: String,
    bytes: u64,
}

/// Слот каталога: язык, вид и файлы по порядку (у видео — видео, обложка, видео, …).
#[derive(Debug, Clone)]
struct Slot {
    language: String,
    kind: MediaKind,
    files: Vec<LocalFile>,
}

impl Slot {
    fn label(&self) -> String {
        format!("{}/{}", self.language, self.kind.name())
    }

    fn manifest_files(&self) -> Vec<ManifestFile> {
        self.files
            .iter()
            .map(|file| ManifestFile {
                name: file.name.clone(),
                sha256: file.sha256.clone(),
                bytes: file.bytes,
            })
            .collect()
    }

    /// Число файлов в терминах AppGallery Connect (у видео — пар).
    fn count(&self) -> usize {
        match self.kind {
            MediaKind::Videos => self.files.len() / 2,
            _ => self.files.len(),
        }
    }
}

/// Читает файл слота: проверка пути и хеш.
fn local_file(
    ctx: &Context,
    path: &Path,
    name: String,
    errors: &mut Vec<String>,
) -> Result<Option<LocalFile>, ToolError> {
    let canonical = match ctx.files.resolve_read(path_text(path)?) {
        Ok(canonical) => canonical,
        Err(err) => {
            errors.push(format!("{name}: {err}"));
            return Ok(None);
        }
    };
    let (sha256, bytes) = sha256_file(&canonical)?;
    Ok(Some(LocalFile {
        name,
        path: canonical,
        sha256,
        bytes,
    }))
}

/// Слоты каталога; все ошибки устройства каталога — одной ошибкой до запросов.
fn scan(
    ctx: &Context,
    root: &Path,
    languages: Option<&[String]>,
    kinds: &[MediaKind],
) -> Result<Vec<Slot>, ToolError> {
    let mut slots = Vec::new();
    let mut errors = Vec::new();
    let mut found = Vec::new();
    for (language, dir, is_dir) in dir_entries(root)? {
        if !is_dir || !is_language_dir(&language) {
            continue;
        }
        if languages.is_some_and(|wanted| !wanted.contains(&language)) {
            continue;
        }
        found.push(language.clone());
        for kind in kinds {
            let files = match kind {
                MediaKind::Icon => scan_icon(ctx, &dir, &language, &mut errors)?,
                MediaKind::Screenshots => scan_screenshots(ctx, &dir, &language, &mut errors)?,
                MediaKind::Videos => scan_videos(ctx, &dir, &language, &mut errors)?,
            };
            if !files.is_empty() {
                slots.push(Slot {
                    language: language.clone(),
                    kind: *kind,
                    files,
                });
            }
        }
    }
    for wanted in languages.into_iter().flatten() {
        if !found.contains(wanted) {
            errors.push(format!("{wanted}: no such language directory"));
        }
    }
    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "the images and videos do not pass the AppGallery Connect checks; nothing was \
             sent:\n- {}",
            errors.join("\n- ")
        )));
    }
    if slots.is_empty() {
        return Err(ToolError::new(
            "the directory has no images or videos (<lang>/images/icon.png, \
             <lang>/images/screenshots/1.png, <lang>/videos/1.mp4 with 1.jpg)",
        ));
    }
    Ok(slots)
}

fn scan_icon(
    ctx: &Context,
    dir: &Path,
    language: &str,
    errors: &mut Vec<String>,
) -> Result<Vec<LocalFile>, ToolError> {
    let images = dir.join("images");
    if !images.is_dir() {
        return Ok(Vec::new());
    }
    let icons: Vec<(String, PathBuf)> = dir_entries(&images)?
        .into_iter()
        .filter(|(name, _, is_dir)| {
            !is_dir
                && is_image_name(name)
                && Path::new(name).file_stem().and_then(|s| s.to_str()) == Some("icon")
        })
        .map(|(name, path, _)| (name, path))
        .collect();
    if icons.len() > 1 {
        errors.push(format!(
            "{language}/images: more than one icon file; keep one icon.png"
        ));
        return Ok(Vec::new());
    }
    let mut files = Vec::new();
    for (name, path) in icons {
        files.extend(local_file(ctx, &path, format!("images/{name}"), errors)?);
    }
    Ok(files)
}

fn scan_screenshots(
    ctx: &Context,
    dir: &Path,
    language: &str,
    errors: &mut Vec<String>,
) -> Result<Vec<LocalFile>, ToolError> {
    let shots = dir.join("images").join("screenshots");
    if !shots.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<LocalFile> = Vec::new();
    for (name, path, is_dir) in dir_entries(&shots)? {
        let label = format!("{language}/images/screenshots/{name}");
        if is_dir || !is_image_name(&name) {
            errors.push(format!("{label}: not a .png, .jpg or .jpeg file"));
            continue;
        }
        files.extend(local_file(
            ctx,
            &path,
            format!("images/screenshots/{name}"),
            errors,
        )?);
    }
    if !files.is_empty() && !(MIN_SCREENSHOTS..=MAX_SCREENSHOTS).contains(&files.len()) {
        errors.push(format!(
            "{language}/images/screenshots: {} screenshots, AppGallery Connect needs \
             {MIN_SCREENSHOTS} to {MAX_SCREENSHOTS}",
            files.len()
        ));
    }
    for (i, file) in files.iter().enumerate() {
        if let Some(first) = files[..i].iter().find(|o| o.sha256 == file.sha256) {
            errors.push(format!(
                "{language}/{} is the same image as {language}/{}",
                file.name, first.name
            ));
        }
    }
    Ok(files)
}

fn scan_videos(
    ctx: &Context,
    dir: &Path,
    language: &str,
    errors: &mut Vec<String>,
) -> Result<Vec<LocalFile>, ToolError> {
    let videos = dir.join("videos");
    if !videos.is_dir() {
        return Ok(Vec::new());
    }
    // Номер пары → (видео, обложка): файлы с одним именем без расширения.
    type Files = Vec<(String, PathBuf)>;
    let mut pairs: BTreeMap<(u64, String), (Files, Files)> = BTreeMap::new();
    for (name, path, is_dir) in dir_entries(&videos)? {
        let label = format!("{language}/videos/{name}");
        let stem = Path::new(&name)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        let entry = pairs.entry(listing::natural_key(&stem)).or_default();
        if is_dir {
            errors.push(format!("{label}: unexpected directory"));
        } else if is_video_name(&name) {
            entry.0.push((name, path));
        } else if is_image_name(&name) {
            entry.1.push((name, path));
        } else {
            errors.push(format!(
                "{label}: not a video (.mp4, .mov) or a poster (.png, .jpg, .jpeg)"
            ));
        }
    }
    pairs.retain(|_, (video, poster)| !(video.is_empty() && poster.is_empty()));
    if pairs.len() > MAX_VIDEOS {
        errors.push(format!(
            "{language}/videos: {} videos, AppGallery Connect allows at most {MAX_VIDEOS}",
            pairs.len()
        ));
    }
    let mut files = Vec::new();
    for ((_, stem), (video, poster)) in pairs {
        if video.len() != 1 || poster.len() != 1 {
            errors.push(format!(
                "{language}/videos/{stem}: needs exactly one video ({stem}.mp4 or {stem}.mov) \
                 and one poster ({stem}.jpg or {stem}.png)"
            ));
            continue;
        }
        for (name, path) in video.into_iter().chain(poster) {
            files.extend(local_file(ctx, &path, format!("videos/{name}"), errors)?);
        }
    }
    Ok(files)
}

/// Проверка содержимого слота перед загрузкой. Возвращает ориентацию для
/// `imgShowType`/`videoShowType` (0 — портрет, 1 — альбом), если она нужна слоту.
fn check_slot(slot: &Slot, errors: &mut Vec<String>) -> Result<Option<u8>, ToolError> {
    let mut orientations = Vec::new();
    for file in &slot.files {
        let label = format!("{}/{}", slot.language, file.name);
        let ext = extension(&file.name).unwrap_or_default();
        let is_video = slot.kind == MediaKind::Videos && is_video_name(&file.name);
        let limit = if is_video {
            MAX_VIDEO_BYTES
        } else {
            MAX_IMAGE_BYTES
        };
        if file.bytes > limit {
            errors.push(format!(
                "{label}: {} bytes, AppGallery Connect allows at most {} MB",
                file.bytes,
                limit / MIB
            ));
            continue;
        }
        let head = if is_video {
            read_head(&file.path, 16)?
        } else {
            std::fs::read(&file.path)
                .map_err(|e| ToolError::new(format!("cannot read `{label}`: {e}")))?
        };
        let Some(format) = FileFormat::sniff(&head) else {
            errors.push(format!(
                "{label}: not a {} file",
                if is_video {
                    "MP4 or MOV"
                } else {
                    "PNG or JPEG"
                }
            ));
            continue;
        };
        if !format.matches_extension(&ext) {
            errors.push(format!(
                "{label}: the file is a {} file, but its extension says otherwise",
                format.extension()
            ));
            continue;
        }
        if is_video {
            if !format.is_video() {
                errors.push(format!("{label}: videos must be MP4 or MOV"));
            }
            continue;
        }
        let allowed: &[FileFormat] = match slot.kind {
            MediaKind::Icon => &[FileFormat::Png],
            _ => &[FileFormat::Png, FileFormat::Jpeg],
        };
        if !allowed.contains(&format) {
            errors.push(format!(
                "{label}: {} is not allowed here ({})",
                format.extension(),
                if slot.kind == MediaKind::Icon {
                    "the icon must be a PNG"
                } else {
                    "use JPG or PNG"
                }
            ));
            continue;
        }
        let Some((width, height)) = image_size(format, &head) else {
            errors.push(format!("{label}: cannot read the image size"));
            continue;
        };
        if slot.kind == MediaKind::Icon {
            if (width, height) != (ICON_SIDE, ICON_SIDE) {
                errors.push(format!(
                    "{label}: {width} x {height} px, the icon must be {ICON_SIDE} x {ICON_SIDE} px"
                ));
            }
            continue;
        }
        if !fits_screen(width, height) {
            errors.push(format!(
                "{label}: {width} x {height} px, AppGallery Connect needs 9:16 (portrait) or \
                 16:9 (landscape) with the short side at least {MIN_SHORT_SIDE} px, like \
                 450 x 800 or 1080 x 1920"
            ));
            continue;
        }
        orientations.push(u8::from(width > height));
    }
    if slot.kind == MediaKind::Icon {
        return Ok(None);
    }
    let first = orientations.first().copied();
    if orientations.iter().any(|o| Some(*o) != first) {
        errors.push(format!(
            "{}: mixes portrait and landscape images; AppGallery Connect shows all {} of a \
             language in one orientation",
            slot.label(),
            if slot.kind == MediaKind::Videos {
                "video posters"
            } else {
                "screenshots"
            }
        ));
    }
    Ok(first)
}

/// 9:16 или 16:9 (с точностью до 1 %) и короткая сторона не меньше [`MIN_SHORT_SIDE`].
fn fits_screen(width: u32, height: u32) -> bool {
    let (short, long) = (u64::from(width.min(height)), u64::from(width.max(height)));
    if short < u64::from(MIN_SHORT_SIDE) {
        return false;
    }
    // |long / short − 16 / 9| ≤ 1 %: |9·long − 16·short| ≤ 0,01 · 16·short.
    let diff = (9 * long).abs_diff(16 * short);
    diff * 100 <= 16 * short
}

// ---------------------------------------------------------------------------
// Загрузка.

/// Убирает из текста адреса, заголовки и идентификаторы уже загруженных файлов слота и
/// известные секреты клиента API.
fn scrub_uploads(ctx: &Context, uploaded: &[UploadedFile], text: &str) -> String {
    let text = uploaded
        .iter()
        .fold(text.to_owned(), |text, file| file.scrub(&text));
    ctx.api.scrub(&text)
}

/// Загружает один файл слота и возвращает загруженный объект. Файлы страницы приложения не
/// больше 500 МБ, поэтому только целиком (`multipart: None`), без SHA-256 и с
/// `chineseMainlandFlag` из плана.
async fn upload_file(
    ctx: &Context,
    plan: &Plan<'_>,
    file: &LocalFile,
    label: &str,
    uploaded: &[UploadedFile],
) -> Result<UploadedFile, ToolError> {
    let file_name = Path::new(&file.name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let spec = UploadSpec {
        app_id: &plan.app_id,
        path: &file.path,
        file_name,
        size: file.bytes,
        sha256: None,
        chinese_mainland: plan.chinese_mainland,
        multipart: None,
    };
    upload::upload(ctx, &spec, &plan.tuning)
        .await
        .map_err(|err| {
            ToolError::new(scrub_uploads(
                ctx,
                uploaded,
                &format!("uploading {label} failed: {}", err.message()),
            ))
        })
}

/// Загружает файлы слота и прикрепляет их к приложению.
async fn apply_slot(
    ctx: &Context,
    plan: &Plan<'_>,
    slot: &Slot,
    show_type: Option<u8>,
) -> Result<(), ToolError> {
    let mut uploaded: Vec<UploadedFile> = Vec::new();
    let mut files = Vec::new();
    for file in &slot.files {
        let label = format!("{}/{}", slot.language, file.name);
        let object = upload_file(ctx, plan, file, &label, &uploaded).await?;
        let name = Path::new(&file.name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        files.push(json!({"fileDestUrl": object.object_id(), "fileName": name}));
        uploaded.push(object);
    }
    let mut body = Map::new();
    body.insert("lang".into(), json!(slot.language));
    body.insert("fileType".into(), json!(slot.kind.file_type()));
    body.insert("files".into(), Value::Array(files));
    match (slot.kind, show_type) {
        (MediaKind::Screenshots, Some(show)) => {
            body.insert("imgShowType".into(), json!(show));
        }
        (MediaKind::Videos, Some(show)) => {
            body.insert("videoShowType".into(), json!(show));
        }
        _ => {}
    }
    let mut args = app_args(&plan.app_id, plan.release_type);
    args.insert("body".into(), Value::Object(body));
    let op = listing::operation(FILE_INFO_UPDATE)?;
    ctx.api
        .call(op, &Value::Object(args))
        .await
        .map_err(|err| {
            ToolError::new(scrub_uploads(
                ctx,
                &uploaded,
                &format!(
                    "attaching the uploaded {} to the app failed: {}",
                    slot.label(),
                    describe_api_error(&err)
                ),
            ))
        })?;
    Ok(())
}

// ---------------------------------------------------------------------------
// План и выполнение.

/// Общие параметры выполнения.
struct Plan<'a> {
    app_id: String,
    release_type: Option<u64>,
    chinese_mainland: Option<bool>,
    root: &'a Path,
    /// Настройки загрузки: по умолчанию — справочника, в тестах — короткие паузы.
    tuning: UploadTuning,
}

/// Почему слот загружается; `None` — не изменился.
fn change_reason(
    slot: &Slot,
    manifest: Option<&Manifest>,
    remote_count: Option<usize>,
    force: bool,
) -> Option<String> {
    if force {
        return Some("forced".into());
    }
    let Some(recorded) = manifest.and_then(|m| m.slot(&slot.language, slot.kind)) else {
        return Some("not synchronized before".into());
    };
    if recorded.files != slot.manifest_files() {
        return Some("local files changed".into());
    }
    if let (Some(now), Some(then)) = (remote_count, recorded.remote_count)
        && now != then
    {
        return Some(format!(
            "AppGallery Connect has {now} file(s) of this kind, the last sync left {then}"
        ));
    }
    None
}

async fn sync(ctx: &Context, args: Value, tuning: UploadTuning) -> Result<Value, ToolError> {
    let app_id = listing::app_id(ctx, &args)?;
    let dir = required_str(&args, "dir")?;
    let languages = string_list(&args, "languages")?;
    let kinds: Vec<MediaKind> = match string_list(&args, "kinds")? {
        None => MediaKind::ALL.to_vec(),
        Some(names) => names
            .iter()
            .map(|name| {
                MediaKind::from_name(name).ok_or_else(|| {
                    ToolError::new(format!(
                        "unknown kind `{name}`; use icon, screenshots or videos"
                    ))
                })
            })
            .collect::<Result<_, _>>()?,
    };
    let force = bool_arg(&args, "force")?.unwrap_or(false);
    let dry_run = bool_arg(&args, "dry_run")?.unwrap_or(false);
    let release_type = release_type(&args)?;
    if !dry_run && !ctx.access.allows(AccessLevel::Write) {
        return Err(ToolError::new(
            "uploading images needs a server started with --allow-write; dry_run works without",
        ));
    }

    // Устройство каталога — до любых запросов.
    let root = ctx.files.resolve_read_dir(dir)?;
    let slots = scan(ctx, &root, languages.as_deref(), &kinds)?;
    let (manifest, manifest_note) = read_manifest(ctx, &root)?;
    let mut notes: Vec<String> = manifest_note.into_iter().collect();
    let manifest = match manifest {
        Some(manifest) if manifest.app_id != app_id => {
            notes.push(format!(
                "{MANIFEST} belongs to another app; every kind with local files is treated as \
                 changed"
            ));
            None
        }
        other => other,
    };

    let remote = listing::fetch_remote(ctx, &app_id, release_type).await?;
    let mut errors = Vec::new();
    let mut planned: Vec<(&Slot, String, Option<u8>)> = Vec::new();
    let mut unchanged = 0usize;
    for slot in &slots {
        let Some(language) = remote.languages.get(&slot.language) else {
            errors.push(format!(
                "{}: the app has no listing in this language; add it with listing_apply first",
                slot.language
            ));
            continue;
        };
        let count = remote_media(language).count(slot.kind);
        match change_reason(slot, manifest.as_ref(), count, force) {
            Some(reason) => {
                let show = check_slot(slot, &mut errors)?;
                planned.push((slot, reason, show));
            }
            None => unchanged += 1,
        }
    }
    let countries = remote.app_text("publishCountry");
    if !planned.is_empty() && countries.is_empty() {
        errors.push(
            "the app has no release countries (publishCountry); AppGallery Connect attaches \
             files only after they are set: set them in AppGallery Connect or with \
             update_app_info"
                .into(),
        );
    }
    if !errors.is_empty() {
        return Err(ToolError::new(format!(
            "nothing was uploaded:\n- {}",
            errors.join("\n- ")
        )));
    }
    let chinese_mainland = match bool_arg(&args, "chineseMainland")? {
        Some(flag) => Some(flag),
        None => Some(countries.split(',').any(|c| c.trim() == "CN")),
    };

    let describe = |slot: &Slot, reason: &str, show: Option<u8>| {
        let mut entry = json!({
            "language": slot.language,
            "kind": slot.kind.name(),
            "files": slot.files.iter().map(|f| f.name.clone()).collect::<Vec<_>>(),
            "reason": reason,
        });
        if let Some(show) = show {
            entry["orientation"] = json!(if show == 0 { "portrait" } else { "landscape" });
        }
        entry
    };
    let mut result = Map::new();
    result.insert("appId".into(), json!(app_id));
    result.insert("dry_run".into(), json!(dry_run));
    result.insert(
        "changes".into(),
        Value::Array(
            planned
                .iter()
                .map(|(slot, reason, show)| describe(slot, reason, *show))
                .collect(),
        ),
    );
    result.insert("unchanged".into(), json!(unchanged));
    if planned.is_empty() {
        result.insert(
            "next_step".into(),
            json!("The images and videos in AppGallery Connect already match the directory; nothing to upload."),
        );
        insert_notes(&mut result, notes);
        return Ok(Value::Object(result));
    }
    if dry_run {
        result.insert(
            "next_step".into(),
            json!("Call image_sync again with dry_run false to upload these files."),
        );
        insert_notes(&mut result, notes);
        return Ok(Value::Object(result));
    }

    let plan = Plan {
        app_id: app_id.clone(),
        release_type,
        chinese_mainland,
        root: &root,
        tuning,
    };
    let mut manifest = manifest.unwrap_or_else(|| Manifest::new(&app_id));
    let mut done: Vec<&Slot> = Vec::new();
    for (slot, _, show) in &planned {
        tracing::debug!(
            tool = NAME,
            language = %slot.language,
            kind = slot.kind.name(),
            files = slot.files.len(),
            "загрузка файлов страницы приложения"
        );
        if let Err(err) = apply_slot(ctx, &plan, slot, *show).await {
            let saved = finish_manifest(ctx, &plan, &mut manifest, &done, None).await;
            let done_list: Vec<String> = done.iter().map(|s| s.label()).collect();
            let mut message = format!("{err}");
            if done_list.is_empty() {
                message.push_str("\n\nNothing was attached before the failure.");
            } else {
                message.push_str(&format!(
                    "\n\nAlready uploaded and attached before the failure: {}. Call image_sync \
                     again to upload the rest; finished kinds are not uploaded again.",
                    done_list.join(", ")
                ));
            }
            if let Err(note) = saved {
                message.push_str(&format!("\n{note}"));
            }
            return Err(ToolError::new(message));
        }
        done.push(slot);
    }
    if let Err(note) = finish_manifest(ctx, &plan, &mut manifest, &done, Some(&mut notes)).await {
        notes.push(note);
    }
    result.insert(
        "uploaded_files".into(),
        json!(done.iter().map(|s| s.files.len()).sum::<usize>()),
    );
    result.insert(
        "next_step".into(),
        json!("The files are attached to the draft version; they reach users after the app is submitted for review (submit_app) and approved."),
    );
    insert_notes(&mut result, notes);
    Ok(Value::Object(result))
}

/// Записывает в манифест загруженные слоты. Число файлов в AppGallery Connect берётся из
/// нового чтения сведений; если оно не удалось — по локальным файлам, с пометкой.
async fn finish_manifest(
    ctx: &Context,
    plan: &Plan<'_>,
    manifest: &mut Manifest,
    done: &[&Slot],
    notes: Option<&mut Vec<String>>,
) -> Result<(), String> {
    if done.is_empty() {
        return Ok(());
    }
    let remote: Option<Remote> = listing::fetch_remote(ctx, &plan.app_id, plan.release_type)
        .await
        .ok();
    if remote.is_none()
        && let Some(notes) = notes
    {
        notes.push(
            "the app information could not be read again after the upload; the manifest \
             records the local file counts"
                .into(),
        );
    }
    for slot in done {
        let remote_count = remote
            .as_ref()
            .and_then(|r| r.languages.get(&slot.language))
            .map(|language| remote_media(language).count(slot.kind))
            .unwrap_or(Some(slot.count()));
        manifest.put(ManifestSlot {
            language: slot.language.clone(),
            kind: slot.kind.name().to_owned(),
            files: slot.manifest_files(),
            remote_count,
        });
    }
    write_manifest(ctx, plan.root, manifest).map_err(|err| {
        format!(
            "the manifest {MANIFEST} could not be written ({err}); the next image_sync uploads \
             these kinds again"
        )
    })
}

fn insert_notes(result: &mut Map<String, Value>, notes: Vec<String>) {
    if !notes.is_empty() {
        result.insert("notes".into(), json!(notes));
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Картинки и видео для тестов: минимальные заголовки PNG, JPEG и MP4.

    /// PNG с заголовком `IHDR` размера `width` x `height` и меткой `tag` в хвосте (разные
    /// метки — разные SHA-256).
    pub(crate) fn png(width: u32, height: u32, tag: u8) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0, tag]);
        bytes
    }

    /// JPEG с маркером `SOF0` размера `width` x `height`.
    pub(crate) fn jpeg(width: u16, height: u16, tag: u8) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, tag];
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&[0x03, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xFF, 0xD9]);
        bytes
    }

    /// Начало файла MP4 (`ftyp`).
    pub(crate) fn mp4(tag: u8) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 0x18];
        bytes.extend_from_slice(b"ftypisom");
        bytes.extend_from_slice(&[0, 0, 2, 0, tag, 0, 0, 0, 0, 0, 0, 0]);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::fixtures::{jpeg, mp4, png};
    use super::*;

    /// `image_sync` с короткими паузами между повторами отправки.
    async fn sync(ctx: &Context, args: Value) -> Result<Value, ToolError> {
        let tuning = UploadTuning {
            retry_delay: std::time::Duration::from_millis(1),
            ..UploadTuning::default()
        };
        super::sync(ctx, args, tuning).await
    }
    use crate::composite::listing::testing::{
        APP, FILE_INFO, UPLOAD_URL as UPLOAD_PATH, context, mount_app, ok, writes,
    };

    /// Подпись хранилища и адрес загрузки (выдуманы).
    fn signature() -> String {
        [
            "AWS4-HMAC-SHA256 ",
            "Credential=AK",
            "EXAMPLE/x, ",
            "Signature=9a8b7c6d5e4f",
        ]
        .concat()
    }
    const OBJECT: &str = "obj-7f3e2d1c0b";

    fn args(extra: Value) -> Value {
        let mut args = json!({"dir": "listing"});
        if let (Value::Object(args), Value::Object(extra)) = (&mut args, extra) {
            args.extend(extra);
        }
        args
    }

    async fn export(ctx: &Context) {
        let export = crate::composite::listing_export::tools()[0].run;
        export(ctx, json!({"dir": "listing"})).await.unwrap();
    }

    /// Адрес загрузки на `wiremock` и приём файлов: возвращает принятые тела.
    async fn mount_upload(server: &MockServer) -> Arc<Mutex<Vec<Vec<u8>>>> {
        let received = Arc::new(Mutex::new(Vec::new()));
        let counter = Arc::new(Mutex::new(0usize));
        let uri = server.uri();
        let signature = signature();
        Mock::given(method("GET"))
            .and(path(UPLOAD_PATH))
            .respond_with(move |request: &Request| {
                let mut n = counter.lock().unwrap();
                *n += 1;
                let name = request
                    .url
                    .query_pairs()
                    .find(|(k, _)| k == "fileName")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                ok(json!({"urlInfo": {
                    "objectId": format!("{OBJECT}-{n}-{name}"),
                    "url": format!("{uri}/storage/{OBJECT}-{n}"),
                    "method": "PUT",
                    "headers": {
                        "Authorization": signature,
                        "x-amz-date": "20260101T000000Z",
                        "Content-Type": "application/octet-stream"
                    }
                }}))
            })
            .mount(server)
            .await;
        let seen = received.clone();
        Mock::given(method("PUT"))
            .and(wiremock::matchers::path_regex("^/storage/"))
            .respond_with(move |request: &Request| {
                seen.lock().unwrap().push(request.body.clone());
                ResponseTemplate::new(200)
            })
            .mount(server)
            .await;
        Mock::given(method("PUT"))
            .and(path(FILE_INFO))
            .respond_with(ok(json!({"pkgVersion": []})))
            .mount(server)
            .await;
        received
    }

    #[test]
    fn formats_and_sizes() {
        assert_eq!(FileFormat::sniff(&png(1, 1, 0)), Some(FileFormat::Png));
        assert_eq!(FileFormat::sniff(&jpeg(1, 1, 0)), Some(FileFormat::Jpeg));
        assert_eq!(FileFormat::sniff(&mp4(0)), Some(FileFormat::Mp4));
        assert_eq!(
            FileFormat::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(FileFormat::Webp)
        );
        assert_eq!(FileFormat::sniff(b"GIF89a"), None);
        assert_eq!(
            image_size(FileFormat::Png, &png(216, 216, 0)),
            Some((216, 216))
        );
        assert_eq!(
            image_size(FileFormat::Jpeg, &jpeg(1280, 720, 3)),
            Some((1280, 720))
        );
        assert!(fits_screen(450, 800) && fits_screen(800, 450) && fits_screen(1080, 1920));
        assert!(!fits_screen(300, 533) && !fits_screen(1080, 2400) && !fits_screen(500, 500));
    }

    #[test]
    fn remote_media_fields() {
        let media = remote_media(&json!({
            "icon": "https://a/icon.png",
            "introPic": "https://a/1.png, https://a/2.png,",
            "introVideo": "[{\"videoUrl\":\"https://a/v.mp4\",\"frameUrl\":\"https://a/f.jpg\"}]",
        }));
        assert_eq!(media.count(MediaKind::Icon), Some(1));
        assert_eq!(media.count(MediaKind::Screenshots), Some(2));
        assert_eq!(media.count(MediaKind::Videos), Some(1));
        // Поля телефона из deviceMaterials, видео — с опечаткой справочника.
        let media = remote_media(&json!({
            "deviceMaterials": [{"deviceType": 4, "appIcon": "https://a/i.png",
                "screenShots": ["https://a/1.png"],
                "introVedios": ["{\"vedioUrl\":\"https://a/v.mp4\",\"frameUrl\":\"https://a/f.jpg\"}"]}]
        }));
        assert_eq!(media.icon.as_deref(), Some("https://a/i.png"));
        assert_eq!(media.count(MediaKind::Screenshots), Some(1));
        assert_eq!(media.count(MediaKind::Videos), Some(1));
        assert_eq!(
            remote_media(&json!({"introVideo": "{bad"})).count(MediaKind::Videos),
            None
        );
        assert_eq!(
            remote_media(&json!({})),
            RemoteMedia {
                videos: Some(vec![]),
                ..RemoteMedia::default()
            }
        );
    }

    #[tokio::test]
    async fn exported_images_are_in_sync() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        export(&ctx).await;
        let result = sync(&ctx, args(json!({}))).await.unwrap();
        assert_eq!(result["changes"], json!([]), "{result}");
        assert_eq!(result["unchanged"], 3);
        assert!(
            writes(&server).await.is_empty(),
            "{:?}",
            writes(&server).await
        );
    }

    #[tokio::test]
    async fn uploads_only_the_changed_kind_and_hides_upload_urls() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        export(&ctx).await;
        let icon = dir.path().join("listing/en-US/images/icon.png");
        std::fs::remove_file(&icon).unwrap();
        let new_icon = png(216, 216, 77);
        std::fs::write(&icon, &new_icon).unwrap();
        let received = mount_upload(&server).await;

        // План без загрузки.
        let plan = sync(&ctx, args(json!({"dry_run": true}))).await.unwrap();
        let changes = plan["changes"].as_array().unwrap();
        assert_eq!(changes.len(), 1, "{plan}");
        assert_eq!(changes[0]["language"], "en-US");
        assert_eq!(changes[0]["kind"], "icon");
        assert_eq!(changes[0]["reason"], "local files changed");
        assert!(writes(&server).await.is_empty());

        // Логи с адресами загрузки тоже не должны их содержать.
        let logs = Arc::new(Mutex::new(Vec::new()));
        let writer = LogWriter(logs.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let guard = tracing::subscriber::set_default(subscriber);
        let result = sync(&ctx, args(json!({}))).await.unwrap();
        drop(guard);
        assert_eq!(result["uploaded_files"], 1, "{result}");
        assert_eq!(*received.lock().unwrap(), vec![new_icon]);
        let requests = server.received_requests().await.unwrap();
        let attach: Vec<Value> = requests
            .iter()
            .filter(|r| r.method.as_str() == "PUT" && r.url.path() == FILE_INFO)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(attach.len(), 1);
        assert_eq!(attach[0]["lang"], "en-US");
        assert_eq!(attach[0]["fileType"], 0);
        assert_eq!(attach[0]["files"][0]["fileName"], "icon.png");
        assert!(
            attach[0]["files"][0]["fileDestUrl"]
                .as_str()
                .unwrap()
                .starts_with(OBJECT)
        );
        let upload_url = requests
            .iter()
            .find(|r| r.url.path() == UPLOAD_PATH)
            .unwrap();
        assert!(
            upload_url
                .url
                .query()
                .unwrap()
                .contains("chineseMainlandFlag=0")
        );
        assert!(upload_url.url.query().unwrap().contains("contentLength="));

        let text = result.to_string();
        let logs = String::from_utf8(logs.lock().unwrap().concat()).unwrap();
        for secret in [OBJECT, "/storage/", "Signature=", "20260101T000000Z"] {
            assert!(!text.contains(secret), "{secret} in {text}");
            assert!(!logs.contains(secret), "{secret} in logs");
        }

        // Манифест обновлён: повтор ничего не загружает.
        let again = sync(&ctx, args(json!({}))).await.unwrap();
        assert_eq!(again["changes"], json!([]), "{again}");
        assert_eq!(received.lock().unwrap().len(), 1);
    }

    #[derive(Clone)]
    struct LogWriter(Arc<Mutex<Vec<Vec<u8>>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn new_directory_uploads_screenshots_and_videos_with_orientation() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let lang = dir.path().join("listing/ru-RU");
        std::fs::create_dir_all(lang.join("images/screenshots")).unwrap();
        std::fs::create_dir_all(lang.join("videos")).unwrap();
        for i in 1..=3u8 {
            std::fs::write(
                lang.join(format!("images/screenshots/{i}.png")),
                png(1920, 1080, i),
            )
            .unwrap();
        }
        std::fs::write(lang.join("videos/1.mp4"), mp4(1)).unwrap();
        std::fs::write(lang.join("videos/1.jpg"), jpeg(800, 450, 1)).unwrap();
        let received = mount_upload(&server).await;
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        let result = sync(&ctx, args(json!({"chineseMainland": true})))
            .await
            .unwrap();
        assert_eq!(result["uploaded_files"], 5, "{result}");
        assert_eq!(received.lock().unwrap().len(), 5);
        let requests = server.received_requests().await.unwrap();
        let attach: Vec<Value> = requests
            .iter()
            .filter(|r| r.method.as_str() == "PUT" && r.url.path() == FILE_INFO)
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(attach[0]["fileType"], 2);
        assert_eq!(attach[0]["imgShowType"], 1);
        assert_eq!(attach[0]["files"].as_array().unwrap().len(), 3);
        assert_eq!(attach[1]["fileType"], 1);
        assert_eq!(attach[1]["videoShowType"], 1);
        // Видео раньше обложки.
        assert_eq!(attach[1]["files"][0]["fileName"], "1.mp4");
        assert_eq!(attach[1]["files"][1]["fileName"], "1.jpg");
        assert!(
            requests
                .iter()
                .filter(|r| r.url.path() == UPLOAD_PATH)
                .all(|r| r.url.query().unwrap().contains("chineseMainlandFlag=1"))
        );
        let manifest: Manifest = serde_json::from_slice(
            &std::fs::read(dir.path().join("listing").join(MANIFEST)).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.app_id, APP);
        assert_eq!(manifest.slots.len(), 2);
    }

    #[tokio::test]
    async fn invalid_files_fail_before_uploads() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("listing/en-US/images");
        std::fs::create_dir_all(images.join("screenshots")).unwrap();
        std::fs::write(images.join("icon.png"), jpeg(216, 216, 1)).unwrap();
        for (i, bytes) in [png(1080, 1920, 1), png(1920, 1080, 2), png(300, 300, 3)]
            .into_iter()
            .enumerate()
        {
            std::fs::write(images.join(format!("screenshots/{i}.png")), bytes).unwrap();
        }
        let ru = dir.path().join("listing/ru-RU/images");
        std::fs::create_dir_all(&ru).unwrap();
        std::fs::write(ru.join("icon.webp"), b"RIFF\0\0\0\0WEBPVP8 ").unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        let err = sync(&ctx, args(json!({}))).await.unwrap_err();
        let message = err.message();
        assert!(
            message.contains("en-US/images/icon.png: the file is a jpg file"),
            "{message}"
        );
        assert!(
            message.contains("screenshots/2.png: 300 x 300 px"),
            "{message}"
        );
        assert!(
            message.contains("mixes portrait and landscape"),
            "{message}"
        );
        assert!(
            message.contains("ru-RU/images/icon.webp: webp is not allowed"),
            "{message}"
        );
        assert!(writes(&server).await.is_empty());

        // Устройство каталога — до любых запросов.
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        let shots = dir.path().join("listing/en-US/images/screenshots");
        std::fs::create_dir_all(&shots).unwrap();
        std::fs::write(shots.join("1.png"), png(450, 800, 1)).unwrap();
        std::fs::write(shots.join("2.png"), png(450, 800, 1)).unwrap();
        std::fs::write(shots.join("notes.txt"), "x").unwrap();
        let videos = dir.path().join("listing/en-US/videos");
        std::fs::create_dir_all(&videos).unwrap();
        std::fs::write(videos.join("1.mp4"), mp4(1)).unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        let err = sync(&ctx, args(json!({}))).await.unwrap_err();
        let message = err.message();
        assert!(message.contains("2 screenshots"), "{message}");
        assert!(message.contains("notes.txt"), "{message}");
        assert!(message.contains("same image"), "{message}");
        assert!(
            message.contains("videos/1: needs exactly one video"),
            "{message}"
        );
        assert!(server.received_requests().await.unwrap().is_empty());

        let err = sync(&ctx, args(json!({"kinds": ["banner"]})))
            .await
            .unwrap_err();
        assert!(err.message().contains("unknown kind"), "{err}");
        let err = sync(&ctx, args(json!({"languages": ["fr-FR"]})))
            .await
            .unwrap_err();
        assert!(err.message().contains("fr-FR"), "{err}");
    }

    #[tokio::test]
    async fn upload_errors_hide_the_upload_url() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let images = dir.path().join("listing/ru-RU/images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join("icon.png"), png(216, 216, 9)).unwrap();
        let uri = server.uri();
        let sig = signature();
        Mock::given(method("GET"))
            .and(path(UPLOAD_PATH))
            .and(query_param("fileName", "icon.png"))
            .respond_with(ok(json!({"urlInfo": {
                "objectId": OBJECT,
                "url": format!("{uri}/storage/{OBJECT}"),
                "method": "PUT",
                "headers": {"Authorization": sig}
            }})))
            .mount(&server)
            .await;
        // Хранилище отражает адрес и подпись в тексте ошибки.
        Mock::given(method("PUT"))
            .and(path(format!("/storage/{OBJECT}")))
            .respond_with(ResponseTemplate::new(403).set_body_string(format!(
                "<Error><Code>AccessDenied</Code><Resource>/storage/{OBJECT}</Resource>\
                 <Auth>{}</Auth></Error>",
                signature()
            )))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path(FILE_INFO))
            .and(body_partial_json(json!({"fileType": 0})))
            .respond_with(ok(json!({})))
            .expect(0)
            .mount(&server)
            .await;
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        let err = sync(&ctx, args(json!({}))).await.unwrap_err();
        let message = err.message();
        assert!(
            message.contains("uploading ru-RU/images/icon.png failed"),
            "{message}"
        );
        assert!(message.contains("Nothing was attached"), "{message}");
        for secret in [OBJECT, "Signature=", "/storage/"] {
            assert!(!message.contains(secret), "{secret} in {message}");
        }
        assert!(!dir.path().join("listing").join(MANIFEST).exists());
    }

    #[tokio::test]
    async fn changes_in_the_console_and_force_are_uploaded() {
        let server = MockServer::start().await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Write);
        export(&ctx).await;
        // В манифесте записано другое число скриншотов: их поменяли в консоли.
        let path = dir.path().join("listing").join(MANIFEST);
        let mut manifest: Manifest =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for slot in &mut manifest.slots {
            if slot.kind == "screenshots" {
                slot.remote_count = Some(4);
            }
        }
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let plan = sync(&ctx, args(json!({"dry_run": true}))).await.unwrap();
        assert_eq!(plan["changes"][0]["kind"], "screenshots", "{plan}");
        assert!(
            plan["changes"][0]["reason"]
                .as_str()
                .unwrap()
                .contains("has 3 file(s)"),
            "{plan}"
        );
        let forced = sync(
            &ctx,
            args(json!({"dry_run": true, "force": true, "kinds": ["icon"]})),
        )
        .await
        .unwrap();
        assert_eq!(forced["changes"].as_array().unwrap().len(), 2, "{forced}");
        assert!(writes(&server).await.is_empty());
    }
}
