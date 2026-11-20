//! `appgallery-connect-mcp key exec` — учётные данные API-клиента профиля для внешних
//! инструментов (Gradle-плагин публикации в AppGallery, fastlane, свои скрипты) временным
//! файлом.
//!
//! Учётные данные пишутся в новый файл с правами 0600 в новом каталоге с правами 0700,
//! команда запускается без оболочки с путём к файлу в переменной окружения (по умолчанию
//! [`DEFAULT_PATH_ENV`]), её stdout и stderr проходят через [`mask::Masker`]. После
//! завершения команды файл и каталог удаляются — и при ошибке запуска, и при ненулевом
//! коде, и при Ctrl-C/SIGTERM: Ctrl-C команда получает от терминала сама (общая группа
//! процессов), а мы его только переживаем; SIGTERM и SIGHUP пересылаются команде.
//! Обработчики сигналов ставятся до записи файла. От SIGKILL защиты нет: файл останется во
//! временном каталоге пользователя, доступный только ему.
//!
//! На Windows вместо режимов — ACL только для текущего пользователя, вместо пересылки
//! сигналов — объект задания для дерева процессов команды (модуль `windows`).
//!
//! Форматы файла ([`FileFormat`]):
//! - `json` (по умолчанию) — `{"client_id": "...", "client_secret": "..."}`: так выглядит
//!   файл `credentialsPath` Gradle-плагина `ru.cian.huawei-publish-gradle-plugin` и запись
//!   хранилища профиля. Других полей нет, чтобы файл принимали и строгие разборщики;
//!   регион передаётся отдельной несекретной переменной [`REGION_ENV`];
//! - `dotenv` — строки `HUAWEI_CLIENT_ID='...'` и `HUAWEI_CLIENT_SECRET='...'` для
//!   `Dotenv.load` в Fastfile (параметры `client_id`/`client_secret` плагина
//!   `fastlane-plugin-huawei_appgallery_connect`) или `. "$FILE"` в оболочке.
//!
//! Секрет в переменные окружения не кладётся — только путь к файлу; унаследованные
//! переменные с учётными данными ([`CONTENT_ENVS`]) убираются, чтобы инструмент не взял
//! чужой API-клиент. Аргументы команды мы не раскрываем: `$APPGALLERY_CONNECT_CREDENTIALS_FILE`
//! раскрывает оболочка дочерней команды (`-- sh -c '…'`).

mod mask;
#[cfg(windows)]
mod windows;

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};

use anyhow::{Context, anyhow, bail};
use appgallery_connect_api::{
    ApiClientCredentials, CredentialStore, KeyringStore, Region, validate_project_id,
};
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;
use tokio::io::AsyncWrite;
use tokio::process::Command;
use zeroize::Zeroizing;

use self::mask::{Masker, needles_for_credentials, pump};
use super::ProfileArg;
use crate::profiles::{
    ProfileAuth, ProfileKind, ProfileRepo, load_profile_with, validate_profile_name,
};

/// Переменная по умолчанию для пути к файлу учётных данных.
pub const DEFAULT_PATH_ENV: &str = "APPGALLERY_CONNECT_CREDENTIALS_FILE";
/// Регион профиля (`cn`, `de`, `sg`, `ru`): не секрет, задаётся всегда.
pub const REGION_ENV: &str = "APPGALLERY_CONNECT_REGION";
/// Переменные с самими учётными данными (так их называют примеры плагина fastlane):
/// унаследованные от родителя убираются, чтобы инструмент не взял вместо файла чужой
/// API-клиент.
pub const CONTENT_ENVS: [&str; 2] = ["HUAWEI_CLIENT_ID", "HUAWEI_CLIENT_SECRET"];

/// Префикс имени временного каталога.
const TEMP_DIR_PREFIX: &str = "appgallery-connect-mcp-key-";

const EXEC_HELP: &str = "\
The API client credentials (client ID and secret) of the profile are written to a new \
temporary file (mode 0600, in a new directory with mode 0700) and the command runs with the \
path to it in APPGALLERY_CONNECT_CREDENTIALS_FILE and the region of the profile (cn, de, sg, \
ru) in APPGALLERY_CONNECT_REGION. --env NAME (repeatable) sets the path in NAME instead; \
APPGALLERY_CONNECT_CREDENTIALS_FILE is then removed from the environment of the command. \
The secret itself is never put into the environment; inherited HUAWEI_CLIENT_ID and \
HUAWEI_CLIENT_SECRET are removed. With --project-id the file holds the project-level API \
client of that project (stored with `auth project add`) instead of the team-level client of \
the profile, in the same format.

File formats (--format):
  json    credentials.json: {\"client_id\": \"...\", \"client_secret\": \"...\"}, as the \
credentialsPath file of the Huawei publish Gradle plugin (ru.cian.huawei-publish-gradle-plugin)
  dotenv  credentials.env: HUAWEI_CLIENT_ID='...' and HUAWEI_CLIENT_SECRET='...', for \
Dotenv.load in a Fastfile (client_id and client_secret of the huawei_appgallery_connect \
fastlane plugin) or `. \"$FILE\"` in a shell

The file and its directory are deleted when the command exits, also when it fails, cannot be \
started or is interrupted: Ctrl-C reaches the command through the terminal, SIGTERM and SIGHUP \
are forwarded to it. The exit code of the command is returned.

On Windows the directory and the file are accessible only to the current user (the directory \
does not inherit permissions), the command and the processes it starts run in a job object: \
when the command exits or the console window is closed, the remaining processes are stopped \
and the file is deleted. A command without an extension is also looked up as .cmd and .bat \
(gradlew.bat, fastlane.bat).

The command runs directly, without a shell; its stdin is inherited. The client secret and the \
client ID (as they are and in base64), the whole file and AppGallery Connect access tokens \
(JWT) are replaced with *** in its stdout and stderr. To use the variables in arguments, run a \
shell yourself, in single quotes, so that the child shell expands them:
  appgallery-connect-mcp key exec --profile work -- sh -c \\
    './gradlew publishHuaweiAppGalleryRelease --credentialsPath \"$APPGALLERY_CONNECT_CREDENTIALS_FILE\"'
  appgallery-connect-mcp key exec --profile work --format dotenv -- fastlane android deploy
  appgallery-connect-mcp key exec --profile work --env AGC_CREDENTIALS -- ./scripts/upload.sh

Never print the credentials: masking is a safety net against accidental output, not a \
security boundary.";

#[derive(Debug, Subcommand)]
pub enum KeyCommand {
    /// Run a command with the API client credentials of the profile in a temporary file
    /// (mode 0600) that is deleted when the command exits.
    #[command(after_help = EXEC_HELP)]
    Exec(ExecArgs),
}

/// Формат файла учётных данных.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum FileFormat {
    /// JSON with client_id and client_secret.
    #[default]
    Json,
    /// HUAWEI_CLIENT_ID and HUAWEI_CLIENT_SECRET lines (dotenv, shell).
    Dotenv,
}

impl FileFormat {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::Json => "credentials.json",
            Self::Dotenv => "credentials.env",
        }
    }

    /// Содержимое файла. Ошибка не содержит значений.
    pub fn render(self, credentials: &ApiClientCredentials) -> anyhow::Result<Zeroizing<String>> {
        let client_id = credentials.client_id().expose();
        let client_secret = credentials.client_secret().expose();
        match self {
            Self::Json => {
                #[derive(Serialize)]
                struct File<'a> {
                    client_id: &'a str,
                    client_secret: &'a str,
                }
                let mut text = Zeroizing::new(
                    serde_json::to_string_pretty(&File {
                        client_id,
                        client_secret,
                    })
                    .context("cannot serialize the credentials")?,
                );
                text.push('\n');
                Ok(text)
            }
            Self::Dotenv => {
                // Значения в одинарных кавычках: ни оболочка, ни dotenv не раскрывают в них
                // `$`. Кавычек и обратной косой в значениях нет (`ClientId::new`,
                // `ClientSecret::new`), одинарная кавычка допустима — её не выразить.
                for (field, value) in [("client ID", client_id), ("client secret", client_secret)] {
                    if value.contains('\'') {
                        bail!(
                            "the {field} contains a single quote and cannot be written in the \
                             dotenv format; use --format json"
                        );
                    }
                }
                Ok(Zeroizing::new(format!(
                    "{}='{client_id}'\n{}='{client_secret}'\n",
                    CONTENT_ENVS[0], CONTENT_ENVS[1]
                )))
            }
        }
    }
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Format of the credentials file.
    #[arg(long, value_enum, default_value_t = FileFormat::Json)]
    pub format: FileFormat,
    /// Environment variable for the path to the credentials file (repeatable). Replaces the
    /// default APPGALLERY_CONNECT_CREDENTIALS_FILE.
    #[arg(long = "env", value_name = "NAME")]
    pub env: Vec<String>,
    /// Use the project-level API client of this project (`auth project add`) instead of
    /// the team-level API client of the profile.
    #[arg(long, value_name = "ID")]
    pub project_id: Option<String>,
    /// The command and its arguments, after `--`.
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<OsString>,
}

pub async fn run(command: KeyCommand) -> anyhow::Result<ExitCode> {
    match command {
        KeyCommand::Exec(args) => {
            // Всё, что может не пройти, — до создания файла.
            let names = path_env_names(&args.env)?;
            let repo = ProfileRepo::open_default()?;
            let (credentials, region) = load_credentials(
                &repo,
                &KeyringStore,
                &args.profile.profile,
                args.project_id.as_deref(),
            )?;
            // Обработчики ставятся до создания файла: сигнал между записью учётных данных и
            // запуском команды не должен убить процесс, оставив файл на диске.
            let mut signals = ProcessSignals::new()?;
            let code = exec_with_credentials(
                &std::env::temp_dir(),
                &Exec {
                    credentials: &credentials,
                    region,
                    format: args.format,
                    names: &names,
                },
                &args.command,
                tokio::io::stdout(),
                tokio::io::stderr(),
                &mut signals,
            )
            .await?;
            Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
        }
    }
}

/// Учётные данные и регион профиля: клиента команды или, с `project`, клиента этого
/// проекта (клиент команды тогда не нужен). Нет учётных данных — ошибка с подсказкой
/// `auth login --profile` или `auth project add`.
pub fn load_credentials(
    repo: &ProfileRepo,
    store: &dyn CredentialStore,
    profile: &str,
    project: Option<&str>,
) -> anyhow::Result<(ApiClientCredentials, Region)> {
    let Some(project) = project else {
        validate_profile_name(profile)?;
        // Вход пользователя другим программам не отдаётся: refresh token даёт доступ от
        // имени человека на полгода, а форматов файла для него у Gradle и fastlane нет.
        if repo
            .get(profile)?
            .is_some_and(|settings| settings.kind == ProfileKind::Oauth)
        {
            bail!(
                "profile `{profile}` signs in as a HUAWEI ID user (auth login --oauth): key exec \
                 passes only API client credentials, and a user sign-in is never given to other \
                 programs. Use a profile with an API client (`appgallery-connect-mcp auth login \
                 --profile <name>`) or a project-level API client (--project-id)"
            );
        }
        let profile = load_profile_with(repo, store, profile)?;
        let ProfileAuth::ApiClient(credentials) = profile.auth else {
            bail!("profile `{}` has no API client credentials", profile.name);
        };
        return Ok((credentials, profile.settings.region));
    };
    validate_profile_name(profile)?;
    let project = project.trim();
    validate_project_id(project).map_err(|err| anyhow!("invalid --project-id: {err}"))?;
    let settings = repo.get(profile)?.unwrap_or_default();
    let credentials = store.get_project(profile, project)?;
    Ok((credentials, settings.region))
}

/// Проверяет имя переменной окружения: латинские буквы, цифры и `_`, не с цифры.
fn validate_env_name(name: &str) -> anyhow::Result<()> {
    let valid = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && name.chars().next().is_some_and(|c| !c.is_ascii_digit());
    if !valid {
        bail!(
            "invalid environment variable name `{name}`: use A-Z, a-z, 0-9 and `_`, not \
             starting with a digit"
        );
    }
    if CONTENT_ENVS.contains(&name) {
        bail!(
            "`{name}` holds a credential, not a path; key exec removes it and passes only the \
             path to the credentials file"
        );
    }
    if name == REGION_ENV {
        bail!("`{name}` holds the region of the profile, not a path");
    }
    Ok(())
}

/// Переменные для пути к файлу: из `--env` (без повторов) или по умолчанию.
pub fn path_env_names(requested: &[String]) -> anyhow::Result<Vec<String>> {
    if requested.is_empty() {
        return Ok(vec![DEFAULT_PATH_ENV.to_owned()]);
    }
    let mut names: Vec<String> = Vec::new();
    for name in requested {
        validate_env_name(name)?;
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    Ok(names)
}

/// Сигнал, который пересылается дочерней команде.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Forward {
    #[cfg(unix)]
    Terminate,
    #[cfg(unix)]
    Hangup,
    /// Windows: закрывается консоль, сеанс или система — завершить дерево процессов команды.
    #[cfg(windows)]
    Close,
}

/// Источник сигналов для пересылки; в тестах подменяется каналом, чтобы не трогать
/// обработчики сигналов процесса тестов.
pub trait SignalSource {
    /// Следующий сигнал для пересылки; `None` — сигналов больше не будет.
    async fn recv(&mut self) -> Option<Forward>;
}

/// Сигналы процесса.
#[cfg(unix)]
pub struct ProcessSignals {
    term: tokio::signal::unix::Signal,
    hup: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ProcessSignals {
    pub fn new() -> anyhow::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let context = "cannot install signal handlers";
        Ok(Self {
            term: signal(SignalKind::terminate()).context(context)?,
            hup: signal(SignalKind::hangup()).context(context)?,
            int: signal(SignalKind::interrupt()).context(context)?,
        })
    }
}

#[cfg(unix)]
impl SignalSource for ProcessSignals {
    async fn recv(&mut self) -> Option<Forward> {
        loop {
            tokio::select! {
                got = self.term.recv() => return got.map(|()| Forward::Terminate),
                got = self.hup.recv() => return got.map(|()| Forward::Hangup),
                // Ctrl-C команда получает от терминала сама; повторная пересылка дала бы ей
                // два сигнала. Сами переживаем его, чтобы дождаться команды и удалить файл.
                got = self.int.recv() => got?,
            }
        }
    }
}

#[cfg(windows)]
use self::windows::ChildTree;
#[cfg(windows)]
pub use self::windows::ProcessSignals;

/// Дочерняя команда для пересылки сигналов (на Unix — по PID).
#[cfg(unix)]
struct ChildTree {
    pid: Option<u32>,
}

#[cfg(unix)]
impl ChildTree {
    fn adopt(child: &tokio::process::Child) -> Self {
        Self { pid: child.id() }
    }

    /// Пересылает сигнал дочернему процессу. Процесс ещё не «пожат» (`wait` не завершён),
    /// поэтому его PID не может достаться другому процессу.
    fn forward(&mut self, signal: Forward) {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) else {
            return;
        };
        let signal = match signal {
            Forward::Terminate => Signal::SIGTERM,
            Forward::Hangup => Signal::SIGHUP,
        };
        // Процесс мог уже завершиться: ошибка `ESRCH` не важна.
        let _ = kill(Pid::from_raw(pid), signal);
    }

    /// Команда завершилась: потомки, которые она оставила, на Unix продолжают работать.
    fn release(&mut self) {
        self.pid = None;
    }
}

/// Предупреждение в stderr. Сбой записи не важен (на Windows консоль могла уже закрыться),
/// а `eprintln!` в этом случае паникует.
fn warn(message: &str) {
    use std::io::Write as _;
    let _ = writeln!(
        std::io::stderr(),
        "appgallery-connect-mcp: warning: {message}"
    );
}

/// Попыток удалить каталог файла. На Windows файл, открытый другим процессом (антивирус,
/// индексатор), удалить нельзя, пока его не закроют: несколько повторов с паузой.
const REMOVE_ATTEMPTS: u32 = if cfg!(windows) { 10 } else { 1 };
const REMOVE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// Временный файл учётных данных в собственном каталоге. Каталог удаляется и при `drop`
/// (паника, ранний выход), но обычный путь — [`CredentialsFile::remove`] с проверкой
/// результата.
pub struct CredentialsFile {
    dir: tempfile::TempDir,
    path: PathBuf,
}

impl CredentialsFile {
    /// Создаёт в `root` новый каталог (0700) и в нём новый файл `name` (0600) с `contents`.
    pub fn create(root: &Path, name: &str, contents: &str) -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix(TEMP_DIR_PREFIX)
            .tempdir_in(root)
            .context("cannot create a temporary directory for the credentials file")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
                .context("cannot restrict the temporary directory to the current user")?;
        }
        // Права каталога — до создания файла: файл наследует их при создании.
        #[cfg(windows)]
        windows::restrict_to_current_user(dir.path())
            .context("cannot restrict the temporary directory to the current user")?;
        let path = dir.path().join(name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .with_context(|| format!("cannot create `{}`", path.display()))?;
        // Права задаются явно ещё раз: `mode` при создании урезается umask (только сужает,
        // но так права не зависят от umask вовсе).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .with_context(|| format!("cannot set the mode of `{}`", path.display()))?;
        }
        file.write_all(contents.as_bytes())
            .and_then(|()| file.sync_all())
            .with_context(|| format!("cannot write `{}`", path.display()))?;
        Ok(Self { dir, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Удаляет файл и каталог.
    pub fn remove(self) -> anyhow::Result<()> {
        let dir = self.dir.keep();
        let mut attempt = 1;
        loop {
            match fs::remove_dir_all(&dir) {
                Ok(()) => return Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(_) if attempt < REMOVE_ATTEMPTS => {
                    attempt += 1;
                    std::thread::sleep(REMOVE_RETRY_DELAY);
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "could not delete the temporary credentials directory {}; delete \
                             it yourself",
                            dir.display()
                        )
                    });
                }
            }
        }
    }
}

/// Переменные окружения команды: что задать и что убрать из унаследованных.
pub struct ExecEnv {
    pub set: Vec<(String, OsString)>,
    pub remove: Vec<String>,
}

/// Путь к файлу — в каждой из `names`, регион — в [`REGION_ENV`]; переменные с учётными
/// данными и не выбранная переменная по умолчанию (она указывала бы на другой файл)
/// убираются.
pub fn exec_env(file: &CredentialsFile, region: Region, names: &[String]) -> ExecEnv {
    let path = file.path().as_os_str().to_os_string();
    let mut set: Vec<(String, OsString)> = names
        .iter()
        .map(|name| (name.clone(), path.clone()))
        .collect();
    set.push((REGION_ENV.to_owned(), region.slug().into()));
    let remove = CONTENT_ENVS
        .iter()
        .chain([DEFAULT_PATH_ENV].iter())
        .map(|name| (*name).to_owned())
        .filter(|name| !names.contains(name))
        .collect();
    ExecEnv { set, remove }
}

/// Что передать команде.
pub struct Exec<'a> {
    pub credentials: &'a ApiClientCredentials,
    pub region: Region,
    pub format: FileFormat,
    /// Переменные для пути к файлу ([`path_env_names`]).
    pub names: &'a [String],
}

/// Пишет учётные данные во временный файл в `temp_root`, запускает команду и удаляет файл.
/// Возвращает код завершения команды (для завершения сигналом на Unix — `128 + номер
/// сигнала`).
pub async fn exec_with_credentials<O, E, S>(
    temp_root: &Path,
    exec: &Exec<'_>,
    command: &[OsString],
    stdout: O,
    stderr: E,
    signals: &mut S,
) -> anyhow::Result<i32>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
    S: SignalSource,
{
    let Some((program, args)) = command.split_first() else {
        bail!("no command given: pass it after `--`");
    };
    let contents = exec.format.render(exec.credentials)?;
    let file = CredentialsFile::create(temp_root, exec.format.file_name(), &contents)?;
    let env = exec_env(&file, exec.region, exec.names);
    let needles = needles_for_credentials(exec.credentials, &contents);
    drop(contents);
    let result = run_masked(program, args, &env, &needles, stdout, stderr, signals).await;
    let removed = file.remove();
    match (result, removed) {
        (Ok(code), Ok(())) => Ok(code),
        (Ok(code), Err(err)) => Err(err.context(format!("the command exited with code {code}"))),
        (Err(err), Ok(())) => Err(err),
        (Err(err), Err(removal)) => Err(err.context(format!("{removal:#}"))),
    }
}

/// Запускает команду, копирует её вывод с маскированием и пересылает ей сигналы.
async fn run_masked<O, E, S>(
    program: &OsString,
    args: &[OsString],
    env: &ExecEnv,
    needles: &[Zeroizing<Vec<u8>>],
    stdout: O,
    stderr: E,
    signals: &mut S,
) -> anyhow::Result<i32>
where
    O: AsyncWrite + Unpin,
    E: AsyncWrite + Unpin,
    S: SignalSource,
{
    let command = |program: &OsString| {
        let mut command = Command::new(program);
        command
            .args(args)
            .envs(env.set.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::inherit())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in &env.remove {
            command.env_remove(name);
        }
        command
    };
    #[cfg(windows)]
    let spawned = windows::spawn_with_fallbacks(program, |program| command(program).spawn());
    #[cfg(not(windows))]
    let spawned = command(program).spawn();
    let mut child =
        spawned.with_context(|| format!("cannot run `{}`", program.to_string_lossy()))?;
    let mut tree = ChildTree::adopt(&child);
    let child_out = child.stdout.take().context("child stdout is not piped")?;
    let child_err = child.stderr.take().context("child stderr is not piped")?;

    let wait = async {
        let mut signals_open = true;
        loop {
            tokio::select! {
                status = child.wait() => {
                    // На Windows закрытие задания завершает оставленных командой потомков:
                    // они держали бы файл и вывод команды открытыми.
                    tree.release();
                    return status;
                }
                signal = signals.recv(), if signals_open => match signal {
                    Some(signal) => tree.forward(signal),
                    None => signals_open = false,
                },
            }
        }
    };
    let (status, out_result, err_result) = tokio::join!(
        wait,
        pump(child_out, stdout, Masker::new(needles)),
        pump(child_err, stderr, Masker::new(needles)),
    );
    for (stream, result) in [("stdout", out_result), ("stderr", err_result)] {
        if let Err(err) = result {
            // Закрытый канал (`| head`) — обычная ситуация конвейера.
            if err.kind() != std::io::ErrorKind::BrokenPipe {
                warn(&format!("copying the command {stream} failed: {err}"));
            }
        }
    }
    let status = status.context("waiting for the command failed")?;
    Ok(exit_code(&status))
}

fn exit_code(status: &std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

#[cfg(test)]
mod name_tests {
    use appgallery_connect_api::MemoryStore;

    use super::*;
    use crate::profiles::ProfileSettings;
    use crate::testing::credentials;

    #[test]
    fn env_names() {
        assert_eq!(path_env_names(&[]).unwrap(), [DEFAULT_PATH_ENV]);
        assert_eq!(
            path_env_names(&["AGC_FILE".into(), "_X1".into(), "AGC_FILE".into()]).unwrap(),
            ["AGC_FILE", "_X1"]
        );
        for bad in [
            "",
            "1KEY",
            "A-B",
            "A B",
            "KEY=1",
            "HUAWEI_CLIENT_SECRET",
            "HUAWEI_CLIENT_ID",
            REGION_ENV,
        ] {
            assert!(path_env_names(&[bad.into()]).is_err(), "{bad}");
        }
    }

    #[test]
    fn formats_hold_the_credentials() {
        let creds = credentials("1");
        let id = creds.client_id().expose().to_owned();
        let secret = creds.client_secret().expose().to_owned();

        let json = FileFormat::Json.render(&creds).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"client_id": id, "client_secret": secret})
        );
        assert!(json.ends_with("}\n"));

        let dotenv = FileFormat::Dotenv.render(&creds).unwrap();
        assert_eq!(
            dotenv.as_str(),
            format!("HUAWEI_CLIENT_ID='{id}'\nHUAWEI_CLIENT_SECRET='{secret}'\n")
        );

        // Одинарную кавычку в dotenv не выразить: ошибка без значения.
        let quoted = ApiClientCredentials::from_parts(id.clone(), format!("{secret}'x")).unwrap();
        let err = FileFormat::Dotenv.render(&quoted).unwrap_err().to_string();
        assert!(err.contains("--format json"), "{err}");
        assert!(!err.contains(&secret), "{err}");
        assert!(FileFormat::Json.render(&quoted).is_ok());
    }

    #[test]
    fn profile_without_credentials_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        let store = MemoryStore::new();
        let err = load_credentials(&repo, &store, "work", None)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("auth login --profile work"), "{err}");
        assert!(load_credentials(&repo, &store, "bad name", None).is_err());

        store.set("work", &credentials("1")).unwrap();
        repo.upsert(
            "work",
            ProfileSettings {
                region: Region::Ru,
                ..ProfileSettings::default()
            },
        )
        .unwrap();
        let (loaded, region) = load_credentials(&repo, &store, "work", None).unwrap();
        assert_eq!(loaded, credentials("1"));
        assert_eq!(region, Region::Ru);

        // Клиент проекта: нет записи — подсказка `auth project add`, есть — его данные.
        let err = load_credentials(&repo, &store, "work", Some("100"))
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("auth project add --profile work --project-id 100"),
            "{err}"
        );
        assert!(load_credentials(&repo, &store, "work", Some("1/2")).is_err());
        store.set_project("work", "100", &credentials("2")).unwrap();
        let (loaded, region) = load_credentials(&repo, &store, "work", Some(" 100")).unwrap();
        assert_eq!(loaded, credentials("2"));
        assert_eq!(region, Region::Ru);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::testing::credentials;

    /// Сигналы из канала: обработчики сигналов процесса тестов не трогаются.
    struct ChannelSignals(mpsc::UnboundedReceiver<Forward>);

    impl SignalSource for ChannelSignals {
        async fn recv(&mut self) -> Option<Forward> {
            self.0.recv().await
        }
    }

    fn channel() -> (mpsc::UnboundedSender<Forward>, ChannelSignals) {
        let (tx, rx) = mpsc::unbounded_channel();
        (tx, ChannelSignals(rx))
    }

    fn strings(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    struct Run {
        code: i32,
        out: String,
        err: String,
    }

    /// Запуск во временном корне; после запуска в корне ничего не должно остаться.
    struct Runner {
        root: tempfile::TempDir,
        credentials: ApiClientCredentials,
        format: FileFormat,
        names: Vec<String>,
    }

    impl Runner {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(),
                credentials: credentials("7"),
                format: FileFormat::Json,
                names: path_env_names(&[]).unwrap(),
            }
        }

        fn secret(&self) -> String {
            self.credentials.client_secret().expose().to_owned()
        }

        fn id(&self) -> String {
            self.credentials.client_id().expose().to_owned()
        }

        async fn command(
            &self,
            command: &[OsString],
            signals: &mut ChannelSignals,
        ) -> anyhow::Result<Run> {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let result = exec_with_credentials(
                self.root.path(),
                &Exec {
                    credentials: &self.credentials,
                    region: Region::De,
                    format: self.format,
                    names: &self.names,
                },
                command,
                &mut out,
                &mut err,
                signals,
            )
            .await;
            assert!(
                fs::read_dir(self.root.path()).unwrap().next().is_none(),
                "the temporary credentials directory is left behind"
            );
            Ok(Run {
                code: result?,
                out: String::from_utf8(out).unwrap(),
                err: String::from_utf8(err).unwrap(),
            })
        }

        async fn sh(&self, script: &str) -> Run {
            let (_tx, mut signals) = channel();
            let run = self
                .command(&strings(&["sh", "-c", script]), &mut signals)
                .await
                .unwrap();
            for text in [&run.out, &run.err] {
                assert!(!text.contains(&self.secret()), "{text}");
                assert!(!text.contains(&self.id()), "{text}");
            }
            run
        }
    }

    #[tokio::test]
    async fn child_gets_env_and_private_file() {
        let runner = Runner::new();
        let run = runner
            .sh(r#"f="$APPGALLERY_CONNECT_CREDENTIALS_FILE"
echo "$f"
echo "region=$APPGALLERY_CONNECT_REGION"
ls -ld "$f" "$(dirname "$f")" | cut -c1-10
grep -c '"client_secret"' "$f""#)
            .await;
        assert_eq!(run.code, 0, "{}", run.err);
        let path = PathBuf::from(run.out.lines().next().unwrap());
        assert_eq!(path.file_name().unwrap(), "credentials.json");
        let dir = path
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy();
        assert!(dir.starts_with(TEMP_DIR_PREFIX), "{dir}");
        assert!(!path.exists());
        let text = run.out;
        assert!(text.contains("region=de\n"), "{text}");
        assert!(text.contains("-rw-------"), "{text}");
        assert!(text.contains("drwx------"), "{text}");
        assert!(text.ends_with("1\n"), "{text}");
    }

    #[tokio::test]
    async fn custom_env_names_replace_the_default() {
        let mut runner = Runner::new();
        runner.names = vec!["AGC_FILE".into()];
        let run = runner
            .sh(r#"test -f "$AGC_FILE" && echo file; echo "[${APPGALLERY_CONNECT_CREDENTIALS_FILE-unset}]""#)
            .await;
        assert_eq!(run.out, "file\n[unset]\n", "{}", run.err);
    }

    #[tokio::test]
    async fn child_reads_the_credentials_and_output_is_masked() {
        let runner = Runner::new();
        let script = r#"f="$APPGALLERY_CONNECT_CREDENTIALS_FILE"
cat "$f"; echo; grep client_id "$f" >&2
grep client_secret "$f"
base64 < "$f" | tr -d '\n'; echo
printf %s "$(sed -n 's/.*"client_secret": "\(.*\)".*/\1/p' "$f")" | base64 | tr -d '\n'; echo
echo "token eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.c2lnbmF0dXJl end"
wc -c < "$f"
env | grep -c HUAWEI_CLIENT"#;
        let run = runner.sh(script).await;
        assert_eq!(run.code, 1, "grep -c finds nothing: {}", run.err);
        let lines: Vec<&str> = run.out.lines().collect();
        assert_eq!(lines[0], "***", "{}", run.out);
        assert_eq!(lines[1], r#"  "client_secret": "***""#, "{}", run.out);
        assert!(lines[2].starts_with("***"), "{}", run.out);
        assert!(lines[3].starts_with("***"), "{}", run.out);
        assert_eq!(lines[4], "token *** end");
        let expected = FileFormat::Json.render(&runner.credentials).unwrap().len();
        assert_eq!(lines[5].trim(), expected.to_string());
        assert_eq!(lines[6].trim(), "0");
        assert_eq!(run.err, "  \"client_id\": \"***\",\n");
    }

    #[tokio::test]
    async fn dotenv_file_is_sourced_by_a_shell() {
        let mut runner = Runner::new();
        runner.format = FileFormat::Dotenv;
        let expected = format!("{}|{}", runner.id(), runner.secret());
        let run = runner
            .sh(&format!(
                r#"f="$APPGALLERY_CONNECT_CREDENTIALS_FILE"
basename "$f"
. "$f"
test "$HUAWEI_CLIENT_ID|$HUAWEI_CLIENT_SECRET" = '{expected}' && echo same
cat "$f""#
            ))
            .await;
        assert_eq!(run.code, 0, "{}", run.err);
        assert_eq!(run.out, "credentials.env\nsame\n***", "{}", run.err);
    }

    #[test]
    fn inherited_credential_variables_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let file = CredentialsFile::create(root.path(), "credentials.json", "{}").unwrap();
        let env = exec_env(&file, Region::Sg, &path_env_names(&[]).unwrap());
        assert_eq!(env.remove, CONTENT_ENVS);
        let set: Vec<(&str, &OsString)> = env.set.iter().map(|(n, v)| (n.as_str(), v)).collect();
        assert_eq!(
            set,
            [
                (DEFAULT_PATH_ENV, &file.path().as_os_str().to_os_string()),
                (REGION_ENV, &OsString::from("sg")),
            ]
        );

        let custom = exec_env(&file, Region::Cn, &["AGC_FILE".into()]);
        assert_eq!(
            custom.remove,
            ["HUAWEI_CLIENT_ID", "HUAWEI_CLIENT_SECRET", DEFAULT_PATH_ENV]
        );
        file.remove().unwrap();
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn file_is_removed_after_failure() {
        let runner = Runner::new();
        let run = runner
            .sh("cat \"$APPGALLERY_CONNECT_CREDENTIALS_FILE\" >&2; exit 7")
            .await;
        assert_eq!(run.code, 7);
        assert_eq!(run.err, "***");

        // Команда не запустилась: ошибка, файла тоже нет (это проверяет `Runner::command`).
        let (_tx, mut signals) = channel();
        let err = runner
            .command(&strings(&["/nonexistent/program"]), &mut signals)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("cannot run `/nonexistent/program`"), "{err}");

        let err = runner
            .command(&[], &mut signals)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("no command"), "{err}");
    }

    #[tokio::test]
    async fn exit_codes_are_preserved() {
        let runner = Runner::new();
        for code in [0, 1, 2, 42, 255] {
            let run = runner.sh(&format!("exit {code}")).await;
            assert_eq!(run.code, code);
        }
    }

    #[tokio::test]
    async fn signals_are_forwarded_and_file_is_removed() {
        let runner = Runner::new();
        let (tx, mut signals) = channel();
        let sender = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            tx.send(Forward::Terminate).unwrap();
            // Канал остаётся открытым, пока команда не завершится.
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let started = std::time::Instant::now();
        let run = runner
            .command(&strings(&["sh", "-c", "exec sleep 20"]), &mut signals)
            .await
            .unwrap();
        sender.abort();
        assert_eq!(run.code, 128 + 15);
        assert!(started.elapsed() < std::time::Duration::from_secs(10));

        // SIGHUP тоже пересылается.
        let (tx, mut signals) = channel();
        tx.send(Forward::Hangup).unwrap();
        let run = runner
            .command(&strings(&["sh", "-c", "exec sleep 20"]), &mut signals)
            .await
            .unwrap();
        assert_eq!(run.code, 128 + 1);

        // Закрытый источник сигналов не мешает дождаться команды.
        let (tx, mut signals) = channel();
        drop(tx);
        let run = runner
            .command(&strings(&["sh", "-c", "exit 3"]), &mut signals)
            .await
            .unwrap();
        assert_eq!(run.code, 3);
    }

    #[test]
    fn file_is_created_new_with_private_modes() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let contents = FileFormat::Json.render(&credentials("1")).unwrap();
        let file = CredentialsFile::create(root.path(), "credentials.json", &contents).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(file.path()), 0o600);
        assert_eq!(mode(file.path().parent().unwrap()), 0o700);
        assert_eq!(fs::read_to_string(file.path()).unwrap(), contents.as_str());
        // Второй файл — в своём каталоге, первый не перезаписывается.
        let second = CredentialsFile::create(root.path(), "credentials.json", &contents).unwrap();
        assert_ne!(second.path(), file.path());
        file.remove().unwrap();
        second.remove().unwrap();
        assert!(fs::read_dir(root.path()).unwrap().next().is_none());
    }
}
