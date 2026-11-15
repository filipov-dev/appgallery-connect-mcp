//! `appgallery-connect-mcp serve` — запуск MCP-сервера через stdio.
//!
//! stdout занят протоколом MCP, всё остальное (логи, ошибки запуска) — только в stderr.
//! Ошибка запуска (нет учётных данных, хранилище недоступно, неизвестный набор) возвращается
//! до начала протокола: в stdout ничего не пишется, код выхода ненулевой.
//!
//! Уровень доступа и наборы: флаги командной строки, если заданы, иначе настройки профиля,
//! иначе только чтение и наборы по умолчанию. Учётные данные API-клиента — только из
//! системного хранилища по профилю; access token выпускается лениво, при первом запросе к
//! API, поэтому запуск и `initialize`/`tools/list` обходятся без сети.
//!
//! `--dynamic-toolsets` (или `dynamic_toolsets` профиля) включает наборы по запросу: при
//! запуске включены наборы из `--toolsets` или профиля, иначе ни одного. Пользовательские
//! инструменты — из `tools.toml` рядом с файлом профилей и из `--tools-file`; они
//! загружаются и проверяются до начала протокола ([`crate::cli::tools`]). Фильтры `jq`
//! выполняются в дочернем процессе этого же бинарника ([`crate::cli::jq_worker`]).
//!
//! `--allow-path` добавляет каталог к разрешённым для локальных файлов (`save_to`) вместе с
//! рабочим и временным каталогом; несуществующий каталог — ошибка запуска. Секреты
//! составных инструментов — в отдельном сервисе системного хранилища (`KeyringVault`).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use appgallery_connect_api::{
    AccessLevel, ApiClient, ClientConfig, CredentialStore, KEYRING_SERVICE, KeyringStore,
    KeyringVault,
};
use appgallery_connect_mcp_core::{
    ALL_TOOLSETS, AppGalleryServer, DEFAULT_PROFILE_NAME, ServerOptions, Services, ToolRegistry,
    access_from_flags, access_name, normalize_toolsets, server_name,
};
use clap::Args;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

use crate::cli::{ProfileArg, jq_worker, tools};
use crate::profiles::{
    ProfileRepo, ProfileSettings, describe_region, load_profile_with, validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};

/// Таймаут запроса к API по умолчанию, секунды.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Лимит ответа, возвращаемого агенту, по умолчанию: 256 KiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Уровень логов в stderr, если не задан ни `--log-level`, ни `RUST_LOG`.
const DEFAULT_LOG_LEVEL: &str = "warn";

#[derive(Debug, Clone, Args)]
#[command(after_help = "\
Access level and toolsets: flags given on the command line win; without them the settings \
saved for the profile are used. Without any, the server is read-only and exposes the default \
toolsets: apps, releases, reviews. --allow-write registers tools that change data (app \
information, store listings, package and release settings); --allow-destructive also registers \
tools whose effect reaches users or money or cannot be undone (submitting a version for \
review, phased releases and release time, public replies to reviews, products, prices and \
promotions, deleting data) and implies --allow-write. \
Use --toolsets all for every toolset. Tools above the access level or outside the enabled \
toolsets are not registered: calling them explains which flag or toolset enables them.

Dynamic toolsets: with --dynamic-toolsets (or `dynamic_toolsets = true` in the profile) the \
server starts with only `list_toolsets`, `enable_toolset` and `disable_toolset`, plus the \
toolsets of --toolsets or of the profile (without them, none); the agent enables the \
toolsets it needs, and the client is notified that the tool list changed. The access level \
does not change: enabled toolsets get only the tools the flags allow.

Custom tools: the file tools.toml next to profiles.toml \
(~/.config/appgallery-connect-mcp/tools.toml) is always read when it exists; --tools-file adds \
more files. Each custom tool calls one operation with fixed arguments and an optional `jq` \
filter, and has the access level and the toolset of that operation; see \
`appgallery-connect-mcp tools check --help` for the format. A broken file stops the server \
before it starts.

Credentials: the API client ID and secret are read only from the system keychain (service \
`dev.filipov.appgallery-connect-mcp`) by profile; store them with `appgallery-connect-mcp auth \
login`. They are never read from environment variables, files or command-line arguments. The \
region of the profile selects the API host. Access tokens are requested on the first API call \
and kept in memory only, so starting the server needs no network.

Responses: tools return the JSON response of the AppGallery Connect API. Huawei reports many \
errors with HTTP 200 and a non-zero result code; such responses are tool errors. Tools with a \
JSON response take an optional `jq` filter (jaq dialect), run in a separate process with an \
empty environment and a timeout. --max-response-bytes limits the text returned to the agent \
(after the filter). Values that look like credentials \
(client secrets, tokens, signed URLs) are replaced with \"***\" in every response and error, \
and a note says so. The app secret is never returned; the app configuration file and \
temporary links to reports, certificates and profiles are hidden, and those tools take \
save_to: the server saves the file and returns its path and size.

Local files: save_to accepts only new files inside the working directory of the server, the \
temporary directory and the directories given with --allow-path; existing files are never \
overwritten. Files are downloaded only from Huawei hosts.

Logs go to stderr only (stdout carries the MCP protocol). The level comes from --log-level, \
otherwise from RUST_LOG, otherwise `warn`.")]
pub struct ServeArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Register tools that create or change data.
    #[arg(long)]
    pub allow_write: bool,
    /// Register tools whose effect reaches users or money or cannot be undone: submitting for
    /// review, releases, replies to reviews, prices, deleting data (implies --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable, or `all` [default: apps,releases,reviews].
    /// With --dynamic-toolsets: the toolsets enabled at start [default: none].
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Load toolsets on demand: start with only the toolset tools (and the toolsets of
    /// --toolsets), and let the agent enable toolsets with `enable_toolset`.
    #[arg(long)]
    pub dynamic_toolsets: bool,
    /// Additional custom tools file (same format as
    /// ~/.config/appgallery-connect-mcp/tools.toml). Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
    /// Extra directory that tools may read files from or save files to (`save_to`), besides
    /// the working directory and the temporary directory. Repeatable; must exist.
    #[arg(long, value_name = "DIR")]
    pub allow_path: Vec<PathBuf>,
    /// Timeout of one API request, in seconds.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
    /// Maximum size of an API response and of a tool result returned to the agent; larger
    /// results are an error.
    #[arg(long, value_name = "BYTES", default_value_t = DEFAULT_MAX_RESPONSE_BYTES)]
    pub max_response_bytes: usize,
    /// Log level or filter for stderr, e.g. `info` or `appgallery_connect_mcp_core=debug`.
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,
    /// Print the effective profile, access level and toolsets, then exit without starting
    /// the server (no network).
    #[arg(long)]
    pub status: bool,
}

impl ServeArgs {
    fn profile(&self) -> &str {
        &self.profile.profile
    }

    /// Проверка аргументов до обращения к профилю и хранилищу.
    fn validate(&self) -> anyhow::Result<()> {
        if self.timeout == 0 {
            bail!("--timeout must be greater than 0");
        }
        if self.max_response_bytes == 0 {
            bail!("--max-response-bytes must be greater than 0");
        }
        // Неизвестный набор в флагах — ошибка до обращения к хранилищу учётных данных.
        normalize_toolsets(&self.toolsets).map_err(|err| anyhow!(err))?;
        validate_profile_name(self.profile())
    }
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    init_logging(args.log_level.as_deref())?;
    args.validate()?;
    let repo = ProfileRepo::open_default()?;
    if args.status {
        return status(&KeyringStore, &repo, &args, &mut TerminalUi);
    }
    let setup = Setup::load(&args, &KeyringStore, &repo)?;
    tracing::info!(
        profile = %args.profile(),
        access = ?setup.options.max_access,
        toolsets = ?setup.options.toolsets,
        dynamic_toolsets = setup.options.dynamic_toolsets,
        custom_tools = setup.options.custom_tools.len(),
        "запуск MCP-сервера AppGallery Connect"
    );
    let server = setup.server()?;
    let running = server
        .serve(stdio())
        .await
        .context("MCP initialization over stdio failed")?;
    running.waiting().await.context("MCP server task failed")?;
    Ok(())
}

/// Логи — только в stderr: stdout занят протоколом MCP.
fn init_logging(level: Option<&str>) -> anyhow::Result<()> {
    let filter = match level {
        Some(level) => EnvFilter::try_new(level)
            .map_err(|err| anyhow!("invalid --log-level `{level}`: {err}"))?,
        None => {
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_LEVEL))
        }
    };
    // Повторная инициализация (тесты) не ошибка: остаётся первый подписчик.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(filter)
        .try_init();
    Ok(())
}

/// Всё, что нужно для запуска сервера: настройки ядра и клиента API.
pub struct Setup {
    pub options: ServerOptions,
    pub config: ClientConfig,
}

impl Setup {
    /// Читает профиль (учётные данные — из `store`) и пользовательские инструменты и
    /// собирает настройки сервера. Ошибка в `tools.toml` — ошибка запуска.
    pub fn load(
        args: &ServeArgs,
        store: &dyn CredentialStore,
        repo: &ProfileRepo,
    ) -> anyhow::Result<Self> {
        args.validate()?;
        let custom_tools = tools::load(Some(&tools::user_tools_file(repo)), &args.tools_file)?;
        let profile_name = args.profile();
        let profile = load_profile_with(repo, store, profile_name)
            .with_context(|| format!("cannot load profile `{profile_name}`"))?;
        let mut options = effective_options(args, &profile.settings)?;
        options.custom_tools = custom_tools;
        options.jq = jq_worker::current_runner();
        let mut config = profile.client_config();
        config.timeout = Duration::from_secs(args.timeout);
        config.max_response_bytes = args.max_response_bytes;
        Ok(Self { options, config })
    }

    pub fn server(self) -> anyhow::Result<AppGalleryServer> {
        let api = ApiClient::new(self.config)
            .map_err(|err| anyhow!("cannot create the AppGallery Connect API client: {err}"))?;
        // Секреты составных инструментов — в отдельном сервисе системного хранилища,
        // записи профиля.
        let profile = self
            .options
            .profile
            .clone()
            .unwrap_or_else(|| DEFAULT_PROFILE_NAME.to_owned());
        let services = Services::new(api).with_vault(Arc::new(KeyringVault::new(profile)));
        AppGalleryServer::new(services, self.options).map_err(|err| anyhow!(err))
    }
}

/// Откуда взята настройка.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Flags,
    Profile,
    Default,
}

impl Source {
    fn label(self) -> &'static str {
        match self {
            Self::Flags => "from command-line flags",
            Self::Profile => "from profile settings",
            Self::Default => "default",
        }
    }
}

/// Уровень доступа: флаги командной строки, если заданы, иначе настройки профиля.
/// В обоих случаях `allow_destructive` подразумевает запись.
fn effective_access(args: &ServeArgs, settings: &ProfileSettings) -> (AccessLevel, Source) {
    if args.allow_write || args.allow_destructive {
        (
            access_from_flags(args.allow_write, args.allow_destructive),
            Source::Flags,
        )
    } else {
        let level = access_from_flags(settings.allow_write, settings.allow_destructive);
        let source = if level == AccessLevel::Read {
            Source::Default
        } else {
            Source::Profile
        };
        (level, source)
    }
}

/// Наборы: `--toolsets`, если задан, иначе из профиля; пусто — наборы по умолчанию (`None`).
fn effective_toolsets(
    args: &ServeArgs,
    settings: &ProfileSettings,
) -> anyhow::Result<(Option<Vec<String>>, Source)> {
    let cli = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
    if !cli.is_empty() {
        return Ok((Some(cli), Source::Flags));
    }
    let saved =
        normalize_toolsets(&settings.toolsets).map_err(|err| anyhow!("profile settings: {err}"))?;
    Ok(if saved.is_empty() {
        (None, Source::Default)
    } else {
        (Some(saved), Source::Profile)
    })
}

fn effective_options(
    args: &ServeArgs,
    settings: &ProfileSettings,
) -> anyhow::Result<ServerOptions> {
    let (max_access, _) = effective_access(args, settings);
    let (toolsets, _) = effective_toolsets(args, settings)?;
    Ok(ServerOptions {
        max_access,
        toolsets,
        dynamic_toolsets: args.dynamic_toolsets || settings.dynamic_toolsets,
        allowed_paths: args.allow_path.clone(),
        profile: Some(args.profile().to_owned()),
        apps: settings.apps.clone(),
        packages: settings.packages.clone(),
        ..ServerOptions::default()
    })
}

/// `serve --status`: действующие настройки сервера без запуска и без запросов к API.
/// Учётные данные не выводятся: печатается только, сохранены ли они.
pub fn status(
    store: &dyn CredentialStore,
    repo: &ProfileRepo,
    args: &ServeArgs,
    ui: &mut dyn Ui,
) -> anyhow::Result<()> {
    args.validate()?;
    let profile = args.profile();
    let settings = repo.get(profile)?.unwrap_or_default();
    let has_credentials = store.contains(profile)?;
    let (access, access_source) = effective_access(args, &settings);
    let (_, toolsets_source) = effective_toolsets(args, &settings)?;
    let tools_file = tools::user_tools_file(repo);
    let options = ServerOptions {
        custom_tools: tools::load(Some(&tools_file), &args.tools_file)?,
        ..effective_options(args, &settings)?
    };
    let registry = ToolRegistry::new(&options).map_err(|err| anyhow!(err))?;

    ui.say(&format!("Profile:     {profile}"));
    ui.say(&format!("Server name: {}", server_name(profile)));
    let credentials = if has_credentials {
        format!("stored in the system keychain (service `{KEYRING_SERVICE}`)")
    } else {
        format!(
            "not found; store them with `appgallery-connect-mcp auth login --profile {profile}`"
        )
    };
    ui.say(&format!("Credentials: {credentials}"));
    ui.say(&format!(
        "Region:      {}",
        describe_region(settings.region)
    ));
    ui.say(&format!(
        "Access:      {} ({}; {})",
        access_name(access),
        access_description(access),
        access_source.label()
    ));
    let enabled = registry.enabled_toolsets();
    let toolsets_text = if enabled.len() == appgallery_connect_api::spec::toolsets().len() {
        ALL_TOOLSETS.to_owned()
    } else if enabled.is_empty() {
        "none".to_owned()
    } else {
        enabled.join(",")
    };
    ui.say(&format!(
        "Toolsets:    {toolsets_text} ({})",
        toolsets_source.label()
    ));
    // В динамическом режиме это наборы при запуске (без выбора — ни одного), остальные
    // агент включает сам.
    let dynamic = match (args.dynamic_toolsets, settings.dynamic_toolsets) {
        (true, _) => format!("on ({})", Source::Flags.label()),
        (false, true) => format!("on ({})", Source::Profile.label()),
        (false, false) => "off".to_owned(),
    };
    ui.say(&format!("Dynamic:     {dynamic}"));
    ui.say(&format!("Tools:       {}", registry.len()));
    let custom_registered = options
        .custom_tools
        .iter()
        .filter(|tool| registry.get(&tool.name).is_some())
        .count();
    ui.say(&format!(
        "Custom:      {} defined, {custom_registered} registered (user file: {}{})",
        options.custom_tools.len(),
        tools_file.display(),
        if tools_file.exists() {
            ""
        } else {
            ", not found"
        }
    ));
    ui.say(&format!("Apps:        {}", list_or_none(&settings.apps)));
    ui.say(&format!(
        "Packages:    {}",
        list_or_none(&settings.packages)
    ));
    if !args.allow_path.is_empty() {
        let paths: Vec<String> = args
            .allow_path
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        ui.say(&format!(
            "Files:       working directory, temporary directory, {}",
            paths.join(", ")
        ));
    }
    ui.say(&format!("Profiles:    {}", repo.path().display()));
    if !has_credentials {
        ui.say("The server cannot start until the profile has credentials.");
    }
    Ok(())
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

fn access_description(access: AccessLevel) -> &'static str {
    match access {
        AccessLevel::Read => "read tools only",
        AccessLevel::Write => "read and write tools, nothing that reaches users, money or deletes",
        AccessLevel::Destructive => "read, write, release, pricing and delete tools",
    }
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::{MemoryStore, Region};
    use rmcp::model::CallToolRequestParams;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::testing::{client_id, credentials};
    use crate::ui::testing::ScriptedUi;

    #[derive(Debug, clap::Parser)]
    struct Wrapper {
        #[command(flatten)]
        serve: ServeArgs,
    }

    fn try_parse(args: &[&str]) -> Result<ServeArgs, clap::Error> {
        use clap::Parser;
        let mut argv = vec!["serve"];
        argv.extend_from_slice(args);
        Wrapper::try_parse_from(argv).map(|w| w.serve)
    }

    fn parse(args: &[&str]) -> ServeArgs {
        try_parse(args).unwrap()
    }

    fn settings(write: bool, destructive: bool, toolsets: &[&str]) -> ProfileSettings {
        ProfileSettings {
            region: Region::Ru,
            apps: vec!["100000001".into()],
            packages: vec!["com.example.app".into()],
            allow_write: write,
            allow_destructive: destructive,
            toolsets: toolsets.iter().map(|s| (*s).to_owned()).collect(),
            ..ProfileSettings::default()
        }
    }

    fn options(args: &[&str], saved: &ProfileSettings) -> ServerOptions {
        effective_options(&parse(args), saved).unwrap()
    }

    /// Временный каталог профилей и хранилище в памяти: настоящий keychain не трогаем.
    fn fixture(
        settings: Option<ProfileSettings>,
        with_credentials: bool,
    ) -> (tempfile::TempDir, ProfileRepo, MemoryStore) {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path().join("config"));
        if let Some(settings) = settings {
            repo.upsert("ci", settings).unwrap();
        }
        let store = MemoryStore::new();
        if with_credentials {
            store.set("ci", &credentials("serve")).unwrap();
        }
        (dir, repo, store)
    }

    fn status_text(args: &[&str], repo: &ProfileRepo, store: &MemoryStore) -> String {
        let mut ui = ScriptedUi::default();
        status(store, repo, &parse(args), &mut ui).unwrap();
        ui.text()
    }

    #[test]
    fn defaults() {
        let args = parse(&[]);
        assert_eq!(args.profile(), "default");
        assert_eq!(args.timeout, DEFAULT_TIMEOUT_SECS);
        assert_eq!(args.max_response_bytes, DEFAULT_MAX_RESPONSE_BYTES);
        let options = options(&[], &ProfileSettings::default());
        assert_eq!(options.max_access, AccessLevel::Read);
        assert_eq!(options.toolsets, None);
        assert!(!options.dynamic_toolsets && options.allowed_paths.is_empty());
        assert!(parse(&["--timeout", "0"]).validate().is_err());
        assert!(parse(&["--max-response-bytes", "0"]).validate().is_err());
        assert!(parse(&["--profile", "bad name"]).validate().is_err());
        let err = parse(&["--toolsets", "nope"]).validate().unwrap_err();
        assert!(err.to_string().contains("unknown toolset `nope`"), "{err}");
    }

    #[test]
    fn cli_flags_win_over_profile() {
        let saved = settings(true, false, &["listing"]);
        let from_profile = options(&[], &saved);
        assert_eq!(from_profile.max_access, AccessLevel::Write);
        assert_eq!(from_profile.toolsets, Some(vec!["listing".to_owned()]));
        assert_eq!(from_profile.apps, ["100000001"]);
        assert_eq!(from_profile.packages, ["com.example.app"]);
        assert_eq!(from_profile.profile.as_deref(), Some("default"));

        let flags = options(
            &["--allow-destructive", "--toolsets", "apps, reviews"],
            &saved,
        );
        assert_eq!(flags.max_access, AccessLevel::Destructive);
        assert_eq!(
            flags.toolsets,
            Some(vec!["apps".to_owned(), "reviews".to_owned()])
        );
        // `--allow-write` при `allow_destructive` в профиле — флаг побеждает.
        let saved = settings(false, true, &[]);
        assert_eq!(
            options(&["--allow-write"], &saved).max_access,
            AccessLevel::Write
        );
        assert_eq!(options(&[], &saved).max_access, AccessLevel::Destructive);

        let all = options(&["--toolsets", "all"], &saved);
        assert_eq!(
            all.toolsets.unwrap().len(),
            appgallery_connect_api::spec::toolsets().len()
        );
        let err = effective_options(&parse(&["--toolsets", "nope"]), &saved).unwrap_err();
        assert!(err.to_string().contains("unknown toolset `nope`"), "{err}");
        let broken = settings(false, false, &["nope"]);
        let err = effective_options(&parse(&[]), &broken).unwrap_err();
        assert!(err.to_string().contains("profile settings"), "{err}");
    }

    #[test]
    fn dynamic_toolsets_from_flag_or_profile() {
        let args = [
            "--profile",
            "ci",
            "--dynamic-toolsets",
            "--allow-path",
            "builds",
            "--allow-path",
            "reports",
        ];
        let saved = settings(false, false, &[]);
        let with = options(&args, &saved);
        assert!(with.dynamic_toolsets);
        assert_eq!(
            with.allowed_paths,
            [PathBuf::from("builds"), PathBuf::from("reports")]
        );
        assert_eq!(with.toolsets, None);
        let from_profile = options(
            &[],
            &ProfileSettings {
                dynamic_toolsets: true,
                ..saved.clone()
            },
        );
        assert!(from_profile.dynamic_toolsets);

        // Без выбора наборов динамический сервер стартует без наборов, с выбором — с ними
        // (флаг или профиль).
        let names = |opts: &ServerOptions| -> Vec<String> {
            let mut names: Vec<String> = ToolRegistry::new(opts)
                .unwrap()
                .iter()
                .map(|tool| tool.tool.name.to_string())
                .collect();
            names.sort();
            names
        };
        assert_eq!(
            names(&with),
            [
                "api_request",
                "api_search",
                "disable_toolset",
                "enable_toolset",
                "list_toolsets"
            ]
        );
        let reviews = options(&["--dynamic-toolsets", "--toolsets", "reviews"], &saved);
        assert!(names(&reviews).iter().any(|n| n == "list_reviews"));
        let saved_reviews = ProfileSettings {
            dynamic_toolsets: true,
            ..settings(false, false, &["reviews"])
        };
        let from_profile = options(&[], &saved_reviews);
        assert_eq!(names(&from_profile), names(&reviews));
        let static_names = names(&options(&["--profile", "ci"], &saved));
        assert!(static_names.iter().any(|n| n == "get_app_info"));
        assert!(!static_names.iter().any(|n| n == "enable_toolset"));

        let (_dir, repo, store) = fixture(Some(saved), true);
        let text = status_text(&args, &repo, &store);
        assert!(
            text.contains("Dynamic:     on (from command-line flags)"),
            "{text}"
        );
        assert!(text.contains("Toolsets:    none (default)"), "{text}");
        assert!(text.contains("Tools:       5"), "{text}");
        assert!(
            text.contains("Files:       working directory, temporary directory, builds, reports"),
            "{text}"
        );
    }

    #[test]
    fn custom_tools_come_from_the_config_dir_and_tools_file() {
        let (dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let user_file = tools::user_tools_file(&repo);
        std::fs::create_dir_all(user_file.parent().unwrap()).unwrap();
        std::fs::write(&user_file, tools::tests::SAMPLE).unwrap();
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains("Custom:      2 defined, 1 registered (user file: "),
            "{text}"
        );
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        assert_eq!(setup.options.custom_tools.len(), 2);

        // Ошибка в файле — ошибка запуска с именем файла.
        let broken = dir.path().join("broken.toml");
        std::fs::write(
            &broken,
            "[[tools]]\nname = \"x\"\ndescription = \"d\"\noperation = \"nope\"\n",
        )
        .unwrap();
        let broken_arg = broken.display().to_string();
        let args = parse(&["--profile", "ci", "--tools-file", &broken_arg]);
        let err = Setup::load(&args, &store, &repo).err().unwrap();
        let text = format!("{err:#}");
        assert!(
            text.contains("broken.toml: tool `x`: unknown operation"),
            "{text}"
        );
        let mut ui = ScriptedUi::default();
        assert!(status(&store, &repo, &args, &mut ui).is_err());
    }

    #[test]
    fn status_shows_settings_without_credential_values() {
        let (_dir, repo, store) = fixture(Some(settings(false, true, &[])), true);
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("Profile:     ci"), "{text}");
        assert!(text.contains("Server name: agc-ci"), "{text}");
        assert!(text.contains("stored in the system keychain"), "{text}");
        assert!(text.contains("Region:      "), "{text}");
        assert!(text.contains("Access:      destructive"), "{text}");
        assert!(text.contains("from profile settings"), "{text}");
        assert!(text.contains("Dynamic:     off"), "{text}");
        assert!(text.contains("Custom:      0 defined"), "{text}");
        assert!(text.contains("not found)"), "{text}");
        assert!(text.contains("Apps:        100000001"), "{text}");
        assert!(text.contains("Packages:    com.example.app"), "{text}");
        assert!(!text.contains("Files:"), "{text}");
        assert!(!text.contains("cannot start"), "{text}");
        assert!(!text.contains(&client_id("serve")), "{text}");

        let (_dir, repo, store) = fixture(None, false);
        let text = status_text(&["--profile", "ci", "--toolsets", "all"], &repo, &store);
        assert!(text.contains("Credentials: not found"), "{text}");
        assert!(text.contains("auth login --profile ci"), "{text}");
        assert!(text.contains("Toolsets:    all"), "{text}");
        assert!(text.contains("Access:      read"), "{text}");
        assert!(text.contains("cannot start"), "{text}");
    }

    #[test]
    fn setup_requires_credentials() {
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), false);
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo)
            .err()
            .unwrap();
        let text = format!("{err:#}");
        assert!(text.contains("cannot load profile `ci`"), "{text}");
        assert!(text.contains("auth login --profile ci"), "{text}");

        let (_dir, repo, store) = fixture(None, true);
        let setup = Setup::load(
            &parse(&[
                "--profile",
                "ci",
                "--timeout",
                "5",
                "--max-response-bytes",
                "1000",
            ]),
            &store,
            &repo,
        )
        .unwrap();
        assert_eq!(setup.config.timeout, Duration::from_secs(5));
        assert_eq!(setup.config.max_response_bytes, 1000);
    }

    #[tokio::test]
    async fn serves_tools_from_profile() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth2/v1/token"))
            .and(body_partial_json(json!({
                "grant_type": "client_credentials",
                "client_id": client_id("serve"),
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "served-token-0123456789abcdef",
                "expires_in": 172800
            })))
            .expect(1)
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/publish/v2/app-info"))
            .and(query_param("appId", "100000001"))
            .and(header("client_id", client_id("serve").as_str()))
            .and(header(
                "authorization",
                "Bearer served-token-0123456789abcdef",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 0, "msg": "success"},
                "appInfo": {"defaultLang": "en-US", "releaseState": 0}
            })))
            .expect(1)
            .mount(&api)
            .await;

        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), true);
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        // Запуск не обращается к сети: токен ещё не запрошен.
        assert_eq!(api.received_requests().await.unwrap().len(), 0);
        setup.config.base_url = Some(api.uri());
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();

        let info = client.peer_info().unwrap();
        assert_eq!(
            info.server_info.as_ref().map(|s| s.name.as_str()),
            Some("agc-ci")
        );
        let instructions = info.instructions.as_deref().unwrap();
        assert!(
            instructions.contains("auth login --profile ci"),
            "{instructions}"
        );
        assert!(instructions.contains("`100000001`"), "{instructions}");
        let tools = client.list_all_tools().await.unwrap();
        assert!(tools.iter().any(|t| t.name == "get_app_info"));
        assert!(
            tools
                .iter()
                .all(|t| t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true))
        );

        let result = client
            .call_tool(
                CallToolRequestParams::new("get_app_info")
                    .with_arguments(json!({"appId": "100000001"}).as_object().unwrap().clone()),
            )
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains("en-US"), "{text}");
        assert!(!text.contains("served-token"), "{text}");

        client.cancel().await.unwrap();
        handle.await.unwrap();
    }

    /// Клиент, который ждёт `notifications/tools/list_changed`.
    #[derive(Clone, Default)]
    struct ListChanged(std::sync::Arc<tokio::sync::Notify>);

    impl rmcp::ClientHandler for ListChanged {
        async fn on_tool_list_changed(
            &self,
            _context: rmcp::service::NotificationContext<rmcp::RoleClient>,
        ) {
            self.0.notify_one();
        }
    }

    #[tokio::test]
    async fn dynamic_session_enables_toolsets_with_custom_tools_and_jq() {
        let api = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/oauth2/v1/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "dynamic-token-0123456789abcdef",
                "expires_in": 172800
            })))
            .mount(&api)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/publish/v2/app-info"))
            .and(query_param("appId", "100000001"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 0, "msg": "success"},
                "languages": [
                    {"lang": "en-US", "appName": "Demo"},
                    {"lang": "ru-RU", "appName": "Demo RU"}
                ]
            })))
            .expect(2)
            .mount(&api)
            .await;

        let (_dir, repo, store) = fixture(
            Some(ProfileSettings {
                dynamic_toolsets: true,
                ..settings(false, false, &[])
            }),
            true,
        );
        let user_file = tools::user_tools_file(&repo);
        std::fs::create_dir_all(user_file.parent().unwrap()).unwrap();
        std::fs::write(&user_file, tools::tests::SAMPLE).unwrap();
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo).unwrap();
        assert!(setup.options.dynamic_toolsets);
        setup.config.base_url = Some(api.uri());
        // Бинарника с `jq-worker` у модульного теста нет: фильтр — в потоке. Отдельный
        // процесс проверяет `tests/jq_worker.rs` на настоящем бинарнике.
        setup.options.jq = appgallery_connect_mcp_core::JqRunner::in_process();
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let handler = ListChanged::default();
        let client = handler.clone().serve(client_io).await.unwrap();
        let names = |tools: Vec<rmcp::model::Tool>| -> Vec<String> {
            let mut names: Vec<String> = tools.into_iter().map(|t| t.name.to_string()).collect();
            names.sort();
            names
        };
        assert_eq!(
            names(client.list_all_tools().await.unwrap()),
            [
                "api_request",
                "api_search",
                "disable_toolset",
                "enable_toolset",
                "list_toolsets"
            ]
        );
        let call = |name: &str, args: serde_json::Value| {
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(args.as_object().unwrap().clone())
        };
        let result = client
            .call_tool(call("enable_toolset", json!({"toolset": "apps,listing"})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        tokio::time::timeout(Duration::from_secs(5), handler.0.notified())
            .await
            .unwrap();
        let tools = names(client.list_all_tools().await.unwrap());
        assert!(tools.iter().any(|n| n == "get_app_info"), "{tools:?}");
        // Пользовательский инструмент набора `apps` включился вместе с ним; инструменты
        // записи (`listing`, `rename_main_app`) — нет: уровень доступа не меняется.
        assert!(tools.iter().any(|n| n == "main_app_languages"), "{tools:?}");
        assert!(!tools.iter().any(|n| n == "update_app_info"), "{tools:?}");
        assert!(!tools.iter().any(|n| n == "rename_main_app"), "{tools:?}");

        let result = client
            .call_tool(call(
                "get_app_info",
                json!({"appId": "100000001", "jq": "[.languages[].lang]"}),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains(r#"[\"en-US\",\"ru-RU\"]"#), "{text}");
        let result = client
            .call_tool(call("main_app_languages", json!({"jq": ".[1].appName"})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains("Demo RU"), "{text}");

        client.cancel().await.unwrap();
        handle.await.unwrap();
    }
}
