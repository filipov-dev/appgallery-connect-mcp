//! `listing_export`: страница приложения из AppGallery Connect в локальный каталог (формат
//! — [`super::listing`]).
//!
//! Один запрос `publishing.appInfo.get` (все языки) и скачивание картинок по адресам из
//! ответа (только хосты Huawei). Каталог должен быть новым или пустым: существующие файлы
//! не перезаписываются. Пишутся только непустые поля. Картинки скачиваются во временное имя
//! `<n>.download` и получают расширение по первым байтам (PNG, JPEG, WebP, MP4, MOV). Если
//! файл слота (язык и вид) не скачался, файлы этого слота удаляются целиком и в ответе
//! предупреждение: неполный набор скриншотов `image_sync` загрузил бы вместо полного.
//! По скачанным слотам пишется манифест `image_sync` — следующий `image_sync` без правок
//! ничего не загружает.

use std::path::{Path, PathBuf};

use appgallery_connect_api::AccessLevel;
use serde_json::{Map, Value, json};

use super::image_sync::{
    self, FileFormat, MANIFEST, Manifest, ManifestFile, ManifestSlot, MediaKind, remote_media,
    sha256_file,
};
use super::listing::{
    self, APP_FIELDS, LANGUAGE_FIELDS, TOOLSET, app_id_schema, bool_arg, is_language_dir,
    path_text, release_type, release_type_schema, required_str, text_of,
};
use super::{CompositeTool, Context, ToolFuture};
use crate::error::{ToolError, describe_api_error};

/// Имя инструмента.
pub const NAME: &str = "listing_export";

const DESCRIPTION: &str = "Export the AppGallery store listing of an app to a local directory \
(fastlane-like layout): default_language.txt, privacy_policy.txt, developer_website.txt and \
privacy_rights_url.txt at the top, and per language (<lang>/, AppGallery Connect language codes \
such as en-US) title.txt, short_description.txt, full_description.txt, release_notes.txt, \
images/icon.png, images/screenshots/1.png ... and, with include_videos, videos/1.mp4 with its \
poster 1.jpg. Only non-empty fields are written. The directory must be new or empty; existing \
files are never overwritten. It also writes .appgallery-images.json, the record image_sync \
compares the images with. Edit the files and apply them with listing_apply (texts) and \
image_sync (images and videos). Local paths are allowed only inside the server's allowed \
directories.";

/// Регистрация инструмента.
pub fn tools() -> Vec<CompositeTool> {
    vec![CompositeTool {
        name: NAME,
        toolset: TOOLSET,
        access: AccessLevel::Read,
        description: DESCRIPTION,
        input_schema,
        run,
    }]
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "appId": app_id_schema(),
            "dir": {
                "type": "string",
                "description": "New or empty directory to write the listing to (inside the \
                    allowed directories); it is created when missing."
            },
            "include_images": {
                "type": "boolean",
                "description": "Download the icon and screenshots. Default: true."
            },
            "include_videos": {
                "type": "boolean",
                "description": "Download the introduction videos and their posters (videos can \
                    be large). Default: false."
            },
            "releaseType": release_type_schema()
        },
        "required": ["dir"]
    })
}

fn run(ctx: &Context, args: Value) -> ToolFuture<'_> {
    Box::pin(export(ctx, args))
}

/// Скачиваемый файл слота: адрес и путь от каталога языка без расширения.
struct Download {
    url: String,
    stem: String,
    /// Видео (иначе картинка).
    video: bool,
}

/// Что получилось при выгрузке.
#[derive(Default)]
struct Stats {
    text_files: usize,
    images: usize,
    videos: usize,
    bytes: u64,
    warnings: Vec<String>,
}

async fn export(ctx: &Context, args: Value) -> Result<Value, ToolError> {
    let app_id = listing::app_id(ctx, &args)?;
    let dir = required_str(&args, "dir")?;
    let include_images = bool_arg(&args, "include_images")?.unwrap_or(true);
    let include_videos = bool_arg(&args, "include_videos")?.unwrap_or(false);
    let release_type = release_type(&args)?;

    let root = ctx.files.resolve_write_dir(dir)?;
    let not_empty = std::fs::read_dir(&root)
        .map_err(|e| ToolError::new(format!("cannot read `{dir}`: {e}")))?
        .next()
        .is_some();
    if not_empty {
        return Err(ToolError::new(format!(
            "`{dir}` is not empty; export to a new or empty directory (existing files are never \
             overwritten)"
        )));
    }

    let remote = listing::fetch_remote(ctx, &app_id, release_type).await?;
    let mut stats = Stats::default();
    let mut app_files = Vec::new();
    for (file, field, _) in APP_FIELDS {
        let value = remote.app_text(field);
        if !value.is_empty() {
            write_text(ctx, &root.join(file), &value)?;
            stats.text_files += 1;
            app_files.push(*file);
        }
    }

    let mut manifest = Manifest::new(&app_id);
    let mut languages = Vec::new();
    for (code, language) in &remote.languages {
        // Код языка — имя каталога: только безопасные имена.
        if !is_language_dir(code) {
            stats.warnings.push(format!(
                "language `{code}` was skipped: it cannot be a directory name"
            ));
            continue;
        }
        let lang_dir = ctx.files.resolve_write_dir(path_text(&root.join(code))?)?;
        for (file, field, _) in LANGUAGE_FIELDS {
            let value = text_of(language, field);
            if !value.is_empty() {
                write_text(ctx, &lang_dir.join(file), &value)?;
                stats.text_files += 1;
            }
        }
        languages.push(code.clone());
        if !include_images && !include_videos {
            continue;
        }
        let media = remote_media(language);
        for kind in MediaKind::ALL {
            let wanted = match kind {
                MediaKind::Videos => include_videos,
                _ => include_images,
            };
            if !wanted {
                continue;
            }
            let downloads = match kind {
                MediaKind::Icon => media
                    .icon
                    .iter()
                    .map(|url| Download {
                        url: url.clone(),
                        stem: "images/icon".into(),
                        video: false,
                    })
                    .collect(),
                MediaKind::Screenshots => media
                    .screenshots
                    .iter()
                    .enumerate()
                    .map(|(i, url)| Download {
                        url: url.clone(),
                        stem: format!("images/screenshots/{}", i + 1),
                        video: false,
                    })
                    .collect(),
                MediaKind::Videos => match &media.videos {
                    None => {
                        stats.warnings.push(format!(
                            "{code}/videos: the video list of AppGallery Connect could not be \
                             read; videos were not exported"
                        ));
                        continue;
                    }
                    Some(videos) => {
                        let mut list = Vec::new();
                        for (i, (video, poster)) in videos.iter().enumerate() {
                            list.push(Download {
                                url: video.clone(),
                                stem: format!("videos/{}", i + 1),
                                video: true,
                            });
                            if !poster.is_empty() {
                                list.push(Download {
                                    url: poster.clone(),
                                    stem: format!("videos/{}", i + 1),
                                    video: false,
                                });
                            }
                        }
                        list
                    }
                },
            };
            if downloads.is_empty() {
                continue;
            }
            if let Some(slot) =
                export_slot(ctx, &lang_dir, code, kind, &downloads, &mut stats).await?
            {
                manifest.put(ManifestSlot {
                    remote_count: media.count(kind),
                    ..slot
                });
            }
        }
    }
    if !manifest.slots.is_empty() {
        image_sync::write_manifest(ctx, &root, &manifest)?;
    }

    let mut result = Map::new();
    result.insert("appId".into(), json!(app_id));
    result.insert("dir".into(), json!(root.display().to_string()));
    result.insert("languages".into(), json!(languages));
    result.insert(
        "default_language".into(),
        json!(remote.app_text("defaultLang")),
    );
    result.insert("app".into(), json!(app_files));
    result.insert("text_files".into(), json!(stats.text_files));
    result.insert("images".into(), json!(stats.images));
    result.insert("videos".into(), json!(stats.videos));
    result.insert("image_bytes".into(), json!(stats.bytes));
    if !stats.warnings.is_empty() {
        result.insert("warnings".into(), json!(stats.warnings));
    }
    result.insert(
        "next_step".into(),
        json!(format!(
            "Edit the files, then call listing_apply (texts) and image_sync (images and videos) \
             with dir `{dir}`; both support dry_run. Keep {MANIFEST}: image_sync compares the \
             images with it."
        )),
    );
    Ok(Value::Object(result))
}

/// Текст поля с переводом строки в конце.
fn write_text(ctx: &Context, path: &Path, value: &str) -> Result<(), ToolError> {
    let target = ctx.files.resolve_write(path_text(path)?)?;
    crate::files::write_new_file(&target, format!("{value}\n").as_bytes())?;
    Ok(())
}

/// Скачивает файлы слота. Если хоть один не скачался — удаляет скачанные файлы слота и
/// возвращает `None` с предупреждением.
async fn export_slot(
    ctx: &Context,
    lang_dir: &Path,
    code: &str,
    kind: MediaKind,
    downloads: &[Download],
    stats: &mut Stats,
) -> Result<Option<ManifestSlot>, ToolError> {
    let mut created: Vec<PathBuf> = Vec::new();
    let mut files = Vec::new();
    let mut failure = None;
    for download in downloads {
        match download_one(ctx, lang_dir, download, &mut created).await {
            Ok(file) => files.push(file),
            Err(err) => {
                failure = Some(format!("{code}/{}: {err}", download.stem));
                break;
            }
        }
    }
    if let Some(failure) = failure {
        for path in created {
            let _ = std::fs::remove_file(path);
        }
        stats.warnings.push(format!(
            "{failure}; the {} of {code} were not exported (a partial set would replace the \
             full one on the next image_sync)",
            kind.name()
        ));
        return Ok(None);
    }
    for (file, download) in files.iter().zip(downloads) {
        stats.bytes += file.bytes;
        if download.video {
            stats.videos += 1;
        } else {
            stats.images += 1;
        }
    }
    Ok(Some(ManifestSlot {
        language: code.to_owned(),
        kind: kind.name().to_owned(),
        files,
        remote_count: None,
    }))
}

/// Скачивает один файл: во временное имя, затем переименование по формату.
async fn download_one(
    ctx: &Context,
    lang_dir: &Path,
    download: &Download,
    created: &mut Vec<PathBuf>,
) -> Result<ManifestFile, String> {
    let target = lang_dir.join(&download.stem);
    let parent = target.parent().unwrap_or(lang_dir);
    let parent = ctx
        .files
        .resolve_write_dir(path_text(parent).map_err(|e| e.message().to_owned())?)
        .map_err(|e| e.message().to_owned())?;
    let file_stem = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .to_owned();
    let suffix = if download.video { "video" } else { "image" };
    let temp_path = parent.join(format!("{file_stem}.{suffix}.download"));
    let temp = ctx
        .files
        .resolve_write(path_text(&temp_path).map_err(|e| e.message().to_owned())?)
        .map_err(|e| e.message().to_owned())?;
    // Адрес на CDN в текст ошибки не попадает.
    let downloaded = ctx
        .api
        .download_to_file(&download.url, &temp)
        .await
        .map_err(|err| {
            format!(
                "download failed: {}",
                ctx.api.scrub(&describe_api_error(&err))
            )
        })?;
    created.push(downloaded.path.clone());
    let head = read_head(&downloaded.path)?;
    let format = FileFormat::sniff(&head);
    let ext = match (format, download.video) {
        (Some(format @ (FileFormat::Mp4 | FileFormat::Mov)), true) => format.extension(),
        (Some(format @ (FileFormat::Png | FileFormat::Jpeg | FileFormat::Webp)), false) => {
            format.extension()
        }
        _ => {
            return Err(format!(
                "the downloaded file is not a known {}",
                if download.video {
                    "video format (MP4, MOV)"
                } else {
                    "image format (PNG, JPEG, WebP)"
                }
            ));
        }
    };
    let final_path = parent.join(format!("{file_stem}.{ext}"));
    let final_path = ctx
        .files
        .resolve_write(path_text(&final_path).map_err(|e| e.message().to_owned())?)
        .map_err(|e| e.message().to_owned())?;
    std::fs::rename(&downloaded.path, &final_path).map_err(|e| format!("cannot rename: {e}"))?;
    created.pop();
    created.push(final_path.clone());
    let (sha256, bytes) = sha256_file(&final_path).map_err(|e| e.message().to_owned())?;
    let name = final_path
        .strip_prefix(lang_dir)
        .ok()
        .and_then(|p| p.to_str())
        .map(|p| p.replace('\\', "/"))
        .unwrap_or_else(|| format!("{}.{ext}", download.stem));
    Ok(ManifestFile {
        name,
        sha256,
        bytes,
    })
}

fn read_head(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read as _;
    let file = std::fs::File::open(path).map_err(|e| format!("cannot read the file: {e}"))?;
    let mut head = Vec::new();
    file.take(16)
        .read_to_end(&mut head)
        .map_err(|e| format!("cannot read the file: {e}"))?;
    Ok(head)
}

#[cfg(test)]
mod tests {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::composite::listing::testing::{APP, context, mount_app, writes};

    #[tokio::test]
    async fn exports_texts_images_and_manifest() {
        let server = MockServer::start().await;
        let images = mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Read);
        let result = export(&ctx, json!({"dir": "out"})).await.unwrap();
        assert_eq!(result["languages"], json!(["en-US", "ru-RU"]), "{result}");
        assert_eq!(result["images"], 5);
        assert!(result.get("warnings").is_none(), "{result}");
        let out = dir.path().join("out");
        let read = |p: &str| std::fs::read_to_string(out.join(p)).unwrap();
        assert_eq!(read("default_language.txt"), "en-US\n");
        assert_eq!(read("privacy_policy.txt"), "https://example.com/privacy\n");
        assert!(!out.join("privacy_rights_url.txt").exists());
        assert_eq!(read("en-US/title.txt"), "Example\n");
        assert_eq!(read("en-US/full_description.txt"), "Line one\nLine two\n");
        // Пустое поле не пишется.
        assert!(!out.join("en-US/release_notes.txt").exists());
        assert_eq!(read("ru-RU/release_notes.txt"), "Исправления\n");
        assert_eq!(
            std::fs::read(out.join("en-US/images/icon.png")).unwrap(),
            images.icon_en
        );
        assert_eq!(
            std::fs::read(out.join("en-US/images/screenshots/2.jpg")).unwrap(),
            images.shots_en[1]
        );
        assert!(out.join("ru-RU/images/icon.png").exists());
        assert!(!out.join("ru-RU/images/screenshots").exists());
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(out.join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(manifest.app_id, APP);
        let kinds: Vec<(String, String, Option<usize>)> = manifest
            .slots
            .iter()
            .map(|s| (s.language.clone(), s.kind.clone(), s.remote_count))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("en-US".into(), "icon".into(), Some(1)),
                ("en-US".into(), "screenshots".into(), Some(3)),
                ("ru-RU".into(), "icon".into(), Some(1)),
            ]
        );
        assert_eq!(manifest.slots[1].files[1].name, "images/screenshots/2.jpg");
        // Ни одного временного файла.
        let leftovers = walk(&out)
            .into_iter()
            .filter(|p| p.ends_with(".download"))
            .count();
        assert_eq!(leftovers, 0);
        assert!(writes(&server).await.is_empty());
        // Адресов картинок в ответе нет.
        assert!(!result.to_string().contains("/img/"));
    }

    fn walk(dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path.display().to_string());
            }
        }
        out
    }

    #[tokio::test]
    async fn refuses_a_directory_with_files() {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("out")).unwrap();
        std::fs::write(dir.path().join("out/notes.txt"), "keep").unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Read);
        let err = export(&ctx, json!({"dir": "out"})).await.unwrap_err();
        assert!(err.message().contains("is not empty"), "{err}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out/notes.txt")).unwrap(),
            "keep"
        );
        assert!(server.received_requests().await.unwrap().is_empty());
        // Вне разрешённых каталогов.
        let err = export(&ctx, json!({"dir": "../elsewhere"}))
            .await
            .unwrap_err();
        assert!(!err.message().is_empty());
    }

    #[tokio::test]
    async fn failed_image_drops_the_whole_slot() {
        let server = MockServer::start().await;
        // Скриншот s2 не скачивается: его заглушка отвечает 404 раньше общей.
        Mock::given(method("GET"))
            .and(path("/img/s2.jpg"))
            .respond_with(ResponseTemplate::new(404))
            .with_priority(1)
            .mount(&server)
            .await;
        mount_app(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = context(&server, dir.path(), AccessLevel::Read);
        let result = export(&ctx, json!({"dir": "out"})).await.unwrap();
        let warnings = result["warnings"].to_string();
        assert!(
            warnings.contains("en-US/images/screenshots/2"),
            "{warnings}"
        );
        assert!(!warnings.contains("/img/"), "{warnings}");
        let out = dir.path().join("out");
        assert!(out.join("en-US/images/icon.png").exists());
        assert!(!out.join("en-US/images/screenshots/1.png").exists());
        let manifest: Manifest =
            serde_json::from_slice(&std::fs::read(out.join(MANIFEST)).unwrap()).unwrap();
        assert!(manifest.slots.iter().all(|s| s.kind != "screenshots"));
    }
}
