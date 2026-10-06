//! `forskap issue create` — file a new issue in a project.
//!
//! Unlike the other writes this one is not queued by the daemon: it either
//! reaches GitLab now or fails, so a failure never turns into an issue later.
//!
//! The description is `--description` as given; without it, on a terminal
//! with `$VISUAL` or `$EDITOR` set, it is written in that editor, starting
//! from one of the project's issue templates (`--template`, or the one
//! picked) where the cache holds any. Without an editor the terminal is told
//! how to get one, and the template (or nothing) is sent as is.

use anyhow::{Context, Result, bail};
use forskap_api::{
    DescriptionTemplate, ErrorKind, IssuableKind, NewWorkItem, VarlinkClient,
    VarlinkClientInterface, WorkItemRef,
};

use crate::cli::CreateArgs;
use crate::cmd::{epic, project};
use crate::friendly::friendly;
use crate::{client, editor, output, pick, style};

pub async fn run(args: CreateArgs) -> Result<()> {
    let client = client::connect_default().await?;
    let project_id = project::by_arg(&client, &args.project).await?;
    let parent = match args.epic {
        Some(iid) => {
            let epic = epic::resolve(&client, iid, args.group.as_deref()).await?;
            Some(WorkItemRef {
                project_id: None,
                group_id: epic.group_id,
                iid: epic.iid,
                r#type: Some(epic.r#type),
                title: None,
                web_url: None,
            })
        }
        None => None,
    };
    let description = description(&client, project_id, &args).await?;
    let item = NewWorkItem {
        title: args.title.join(" "),
        description,
        labels: (!args.labels.is_empty()).then_some(args.labels),
        assign_self: Some(!args.no_assign),
        parent,
    };

    let reply = client
        .create_work_item(project_id, item)
        .call()
        .await
        .map_err(failed)?;

    output::emit(args.output.output, &reply, |created| {
        match (created.iid, &created.web_url) {
            (Some(iid), Some(url)) => {
                outln!("{} {}", style::reference('#', iid), style::muted(url))
            }
            (Some(iid), None) => outln!("{}", style::reference('#', iid)),
            (None, Some(url)) => outln!("{url}"),
            (None, None) => outln!("created, but GitLab's answer doesn't say which issue it is"),
        }
    })
}

/// The description to send: the text given, else a template of the project
/// (the one named, or the one picked on a terminal) edited in the user's
/// editor on a terminal unless `--no-edit`; nothing where that leaves no
/// text.
async fn description(
    client: &VarlinkClient,
    project_id: i64,
    args: &CreateArgs,
) -> Result<Option<String>> {
    if args.description.is_some() {
        return Ok(args.description.clone());
    }
    let terminal = !args.no_edit && editor::available();
    let editor = terminal.then(editor::configured).flatten();
    if terminal && editor.is_none() {
        eprintln!(
            "{}",
            style::muted("note: set $VISUAL or $EDITOR to edit the description in an editor")
        );
    }
    // Asking which template is as interactive as editing it.
    let ask = args.template.is_none() && editor.is_some();
    let templates = if args.template.is_some() || ask {
        client
            .get_description_templates(project_id, Some(IssuableKind::work_item))
            .call()
            .await
            .map_err(|e| friendly("GetDescriptionTemplates", e))?
            .templates
    } else {
        Vec::new()
    };
    let start = match &args.template {
        Some(name) => Some(named(templates, name, &args.project)?),
        None if ask && !templates.is_empty() => {
            let picked =
                tokio::task::spawn_blocking(move || pick::select("Template", choices(templates)))
                    .await??;
            picked.ok_or(pick::Cancelled)?
        }
        None => None,
    };
    let text = start.map(|t| t.content).unwrap_or_default();
    let text = match editor {
        Some(editor) => tokio::task::spawn_blocking(move || editor::edit(&editor, &text))
            .await?
            .context("the description was not saved; nothing created")?,
        None => text,
    };
    let text = text.trim_end();
    Ok((!text.is_empty()).then(|| text.to_string()))
}

/// The template called `name` among the cached ones, by exact name first.
fn named(
    mut templates: Vec<DescriptionTemplate>,
    name: &str,
    project: &str,
) -> Result<DescriptionTemplate> {
    if templates.is_empty() {
        bail!(
            "no issue templates cached for {project}: the project may have none, or they \
             haven't synced yet (`forskap sync jobs` lists `issue_templates`)"
        );
    }
    let exact = templates.iter().position(|t| t.name == name);
    let close = || {
        templates
            .iter()
            .position(|t| t.name.eq_ignore_ascii_case(name))
    };
    match exact.or_else(close) {
        Some(at) => Ok(templates.swap_remove(at)),
        None => {
            let names: Vec<&str> = templates.iter().map(|t| t.name.as_str()).collect();
            bail!(
                "no cached issue template {name:?} in {project}; cached: {}",
                names.join(", ")
            )
        }
    }
}

/// The picker's lines: no template first, then each by name.
fn choices(templates: Vec<DescriptionTemplate>) -> Vec<pick::Labeled<Option<DescriptionTemplate>>> {
    let none = pick::Labeled {
        label: "(no template)".to_string(),
        value: None,
    };
    std::iter::once(none)
        .chain(templates.into_iter().map(|t| pick::Labeled {
            label: t.name.clone(),
            value: Some(t),
        }))
        .collect()
}

/// The failure as the user reads it: where GitLab's outcome is unknown, the
/// issue may exist all the same, and creating it again files it twice.
fn failed(e: forskap_api::Error) -> anyhow::Error {
    if matches!(e.kind(), ErrorKind::GitlabUnavailable(_)) {
        let unknown =
            "GitLab may have created the issue all the same: look for it before creating it again";
        return friendly("CreateWorkItem", e).context(unknown);
    }
    friendly("CreateWorkItem", e)
}

#[cfg(test)]
mod tests {
    use forskap_api::{GitlabError_Args, GitlabUnavailable_Args};

    use super::*;

    #[test]
    fn only_an_unknown_outcome_warns_of_a_second_issue() {
        let unavailable = ErrorKind::GitlabUnavailable(Some(GitlabUnavailable_Args {
            message: "network error: connection reset".into(),
        }));
        let shown = format!("{:#}", failed(unavailable.into()));
        assert!(
            shown.starts_with("GitLab may have created the issue all the same")
                && shown.ends_with(": CreateWorkItem failed: network error: connection reset"),
            "{shown}"
        );

        let refused = ErrorKind::GitlabError(Some(GitlabError_Args {
            message: "GitLab error: 403 Forbidden".into(),
            status: Some(403),
        }));
        let shown = format!("{:#}", failed(refused.into()));
        assert_eq!(
            shown,
            "CreateWorkItem failed: GitLab error: 403 Forbidden (HTTP 403)"
        );
    }

    fn template(name: &str) -> DescriptionTemplate {
        DescriptionTemplate {
            kind: IssuableKind::work_item,
            name: name.into(),
            content: format!("## {name}\n"),
        }
    }

    #[test]
    fn a_template_is_named_exactly_or_by_case_and_the_rest_are_listed() {
        let cached = || {
            vec![
                template("Bug"),
                template("bug"),
                template("Feature request"),
            ]
        };
        assert_eq!(named(cached(), "bug", "team/api").unwrap().name, "bug");
        assert_eq!(named(cached(), "BUG", "team/api").unwrap().name, "Bug");
        assert_eq!(
            named(cached(), "feature request", "team/api")
                .unwrap()
                .content,
            "## Feature request\n"
        );
        let missing = named(cached(), "Task", "team/api").unwrap_err().to_string();
        assert_eq!(
            missing,
            "no cached issue template \"Task\" in team/api; cached: Bug, bug, Feature request"
        );
        let none = named(Vec::new(), "Bug", "team/api")
            .unwrap_err()
            .to_string();
        assert!(
            none.starts_with("no issue templates cached for team/api"),
            "{none}"
        );

        let labels: Vec<String> = choices(cached()).into_iter().map(|c| c.label).collect();
        assert_eq!(labels, ["(no template)", "Bug", "bug", "Feature request"]);
    }
}
