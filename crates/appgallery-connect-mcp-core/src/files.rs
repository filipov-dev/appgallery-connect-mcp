//! Политика доступа к локальным файлам.
//!
//! Инструменты пишут локальные файлы (`save_to`: отчёт, сертификат или профиль по
//! временной ссылке из ответа, файл конфигурации приложения; экспорт страницы приложения)
//! и читают их (загрузка сборок и изображений составными инструментами). Агент не должен
//! через это читать или перезаписывать произвольные файлы пользователя (`~/.ssh`,
//! `agc-apiclient-*.json`, хранилища ключей подписи, конфиги), поэтому любой путь должен
//! лежать внутри одного из разрешённых корней:
//!
//! - рабочий каталог процесса сервера (stdio-клиенты запускают сервер в каталоге проекта),
//!   если это не корень файловой системы и не домашний каталог — они слишком широкие;
//! - системный временный каталог;
//! - каталоги из `--allow-path`.
//!
//! Временный каталог разрешён, чтобы работали подготовленные там файлы и скачивание во
//! временные файлы; он общий для всех программ пользователя, поэтому это отмечено в `--help`
//! и документации.
//!
//! Корни и пути канонизируются (`canonicalize`), поэтому выход через `..` и символические
//! ссылки обнаруживается: проверяется реальный путь, а не то, как он записан.
//! Относительный путь разрешается относительно рабочего каталога сервера.
//!
//! Проверка возвращает канонический путь (для записи — канонический каталог и имя файла),
//! и файл открывается именно по нему. Существующий файл (и символическая ссылка) при записи
//! не перезаписывается: файл создаётся с `create_new` ([`write_new_file`], скачивание
//! `ApiClient::download_to_file`), поэтому и файл, появившийся после проверки, не будет
//! затёрт.
//!
//! Windows: канонические пути там verbatim (`\\?\C:\…`), а в verbatim-пути `/` не
//! разделитель. Поэтому `/` в пути агента заменяется на `\` до разбора, относительный путь
//! присоединяется к базовому каталогу по компонентам, а `..` ищется и в самой строке:
//! `sub/../../x` не превращается в одно имя внутри корня.
//!
//! [`FilePolicy`] реализует контракт составных инструментов [`LocalFiles`].

use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};

use appgallery_connect_api::spec::OperationDef;
use rmcp::model::JsonObject;
use serde_json::{Value, json};

use crate::composite::LocalFiles;
use crate::error::ToolError;

/// Аргумент ядра: путь нового локального файла, в который сервер сохраняет файл из ответа
/// (скачивает по временной ссылке или записывает содержимое). Есть только у операций с
/// сохраняемыми секретными полями (`OperationDef::saves_file`).
pub const SAVE_TO_ARG: &str = "save_to";

/// Пояснение в описании инструмента с `save_to`.
pub const SAVE_TO_DESCRIPTION: &str = "The response holds a temporary link to a file or the \
content of a file with credentials; the server hides it. Pass `save_to` to get the file: the \
server saves it to that new local file (downloading only from Huawei hosts) and returns its \
path and size.";

/// Добавляет аргумент `save_to` в схему инструмента; ошибка — свойство с этим именем уже
/// есть (ошибка генератора или снимка).
pub fn add_save_to(tool: &str, schema: &mut JsonObject) -> Result<(), String> {
    let Some(Value::Object(properties)) = schema.get_mut("properties") else {
        return Err(format!("{tool}: the input schema has no properties"));
    };
    if properties.contains_key(SAVE_TO_ARG) {
        return Err(format!(
            "{tool}: the operation already has a `{SAVE_TO_ARG}` parameter"
        ));
    }
    properties.insert(
        SAVE_TO_ARG.to_owned(),
        json!({
            "type": "string",
            "description": "Path of a new local file to save the file from the response to \
                (absolute, or relative to the server's working directory; only inside the \
                allowed directories listed in the server instructions). The parent directory \
                must exist; existing files are never overwritten. The result then has `saved` \
                with the path and size instead of the hidden link or content.",
        }),
    );
    Ok(())
}

/// Забирает `save_to` из аргументов операции, которая сохраняет файлы. У остальных
/// операций аргумент остаётся и отклоняется проверкой аргументов как неизвестный.
pub fn take_save_to(op: &OperationDef, args: &mut JsonObject) -> Result<Option<String>, ToolError> {
    if !op.saves_file() {
        return Ok(None);
    }
    match args.remove(SAVE_TO_ARG) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) if !path.trim().is_empty() => Ok(Some(path)),
        Some(_) => Err(ToolError::new(format!(
            "argument `{SAVE_TO_ARG}` must be a non-empty path of a new local file"
        ))),
    }
}

/// Ошибка проверки локального пути; текст предназначен для агента.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePolicyError(pub String);

impl fmt::Display for FilePolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FilePolicyError {}

impl From<FilePolicyError> for ToolError {
    fn from(err: FilePolicyError) -> Self {
        ToolError::new(err.0)
    }
}

/// Разрешённые корни для локальных файлов.
#[derive(Debug, Clone)]
pub struct FilePolicy {
    /// Канонические пути корней без дубликатов.
    roots: Vec<PathBuf>,
    /// Каталог, относительно которого разрешаются относительные пути.
    base_dir: Option<PathBuf>,
    /// Локальные файлы выключены: любой путь отклоняется.
    disabled: bool,
}

/// Сообщение для агента, когда локальные файлы выключены.
pub const LOCAL_FILES_DISABLED: &str =
    "local files are not available: this server is not allowed to read or write local files";

impl FilePolicy {
    /// Политика с корнями по умолчанию (рабочий каталог и временный каталог) и `extra`.
    ///
    /// Каталоги из `extra` (каталоги профиля) должны существовать: опечатка — ошибка запуска.
    /// Недоступные корни по умолчанию пропускаются с предупреждением в лог.
    pub fn with_defaults(extra: &[PathBuf]) -> Result<Self, FilePolicyError> {
        let cwd = std::env::current_dir().ok();
        let home = home_dir().and_then(|home| home.canonicalize().ok());
        let mut defaults = Vec::new();
        if let Some(cwd) = cwd.as_ref().and_then(|cwd| cwd.canonicalize().ok()) {
            if cwd.parent().is_none() || Some(&cwd) == home.as_ref() {
                tracing::warn!(
                    cwd = %cwd.display(),
                    "рабочий каталог слишком широкий и не добавлен в разрешённые корни файлов"
                );
            } else {
                defaults.push(cwd);
            }
        }
        match std::env::temp_dir().canonicalize() {
            Ok(tmp) => defaults.push(tmp),
            Err(err) => tracing::warn!(%err, "временный каталог недоступен"),
        }

        let mut policy = Self::new(extra)?;
        for root in defaults {
            policy.push_root(root);
        }
        policy.base_dir = cwd;
        Ok(policy)
    }

    /// Политика только с указанными корнями (без корней по умолчанию).
    pub fn new(roots: &[PathBuf]) -> Result<Self, FilePolicyError> {
        let mut policy = Self {
            roots: Vec::new(),
            base_dir: None,
            disabled: false,
        };
        for root in roots {
            let canonical = root.canonicalize().map_err(|err| {
                FilePolicyError(format!(
                    "allowed path `{}` is not accessible: {err}",
                    root.display()
                ))
            })?;
            if !canonical.is_dir() {
                return Err(FilePolicyError(format!(
                    "allowed path `{}` is not a directory",
                    root.display()
                )));
            }
            policy.push_root(canonical);
        }
        Ok(policy)
    }

    /// Политика без локальных файлов: любой путь отклоняется.
    pub fn disabled() -> Self {
        Self {
            roots: Vec::new(),
            base_dir: None,
            disabled: true,
        }
    }

    /// Выключены ли локальные файлы ([`FilePolicy::disabled`]).
    pub fn is_disabled(&self) -> bool {
        self.disabled
    }

    /// Каталог для относительных путей. По умолчанию — рабочий каталог на момент создания.
    pub fn with_base_dir(mut self, base_dir: impl Into<PathBuf>) -> Self {
        self.base_dir = Some(base_dir.into());
        self
    }

    /// Канонические разрешённые корни.
    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    fn push_root(&mut self, root: PathBuf) {
        if !self.roots.contains(&root) {
            self.roots.push(root);
        }
    }

    /// Проверяет файл для чтения. Возвращает канонический путь существующего обычного файла
    /// внутри разрешённого корня.
    pub fn check_read(&self, path: &str) -> Result<PathBuf, FilePolicyError> {
        self.ensure_enabled()?;
        let absolute = self.absolute(path)?;
        let canonical = absolute
            .canonicalize()
            .map_err(|err| self.io_error(path, &err))?;
        self.ensure_inside(path, &canonical)?;
        if !canonical.is_file() {
            return Err(FilePolicyError(format!("`{path}` is not a regular file")));
        }
        Ok(canonical)
    }

    /// Проверяет каталог для чтения (например, каталог листинга). Возвращает канонический
    /// путь существующего каталога внутри разрешённого корня. Файлы внутри каталога
    /// проверяются по одному ([`Self::check_read`]): символическая ссылка внутри может вести
    /// наружу.
    pub fn check_read_dir(&self, path: &str) -> Result<PathBuf, FilePolicyError> {
        self.ensure_enabled()?;
        let absolute = self.absolute(path)?;
        let canonical = absolute
            .canonicalize()
            .map_err(|err| self.io_error(path, &err))?;
        self.ensure_inside(path, &canonical)?;
        if !canonical.is_dir() {
            return Err(FilePolicyError(format!("`{path}` is not a directory")));
        }
        Ok(canonical)
    }

    /// Проверяет путь для записи. Каталог должен существовать и лежать внутри разрешённого
    /// корня, файла с таким именем быть не должно (в том числе символической ссылки).
    /// Возвращает путь вида `<канонический каталог>/<имя файла>`.
    pub fn check_write(&self, path: &str) -> Result<PathBuf, FilePolicyError> {
        self.ensure_enabled()?;
        let absolute = self.absolute(path)?;
        let file_name = match absolute.components().next_back() {
            Some(Component::Normal(name)) => name.to_owned(),
            _ => {
                return Err(FilePolicyError(format!(
                    "`{path}` must end with a file name"
                )));
            }
        };
        let parent = absolute
            .parent()
            .ok_or_else(|| FilePolicyError(format!("`{path}` has no parent directory")))?;
        let parent = parent
            .canonicalize()
            .map_err(|err| self.io_error(path, &err))?;
        self.ensure_inside(path, &parent)?;
        let target = parent.join(file_name);
        if target.symlink_metadata().is_ok() {
            return Err(FilePolicyError(format!(
                "`{path}` already exists; choose a new file name (existing files are never overwritten)"
            )));
        }
        Ok(target)
    }

    /// Проверяет каталог для записи нескольких новых файлов и создаёт его, если его нет.
    /// Ближайший существующий предок должен лежать внутри разрешённого корня; созданный
    /// каталог проверяется ещё раз после канонизации. Возвращает канонический путь.
    pub fn check_write_dir(&self, path: &str) -> Result<PathBuf, FilePolicyError> {
        self.ensure_enabled()?;
        let absolute = self.absolute(path)?;
        // Строка проверяется и сама по себе: в verbatim-пути Windows (`\\?\C:\…`) `/` не
        // разделитель для `components`, а файловая система его так понимает.
        if absolute
            .components()
            .any(|c| matches!(c, Component::ParentDir))
            || path.split(['/', '\\']).any(|segment| segment == "..")
        {
            return Err(FilePolicyError(format!(
                "`{path}` must not contain `..` segments"
            )));
        }
        // Ближайший существующий предок: через него не должно быть выхода наружу.
        let mut existing = absolute.as_path();
        while existing.symlink_metadata().is_err() {
            existing = existing
                .parent()
                .ok_or_else(|| FilePolicyError(format!("`{path}` has no existing parent")))?;
        }
        let canonical_existing = existing
            .canonicalize()
            .map_err(|err| self.io_error(path, &err))?;
        self.ensure_inside(path, &canonical_existing)?;
        std::fs::create_dir_all(&absolute).map_err(|err| self.io_error(path, &err))?;
        let canonical = absolute
            .canonicalize()
            .map_err(|err| self.io_error(path, &err))?;
        self.ensure_inside(path, &canonical)?;
        if !canonical.is_dir() {
            return Err(FilePolicyError(format!("`{path}` is not a directory")));
        }
        Ok(canonical)
    }

    fn ensure_enabled(&self) -> Result<(), FilePolicyError> {
        if self.disabled {
            return Err(FilePolicyError(LOCAL_FILES_DISABLED.to_owned()));
        }
        Ok(())
    }

    fn absolute(&self, path: &str) -> Result<PathBuf, FilePolicyError> {
        if path.trim().is_empty() {
            return Err(FilePolicyError("file path is empty".into()));
        }
        // На Windows корни и рабочий каталог канонические, то есть verbatim (`\\?\C:\…`),
        // а в verbatim-пути `/` не разделитель: `sub/../x`, присоединённый к корню, стал бы
        // одним компонентом, и проверки `..` и родительского каталога его бы не видели.
        // Windows и так принимает `/` как разделитель, поэтому смысл пути не меняется.
        #[cfg(windows)]
        let path = &path.replace('/', "\\");
        let path = Path::new(path);
        if path.is_absolute() {
            return Ok(path.to_path_buf());
        }
        match &self.base_dir {
            // На Windows базовый каталог канонический, то есть verbatim-путь
            // (`\\?\C:\…`), а в нём разделитель только `\`: `base.join("a/../b")` дал бы
            // один компонент, и проверка `..` его бы не увидела. Поэтому относительный
            // путь разбирается по обычным правилам, а компоненты добавляются по одному;
            // `.` пропускается — в verbatim-пути он не сворачивается.
            Some(base) => {
                let mut joined = base.clone();
                for component in path.components() {
                    if component != Component::CurDir {
                        joined.push(component);
                    }
                }
                Ok(joined)
            }
            None => Err(FilePolicyError(format!(
                "`{}` is not an absolute path",
                path.display()
            ))),
        }
    }

    fn ensure_inside(&self, original: &str, canonical: &Path) -> Result<(), FilePolicyError> {
        if self.roots.iter().any(|root| canonical.starts_with(root)) {
            return Ok(());
        }
        Err(FilePolicyError(format!(
            "`{original}` is outside the allowed directories ({}); the user can allow more: \
             extra directories of the server's profile (`appgallery-connect-mcp ui`), or \
             `--allow-path <dir>` of a server over HTTP",
            self.describe_roots()
        )))
    }

    fn io_error(&self, path: &str, err: &io::Error) -> FilePolicyError {
        let reason = match err.kind() {
            io::ErrorKind::NotFound => "does not exist".to_owned(),
            io::ErrorKind::PermissionDenied => "is not accessible (permission denied)".to_owned(),
            _ => format!("is not accessible: {err}"),
        };
        FilePolicyError(format!("`{path}` {reason}"))
    }

    /// Список корней для сообщений и инструкций сервера.
    pub fn describe_roots(&self) -> String {
        if self.disabled {
            return "none (local files are disabled)".to_owned();
        }
        if self.roots.is_empty() {
            return "none".to_owned();
        }
        self.roots
            .iter()
            .map(|root| display_path(root))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl LocalFiles for FilePolicy {
    fn resolve_read(&self, path: &str) -> Result<PathBuf, ToolError> {
        Ok(self.check_read(path)?)
    }

    fn resolve_write(&self, path: &str) -> Result<PathBuf, ToolError> {
        Ok(self.check_write(path)?)
    }

    fn resolve_write_dir(&self, path: &str) -> Result<PathBuf, ToolError> {
        Ok(self.check_write_dir(path)?)
    }

    fn resolve_read_dir(&self, path: &str) -> Result<PathBuf, ToolError> {
        Ok(self.check_read_dir(path)?)
    }
}

/// Записывает `bytes` в новый файл `path`, уже проверенный [`FilePolicy::check_write`].
/// Файл создаётся с `create_new`: существующий файл (и символическая ссылка, появившаяся
/// после проверки) не перезаписывается. При ошибке записи частично записанный файл
/// удаляется. Возвращает число записанных байт.
pub fn write_new_file(path: &Path, bytes: &[u8]) -> Result<u64, ToolError> {
    use std::io::Write as _;
    let shown = display_path(path);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|err| match err.kind() {
            io::ErrorKind::AlreadyExists => ToolError::new(format!(
                "`{shown}` already exists; choose a new file name (existing files are never \
                 overwritten)"
            )),
            _ => ToolError::new(format!("could not create `{shown}`: {err}")),
        })?;
    let written = file.write_all(bytes).and_then(|()| file.flush());
    drop(file);
    if let Err(err) = written {
        let _ = std::fs::remove_file(path);
        return Err(ToolError::new(format!("could not write `{shown}`: {err}")));
    }
    Ok(bytes.len() as u64)
}

/// Записывает содержимое файла из ответа (поле вида `file-content`, например
/// `agconnect-services.json`) в новый файл `path`, уже проверенный
/// [`FilePolicy::check_write`]. Содержимое — строка (так описано в справочнике); если API
/// отдаст объект, сохраняется его JSON. Возвращает число записанных байт. Общая часть
/// `save_to` сервера и составных инструментов.
pub fn write_file_content(path: &Path, content: &Value) -> Result<u64, ToolError> {
    let bytes = match content {
        Value::String(text) => text.as_bytes().to_vec(),
        other => serde_json::to_vec_pretty(other).map_err(|err| {
            ToolError::new(format!("could not serialize the file content: {err}"))
        })?,
    };
    write_new_file(path, &bytes)
}

/// Путь для сообщений агенту. На Windows `canonicalize` возвращает verbatim-пути
/// (`\\?\C:\…`, `\\?\UNC\server\share\…`); агенту и пользователю показывается обычная
/// запись. Проверки по-прежнему идут по каноническим путям.
pub fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}")
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        local.to_owned()
    } else {
        text
    }
}

/// Домашний каталог из окружения (`std::env::home_dir` помечен устаревшим в MSRV 1.85).
fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        root: PathBuf,
        outside: PathBuf,
        policy: FilePolicy,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("sub/file.txt"), "inside").unwrap();
        fs::write(outside.join("secret.txt"), "outside").unwrap();
        let policy = FilePolicy::new(std::slice::from_ref(&root))
            .unwrap()
            .with_base_dir(&root);
        Fixture {
            _dir: dir,
            root,
            outside,
            policy,
        }
    }

    fn s(path: &Path) -> String {
        path.display().to_string()
    }

    #[test]
    fn reads_files_inside_root() {
        let f = fixture();
        let file = f.root.join("sub/file.txt");
        assert_eq!(f.policy.check_read(&s(&file)).unwrap(), file);
        assert_eq!(f.policy.check_read("sub/file.txt").unwrap(), file);
        assert_eq!(f.policy.check_read("sub/../sub/file.txt").unwrap(), file);
        assert_eq!(f.policy.resolve_read("sub/file.txt").unwrap(), file);
    }

    #[test]
    fn reads_directories_inside_root() {
        let f = fixture();
        let dir = f.root.join("sub");
        assert_eq!(f.policy.check_read_dir("sub").unwrap(), dir);
        assert_eq!(f.policy.resolve_read_dir(&s(&dir)).unwrap(), dir);
        let err = f.policy.check_read_dir("sub/file.txt").unwrap_err();
        assert!(err.0.contains("not a directory"), "{err}");
        let err = f.policy.check_read_dir(&s(&f.outside)).unwrap_err();
        assert!(err.0.contains("outside the allowed directories"), "{err}");
        assert!(f.policy.check_read_dir("missing").is_err());
    }

    #[test]
    fn rejects_escape_via_dotdot_and_relative() {
        let f = fixture();
        let escaped = f.root.join("../outside/secret.txt");
        let err = f.policy.check_read(&s(&escaped)).unwrap_err();
        assert!(err.0.contains("outside the allowed directories"), "{err}");
        assert!(err.0.contains("--allow-path"), "{err}");
        let err = f.policy.check_read("../outside/secret.txt").unwrap_err();
        assert!(err.0.contains("outside the allowed directories"), "{err}");
        assert!(
            f.policy
                .check_read(&s(&f.outside.join("secret.txt")))
                .is_err()
        );
        let err = f.policy.resolve_read("../outside/secret.txt").unwrap_err();
        assert!(err.message().contains("outside"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape() {
        let f = fixture();
        let link = f.root.join("link.txt");
        std::os::unix::fs::symlink(f.outside.join("secret.txt"), &link).unwrap();
        let err = f.policy.check_read(&s(&link)).unwrap_err();
        assert!(err.0.contains("outside the allowed directories"), "{err}");

        let dir_link = f.root.join("out");
        std::os::unix::fs::symlink(&f.outside, &dir_link).unwrap();
        assert!(
            f.policy
                .check_read(&s(&dir_link.join("secret.txt")))
                .is_err()
        );
        assert!(f.policy.check_write(&s(&dir_link.join("new.txt"))).is_err());
        assert!(f.policy.check_write(&s(&link)).is_err());
        assert!(f.policy.check_write_dir(&s(&dir_link.join("new"))).is_err());
        assert!(f.policy.check_write_dir(&s(&dir_link)).is_err());
        assert!(!f.outside.join("new").exists());
    }

    #[test]
    fn read_errors_are_clear() {
        let f = fixture();
        let err = f
            .policy
            .check_read(&s(&f.root.join("missing")))
            .unwrap_err();
        assert!(err.0.contains("does not exist"), "{err}");
        let err = f.policy.check_read(&s(&f.root.join("sub"))).unwrap_err();
        assert!(err.0.contains("not a regular file"), "{err}");
        assert!(f.policy.check_read("  ").is_err());
    }

    #[test]
    fn write_checks() {
        let f = fixture();
        let target = f.root.join("sub/new.bin");
        assert_eq!(f.policy.check_write(&s(&target)).unwrap(), target);
        assert_eq!(f.policy.check_write("sub/new.bin").unwrap(), target);
        assert_eq!(f.policy.resolve_write("sub/new.bin").unwrap(), target);
        let err = f
            .policy
            .check_write(&s(&f.root.join("sub/file.txt")))
            .unwrap_err();
        assert!(err.0.contains("already exists"), "{err}");
        assert!(f.policy.check_write(&s(&f.outside.join("x"))).is_err());
        assert!(f.policy.check_write(&s(&f.root.join("missing/x"))).is_err());
        assert!(f.policy.check_write(&s(&f.root.join("sub/.."))).is_err());
        // Относительный путь с `/` и `..`: на Windows он не должен стать одним именем
        // внутри verbatim-корня.
        assert!(f.policy.check_write("sub/../../outside/z").is_err());
        assert!(f.policy.check_write("../outside/z").is_err());
    }

    #[test]
    fn new_files_are_written_once() {
        let f = fixture();
        let target = f.policy.check_write("sub/config.json").unwrap();
        assert_eq!(write_new_file(&target, b"{}").unwrap(), 2);
        assert_eq!(fs::read(&target).unwrap(), b"{}");
        // Второй раз — отказ, содержимое не меняется.
        let err = write_new_file(&target, b"other").unwrap_err();
        assert!(err.message().contains("already exists"), "{err}");
        assert_eq!(fs::read(&target).unwrap(), b"{}");
        let err = f.policy.check_write("sub/config.json").unwrap_err();
        assert!(err.0.contains("already exists"), "{err}");
    }

    /// Пути с прямым слэшем под Windows: относительные и абсолютные (`C:/…`) работают, а
    /// `..` не выводит из корня.
    #[cfg(windows)]
    #[test]
    fn forward_slashes_work_on_windows() {
        let f = fixture();
        let file = f.root.join("sub").join("file.txt");
        assert_eq!(f.policy.check_read("sub/file.txt").unwrap(), file);
        assert_eq!(f.policy.check_read("./sub/file.txt").unwrap(), file);
        let absolute = format!("{}/sub/file.txt", display_path(&f.root).replace('\\', "/"));
        assert_eq!(f.policy.check_read(&absolute).unwrap(), file);
        assert_eq!(
            f.policy.check_write("sub/new.txt").unwrap(),
            f.root.join("sub").join("new.txt")
        );
        let absolute_new = format!("{}/sub/new2.txt", display_path(&f.root).replace('\\', "/"));
        assert_eq!(
            f.policy.check_write(&absolute_new).unwrap(),
            f.root.join("sub").join("new2.txt")
        );
        assert!(f.policy.check_read("sub/../../outside/secret.txt").is_err());
        assert!(f.policy.check_write("sub/../../outside/z").is_err());
        let err = f.policy.check_write_dir("sub/../../outside/y").unwrap_err();
        assert!(err.0.contains("`..`") || err.0.contains("outside"), "{err}");
        assert!(!f.outside.join("y").exists());
        assert!(!f.outside.join("z").exists());
    }

    #[test]
    fn write_dir_is_created_inside_roots() {
        let f = fixture();
        let created = f.policy.check_write_dir("export/en-US").unwrap();
        assert_eq!(created, f.root.join("export/en-US"));
        assert!(created.is_dir());
        // Существующий каталог допустим.
        assert_eq!(
            f.policy.resolve_write_dir("sub").unwrap(),
            f.root.join("sub")
        );
        let err = f
            .policy
            .check_write_dir(&s(&f.outside.join("x")))
            .unwrap_err();
        assert!(err.0.contains("outside"), "{err}");
        assert!(!f.outside.join("x").exists());
        // На Windows `join` к verbatim-корню сам сворачивает `..`, и путь может отклонить
        // проверка корней, а не проверка `..`: важно, что каталог не создан.
        let err = f.policy.check_write_dir("sub/../../outside/y").unwrap_err();
        assert!(err.0.contains("`..`") || err.0.contains("outside"), "{err}");
        assert!(!f.outside.join("y").exists());
        let err = f
            .policy
            .check_write_dir(r"sub\..\..\outside\z")
            .unwrap_err();
        assert!(err.0.contains("`..`"), "{err}");
        assert!(!f.outside.join("z").exists());
        assert!(f.policy.check_write_dir("sub/file.txt").is_err());
    }

    #[test]
    fn relative_paths_need_base_dir() {
        let f = fixture();
        let policy = FilePolicy::new(std::slice::from_ref(&f.root)).unwrap();
        let err = policy.check_read("sub/file.txt").unwrap_err();
        assert!(err.0.contains("not an absolute path"), "{err}");
    }

    #[test]
    fn extra_roots_must_exist() {
        let f = fixture();
        assert!(FilePolicy::new(&[f.root.join("missing")]).is_err());
        assert!(FilePolicy::new(&[f.root.join("sub/file.txt")]).is_err());
        let policy = FilePolicy::with_defaults(std::slice::from_ref(&f.outside)).unwrap();
        assert!(policy.roots().contains(&f.outside));
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        assert!(policy.roots().contains(&tmp));
    }

    #[test]
    fn disabled_policy_rejects_every_path() {
        let f = fixture();
        let policy = FilePolicy::disabled();
        assert!(policy.is_disabled());
        let inside = s(&f.root.join("sub/file.txt"));
        for err in [
            policy.check_read(&inside).unwrap_err(),
            policy
                .check_write(&s(&f.root.join("sub/new.bin")))
                .unwrap_err(),
            policy.check_write_dir(&s(&f.root)).unwrap_err(),
        ] {
            assert_eq!(err.0, LOCAL_FILES_DISABLED);
        }
        assert!(policy.describe_roots().contains("disabled"));
    }

    #[test]
    fn display_path_hides_windows_verbatim_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\me\project")),
            r"C:\Users\me\project"
        );
        assert_eq!(
            display_path(Path::new("/home/me/project")),
            "/home/me/project"
        );
    }
}
