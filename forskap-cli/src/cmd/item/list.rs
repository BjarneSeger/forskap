//! `forskap issue list` / `forskap mr list` — your assigned, open issues or merge
//! requests.
//!
//! Pure cache read: the daemon's background sync owns freshness, so this just
//! serves whatever was last synced — no fetch, effectively free.

use anyhow::Result;
use forskap_api::{Scope, VarlinkClientInterface};

use crate::cli::OutputFormat;
use crate::friendly::friendly;
use crate::refspec::{self, RefKind};
use crate::{client, output, style};

pub async fn run(kind: RefKind, groups: Vec<String>, format: OutputFormat) -> Result<()> {
    let client = client::connect_default().await?;
    let scope = (!groups.is_empty()).then_some(Scope {
        projects: None,
        groups: Some(groups),
    });

    match kind {
        RefKind::Issue => {
            let reply = client
                .get_assigned_work_items(scope)
                .call()
                .await
                .map_err(|e| friendly("GetAssignedWorkItems", e))?;
            output::emit(format, &reply.work_items, |issues| {
                for i in issues {
                    outln!(
                        "{:<6} {:<8} {}  {}",
                        style::reference(refspec::sigil(kind), i.iid),
                        style::state(&i.state),
                        i.title,
                        i.web_url
                    )?;
                }
                Ok(())
            })
        }
        RefKind::Mr => {
            let reply = client
                .get_assigned_merge_requests(scope)
                .call()
                .await
                .map_err(|e| friendly("GetAssignedMergeRequests", e))?;
            output::emit(format, &reply.merge_requests, |mrs| {
                for m in mrs {
                    outln!(
                        "{:<6} {:<8} {}  {}",
                        style::reference(refspec::sigil(kind), m.iid),
                        style::state(&m.state),
                        m.title,
                        m.web_url
                    )?;
                }
                Ok(())
            })
        }
    }
}
