//! Бинарник `appgallery-connect-mcp`: CLI и stdio-транспорт MCP-сервера AppGallery Connect.
//!
//! Подкоманды: `auth` (учётные данные API-клиентов и профили), `connect` (регистрация
//! сервера в MCP-клиенте), `serve` (MCP-сервер через stdio), `tools` (инструменты сервера и
//! `tools.toml`), `toolsets` (наборы) и скрытая `jq-worker` (фильтр `jq` для `serve`).
//!
//! Вывод команд — в stdout, ошибки и диагностика — в stderr.

mod cli;
mod clients;
mod login;
mod profiles;
mod ui;
mod verify;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

use crate::cli::auth::AuthCommand;
use crate::cli::serve::ServeArgs;
use crate::cli::tools::ToolsCommand;
use crate::cli::toolsets::ToolsetsArgs;

/// MCP server and CLI for the Huawei AppGallery Connect API.
#[derive(Debug, Parser)]
#[command(name = "appgallery-connect-mcp", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Manage the AppGallery Connect API clients of the profiles.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Run the MCP server over stdio with the API client of a profile.
    Serve(ServeArgs),
    /// Register the server in an MCP client (Claude Code or Codex) for a profile.
    ///
    /// Stores the API client of the profile in the system keychain (as `auth login`) unless
    /// it already has working credentials, saves the access settings in the profile and adds
    /// `appgallery-connect-mcp serve --profile <name> ...` to the client config. The client
    /// secret never gets into the client config.
    Connect(cli::connect::ConnectArgs),
    /// List the tools the server registers and check custom tools (tools.toml).
    #[command(subcommand)]
    Tools(ToolsCommand),
    /// List the toolsets and the number of tools in each by access level.
    Toolsets(ToolsetsArgs),
    /// Internal: evaluate one `jq` filter for `serve` (JSON on stdin and stdout).
    #[command(name = "jq-worker", hide = true)]
    JqWorker,
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    match cli.command {
        Command::Auth(command) => {
            cli::auth::run(command).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve(args) => {
            cli::serve::run(args).await?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Connect(args) => cli::connect::run(args).await.map(|()| ExitCode::SUCCESS),
        Command::Tools(command) => {
            cli::tools::run(command)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Toolsets(args) => {
            cli::toolsets::run(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::JqWorker => {
            cli::jq_worker::run()?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod testing {
    use appgallery_connect_api::ApiClientCredentials;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Путь метода получения токена.
    pub const TOKEN_PATH: &str = "/api/oauth2/v1/token";

    /// Выдуманный access token, собранный из частей.
    pub fn access_token() -> String {
        ["access", "-token-", "value-", "0123456789"].concat()
    }

    /// Выдуманный идентификатор API-клиента, собранный из частей.
    pub fn client_id(tag: &str) -> String {
        ["1088", "9300", "8395", tag].concat()
    }

    /// Выдуманный секрет API-клиента, собранный из частей.
    pub fn client_secret(tag: &str) -> String {
        ["B15B", "497B", "44E0", "80EB", tag].concat().repeat(3)
    }

    pub fn credentials(tag: &str) -> ApiClientCredentials {
        ApiClientCredentials::from_parts(client_id(tag), client_secret(tag)).unwrap()
    }

    /// Метод получения токена, который выдаёт токен только для `credentials`.
    pub async fn token_server(credentials: &ApiClientCredentials) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .and(body_partial_json(json!({
                "grant_type": "client_credentials",
                "client_id": credentials.client_id().expose(),
                "client_secret": credentials.client_secret().expose(),
            })))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"access_token": access_token(), "expires_in": 172_800})),
            )
            .mount(&server)
            .await;
        // Остальные учётные данные Huawei отклоняет с HTTP 200 и кодом в `ret`, отражая секрет.
        Mock::given(method("POST"))
            .and(path(TOKEN_PATH))
            .respond_with(move |req: &wiremock::Request| {
                let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
                let secret = body["client_secret"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                ResponseTemplate::new(200).set_body_json(json!({
                    "ret": {"code": 203817914, "msg": format!("client secret {secret} is invalid")}
                }))
            })
            .mount(&server)
            .await;
        server
    }
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::Region;
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn parses_auth_commands() {
        let cli = Cli::try_parse_from([
            "appgallery-connect-mcp",
            "auth",
            "login",
            "--profile",
            "work",
            "--region",
            "RU",
            "--client-id",
            "123",
            "--secret-file",
            "secret.txt",
            "--default-app",
            "100000001,100000002",
            "--default-app",
            "100000003",
            "--default-package",
            "com.example.app",
            "--no-verify",
        ])
        .unwrap();
        let Command::Auth(AuthCommand::Login(args)) = cli.command else {
            panic!("expected auth login");
        };
        assert_eq!(args.profile.profile, "work");
        assert_eq!(args.region, Some(Region::Ru));
        assert_eq!(args.client_id.as_deref(), Some("123"));
        assert_eq!(args.default_apps, ["100000001", "100000002", "100000003"]);
        assert_eq!(args.default_packages, ["com.example.app"]);
        assert!(args.no_verify && !args.secret_stdin);
        assert!(args.secret_argument.is_none() && args.stray.is_empty());

        // Неизвестный регион — ошибка разбора со списком регионов.
        let err =
            Cli::try_parse_from(["appgallery-connect-mcp", "auth", "login", "--region", "us"])
                .unwrap_err()
                .to_string();
        assert!(err.contains("cn, de, sg, ru"), "{err}");

        // --secret-file и --secret-stdin взаимоисключающие; --secret-stdin требует --client-id.
        for bad in [
            &[
                "--secret-file",
                "s.txt",
                "--secret-stdin",
                "--client-id",
                "1",
            ][..],
            &["--secret-stdin"],
        ] {
            let mut argv = vec!["appgallery-connect-mcp", "auth", "login"];
            argv.extend_from_slice(bad);
            assert!(Cli::try_parse_from(argv).is_err(), "{bad:?}");
        }

        // Секрет аргументом разбирается, но значение не сохраняется: `login` его отклонит.
        for argv in [
            &["--secret", "value"][..],
            &["--client-secret=value"],
            &["--key", "value"],
            &["--secret"],
            &["value"],
        ] {
            let mut full = vec!["appgallery-connect-mcp", "auth", "login"];
            full.extend_from_slice(argv);
            let cli = Cli::try_parse_from(full).unwrap();
            let Command::Auth(AuthCommand::Login(args)) = cli.command else {
                panic!("expected auth login");
            };
            assert!(
                args.secret_argument.is_some() || !args.stray.is_empty(),
                "{argv:?}"
            );
            assert!(!format!("{args:?}").contains("value"), "{argv:?}");
        }

        let cli =
            Cli::try_parse_from(["appgallery-connect-mcp", "auth", "status", "--check"]).unwrap();
        let Command::Auth(AuthCommand::Status(args)) = cli.command else {
            panic!("expected auth status");
        };
        assert!(args.check);

        let cli = Cli::try_parse_from([
            "appgallery-connect-mcp",
            "auth",
            "logout",
            "--profile",
            "work",
        ])
        .unwrap();
        let Command::Auth(AuthCommand::Logout(args)) = cli.command else {
            panic!("expected auth logout");
        };
        assert_eq!(args.profile.profile, "work");
    }

    #[test]
    fn parses_serve_command() {
        let cli = Cli::try_parse_from([
            "appgallery-connect-mcp",
            "serve",
            "--profile",
            "work",
            "--allow-write",
            "--toolsets",
            "apps,listing",
            "--dynamic-toolsets",
            "--allow-path",
            "/tmp/builds",
            "--tools-file",
            "project-tools.toml",
        ])
        .unwrap();
        let Command::Serve(args) = cli.command else {
            panic!("expected serve");
        };
        assert_eq!(args.profile.profile, "work");
        assert!(args.allow_write && !args.allow_destructive && args.dynamic_toolsets);
        assert_eq!(args.toolsets, ["apps", "listing"]);
        assert_eq!(args.allow_path, [std::path::PathBuf::from("/tmp/builds")]);
        assert_eq!(
            args.tools_file,
            [std::path::PathBuf::from("project-tools.toml")]
        );
    }

    #[test]
    fn parses_tools_and_toolsets_commands() {
        let cli = Cli::try_parse_from([
            "appgallery-connect-mcp",
            "tools",
            "list",
            "--allow-destructive",
            "--toolsets",
            "all",
            "--dynamic-toolsets",
            "--tools-file",
            "a.toml",
        ])
        .unwrap();
        let Command::Tools(ToolsCommand::List(args)) = cli.command else {
            panic!("expected tools list");
        };
        assert!(args.allow_destructive && args.dynamic_toolsets);
        assert_eq!(args.toolsets, ["all"]);
        assert_eq!(args.files.tools_file.len(), 1);
        let cli = Cli::try_parse_from(["appgallery-connect-mcp", "tools", "check"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Tools(ToolsCommand::Check(_))
        ));
        let cli = Cli::try_parse_from(["appgallery-connect-mcp", "toolsets"]).unwrap();
        assert!(matches!(cli.command, Command::Toolsets(_)));
        let cli = Cli::try_parse_from(["appgallery-connect-mcp", "jq-worker"]).unwrap();
        assert!(matches!(cli.command, Command::JqWorker));
        // Скрытая подкоманда не видна в справке.
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("jq-worker"), "{help}");
        assert!(help.contains("toolsets"), "{help}");
    }

    #[test]
    fn parses_connect_flags() {
        let cli = Cli::try_parse_from([
            "appgallery-connect-mcp",
            "connect",
            "claude-code",
            "--profile",
            "ci",
            "--scope",
            "local",
            "--region",
            "de",
            "--client-id",
            "123",
            "--secret-stdin",
            "--default-app",
            "100000001",
            "--allow-destructive",
            "--toolsets",
            "apps,releases",
            "--no-dynamic-toolsets",
            "--allow-path",
            "/srv/a",
            "--allow-path",
            "/srv/b",
            "--dry-run",
            "--force",
            "-y",
        ])
        .unwrap();
        let Command::Connect(args) = cli.command else {
            panic!("expected connect");
        };
        assert_eq!(args.client, clients::ClientKind::ClaudeCode);
        assert_eq!(args.profile.profile, "ci");
        assert_eq!(args.scope, clients::Scope::Local);
        assert_eq!(args.region, Some(Region::De));
        assert_eq!(args.client_id.as_deref(), Some("123"));
        assert_eq!(args.default_apps, ["100000001"]);
        assert_eq!(args.toolsets, ["apps", "releases"]);
        assert_eq!(args.allow_paths.len(), 2);
        assert!(args.secret_stdin && args.allow_destructive && args.no_dynamic_toolsets);
        assert!(args.dry_run && args.force && args.yes);

        let cli = Cli::try_parse_from(["appgallery-connect-mcp", "connect", "codex"]).unwrap();
        let Command::Connect(args) = cli.command else {
            panic!("expected connect");
        };
        assert_eq!(args.scope, clients::Scope::User);
        assert!(!args.dry_run && !args.silent);
        assert!(Cli::try_parse_from(["appgallery-connect-mcp", "connect", "cursor"]).is_err());
        // --silent и --dry-run несовместимы; --secret-stdin требует --client-id.
        for bad in [&["--silent", "--dry-run"][..], &["--secret-stdin"]] {
            let mut argv = vec!["appgallery-connect-mcp", "connect", "codex"];
            argv.extend_from_slice(bad);
            assert!(Cli::try_parse_from(argv).is_err(), "{bad:?}");
        }
        // Секрет аргументом разбирается, но значение не сохраняется: `connect` его отклонит.
        let cli = Cli::try_parse_from([
            "appgallery-connect-mcp",
            "connect",
            "codex",
            "--secret",
            "value",
            "extra",
        ])
        .unwrap();
        let Command::Connect(args) = cli.command else {
            panic!("expected connect");
        };
        assert!(args.secret_argument.is_some() && args.stray.len() == 1);
        assert!(!format!("{args:?}").contains("value"));
    }
}
