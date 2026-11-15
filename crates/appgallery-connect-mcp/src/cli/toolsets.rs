//! `appgallery-connect-mcp toolsets` — наборы инструментов и число инструментов в них по
//! уровням доступа.
//!
//! Считаются инструменты, которые сервер регистрирует: операции и составные инструменты
//! набора (пользовательские из `tools.toml` — нет: их показывает `tools list`). Ни профиль,
//! ни учётные данные, ни сеть не нужны: таблица операций встроена в бинарник.

use appgallery_connect_api::AccessLevel;
use appgallery_connect_api::spec::operations;
use appgallery_connect_mcp_core::{CompositeTool, available_toolsets, composite};
use clap::Args;

#[derive(Debug, Args)]
#[command(after_help = "\
READ, WRITE and DESTRUCTIVE count the tools of each access level, including composite tools \
(several API calls for one task). A read-only server registers the READ tools of its \
toolsets, --allow-write adds the WRITE tools and --allow-destructive the DESTRUCTIVE ones. \
DEFAULT marks the toolsets enabled without --toolsets; `--toolsets all` enables every \
toolset. With --dynamic-toolsets the server starts with the toolsets of --toolsets (or none) \
and the agent enables more with `enable_toolset`. Custom tools from tools.toml belong to the \
toolset of their operation and are listed by `tools list`.")]
pub struct ToolsetsArgs {}

pub fn run(_args: ToolsetsArgs) -> anyhow::Result<()> {
    print!("{}", render(&composite::all()));
    Ok(())
}

fn level(access: AccessLevel) -> usize {
    match access {
        AccessLevel::Read => 0,
        AccessLevel::Write => 1,
        AccessLevel::Destructive => 2,
    }
}

/// Число инструментов набора по уровням доступа: чтение, запись, удаление.
fn counts(slug: &str, composite: &[CompositeTool]) -> [usize; 3] {
    let mut counts = [0; 3];
    let ops = operations()
        .iter()
        .filter(|op| op.toolset == slug)
        .map(|op| op.access);
    let composite = composite
        .iter()
        .filter(|tool| tool.toolset == slug)
        .map(|tool| tool.access);
    for access in ops.chain(composite) {
        counts[level(access)] += 1;
    }
    counts
}

fn render(composite: &[CompositeTool]) -> String {
    let toolsets = available_toolsets();
    let width = toolsets
        .iter()
        .map(|t| t.slug.len())
        .max()
        .unwrap_or(0)
        .max("TOOLSET".len());
    let mut out = format!(
        "{:<width$}  {:>4}  {:>5}  {:>11}  {:<7}  DESCRIPTION\n",
        "TOOLSET", "READ", "WRITE", "DESTRUCTIVE", "DEFAULT"
    );
    let mut total = [0; 3];
    for toolset in &toolsets {
        let [read, write, destructive] = counts(toolset.slug, composite);
        total[0] += read;
        total[1] += write;
        total[2] += destructive;
        out.push_str(&format!(
            "{:<width$}  {read:>4}  {write:>5}  {destructive:>11}  {:<7}  {}\n",
            toolset.slug,
            if toolset.default { "yes" } else { "" },
            toolset.description
        ));
    }
    out.push_str(&format!(
        "{:<width$}  {:>4}  {:>5}  {:>11}\n",
        "total", total[0], total[1], total[2]
    ));
    out
}

#[cfg(test)]
mod tests {
    use appgallery_connect_api::spec::HttpMethod;
    use appgallery_connect_mcp_core::composite::{Context, ToolFuture};
    use appgallery_connect_mcp_core::default_toolsets;
    use serde_json::json;

    use super::*;

    fn run_nothing(_ctx: &Context, _args: serde_json::Value) -> ToolFuture<'_> {
        Box::pin(async { Ok(json!({})) })
    }

    #[test]
    fn counts_cover_every_registered_tool() {
        let composite = composite::all();
        let total: usize = available_toolsets()
            .iter()
            .map(|t| counts(t.slug, &composite).iter().sum::<usize>())
            .sum();
        assert_eq!(total, operations().len() + composite.len());
        // Составной инструмент считается в своём наборе и на своём уровне.
        let extra = [CompositeTool {
            name: "test_tool",
            toolset: "releases",
            access: AccessLevel::Write,
            description: "Test.",
            input_schema: || json!({"type": "object", "properties": {}}),
            run: run_nothing,
        }];
        let without = counts("releases", &[]);
        let with = counts("releases", &extra);
        assert_eq!(with[1], without[1] + 1, "{with:?} {without:?}");
        // В отзывах есть чтение и публичный ответ (удаление).
        let reviews = counts("reviews", &[]);
        assert!(reviews[0] > 0 && reviews[2] > 0, "{reviews:?}");
        assert!(operations().iter().any(|op| op.method == HttpMethod::Get));

        let text = render(&composite);
        let header = text.lines().next().unwrap();
        assert!(
            header.starts_with("TOOLSET") && header.contains("READ  WRITE  DESTRUCTIVE  DEFAULT"),
            "{header}"
        );
        for slug in default_toolsets() {
            let line = text
                .lines()
                .find(|line| line.split_whitespace().next() == Some(slug))
                .unwrap();
            assert!(line.contains(" yes "), "{line}");
        }
        let provisioning = text
            .lines()
            .find(|line| line.starts_with("provisioning "))
            .unwrap();
        assert!(!provisioning.contains(" yes "), "{provisioning}");
        assert!(text.lines().any(|line| line.starts_with("total")), "{text}");
    }
}
