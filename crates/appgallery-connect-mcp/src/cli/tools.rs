//! `appgallery-connect-mcp tools` — инструменты, которые зарегистрирует `serve`, и
//! пользовательские инструменты из `tools.toml`.
//!
//! `tools list` печатает таблицу инструментов для заданных уровня доступа и наборов — те же
//! флаги, что у `serve`, но без профиля, учётных данных и сети: таблица операций встроена в
//! бинарник. `tools check` проверяет файлы пользовательских инструментов.
//!
//! Файл пользователя `tools.toml` лежит в каталоге конфигурации рядом с `profiles.toml`
//! (`~/.config/appgallery-connect-mcp/tools.toml`, `APPGALLERY_CONNECT_MCP_CONFIG_DIR`,
//! `%APPDATA%` на Windows) и читается всегда, если он есть. Другие файлы (например, из
//! репозитория проекта) — только по явному `--tools-file`: файл из клонированного
//! репозитория — недоверенный ввод и не должен молча добавлять инструменты.
//!
//! Проверку определений и имён выполняет ядро (`appgallery_connect_mcp_core::custom`):
//! имена составных инструментов зарезервированы так же, как имена операций.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use appgallery_connect_mcp_core::custom::check_names;
use appgallery_connect_mcp_core::{
    CustomTool, CustomToolsFile, ServerOptions, ToolKind, ToolRegistry, access_from_flags,
    access_name, composite, normalize_toolsets,
};
use clap::{Args, Subcommand};

use crate::profiles::ProfileRepo;

/// Имя файла пользовательских инструментов в каталоге конфигурации.
pub const TOOLS_FILE: &str = "tools.toml";

/// Набор составного инструмента, которого нет в реестре (не бывает).
const COMPOSITE_TOOLSET: &str = "-";

#[derive(Debug, Subcommand)]
pub enum ToolsCommand {
    /// List the tools `serve` registers with the given access level, toolsets and custom
    /// tools.
    List(ListArgs),
    /// Validate custom tool files: operations, fixed arguments, `jq` filters and names.
    Check(FilesArgs),
}

#[derive(Debug, Args)]
#[command(after_help = "\
The flags mean the same as for `serve`, but profile settings are not read: without flags the \
list shows the read-only tools of the default toolsets. Tools above the access level or \
outside the toolsets are not registered by the server. Custom tools come from the user file \
tools.toml next to profiles.toml and from --tools-file files; they belong to the toolset of \
their operation and are marked `(custom: <operation>)`. Composite tools (several API calls \
for one task) show `(composite)` instead of a path.")]
pub struct ListArgs {
    /// Include tools that create or change data.
    #[arg(long)]
    pub allow_write: bool,
    /// Include tools whose effect reaches users or money or cannot be undone (implies
    /// --allow-write).
    #[arg(long)]
    pub allow_destructive: bool,
    /// Comma-separated toolsets, or `all` [default: apps,releases,reviews].
    /// With --dynamic-toolsets: the toolsets enabled at start [default: none].
    #[arg(long, value_delimiter = ',', value_name = "TOOLSET,...")]
    pub toolsets: Vec<String>,
    /// Show the tools the server starts with when toolsets are enabled on demand.
    #[arg(long)]
    pub dynamic_toolsets: bool,
    #[command(flatten)]
    pub files: FilesArgs,
}

#[derive(Debug, Args)]
#[command(after_help = "\
The user file tools.toml next to profiles.toml (~/.config/appgallery-connect-mcp/tools.toml, \
%APPDATA%\\appgallery-connect-mcp\\tools.toml on Windows) is always read when it exists. Pass \
the same --tools-file options as to `serve` to include more files.

Format:

  [[tools]]
  name = \"main_app_languages\"
  description = \"Localized names of the main app\"
  operation = \"get_app_info\"
  jq = \"[.languages[] | {lang, appName}]\"

  [tools.args]
  appId = \"100000001\"

`operation` is the tool name of an AppGallery Connect API operation (`get_app_info`) or its \
operationId (`publishing.appInfo.get`), see `appgallery-connect-mcp tools list --toolsets all \
--allow-destructive`; `args` are fixed arguments of that tool, which the agent cannot \
override; `jq` is an optional filter for the response. A custom tool has the access level and \
the toolset of its operation. Names of operations, composite tools and other built-in tools \
cannot be reused.")]
pub struct FilesArgs {
    /// Additional custom tools file, as passed to `serve --tools-file`. Repeatable.
    #[arg(long, value_name = "PATH")]
    pub tools_file: Vec<PathBuf>,
}

pub fn run(command: ToolsCommand) -> anyhow::Result<()> {
    let repo = ProfileRepo::open_default()?;
    let default = user_tools_file(&repo);
    match command {
        ToolsCommand::List(args) => {
            let custom = load(Some(&default), &args.files.tools_file)?;
            let options = options(&args, custom)?;
            let registry = ToolRegistry::new(&options).map_err(|err| anyhow!(err))?;
            print!("{}", render(&registry, &options.custom_tools));
        }
        ToolsCommand::Check(args) => {
            let tools = load(Some(&default), &args.tools_file)?;
            print!("{}", check_report(&tools, &default, &args.tools_file));
        }
    }
    Ok(())
}

/// Файл пользователя: `tools.toml` в каталоге файла профилей.
pub fn user_tools_file(repo: &ProfileRepo) -> PathBuf {
    repo.path()
        .parent()
        .map_or_else(|| PathBuf::from(TOOLS_FILE), |dir| dir.join(TOOLS_FILE))
}

/// Загружает инструменты из файла пользователя `default` (если он есть) и из `extra`
/// (обязательны). Один и тот же файл, переданный дважды, читается один раз.
pub fn load(default: Option<&Path>, extra: &[PathBuf]) -> anyhow::Result<Vec<CustomTool>> {
    let default = default.filter(|path| path.exists());
    let mut tools = Vec::new();
    let mut seen = Vec::new();
    for path in default
        .into_iter()
        .chain(extra.iter().map(PathBuf::as_path))
    {
        let key = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        tools.extend(load_file(path)?);
    }
    let composite: Vec<&str> = composite::all().iter().map(|tool| tool.name).collect();
    check_names(&tools, &composite).map_err(|err| anyhow!(err))?;
    Ok(tools)
}

fn load_file(path: &Path) -> anyhow::Result<Vec<CustomTool>> {
    let source = path.display().to_string();
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the custom tools file {source}"))?;
    let file: CustomToolsFile =
        toml::from_str(&text).map_err(|err| anyhow!("{source}: invalid tools file: {err}"))?;
    file.tools
        .into_iter()
        .map(|def| CustomTool::compile(def, &source).map_err(|err| anyhow!(err)))
        .collect()
}

fn options(args: &ListArgs, custom_tools: Vec<CustomTool>) -> anyhow::Result<ServerOptions> {
    let toolsets = normalize_toolsets(&args.toolsets).map_err(|err| anyhow!(err))?;
    Ok(ServerOptions {
        max_access: access_from_flags(args.allow_write, args.allow_destructive),
        toolsets: (!toolsets.is_empty()).then_some(toolsets),
        dynamic_toolsets: args.dynamic_toolsets,
        custom_tools,
        ..ServerOptions::default()
    })
}

/// Таблица `TOOL ACCESS TOOLSET GROUP METHOD PATH`, отсортированная по набору и имени.
/// `custom` — пользовательские инструменты, по которым построен реестр.
fn render(registry: &ToolRegistry, custom: &[CustomTool]) -> String {
    let mut rows: Vec<[String; 5]> = registry
        .iter()
        .map(|tool| {
            let (toolset, group, path) = match tool.kind {
                ToolKind::Operation(op) => (
                    op.toolset,
                    op.group.to_owned(),
                    format!("{} {}", op.method.as_str(), op.path),
                ),
                ToolKind::Composite(index) => (
                    registry
                        .composite_tools()
                        .get(index)
                        .map_or(COMPOSITE_TOOLSET, |tool| tool.toolset),
                    "-".to_owned(),
                    "(composite)".to_owned(),
                ),
                ToolKind::Custom(index) => match custom.get(index) {
                    Some(def) => {
                        let op = def.operation;
                        (
                            op.toolset,
                            op.group.to_owned(),
                            format!(
                                "{} {} (custom: {})",
                                op.method.as_str(),
                                op.path,
                                op.tool_name
                            ),
                        )
                    }
                    None => ("-", "-".to_owned(), "(custom)".to_owned()),
                },
                ToolKind::ApiSearch => ("-", "-".to_owned(), "(search, no API call)".to_owned()),
                ToolKind::ApiRequest => ("-", "any".to_owned(), "(any API path)".to_owned()),
                ToolKind::ListToolsets | ToolKind::EnableToolset | ToolKind::DisableToolset => (
                    "-",
                    "-".to_owned(),
                    "(toolsets of this session, no API call)".to_owned(),
                ),
            };
            [
                tool.tool.name.to_string(),
                access_name(tool.access).to_owned(),
                toolset.to_owned(),
                group,
                path,
            ]
        })
        .collect();
    rows.sort_by(|a, b| (&a[2], &a[0]).cmp(&(&b[2], &b[0])));
    let header = ["TOOL", "ACCESS", "TOOLSET", "GROUP", "METHOD PATH"];
    let mut widths = header.map(str::len);
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row.iter()) {
            *width = (*width).max(cell.len());
        }
    }
    let line = |cells: [&str; 5]| {
        let [tool, access, toolset, group, path] = cells;
        format!(
            "{tool:<w0$}  {access:<w1$}  {toolset:<w2$}  {group:<w3$}  {path}\n",
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2],
            w3 = widths[3],
        )
    };
    let mut out = line(header);
    for row in &rows {
        out.push_str(&line([&row[0], &row[1], &row[2], &row[3], &row[4]]));
    }
    let enabled = registry.enabled_toolsets();
    let toolsets = if enabled.is_empty() {
        "none".to_owned()
    } else {
        enabled.join(",")
    };
    out.push_str(&format!(
        "{} tools; access: {}; toolsets: {}{}\n",
        rows.len(),
        access_name(registry.max_access()),
        toolsets,
        if registry.is_dynamic() {
            " (more with `enable_toolset`)"
        } else {
            ""
        }
    ));
    let above = custom
        .iter()
        .filter(|tool| !registry.max_access().allows(tool.access()))
        .count();
    if above > 0 {
        out.push_str(&format!(
            "{above} custom tool(s) above the access level are not registered.\n"
        ));
    }
    let outside = custom
        .iter()
        .filter(|tool| {
            registry.max_access().allows(tool.access()) && registry.get(&tool.name).is_none()
        })
        .count();
    if outside > 0 {
        out.push_str(&format!(
            "{outside} custom tool(s) of toolsets that are not enabled are not registered.\n"
        ));
    }
    out
}

/// Таблица пользовательских инструментов для `tools check`.
fn render_custom(tools: &[CustomTool]) -> String {
    let width = tools
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(0)
        .max("TOOL".len());
    let toolset_width = tools
        .iter()
        .map(|t| t.toolset().len())
        .max()
        .unwrap_or(0)
        .max("TOOLSET".len());
    let mut out = format!(
        "{:<width$}  {:<11}  {:<toolset_width$}  OPERATION\n",
        "TOOL", "ACCESS", "TOOLSET"
    );
    for tool in tools {
        let mut details = Vec::new();
        if !tool.fixed_args().is_empty() {
            let names: Vec<&str> = tool.fixed_args().keys().map(String::as_str).collect();
            details.push(format!("fixed: {}", names.join(", ")));
        }
        if tool.jq().is_some() {
            details.push("jq".to_owned());
        }
        details.push(tool.source.clone());
        out.push_str(&format!(
            "{:<width$}  {:<11}  {:<toolset_width$}  {}  ({})\n",
            tool.name,
            access_name(tool.access()),
            tool.toolset(),
            tool.operation.tool_name,
            details.join("; ")
        ));
    }
    out.push_str(
        "\nTools above the server's access level (--allow-write, --allow-destructive) or \
         outside its toolsets are not registered.\n",
    );
    out
}

/// Итог `tools check`: какие файлы проверены и сколько в них инструментов.
fn check_report(tools: &[CustomTool], default: &Path, extra: &[PathBuf]) -> String {
    let mut out = String::new();
    let mut files: Vec<String> = Vec::new();
    if default.exists() {
        files.push(default.display().to_string());
    } else {
        out.push_str(&format!(
            "User file {} does not exist.\n",
            default.display()
        ));
    }
    for path in extra {
        let file = path.display().to_string();
        if !files.contains(&file) {
            files.push(file);
        }
    }
    for file in &files {
        let count = tools.iter().filter(|tool| &tool.source == file).count();
        out.push_str(&format!("{file}: OK, {count} tool(s)\n"));
    }
    if tools.is_empty() {
        out.push_str("No custom tools defined.\n");
    } else {
        out.push('\n');
        out.push_str(&render_custom(tools));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use appgallery_connect_api::AccessLevel;

    use super::*;

    pub(crate) const SAMPLE: &str = r#"
[[tools]]
name = "main_app_languages"
description = "Localized names of the main app"
operation = "get_app_info"
jq = "[.languages[] | {lang, appName}]"

[tools.args]
appId = "100000001"

[[tools]]
name = "rename_main_app"
description = "Change a localized name of the main app"
operation = "publishing.languageInfo.update"

[tools.args]
appId = "100000001"
"#;

    fn write(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn list_with(args: &[&str], custom: Vec<CustomTool>) -> String {
        #[derive(Debug, clap::Parser)]
        struct Wrapper {
            #[command(subcommand)]
            command: ToolsCommand,
        }
        use clap::Parser;
        let mut argv = vec!["tools", "list"];
        argv.extend_from_slice(args);
        let ToolsCommand::List(args) = Wrapper::try_parse_from(argv).unwrap().command else {
            panic!("expected tools list");
        };
        let options = options(&args, custom).unwrap();
        render(&ToolRegistry::new(&options).unwrap(), &options.custom_tools)
    }

    fn list(args: &[&str]) -> String {
        list_with(args, Vec::new())
    }

    fn row(text: &str, name: &str) -> String {
        text.lines()
            .find(|line| line.starts_with(&format!("{name} ")))
            .unwrap_or_else(|| panic!("{name}: {text}"))
            .to_owned()
    }

    #[test]
    fn lists_registered_tools() {
        let text = list(&[]);
        assert!(text.starts_with("TOOL "), "{text}");
        let info = row(&text, "get_app_info");
        assert!(
            info.contains("read-only") && info.contains("publishing"),
            "{info}"
        );
        assert!(info.ends_with("GET /api/publish/v2/app-info"), "{info}");
        assert!(!text.contains("update_app_info"), "{text}");
        assert!(text.contains("access: read-only; toolsets: apps,releases,reviews"));

        let text = list(&["--allow-write", "--toolsets", "listing"]);
        assert!(row(&text, "update_app_info").contains(" write "));
        assert!(!text.contains("delete_app_language_info"), "{text}");
        assert!(!text.contains("get_app_info"), "{text}");

        let text = list(&["--allow-destructive", "--toolsets", "all"]);
        assert!(text.contains("delete_app_language_info"), "{text}");
        assert!(text.contains("list_certificates"), "{text}");

        let text = list(&["--dynamic-toolsets"]);
        assert!(row(&text, "enable_toolset").contains("no API call"));
        assert!(!text.contains("get_app_info"), "{text}");
        assert!(text.contains("toolsets: none (more with"), "{text}");
        let text = list(&["--dynamic-toolsets", "--toolsets", "reviews"]);
        assert!(text.contains("list_reviews"), "{text}");
    }

    #[test]
    fn lists_custom_tools() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "tools.toml", SAMPLE);
        let custom = load(Some(&path), &[]).unwrap();
        let text = list_with(&[], custom.clone());
        let line = row(&text, "main_app_languages");
        assert!(
            line.contains(" apps ") && line.contains("(custom: get_app_info)"),
            "{line}"
        );
        assert!(!text.contains("rename_main_app"), "{text}");
        assert!(
            text.contains("1 custom tool(s) above the access level"),
            "{text}"
        );

        // Набор `listing` не включён — инструмент записи не регистрируется и на сервере записи.
        let text = list_with(&["--allow-write"], custom.clone());
        assert!(!text.contains("rename_main_app "), "{text}");
        assert!(
            text.contains("1 custom tool(s) of toolsets that are not enabled"),
            "{text}"
        );
        let text = list_with(&["--allow-write", "--toolsets", "listing"], custom);
        let line = row(&text, "rename_main_app");
        assert!(
            line.contains(" write ") && line.contains(" listing "),
            "{line}"
        );
    }

    #[test]
    fn user_file_is_next_to_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let repo = ProfileRepo::in_dir(dir.path());
        assert_eq!(user_tools_file(&repo), dir.path().join("tools.toml"));
    }

    #[test]
    fn loads_and_checks() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(dir.path(), "tools.toml", SAMPLE);
        let tools = load(Some(&path), &[]).unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].access(), AccessLevel::Read);
        assert_eq!(tools[1].access(), AccessLevel::Write);
        assert_eq!(tools[1].operation.tool_name, "update_app_language_info");

        let report = check_report(&tools, &path, &[]);
        assert!(report.contains("OK, 2 tool(s)"), "{report}");
        assert!(report.contains("main_app_languages"), "{report}");
        assert!(report.contains("fixed: appId; jq"), "{report}");
        assert!(report.contains(" listing "), "{report}");

        let missing = dir.path().join("none.toml");
        assert!(load(Some(&missing), &[]).unwrap().is_empty());
        let report = check_report(&[], &missing, &[]);
        assert!(report.contains("does not exist"), "{report}");
        assert!(report.contains("No custom tools"), "{report}");
    }

    #[test]
    fn reports_errors_with_file_name() {
        let dir = tempfile::tempdir().unwrap();
        let bad_toml = write(dir.path(), "a.toml", "[[tools]\nname = 1");
        let err = load(None, &[bad_toml]).unwrap_err().to_string();
        assert!(err.contains("a.toml: invalid tools file"), "{err}");

        let unknown = write(
            dir.path(),
            "b.toml",
            "[[tools]]\nname = \"x\"\ndescription = \"d\"\noperation = \"get_app_info\"\n\
             headers = { Authorization = \"x\" }\n",
        );
        let err = load(None, &[unknown]).unwrap_err().to_string();
        assert!(err.contains("unknown field `headers`"), "{err}");

        let bad_op = write(
            dir.path(),
            "c.toml",
            "[[tools]]\nname = \"x\"\ndescription = \"d\"\noperation = \"get_everything\"\n",
        );
        let err = load(None, &[bad_op]).unwrap_err().to_string();
        assert!(err.contains("c.toml: tool `x`: unknown operation"), "{err}");

        let bad_arg = write(
            dir.path(),
            "e.toml",
            "[[tools]]\nname = \"x\"\ndescription = \"d\"\noperation = \"get_app_info\"\n\
             [tools.args]\nappld = \"1\"\n",
        );
        let err = load(None, &[bad_arg]).unwrap_err().to_string();
        assert!(err.contains("has no argument `appld`"), "{err}");

        let bad_jq = write(
            dir.path(),
            "f.toml",
            "[[tools]]\nname = \"x\"\ndescription = \"d\"\noperation = \"get_app_info\"\n\
             jq = \".languages |\"\n",
        );
        let err = load(None, &[bad_jq]).unwrap_err().to_string();
        assert!(err.contains("f.toml: tool `x`: `jq`"), "{err}");

        let missing = dir.path().join("missing.toml");
        let err = load(None, &[missing]).unwrap_err().to_string();
        assert!(err.contains("cannot read"), "{err}");

        // Файл пользователя, переданный ещё и через `--tools-file`, читается один раз.
        let user = write(dir.path(), "tools.toml", SAMPLE);
        assert_eq!(
            load(Some(&user), std::slice::from_ref(&user))
                .unwrap()
                .len(),
            2
        );
        let project = write(dir.path(), "project.toml", SAMPLE);
        let err = load(Some(&user), &[project]).unwrap_err().to_string();
        assert!(err.contains("defined more than once"), "{err}");

        for name in [
            "get_app_info",
            "api_request",
            "api_search",
            "enable_toolset",
        ] {
            let builtin = write(
                dir.path(),
                "d.toml",
                &format!(
                    "[[tools]]\nname = \"{name}\"\ndescription = \"d\"\noperation = \"get_app_info\"\n"
                ),
            );
            let err = load(None, &[builtin]).unwrap_err().to_string();
            assert!(
                err.contains("same name as a built-in tool"),
                "{name}: {err}"
            );
        }
    }
}
