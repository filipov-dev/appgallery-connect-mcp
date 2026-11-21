//! `appgallery-connect-mcp serve` — запуск MCP-сервера через stdio или HTTP (`--http`,
//! [`http_server`]: учётные данные на стороне сервера, клиенты приходят с токенами доступа).
//!
//! stdout занят протоколом MCP, всё остальное (логи, ошибки запуска) — только в stderr.
//! Ошибка запуска (нет учётных данных, хранилище недоступно, неизвестный набор) возвращается
//! до начала протокола: в stdout ничего не пишется, код выхода ненулевой.
//!
//! Права (уровень доступа и каталоги `allow_path`) — из настроек профиля; флаги командной
//! строки их только сужают, на macOS права сверх чтения ещё и должны быть одобрены
//! владельцем ([`mcp_kit::approval`]). Наборы: флаги, иначе профиль, иначе по умолчанию.
//! Учётные данные API-клиента — только из
//! системного хранилища по профилю; access token выпускается лениво, при первом запросе к
//! API, поэтому запуск и `initialize`/`tools/list` обходятся без сети.
//!
//! `--dynamic-toolsets` (или `dynamic_toolsets` профиля) включает наборы по запросу: при
//! запуске включены наборы из `--toolsets` или профиля, иначе ни одного. Пользовательские
//! инструменты — из `tools.toml` рядом с файлом профилей и из `--tools-file`; они
//! загружаются и проверяются до начала протокола ([`crate::cli::tools`]). Фильтры `jq`
//! выполняются в дочернем процессе этого же бинарника ([`crate::cli::jq_worker`]).
//!
//! Каталоги `allow_path` профиля (после сужения `--allow-path`) разрешены для локальных
//! файлов (`save_to`) вместе с рабочим и временным каталогом; несуществующий каталог —
//! ошибка запуска. Секреты
//! составных инструментов — в отдельном сервисе системного хранилища (`KeyringVault`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use appgallery_connect_api::{
    AccessLevel, ApiClient, ClientConfig, CredentialStore, KEYRING_SERVICE, KeyringStore,
    KeyringVault, Region, oauth_account,
};
use appgallery_connect_mcp_core::{
    ALL_TOOLSETS, AppGalleryServer, DEFAULT_PROFILE_NAME, ServerOptions, Services, ToolRegistry,
    access_name, normalize_toolsets, server_name,
};
use clap::Args;
use mcp_kit::approval::{self, ApprovalState, ApprovalStore, Resolved};
use mcp_kit::{AccessRequest, ProfileAccess};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

use crate::cli::{ProfileArg, jq_worker, tools};
use crate::clients::EXE_STEM;
use crate::profiles::{
    ProfileKind, ProfileRepo, ProfileSettings, describe_region, load_app_secrets,
    load_profile_with, load_projects, parse_region, save_rotated_sign_in, validate_profile_name,
};
use crate::ui::{TerminalUi, Ui};

mod access_tokens;
mod http_server;
mod secret_file;

/// Таймаут запроса к API по умолчанию, секунды.
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;
/// Лимит ответа, возвращаемого агенту, по умолчанию: 256 KiB.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 256 * 1024;
/// Уровень логов в stderr, если не задан ни `--log-level`, ни `RUST_LOG`.
const DEFAULT_LOG_LEVEL: &str = "warn";

#[derive(Debug, Clone, Args)]
#[command(after_help = "\
Access: the access level and the extra directories for local files (allow_path) are settings \
of the profile; change them with `appgallery-connect-mcp ui`. Without them the server is \
read-only. The write level registers tools that change data (app information, store listings, \
package and release settings); the destructive level also registers tools whose effect reaches \
users or money or cannot be undone (submitting a version for review, phased releases and \
release time, public replies to reviews, products, prices and promotions, deleting data). \
--allow-write, --allow-destructive and --allow-path can only narrow the access of the profile: \
asking for more than the profile allows is an error. On macOS, access above read-only takes \
effect only after the owner approved it on this Mac (`appgallery-connect-mcp ui` asks for \
Touch ID or the macOS password); without the approval the server runs read-only and says so \
in stderr. --allow-destructive implies --allow-write.

Toolsets: --toolsets wins; without it the toolsets saved for the profile are used, otherwise \
the default toolsets: apps, releases, reviews. Use --toolsets all for every toolset. Tools \
above the access level or outside the enabled toolsets are not registered: calling them \
explains how to enable them.

Dynamic toolsets: with --dynamic-toolsets (or `dynamic_toolsets = true` in the profile) the \
server starts with only `list_toolsets`, `enable_toolset` and `disable_toolset`, plus the \
toolsets of --toolsets or of the profile (without them, none); the agent enables the \
toolsets it needs, and the client is notified that the tool list changed. The access level \
does not change: enabled toolsets get only the tools the access allows.

Custom tools: the file tools.toml next to profiles.toml \
(~/.config/appgallery-connect-mcp/tools.toml) is always read when it exists; --tools-file adds \
more files. Each custom tool calls one operation with fixed arguments and an optional `jq` \
filter, and has the access level and the toolset of that operation; see \
`appgallery-connect-mcp tools check --help` for the format. A broken file stops the server \
before it starts.

Credentials: the API client ID and secret are read only from the system keychain (service \
`dev.filipov.appgallery-connect-mcp`) by profile; store them with `appgallery-connect-mcp auth \
login`. A profile signed in with `auth login --oauth` works as that HUAWEI ID user in the team \
of the profile instead: its OAuth client and refresh token are read from the same keychain. \
They are never read from environment variables, files or command-line arguments (the only \
exception is --secret-file of HTTP mode, below). The \
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
temporary directory and the allow_path directories of the profile; existing files are never \
overwritten. Files are downloaded only from Huawei hosts.

Logs go to stderr only (stdout carries the MCP protocol). The level comes from --log-level, \
otherwise from RUST_LOG, otherwise `warn`.

HTTP mode (--http): the server listens on --listen (default 127.0.0.1:8769) and serves MCP at \
/mcp (Streamable HTTP, stateless, JSON responses) and a health check at /health. The API \
client credentials stay on the server: they come from the system keychain of --profile or, in \
a container, from --secret-file (a Docker or Kubernetes secret readable only by its owner, \
mode 0600 or 0400) with the client secret and --client-id, or with JSON {\"client_id\", \
\"client_secret\"}; --region sets the site (default: the region of the profile; required without a profile). \
Clients send an access token in `Authorization: Bearer <token>`; --access-tokens names a TOML \
file with the SHA-256 of every token and its access level and toolsets ([[tokens]] with name, \
sha256, access = read|write|destructive, toolsets). The access of the server is the access of \
the profile (approved on this Mac on macOS), narrowed by --allow-write, --allow-destructive and \
--allow-path, as without --http; with --secret-file it is set by those flags alone (read-only \
without them). A token's access cannot exceed the access of the server; tokens without \
toolsets get --toolsets. Requests without a known token get HTTP 401. File arguments work only \
with the directories of the server (allow_path of the profile, or --allow-path with \
--secret-file; relative paths start in the first one); without them tools that need a file are \
not registered. Custom tools come only from --tools-file. Requests must use a \
loopback Host, the --listen address or an --allowed-host; browser requests are rejected unless \
their Origin is allowed with --allowed-origin. Outside loopback, put the server behind a TLS \
reverse proxy.")]
pub struct ServeArgs {
    #[command(flatten)]
    pub profile: ProfileArg,
    /// Limit the access to write (the profile must allow it): tools that create or change
    /// data. With --secret-file: the access of the HTTP server.
    #[arg(long)]
    pub allow_write: bool,
    /// Limit the access to destructive (the profile must allow it; implies --allow-write):
    /// also tools whose effect reaches users or money or cannot be undone. With
    /// --secret-file: the access of the HTTP server.
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets to enable, or `all` [default: apps,releases,reviews].
    /// With --dynamic-toolsets: the toolsets enabled at start [default: none].
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Load toolsets on demand: start with only the toolset tools (and the toolsets of
    /// --toolsets), and let the agent enable toolsets with `enable_toolset`. Not available
    /// with --http.
    #[arg(long, conflicts_with = "http")]
    pub dynamic_toolsets: bool,
    /// Additional custom tools file (same format as
    /// ~/.config/appgallery-connect-mcp/tools.toml). Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
    /// Limit the extra directories of the profile to this one (it must be inside a directory
    /// of the profile). Tools may read files from or save files to (`save_to`) these
    /// directories besides the working directory and the temporary directory (with --http:
    /// instead of them). Repeatable; must exist.
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
    /// the server (no network). Not available with --http.
    #[arg(long, conflicts_with = "http")]
    pub status: bool,
    /// Serve MCP over HTTP instead of stdio; clients authenticate with access tokens and the
    /// API client credentials stay on the server.
    #[arg(long)]
    pub http: bool,
    /// Address and port to listen on in HTTP mode [default: 127.0.0.1:8769].
    #[arg(long, value_name = "IP:PORT", requires = "http")]
    pub listen: Option<SocketAddr>,
    /// HTTP mode: TOML file with the SHA-256 hashes, access levels and toolsets of the
    /// clients' access tokens. Required with --http.
    #[arg(long, value_name = "PATH", requires = "http")]
    pub access_tokens: Option<PathBuf>,
    /// HTTP mode: read the API client secret from this file (a Docker or Kubernetes secret,
    /// mode 0600 or 0400) instead of the system keychain of the profile. The file holds the
    /// secret alone (then pass --client-id) or JSON with client_id and client_secret.
    #[arg(long, value_name = "PATH", requires = "http")]
    pub secret_file: Option<PathBuf>,
    /// HTTP mode: the API client ID for a --secret-file that holds only the secret.
    #[arg(long, value_name = "ID", requires = "secret_file")]
    pub client_id: Option<String>,
    /// HTTP mode: AppGallery Connect site of the API client: cn, de, sg or ru [default: the
    /// region of the profile; required with --secret-file and no saved profile].
    #[arg(long, value_name = "REGION", requires = "http", value_parser = parse_region)]
    pub region: Option<Region>,
    /// Extra allowed Host header value in HTTP mode: a name (any port) or name:port.
    /// Repeatable. Loopback names and the --listen address are always allowed.
    #[arg(long, value_name = "HOST", requires = "http")]
    pub allowed_host: Vec<String>,
    /// Browser Origin allowed in HTTP mode, e.g. http://localhost:6274 (`:*` for any port).
    /// Repeatable. Requests without an Origin header are always allowed.
    #[arg(long, value_name = "ORIGIN", requires = "http")]
    pub allowed_origin: Vec<String>,
    /// Maximum size of an MCP request body in HTTP mode; larger requests get HTTP 413
    /// [default: 1048576].
    #[arg(long, value_name = "BYTES", requires = "http")]
    pub max_request_bytes: Option<usize>,
}

impl ServeArgs {
    /// Аргументы `serve` без флагов для профиля `profile`: так сервер собирают `call` и `ui`.
    pub fn for_profile(profile: &str) -> Self {
        Self {
            profile: ProfileArg {
                profile: profile.to_owned(),
            },
            allow_write: false,
            allow_destructive: false,
            toolsets: Vec::new(),
            dynamic_toolsets: false,
            tools_file: Vec::new(),
            allow_path: Vec::new(),
            timeout: DEFAULT_TIMEOUT_SECS,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            log_level: None,
            status: false,
            http: false,
            listen: None,
            access_tokens: None,
            secret_file: None,
            client_id: None,
            region: None,
            allowed_host: Vec::new(),
            allowed_origin: Vec::new(),
            max_request_bytes: None,
        }
    }

    pub(crate) fn profile(&self) -> &str {
        &self.profile.profile
    }

    /// Что просят флаги доступа. Относительный `--allow-path` считается от рабочего каталога.
    pub(crate) fn access_request(&self) -> AccessRequest {
        let paths = self
            .allow_path
            .iter()
            .map(|path| std::path::absolute(path).unwrap_or_else(|_| path.clone()))
            .collect();
        AccessRequest::from_flags(self.allow_write, self.allow_destructive, paths)
    }

    /// Проверка аргументов до обращения к профилю и хранилищу.
    fn validate(&self) -> anyhow::Result<()> {
        if self.timeout == 0 {
            bail!("--timeout must be greater than 0");
        }
        if self.max_response_bytes == 0 {
            bail!("--max-response-bytes must be greater than 0");
        }
        if self.max_request_bytes == Some(0) {
            bail!("--max-request-bytes must be greater than 0");
        }
        // Неизвестный набор в флагах — ошибка до обращения к хранилищу учётных данных.
        normalize_toolsets(&self.toolsets).map_err(|err| anyhow!(err))?;
        validate_profile_name(self.profile())
    }
}

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    init_logging(args.log_level.as_deref())?;
    args.validate()?;
    if args.http {
        return http_server::run_from_args(&args).await;
    }
    let repo = ProfileRepo::open_default()?;
    let approvals = approval::platform_store(EXE_STEM);
    if args.status {
        return status(
            &KeyringStore,
            &repo,
            approvals.as_deref(),
            &args,
            &mut TerminalUi,
        );
    }
    let mut setup = Setup::load(&args, &KeyringStore, &repo, approvals.as_deref())?;
    if let Some(warning) = &setup.warning {
        eprintln!("appgallery-connect-mcp: {warning}");
    }
    // Новый refresh token входа пользователя (если HUAWEI ID его выдаст) — в хранилище.
    save_rotated_sign_in(&mut setup.config, args.profile());
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
pub(crate) fn init_logging(level: Option<&str>) -> anyhow::Result<()> {
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
    /// Предупреждение для stderr: права профиля урезаны до одобренных (macOS).
    pub warning: Option<String>,
}

impl Setup {
    /// Читает профиль (учётные данные — из `store`, одобренные права — из `approvals`) и
    /// пользовательские инструменты и собирает настройки сервера. Ошибка в `tools.toml` —
    /// ошибка запуска.
    pub fn load(
        args: &ServeArgs,
        store: &dyn CredentialStore,
        repo: &ProfileRepo,
        approvals: Option<&dyn ApprovalStore>,
    ) -> anyhow::Result<Self> {
        args.validate()?;
        let custom_tools = tools::load(Some(&tools::user_tools_file(repo)), &args.tools_file)?;
        let profile_name = args.profile();
        let profile = load_profile_with(repo, store, profile_name)
            .with_context(|| format!("cannot load profile `{profile_name}`"))?;
        // Проект без клиента в хранилище не мешает запуску: его операции объяснят агенту,
        // что делать.
        for problem in profile.project_problems.iter().chain(&profile.app_problems) {
            tracing::warn!("{problem}");
        }
        let resolved = resolve_access(args, &profile.settings, approvals)?;
        let mut options = effective_options(args, &profile.settings, &resolved.access)?;
        options.custom_tools = custom_tools;
        options.jq = jq_worker::current_runner();
        let mut config = profile.client_config();
        config.timeout = Duration::from_secs(args.timeout);
        config.max_response_bytes = args.max_response_bytes;
        Ok(Self {
            options,
            config,
            warning: resolved.verified.warning(EXE_STEM, profile_name),
        })
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

/// Права сервера: права профиля, урезанные до одобренных (macOS), затем суженные флагами.
/// Флаг сверх прав профиля — ошибка с подсказкой, где меняются права.
pub(crate) fn resolve_access(
    args: &ServeArgs,
    settings: &ProfileSettings,
    approvals: Option<&dyn ApprovalStore>,
) -> anyhow::Result<Resolved> {
    let profile = args.profile();
    approval::resolve_with(
        approvals,
        profile,
        &settings.access(),
        &args.access_request(),
    )
    .map_err(|err| {
        anyhow!(
            "{err}. The access of a profile is changed with `appgallery-connect-mcp ui` \
             (profile `{profile}`) or `appgallery-connect-mcp connect --allow-write ...`; \
             registrations made by older versions with access flags are updated by \
             `appgallery-connect-mcp connect`"
        )
    })
}

/// Уровень доступа ядра по уровню из mcp-kit.
pub fn api_level(level: mcp_kit::AccessLevel) -> AccessLevel {
    match level {
        mcp_kit::AccessLevel::Read => AccessLevel::Read,
        mcp_kit::AccessLevel::Write => AccessLevel::Write,
        mcp_kit::AccessLevel::Destructive => AccessLevel::Destructive,
    }
}

/// Откуда взяты права: из флагов (сужены), из профиля или по умолчанию (только чтение).
fn access_source(args: &ServeArgs, settings: &ProfileSettings) -> Source {
    let request = args.access_request();
    if request.level.is_some() || !request.allow_path.is_empty() {
        Source::Flags
    } else if settings.access().is_read_only() {
        Source::Default
    } else {
        Source::Profile
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
    access: &ProfileAccess,
) -> anyhow::Result<ServerOptions> {
    let (toolsets, _) = effective_toolsets(args, settings)?;
    Ok(ServerOptions {
        max_access: api_level(access.level),
        toolsets,
        dynamic_toolsets: args.dynamic_toolsets || settings.dynamic_toolsets,
        allowed_paths: access.allow_path.clone(),
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
    approvals: Option<&dyn ApprovalStore>,
    args: &ServeArgs,
    ui: &mut dyn Ui,
) -> anyhow::Result<()> {
    args.validate()?;
    let profile = args.profile();
    let settings = repo.get(profile)?.unwrap_or_default();
    let has_credentials = match settings.kind {
        ProfileKind::ApiClient => store.contains(profile)?,
        ProfileKind::Oauth => store.contains_oauth(profile)?,
    };
    let resolved = resolve_access(args, &settings, approvals)?;
    let access = api_level(resolved.access.level);
    let access_source = access_source(args, &settings);
    let (_, toolsets_source) = effective_toolsets(args, &settings)?;
    let tools_file = tools::user_tools_file(repo);
    let options = ServerOptions {
        custom_tools: tools::load(Some(&tools_file), &args.tools_file)?,
        ..effective_options(args, &settings, &resolved.access)?
    };
    let registry = ToolRegistry::new(&options).map_err(|err| anyhow!(err))?;

    ui.say(&format!("Profile:     {profile}"));
    ui.say(&format!("Server name: {}", server_name(profile)));
    let credentials = match (settings.kind, has_credentials) {
        (ProfileKind::ApiClient, true) => {
            format!("API client, stored in the system keychain (service `{KEYRING_SERVICE}`)")
        }
        (ProfileKind::ApiClient, false) => format!(
            "not found; store them with `appgallery-connect-mcp auth login --profile {profile}`"
        ),
        (ProfileKind::Oauth, true) => format!(
            "{}, team {}, stored in the system keychain (service `{KEYRING_SERVICE}`, account \
             `{}`)",
            ProfileKind::Oauth.title(),
            settings.team_id.as_deref().unwrap_or("- (sign in again)"),
            oauth_account(profile)
        ),
        (ProfileKind::Oauth, false) => format!(
            "{}: not found; sign in with `appgallery-connect-mcp auth login --oauth --profile \
             {profile}`",
            ProfileKind::Oauth.title()
        ),
    };
    ui.say(&format!("Credentials: {credentials}"));
    if !settings.projects.is_empty() {
        let (found, _) = load_projects(store, profile, &settings.projects)?;
        let found: Vec<&str> = found.iter().map(|p| p.project_id()).collect();
        let missing: Vec<&str> = settings
            .projects
            .iter()
            .map(String::as_str)
            .filter(|id| !found.contains(id))
            .collect();
        let mut line = format!(
            "{} with a project-level API client",
            list_or_none_str(&found)
        );
        if !missing.is_empty() {
            line.push_str(&format!(
                "; not usable: {} (`appgallery-connect-mcp auth project add --profile {profile} \
                 --project-id <ID>`)",
                missing.join(", ")
            ));
        }
        ui.say(&format!("Projects:    {line}"));
    }
    if !settings.app_secrets.is_empty() {
        let (found, _) = load_app_secrets(store, profile, &settings.app_secrets)?;
        let found: Vec<&str> = found.iter().map(|app| app.app_id()).collect();
        let missing: Vec<&str> = settings
            .app_secrets
            .iter()
            .map(String::as_str)
            .filter(|id| !found.contains(id))
            .collect();
        let mut line = format!("{} with an app secret", list_or_none_str(&found));
        if !missing.is_empty() {
            line.push_str(&format!(
                "; not usable: {} (`appgallery-connect-mcp auth app-secret set --profile \
                 {profile} --app <APP_ID>`)",
                missing.join(", ")
            ));
        }
        ui.say(&format!("App secrets: {line}"));
    }
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
    ui.say(&format!(
        "Approval:    {}",
        approval_text(&resolved.verified.state)
    ));
    if let Some(warning) = resolved.verified.warning(EXE_STEM, profile) {
        ui.say(&format!("Warning:     {warning}"));
    }
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
    if !resolved.access.allow_path.is_empty() {
        let paths: Vec<String> = resolved
            .access
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

fn list_or_none_str(values: &[&str]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

fn list_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

/// Состояние одобрения для `serve --status` и `auth status`.
pub fn approval_text(state: &ApprovalState) -> String {
    match state {
        ApprovalState::NotRequired => {
            "not used on this platform (the roles of the API client limit the access)".to_owned()
        }
        ApprovalState::ReadOnly => "not needed (read-only)".to_owned(),
        ApprovalState::Approved => "approved on this Mac".to_owned(),
        ApprovalState::Missing => "not approved on this Mac".to_owned(),
        ApprovalState::Exceeds { approved } => {
            format!("only `{}` is approved on this Mac", approved.summary())
        }
        ApprovalState::Unavailable(reason) => format!("cannot read the keychain: {reason}"),
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
        try_options(args, saved).unwrap()
    }

    fn try_options(args: &[&str], saved: &ProfileSettings) -> anyhow::Result<ServerOptions> {
        let args = parse(args);
        let resolved = resolve_access(&args, saved, None)?;
        effective_options(&args, saved, &resolved.access)
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
        status(store, repo, None, &parse(args), &mut ui).unwrap();
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
    fn flags_narrow_access_and_win_for_toolsets() {
        let saved = settings(true, false, &["listing"]);
        let from_profile = options(&[], &saved);
        assert_eq!(from_profile.max_access, AccessLevel::Write);
        assert_eq!(from_profile.toolsets, Some(vec!["listing".to_owned()]));
        assert_eq!(from_profile.apps, ["100000001"]);
        assert_eq!(from_profile.packages, ["com.example.app"]);
        assert_eq!(from_profile.profile.as_deref(), Some("default"));

        let flags = options(&["--allow-write", "--toolsets", "apps, reviews"], &saved);
        assert_eq!(flags.max_access, AccessLevel::Write);
        assert_eq!(
            flags.toolsets,
            Some(vec!["apps".to_owned(), "reviews".to_owned()])
        );
        // `--allow-write` при `allow_destructive` в профиле сужает права.
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
        let err = try_options(&["--toolsets", "nope"], &saved).unwrap_err();
        assert!(err.to_string().contains("unknown toolset `nope`"), "{err}");
        let broken = settings(false, false, &["nope"]);
        let err = try_options(&[], &broken).unwrap_err();
        assert!(err.to_string().contains("profile settings"), "{err}");

        // Флаги не расширяют права профиля.
        let read_only = settings(false, false, &[]);
        let err = try_options(&["--allow-write"], &read_only).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("profile allows only `read`"), "{text}");
        assert!(
            text.contains("`appgallery-connect-mcp ui` (profile `"),
            "{text}"
        );
        let write = settings(true, false, &[]);
        assert!(try_options(&["--allow-destructive"], &write).is_err());
    }

    #[test]
    fn allow_path_comes_from_profile_and_flags_narrow_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let inner = root.join("inner");
        std::fs::create_dir(&inner).unwrap();
        let saved = ProfileSettings {
            allow_path: vec![root.clone()],
            ..settings(false, false, &[])
        };
        assert_eq!(
            options(&[], &saved).allowed_paths,
            std::slice::from_ref(&root)
        );
        let inner_arg = inner.display().to_string();
        assert_eq!(
            options(&["--allow-path", &inner_arg], &saved).allowed_paths,
            [inner]
        );
        let other = tempfile::tempdir().unwrap();
        let other_arg = other.path().display().to_string();
        let err = try_options(&["--allow-path", &other_arg], &saved).unwrap_err();
        assert!(
            err.to_string().contains("not inside a directory allowed"),
            "{err}"
        );
        // Без каталогов в профиле любой `--allow-path` — расширение прав.
        let bare = settings(false, false, &[]);
        assert!(try_options(&["--allow-path", &inner_arg], &bare).is_err());
    }

    #[test]
    fn unapproved_access_runs_read_only_with_a_warning() {
        use mcp_kit::approval::testing::MemoryStore as MemoryApprovals;

        let (_dir, repo, store) = fixture(Some(settings(true, true, &[])), true);
        let approvals = MemoryApprovals::default();
        let load =
            |args: &[&str]| Setup::load(&parse(args), &store, &repo, Some(&approvals)).unwrap();
        let setup = load(&["--profile", "ci"]);
        assert_eq!(setup.options.max_access, AccessLevel::Read);
        let warning = setup.warning.unwrap();
        assert!(warning.contains("not approved on this Mac"), "{warning}");
        assert!(warning.contains("appgallery-connect-mcp ui"), "{warning}");
        // Старая регистрация с флагами в пределах профиля запускается, но на чтение.
        let setup = load(&["--profile", "ci", "--allow-destructive"]);
        assert_eq!(setup.options.max_access, AccessLevel::Read);

        approvals
            .save(
                "ci",
                &ProfileAccess::new(mcp_kit::AccessLevel::Write, vec![]),
            )
            .unwrap();
        let setup = load(&["--profile", "ci"]);
        assert_eq!(setup.options.max_access, AccessLevel::Write);
        assert!(setup.warning.unwrap().contains("only `write` is approved"));

        let mut ui = ScriptedUi::default();
        status(
            &store,
            &repo,
            Some(&approvals),
            &parse(&["--profile", "ci"]),
            &mut ui,
        )
        .unwrap();
        let text = ui.text();
        assert!(text.contains("Access:      write"), "{text}");
        assert!(
            text.contains("Approval:    only `write` is approved on this Mac"),
            "{text}"
        );
        assert!(text.contains("Warning:     profile `ci`"), "{text}");
    }

    #[test]
    fn dynamic_toolsets_from_flag_or_profile() {
        let files = tempfile::tempdir().unwrap();
        let root = files.path().canonicalize().unwrap();
        let (builds, reports) = (root.join("builds"), root.join("reports"));
        std::fs::create_dir(&builds).unwrap();
        std::fs::create_dir(&reports).unwrap();
        let (builds_arg, reports_arg) =
            (builds.display().to_string(), reports.display().to_string());
        let args = [
            "--profile",
            "ci",
            "--dynamic-toolsets",
            "--allow-path",
            builds_arg.as_str(),
            "--allow-path",
            reports_arg.as_str(),
        ];
        let saved = ProfileSettings {
            allow_path: vec![root.clone()],
            ..settings(false, false, &[])
        };
        let with = options(&args, &saved);
        assert!(with.dynamic_toolsets);
        assert_eq!(with.allowed_paths, [builds.clone(), reports.clone()]);
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
            text.contains(&format!(
                "Files:       working directory, temporary directory, {}, {}",
                builds.display(),
                reports.display()
            )),
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
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
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
        let err = Setup::load(&args, &store, &repo, None).err().unwrap();
        let text = format!("{err:#}");
        assert!(
            text.contains("broken.toml: tool `x`: unknown operation"),
            "{text}"
        );
        let mut ui = ScriptedUi::default();
        assert!(status(&store, &repo, None, &args, &mut ui).is_err());
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
        assert!(
            text.contains("Approval:    not used on this platform"),
            "{text}"
        );
        assert!(!text.contains("Warning:"), "{text}");
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
    fn project_clients_come_from_the_profile() {
        let saved = ProfileSettings {
            projects: vec!["100".into(), "200".into()],
            ..settings(false, false, &[])
        };
        let (_dir, repo, store) = fixture(Some(saved), true);
        store
            .set_project("ci", "100", &credentials("project"))
            .unwrap();
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(
            text.contains("Projects:    100 with a project-level API client; not usable: 200"),
            "{text}"
        );
        assert!(text.contains("auth project add --profile ci"), "{text}");
        assert!(!text.contains(&client_id("project")), "{text}");

        // Клиент проекта без записи не мешает запуску; найденный — в настройках клиента API.
        let setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        assert_eq!(setup.config.projects.len(), 1);
        assert_eq!(setup.config.projects[0].project_id(), "100");
        assert_eq!(setup.config.profile.as_deref(), Some("ci"));
    }

    #[test]
    fn setup_requires_credentials() {
        let (_dir, repo, store) = fixture(Some(settings(false, false, &[])), false);
        let err = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None)
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
            None,
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
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
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

    /// Профиль со входом пользователя: `--status` показывает вход и команду, сервер шлёт
    /// `oauth2Token` и `teamId`, а операции только для API-клиента отклоняет до запроса.
    #[tokio::test]
    async fn user_sign_in_profile_serves_as_the_user_of_its_team() {
        use crate::profiles::ProfileKind;
        use crate::testing::{mount_user_sign_in, refresh_token, sign_in, user_access_token};
        use appgallery_connect_api::OAuthEndpoints;

        let api = MockServer::start().await;
        mount_user_sign_in(&api, "serve", json!([])).await;
        Mock::given(method("GET"))
            .and(path("/api/publish/v2/app-info"))
            .and(header("oauth2Token", user_access_token().as_str()))
            .and(header("teamId", "5100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ret": {"code": 0, "msg": "success"},
                "appInfo": {"defaultLang": "en-US"}
            })))
            .expect(1)
            .mount(&api)
            .await;

        let (_dir, repo, store) = fixture(
            Some(ProfileSettings {
                kind: ProfileKind::Oauth,
                team_id: Some("5100".into()),
                ..settings(false, false, &[])
            }),
            false,
        );
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("(OAuth client): not found"), "{text}");
        assert!(text.contains("auth login --oauth --profile ci"), "{text}");
        store.set_oauth("ci", &sign_in("serve")).unwrap();
        let text = status_text(&["--profile", "ci"], &repo, &store);
        assert!(text.contains("team 5100"), "{text}");
        assert!(text.contains("account `ci/oauth`"), "{text}");
        assert!(!text.contains(&refresh_token("serve")), "{text}");

        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
        assert!(setup.config.credentials.is_none());
        let session = setup.config.oauth.as_mut().unwrap();
        assert_eq!(session.team_id.as_deref(), Some("5100"));
        session.endpoints = OAuthEndpoints::huawei()
            .with_token_url(&format!("{}/oauth2/v3/token", api.uri()))
            .unwrap();
        setup.config.base_url = Some(api.uri());
        // Запуск не обращается к сети.
        assert_eq!(api.received_requests().await.unwrap().len(), 0);
        let server = setup.server().unwrap();

        let (server_io, client_io) = tokio::io::duplex(1024 * 1024);
        let handle = tokio::spawn(async move {
            let running = server.serve(server_io).await.unwrap();
            let _ = running.waiting().await;
        });
        let client = ().serve(client_io).await.unwrap();
        let instructions = client.peer_info().unwrap().instructions.clone().unwrap();
        assert!(instructions.contains("team `5100`"), "{instructions}");

        let call = |name: &str, args: serde_json::Value| {
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(args.as_object().unwrap().clone())
        };
        let result = client
            .call_tool(call("get_app_info", json!({"appId": "100000001"})))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains("en-US"), "{text}");
        assert!(!text.contains(&user_access_token()), "{text}");

        let result = client
            .call_tool(call(
                "list_reviews",
                json!({
                    "appId": "100000001",
                    "countries": "RU",
                    "beginTime": 1_700_000_000_000_i64,
                    "endTime": 1_700_100_000_000_i64
                }),
            ))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true), "{result:?}");
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains("API client"), "{text}");
        // Отзывы не запрашивались: кроме входа и сведений о приложении запросов нет.
        let paths: Vec<String> = api
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| r.url.path().to_owned())
            .collect();
        assert_eq!(paths, ["/oauth2/v3/token", "/api/publish/v2/app-info"]);

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
        let mut setup = Setup::load(&parse(&["--profile", "ci"]), &store, &repo, None).unwrap();
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
