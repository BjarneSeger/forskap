//! `forskap issue view` / `forskap mr view` — what the daemon's caches hold on one item.

use anyhow::Result;

use super::{Item, locate, lookup};
use crate::cli::{OutputFormat, TargetArgs};
use crate::output;
use crate::refspec::RefKind;

pub async fn run(kind: RefKind, target: TargetArgs, format: OutputFormat) -> Result<()> {
    let (client, project_id) = locate(kind, &target).await?;
    match lookup(&client, kind, project_id, target.iid).await? {
        Item::Issue(i) => output::emit(format, &i, |i| {
            println!("#{} {}", i.iid, i.title);
            field("state", &i.state);
            field("project", &i.project_id.to_string());
            field("url", &i.web_url);
            field("time spent", &i.total_time);
            field("status", &i.graph_status);
            field("parent", &i.parent);
            opened(i.open_count);
        }),
        Item::Mr(m) => output::emit(format, &m, |m| {
            println!("!{} {}", m.iid, m.title);
            field("state", &m.state);
            field("project", &m.project_id.to_string());
            field("url", &m.web_url);
            field("assignees", &m.assignees.join(", "));
            opened(m.open_count);
        }),
    }
}

fn field(name: &str, value: &str) {
    if !value.is_empty() {
        println!("  {:<11} {value}", format!("{name}:"));
    }
}

fn opened(count: i64) {
    if count > 0 {
        field("opened", &format!("{count}×"));
    }
}
