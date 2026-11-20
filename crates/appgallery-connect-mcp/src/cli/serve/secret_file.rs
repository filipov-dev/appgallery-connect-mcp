//! Учётные данные API-клиента из файла секрета (`serve --http --secret-file`).
//!
//! Единственное исключение из правила «учётные данные только в системном хранилище»: в
//! контейнере (Docker, Kubernetes) системного хранилища нет, и секрет приходит файлом
//! (`/run/secrets/...`). Файл должен быть обычным файлом без прав для группы и остальных
//! (0600 или 0400): секрет, который может прочитать любой пользователь хоста, — ошибка
//! запуска, а не предупреждение. Права проверяются у уже открытого файла (`fstat`), поэтому
//! подмена файла между проверкой и чтением не проходит. Символическая ссылка допускается
//! (так монтирует секреты Kubernetes): проверяется файл, на который она ведёт.
//!
//! Форматы файла:
//! - только секрет API-клиента (одна строка, перевод строки в конце допускается);
//!   идентификатор клиента — флагом `--client-id`: он не секрет и в файл секрета не нужен;
//! - JSON `{"client_id": "...", "client_secret": "..."}` — как запись системного хранилища
//!   и файл `key exec --format json`; `--client-id` тогда не передаётся.
//!
//! Содержимое файла и путь к нему в ответы MCP не попадают; тексты ошибок разбора не
//! содержат значений ([`appgallery_connect_api::CredentialsError`]).

use std::io::Read;
use std::path::Path;

use anyhow::{Context as _, anyhow, bail};
use appgallery_connect_api::ApiClientCredentials;
use appgallery_connect_api::credentials::MAX_STORED_BYTES;
use zeroize::Zeroizing;

/// Читает и проверяет учётные данные API-клиента из файла секрета. `client_id` — значение
/// `--client-id` (нужно, если в файле только секрет).
pub fn load(path: &Path, client_id: Option<&str>) -> anyhow::Result<ApiClientCredentials> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open the secret file {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("cannot read the secret file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("the secret file {} is not a regular file", path.display());
    }
    check_permissions(path, &metadata)?;
    // Лимит + 1: файл больше лимита распознаётся без чтения его целиком.
    let limit = MAX_STORED_BYTES as u64 + 1;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(limit)
        .read_to_end(&mut bytes)
        .with_context(|| format!("cannot read the secret file {}", path.display()))?;
    if bytes.len() > MAX_STORED_BYTES {
        bail!(
            "the secret file {} is larger than {MAX_STORED_BYTES} bytes: it is not an API \
             client secret",
            path.display()
        );
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("the secret file {} is not UTF-8 text", path.display()))?;
    let invalid = |err| {
        anyhow!(
            "the secret file {} does not hold a valid API client secret: {err}",
            path.display()
        )
    };
    // Секреты Docker и Kubernetes часто заканчиваются переводом строки.
    let text = text.trim_end_matches(['\n', '\r']);
    if text.trim_start().starts_with('{') {
        if client_id.is_some() {
            bail!(
                "the secret file {} already holds the client ID (JSON with client_id and \
                 client_secret): do not pass --client-id",
                path.display()
            );
        }
        return ApiClientCredentials::from_stored_json(text).map_err(invalid);
    }
    let client_id = client_id.ok_or_else(|| {
        anyhow!(
            "the secret file {} holds only the API client secret: pass the client ID with \
             --client-id <ID> (AppGallery Connect > Users and permissions > API key > Connect \
             API)",
            path.display()
        )
    })?;
    ApiClientCredentials::from_parts(client_id.trim(), text.to_owned()).map_err(invalid)
}

#[cfg(unix)]
fn check_permissions(path: &Path, metadata: &std::fs::Metadata) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        bail!(
            "the secret file {} is accessible by other users (mode {mode:04o}): allow access \
             only for its owner (chmod 600, or mode 0400 for a Docker secret)",
            path.display()
        );
    }
    Ok(())
}

/// Без прав Unix проверить, что файл закрыт от других пользователей, нельзя, а файлы
/// секретов Docker и Kubernetes — механизм Linux: на других системах учётные данные берутся
/// из системного хранилища.
#[cfg(not(unix))]
fn check_permissions(path: &Path, _metadata: &std::fs::Metadata) -> anyhow::Result<()> {
    bail!(
        "--secret-file {} is supported only on Unix (Docker and Kubernetes secrets); store the \
         API client in the system keychain with `appgallery-connect-mcp auth login` instead",
        path.display()
    )
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::testing::{client_id, client_secret as secret, credentials};

    fn write(dir: &Path, name: &str, text: &str, mode: u32) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn reads_owner_only_secret_files() {
        let dir = tempfile::tempdir().unwrap();
        let id = client_id("file");
        for mode in [0o600, 0o400] {
            let path = write(
                dir.path(),
                &format!("secret-{mode:o}"),
                &format!("{}\n", secret("file")),
                mode,
            );
            let loaded = load(&path, Some(&id)).unwrap();
            assert_eq!(loaded, credentials("file"));
        }
        // JSON с обоими значениями — как запись хранилища и `key exec --format json`.
        let json = credentials("file").to_stored_json();
        let path = write(dir.path(), "credentials.json", &json, 0o600);
        assert_eq!(load(&path, None).unwrap(), credentials("file"));
        let err = load(&path, Some(&id)).unwrap_err().to_string();
        assert!(err.contains("do not pass --client-id"), "{err}");

        // Ссылка на закрытый файл (так монтирует секреты Kubernetes) допускается.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(load(&link, None).is_ok());
    }

    #[test]
    fn rejects_open_or_broken_secret_files() {
        let dir = tempfile::tempdir().unwrap();
        let value = secret("open");
        let id = client_id("open");
        for mode in [0o644, 0o640, 0o604, 0o660] {
            let path = write(dir.path(), &format!("secret-{mode:o}"), &value, mode);
            let err = load(&path, Some(&id)).unwrap_err().to_string();
            assert!(err.contains("accessible by other users"), "{err}");
            assert!(err.contains(&format!("{mode:04o}")), "{err}");
            assert!(!err.contains(&value), "{err}");
        }

        let err = load(&dir.path().join("missing"), Some(&id))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot open the secret file"), "{err}");
        let err = load(dir.path(), Some(&id)).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");

        let path = write(dir.path(), "big", &"x".repeat(MAX_STORED_BYTES + 1), 0o600);
        let err = load(&path, Some(&id)).unwrap_err().to_string();
        assert!(err.contains("larger than"), "{err}");

        // Только секрет без --client-id — подсказка про флаг.
        let path = write(dir.path(), "plain", &value, 0o600);
        let err = load(&path, None).unwrap_err().to_string();
        assert!(err.contains("--client-id"), "{err}");
        assert!(!err.contains(&value), "{err}");

        // Ошибка разбора не повторяет содержимое файла.
        let path = write(dir.path(), "spaces", &format!("{value} {value}"), 0o600);
        let err = format!("{:#}", load(&path, Some(&id)).unwrap_err());
        assert!(err.contains("not allowed"), "{err}");
        assert!(!err.contains(&value), "{err}");
        let path = write(
            dir.path(),
            "broken.json",
            &format!("{{\"client_secret\": \"{value}\"}}"),
            0o600,
        );
        let err = format!("{:#}", load(&path, None).unwrap_err());
        assert!(err.contains("client_id"), "{err}");
        assert!(!err.contains(&value), "{err}");
    }
}
