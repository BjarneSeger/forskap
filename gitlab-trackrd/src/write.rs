//! GitLab write operations, shared by the handlers' first attempt and the
//! retry queue's replays so both run the exact same call.

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::gitlab::{GitlabApi, Issuable};

/// What a write does. Persisted inside queued and dead-lettered tasks, so the
/// variant names and field aliases are an on-disk format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WriteOp {
    PostTime {
        duration: String,
        summary: Option<String>,
        /// Global numeric issuable ID (issue or MR, per the task's `kind`),
        /// resolved from the caches at enqueue time. When present, a replay
        /// uses GraphQL `timelogCreate` so it can submit the original enqueue
        /// time as `spentAt`. `None` means the caches didn't know the issuable
        /// (or the entry survived a daemon upgrade), and the replay falls back
        /// to REST without `spent_at`. The alias keeps tasks persisted before
        /// MR support readable.
        #[serde(alias = "issue_id")]
        issuable_id: Option<i64>,
    },
    #[serde(alias = "CloseIssue")]
    Close,
    AssignSelf,
    UnassignSelf,
}

impl WriteOp {
    /// Whether repeating the op after an ambiguous failure is harmless.
    /// PostTime adds a new timelog per call; the others converge on a state.
    pub fn idempotent(&self) -> bool {
        !matches!(self, WriteOp::PostTime { .. })
    }

    pub fn name(&self) -> &'static str {
        match self {
            WriteOp::PostTime { .. } => "PostTime",
            WriteOp::Close => "Close",
            WriteOp::AssignSelf => "AssignSelf",
            WriteOp::UnassignSelf => "UnassignSelf",
        }
    }

    /// Human-readable op detail for the `tt queue` view. PostTime shows its
    /// duration (and summary, if any); the other ops carry no extra detail.
    pub fn detail(&self) -> String {
        match self {
            WriteOp::PostTime {
                duration, summary, ..
            } => match summary {
                Some(s) if !s.is_empty() => format!("{duration} – {s}"),
                _ => duration.clone(),
            },
            WriteOp::Close | WriteOp::AssignSelf | WriteOp::UnassignSelf => String::new(),
        }
    }
}

/// One write against one issuable. Persisted in the sync store's noted
/// writes, so the fields are an on-disk format.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Write {
    pub kind: Issuable,
    pub project_id: i64,
    pub iid: i64,
    pub op: WriteOp,
}

impl Write {
    /// Perform the write once. `queued_at_secs` is set for a replay of a
    /// queued write: a PostTime with a known issuable id then goes through
    /// GraphQL so GitLab records the original time rather than now.
    pub async fn apply(&self, gitlab: &dyn GitlabApi, queued_at_secs: Option<u64>) -> Result<()> {
        let (kind, project_id, iid) = (self.kind, self.project_id, self.iid);
        match &self.op {
            WriteOp::PostTime {
                duration,
                summary,
                issuable_id,
            } => match (issuable_id, queued_at_secs) {
                (Some(id), Some(queued_at)) => {
                    let spent_at =
                        chrono::DateTime::<chrono::Utc>::from_timestamp(queued_at as i64, 0)
                            .unwrap_or_else(chrono::Utc::now);
                    gitlab
                        .create_timelog(
                            kind,
                            *id,
                            duration,
                            summary.as_deref().unwrap_or(""),
                            spent_at,
                        )
                        .await
                }
                _ => {
                    gitlab
                        .add_spent_time(kind, project_id, iid, duration, summary.as_deref())
                        .await
                }
            },
            WriteOp::Close => gitlab.close(kind, project_id, iid).await,
            WriteOp::AssignSelf => gitlab.assign_self(kind, project_id, iid).await,
            WriteOp::UnassignSelf => gitlab.unassign_self(kind, project_id, iid).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_post_time_is_non_idempotent() {
        let post = WriteOp::PostTime {
            duration: "1h".into(),
            summary: None,
            issuable_id: None,
        };
        assert_eq!(post.name(), "PostTime");
        assert!(!post.idempotent());
        for op in [WriteOp::Close, WriteOp::AssignSelf, WriteOp::UnassignSelf] {
            assert!(op.idempotent(), "{}", op.name());
        }
    }
}
