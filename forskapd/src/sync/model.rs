//! Typed mirrors of the GitLab resources the daemon caches.
//!
//! Field names follow GitLab's REST JSON, so a response row deserializes
//! straight into these types; fields the daemon doesn't use are ignored.
//! Deserialization is lenient (nulls, non-string labels, string-or-integer
//! timestamps), so the same impls read both GitLab responses and stored rows.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::gitlab::Issuable;

/// Storage key of a row. Both halves are big-endian encoded, so the first
/// half is the scan prefix: the project for issues, MRs and boards, the group
/// for epics, the time for timelogs and events.
pub type RowKey = (u64, u64);

/// A GitLab resource the sync layer stores as one row per item.
pub trait Resource: Serialize + DeserializeOwned + Clone + Send + Sync + 'static {
    /// Plural name for logs, e.g. `"issues"`.
    const NAME: &'static str;
    /// The fjall keyspace holding the rows.
    const KEYSPACE: &'static str;
    /// Bump when stored rows gain data that only a full refetch can fill;
    /// every job syncing this resource then runs full once.
    const SCHEMA: u32;

    fn key(&self) -> RowKey;

    /// Whether a fetched row is usable; malformed rows (id 0, …) are dropped.
    fn is_valid(&self) -> bool;
}

/// A user as embedded in issue/MR `assignees`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UserRef {
    #[serde(default, deserialize_with = "de::nullable")]
    pub id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub username: String,
}

/// The epic an issue belongs to, as embedded in the issue.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EpicRef {
    /// The legacy epic id, not the epic's work item id.
    #[serde(default, deserialize_with = "de::lenient")]
    pub id: i64,
    #[serde(default, deserialize_with = "de::lenient")]
    pub iid: i64,
    #[serde(default, deserialize_with = "de::lenient")]
    pub group_id: i64,
    #[serde(default, deserialize_with = "de::lenient")]
    pub title: String,
    /// Relative to the instance (`/groups/team/-/epics/5`), as GitLab sends it.
    #[serde(default, deserialize_with = "de::lenient")]
    pub url: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TimeStats {
    /// Human-readable total, e.g. `"1h 30m"`; GitLab sends `null` for none.
    #[serde(default, deserialize_with = "de::nullable")]
    pub human_total_time_spent: String,
}

/// `GET /issues`, `/projects/:id/issues`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Issue {
    #[serde(default, deserialize_with = "de::nullable")]
    pub id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub iid: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub project_id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub title: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub web_url: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub state: String,
    /// `"issue"`, `"task"`, `"incident"`, `"test_case"`, …; empty in a row
    /// stored before schema 2.
    #[serde(default, deserialize_with = "de::lenient")]
    pub issue_type: String,
    #[serde(default, deserialize_with = "de::labels")]
    pub labels: Vec<String>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub assignees: Vec<UserRef>,
    #[serde(default)]
    pub epic: Option<EpicRef>,
    #[serde(default)]
    pub time_stats: Option<TimeStats>,
    /// Unix seconds.
    #[serde(default, deserialize_with = "de::timestamp")]
    pub updated_at: u64,
}

impl Issue {
    /// Its work item type; a row without one is an issue.
    pub fn work_item_type(&self) -> &str {
        match self.issue_type.as_str() {
            "" => "issue",
            t => t,
        }
    }

    /// Human-readable `total_time_spent`, empty when none was logged.
    pub fn total_time(&self) -> &str {
        self.time_stats
            .as_ref()
            .map_or("", |t| t.human_total_time_spent.as_str())
    }
}

/// `GET /merge_requests`, `/projects/:id/merge_requests`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MergeRequest {
    #[serde(default, deserialize_with = "de::nullable")]
    pub id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub iid: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub project_id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub title: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub web_url: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "de::labels")]
    pub labels: Vec<String>,
    #[serde(default, deserialize_with = "de::nullable")]
    pub assignees: Vec<UserRef>,
    /// Unix seconds.
    #[serde(default, deserialize_with = "de::timestamp")]
    pub updated_at: u64,
}

/// `GET /projects`, the full representation: the `simple=true` one has no
/// `archived`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Project {
    #[serde(default, deserialize_with = "de::nullable")]
    pub id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    /// e.g. `"team/backend/api"`.
    #[serde(default, deserialize_with = "de::nullable")]
    pub path_with_namespace: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub web_url: String,
    /// Empty when the project has no avatar.
    #[serde(default, deserialize_with = "de::nullable")]
    pub avatar_url: String,
    /// An archived project is read-only on GitLab. A row stored before
    /// schema 3 reads as `false`.
    #[serde(default, deserialize_with = "de::nullable")]
    pub archived: bool,
    /// Who may use the project's issues, and with them its issue boards:
    /// `"disabled"`, `"private"` (members only) or `"enabled"`. Empty where
    /// GitLab didn't say: an instance from before the access levels, or a
    /// row stored before schema 4.
    #[serde(default, deserialize_with = "de::lenient")]
    pub issues_access_level: String,
    /// The same for its merge requests.
    #[serde(default, deserialize_with = "de::lenient")]
    pub merge_requests_access_level: String,
    /// The same for its repository, without which there are no merge
    /// requests either.
    #[serde(default, deserialize_with = "de::lenient")]
    pub repository_access_level: String,
    /// The flag instances had before `issues_access_level` (deprecated
    /// since); `None` where absent.
    #[serde(default, deserialize_with = "de::lenient")]
    pub issues_enabled: Option<bool>,
    /// The same for `merge_requests_access_level`.
    #[serde(default, deserialize_with = "de::lenient")]
    pub merge_requests_enabled: Option<bool>,
}

impl Project {
    /// Whether the project says its issues are switched off: then neither
    /// its issues nor its issue boards can be read. What it doesn't say
    /// doesn't switch anything off.
    pub fn issues_disabled(&self) -> bool {
        disabled(&self.issues_access_level, self.issues_enabled)
    }

    /// Whether the project says its merge requests are switched off, or its
    /// repository, which they need.
    pub fn merge_requests_disabled(&self) -> bool {
        disabled(
            &self.merge_requests_access_level,
            self.merge_requests_enabled,
        ) || disabled(&self.repository_access_level, None)
    }
}

/// A feature's access level, or the legacy flag where the level is missing,
/// says it is off.
fn disabled(level: &str, enabled: Option<bool>) -> bool {
    match level {
        "disabled" => true,
        "" => enabled == Some(false),
        _ => false,
    }
}

/// `GET /groups`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Group {
    #[serde(default, deserialize_with = "de::nullable")]
    pub id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
    /// e.g. `"team/backend"`.
    #[serde(default, deserialize_with = "de::nullable")]
    pub full_path: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub web_url: String,
}

/// `GET /groups/:id/epics`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Epic {
    #[serde(default, deserialize_with = "de::nullable")]
    pub id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub iid: i64,
    /// The group the epic itself belongs to.
    #[serde(default, deserialize_with = "de::nullable")]
    pub group_id: i64,
    /// The id of the epic's work item; `id` is the legacy epic id. GitLab
    /// sends it from 18.4 on; 0 in a row stored before schema 2.
    #[serde(default, deserialize_with = "de::lenient")]
    pub work_item_id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub title: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub web_url: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub state: String,
    #[serde(default, deserialize_with = "de::labels")]
    pub labels: Vec<String>,
    /// Unix seconds.
    #[serde(default, deserialize_with = "de::timestamp")]
    pub updated_at: u64,
}

/// One of the user's own contribution events (`GET /events`): opened
/// issues/MRs, pushes, comments, approvals, …
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Event {
    #[serde(default, deserialize_with = "de::nullable")]
    pub id: i64,
    /// 0 for events outside a project (group or user level).
    #[serde(default, deserialize_with = "de::nullable")]
    pub project_id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub action_name: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub target_type: String,
    /// The target's number within the project; 0 when the event has none
    /// (joining). On a comment it is the note's id, on a push the project's:
    /// see [`Self::target`].
    #[serde(default, deserialize_with = "de::nullable")]
    pub target_iid: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub target_title: String,
    /// Set on pushes only.
    #[serde(default, deserialize_with = "de::nullable")]
    pub push_data: PushData,
    /// Set on comments only.
    #[serde(default, deserialize_with = "de::nullable")]
    pub note: NoteRef,
    /// Unix seconds.
    #[serde(default, deserialize_with = "de::timestamp")]
    pub created_at: u64,
}

/// What a push event pushed.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PushData {
    /// Branch or tag name.
    #[serde(default, rename = "ref", deserialize_with = "de::nullable")]
    pub git_ref: String,
    #[serde(default, deserialize_with = "de::nullable")]
    pub commit_count: i64,
    /// Title of the newest commit; empty when the push deleted the ref.
    #[serde(default, deserialize_with = "de::nullable")]
    pub commit_title: String,
}

/// What a comment event commented on, and how the comment starts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NoteRef {
    /// The note's first line, capped: the full text is never stored.
    #[serde(default, deserialize_with = "de::excerpt")]
    pub body: String,
    /// `"Issue"`, `"MergeRequest"`, `"Commit"`, …
    #[serde(default, deserialize_with = "de::nullable")]
    pub noteable_type: String,
    /// 0 where the commented thing has no number (a commit).
    #[serde(default, deserialize_with = "de::nullable")]
    pub noteable_iid: i64,
}

impl Event {
    /// The kind and number of what the event is about, empty and 0 where
    /// there is none. A comment's own target is the note, so it answers
    /// with what was commented on. GitLab targets a push at its project and
    /// sends the project's id as the number, which is none.
    pub fn target(&self) -> (&str, i64) {
        if !self.note.noteable_type.is_empty() {
            (&self.note.noteable_type, self.note.noteable_iid)
        } else if self.target_type == "Project" {
            (&self.target_type, 0)
        } else {
            (&self.target_type, self.target_iid)
        }
    }

    /// Whether the event shows the user working in its project. Membership
    /// changes don't: being added to a project isn't activity in it.
    pub fn is_activity(&self) -> bool {
        self.project_id > 0 && !matches!(self.action_name.as_str(), "joined" | "left" | "expired")
    }

    /// Whether the event shows the user is a member of its project: joining
    /// is membership, creating a project makes its owner, and only members
    /// push.
    pub fn implies_membership(&self) -> bool {
        self.project_id > 0
            && match self.action_name.as_str() {
                "joined" | "pushed to" | "pushed new" => true,
                // Targeted at the project itself; older GitLab sent no target.
                "created" => matches!(self.target_type.as_str(), "" | "Project"),
                _ => false,
            }
    }
}

/// A timelog of the authenticated user, from GraphQL `currentUser.timelogs`
/// (GitLab has no REST listing for them).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Timelog {
    pub id: u64,
    /// Unix seconds.
    pub spent_at: u64,
    #[serde(default)]
    pub kind: Issuable,
    /// 0 when GitLab returned no project.
    #[serde(default)]
    pub project_id: i64,
    pub iid: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub web_url: String,
    /// Seconds.
    pub time_spent: u64,
    #[serde(default)]
    pub summary: String,
}

/// `GET /projects/:id/boards`, lists embedded.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Board {
    #[serde(default, deserialize_with = "de::nullable")]
    pub id: i64,
    /// Not in GitLab's JSON; stamped by the sync job that fetched the board.
    #[serde(default)]
    pub project_id: i64,
    #[serde(default, deserialize_with = "de::nullable")]
    pub lists: Vec<BoardList>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BoardList {
    /// `None` for the label-less backlog/closed lists.
    #[serde(default)]
    pub label: Option<LabelRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LabelRef {
    #[serde(default, deserialize_with = "de::nullable")]
    pub name: String,
}

impl Board {
    /// Label names of the board's lists, in list order.
    pub fn labels(&self) -> impl Iterator<Item = &str> {
        self.lists
            .iter()
            .filter_map(|l| l.label.as_ref())
            .map(|l| l.name.as_str())
            .filter(|n| !n.is_empty())
    }
}

fn positive(v: i64) -> u64 {
    v.max(0) as u64
}

impl Resource for Issue {
    const NAME: &'static str = "issues";
    const KEYSPACE: &'static str = "gl_issues_v1";
    /// 2: the work item type and the epic's id, number, group and title.
    const SCHEMA: u32 = 2;
    fn key(&self) -> RowKey {
        (positive(self.project_id), positive(self.iid))
    }
    fn is_valid(&self) -> bool {
        self.id > 0 && self.iid > 0 && self.project_id > 0
    }
}

impl Resource for MergeRequest {
    const NAME: &'static str = "merge requests";
    const KEYSPACE: &'static str = "gl_merge_requests_v1";
    const SCHEMA: u32 = 1;
    fn key(&self) -> RowKey {
        (positive(self.project_id), positive(self.iid))
    }
    fn is_valid(&self) -> bool {
        self.id > 0 && self.iid > 0 && self.project_id > 0
    }
}

impl Resource for Project {
    const NAME: &'static str = "projects";
    const KEYSPACE: &'static str = "gl_projects_v1";
    /// 4: the feature access levels.
    const SCHEMA: u32 = 4;
    fn key(&self) -> RowKey {
        (positive(self.id), 0)
    }
    fn is_valid(&self) -> bool {
        self.id > 0
    }
}

impl Resource for Group {
    const NAME: &'static str = "groups";
    const KEYSPACE: &'static str = "gl_groups_v1";
    const SCHEMA: u32 = 1;
    fn key(&self) -> RowKey {
        (positive(self.id), 0)
    }
    fn is_valid(&self) -> bool {
        self.id > 0
    }
}

impl Resource for Epic {
    const NAME: &'static str = "epics";
    const KEYSPACE: &'static str = "gl_epics_v1";
    /// 2: the work item id.
    const SCHEMA: u32 = 2;
    fn key(&self) -> RowKey {
        (positive(self.group_id), positive(self.iid))
    }
    fn is_valid(&self) -> bool {
        self.id > 0 && self.iid > 0 && self.group_id > 0
    }
}

impl Resource for Event {
    const NAME: &'static str = "events";
    const KEYSPACE: &'static str = "gl_events_v1";
    const SCHEMA: u32 = 3;
    fn key(&self) -> RowKey {
        (self.created_at, positive(self.id))
    }
    /// A row without a readable `created_at` would sort before every
    /// window; dropping it here counts it as malformed.
    fn is_valid(&self) -> bool {
        self.id > 0 && self.created_at > 0
    }
}

impl Resource for Timelog {
    const NAME: &'static str = "timelogs";
    const KEYSPACE: &'static str = "gl_timelogs_v1";
    const SCHEMA: u32 = 1;
    fn key(&self) -> RowKey {
        (self.spent_at, self.id)
    }
    fn is_valid(&self) -> bool {
        self.id > 0 && self.iid > 0
    }
}

impl Resource for Board {
    const NAME: &'static str = "boards";
    const KEYSPACE: &'static str = "gl_boards_v1";
    const SCHEMA: u32 = 1;
    fn key(&self) -> RowKey {
        (positive(self.project_id), positive(self.id))
    }
    fn is_valid(&self) -> bool {
        self.id > 0 && self.project_id > 0
    }
}

/// Lenient field deserializers shared by the mirror types.
mod de {
    use std::fmt;

    use serde::de::{IgnoredAny, MapAccess, SeqAccess, Visitor};
    use serde::{Deserialize, Deserializer};
    use serde_json::Value;

    /// `null` reads as the type's default.
    pub fn nullable<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: Default + Deserialize<'de>,
    {
        Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
    }

    /// Like [`nullable`], and a value of another type reads as the default
    /// too: a field the daemon can do without must not cost the whole row.
    pub fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: Default + serde::de::DeserializeOwned,
    {
        let value = Option::<Value>::deserialize(d)?.unwrap_or_default();
        Ok(serde_json::from_value(value).unwrap_or_default())
    }

    /// Longest excerpt kept of a text, in characters.
    const EXCERPT_CHARS: usize = 200;

    /// The first non-blank line of a text, cut to [`EXCERPT_CHARS`] with a
    /// closing `…`. Cutting while reading keeps the rest out of the store.
    pub fn excerpt<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
        let text = Option::<String>::deserialize(d)?.unwrap_or_default();
        let line = text.lines().map(str::trim).find(|l| !l.is_empty());
        let line = line.unwrap_or_default();
        // A stored excerpt is within the cap, so reading it back changes nothing.
        let Some((end, _)) = line.char_indices().nth(EXCERPT_CHARS - 1) else {
            return Ok(line.to_string());
        };
        if line[end..].chars().nth(1).is_none() {
            return Ok(line.to_string());
        }
        Ok(format!("{}…", line[..end].trim_end()))
    }

    /// A label array; non-string entries are skipped. Hand-rolled because
    /// search decodes every stored row, and going through `Value` allocated
    /// per label.
    pub fn labels<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        Ok(Option::<Labels>::deserialize(d)?.map_or_else(Vec::new, |l| l.0))
    }

    struct Labels(Vec<String>);

    impl<'de> Deserialize<'de> for Labels {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Seq;
            impl<'de> Visitor<'de> for Seq {
                type Value = Labels;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a label array")
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Labels, A::Error> {
                    let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                    while let Some(label) = seq.next_element::<Label>()? {
                        out.extend(label.0);
                    }
                    Ok(Labels(out))
                }
            }
            d.deserialize_seq(Seq)
        }
    }

    /// One label entry: the string, or `None` for anything else.
    struct Label(Option<String>);

    impl<'de> Deserialize<'de> for Label {
        fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            struct Any;
            impl<'de> Visitor<'de> for Any {
                type Value = Label;
                fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                    f.write_str("a label")
                }
                fn visit_str<E>(self, v: &str) -> Result<Label, E> {
                    Ok(Label(Some(v.to_string())))
                }
                fn visit_string<E>(self, v: String) -> Result<Label, E> {
                    Ok(Label(Some(v)))
                }
                fn visit_bool<E>(self, _: bool) -> Result<Label, E> {
                    Ok(Label(None))
                }
                fn visit_i64<E>(self, _: i64) -> Result<Label, E> {
                    Ok(Label(None))
                }
                fn visit_u64<E>(self, _: u64) -> Result<Label, E> {
                    Ok(Label(None))
                }
                fn visit_f64<E>(self, _: f64) -> Result<Label, E> {
                    Ok(Label(None))
                }
                fn visit_unit<E>(self) -> Result<Label, E> {
                    Ok(Label(None))
                }
                fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Label, A::Error> {
                    while seq.next_element::<IgnoredAny>()?.is_some() {}
                    Ok(Label(None))
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Label, A::Error> {
                    while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                    Ok(Label(None))
                }
            }
            d.deserialize_any(Any)
        }
    }

    /// Unix seconds from an RFC 3339 string (GitLab) or an integer (stored
    /// rows); anything else reads as 0, which sorts oldest.
    pub fn timestamp<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
        Ok(match Option::<Value>::deserialize(d)? {
            Some(Value::String(s)) => chrono::DateTime::parse_from_rfc3339(&s)
                .map(|t| t.timestamp().max(0) as u64)
                .unwrap_or(0),
            Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
            _ => 0,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn issue_reads_a_gitlab_row() {
        let v = json!({
            "id": 123, "iid": 7, "project_id": 9,
            "title": "Fix it", "web_url": "https://gl/g/p/-/issues/7", "state": "opened",
            "issue_type": "task",
            "epic": {
                "id": 3, "iid": 1, "title": "Roadmap", "url": "/groups/g/-/epics/1",
                "group_id": 4, "human_readable_end_date": null,
            },
            "time_stats": { "human_total_time_spent": "2h", "time_estimate": 0 },
            "labels": ["bug", 42, null, "high", {"name": "x"}, ["y"], true],
            "assignees": [{ "id": 5, "username": "me", "name": "Me" }],
            "updated_at": "2026-07-01T10:00:00.000Z",
            "description": "ignored",
        });
        let i: Issue = serde_json::from_value(v).unwrap();
        assert_eq!(i.key(), (9, 7));
        assert!(i.is_valid());
        assert_eq!(i.work_item_type(), "task");
        assert_eq!(
            i.epic,
            Some(EpicRef {
                id: 3,
                iid: 1,
                group_id: 4,
                title: "Roadmap".into(),
                url: "/groups/g/-/epics/1".into(),
            })
        );
        assert_eq!(i.total_time(), "2h");
        assert_eq!(i.labels, ["bug", "high"], "non-string labels skipped");
        assert_eq!(i.assignees[0].username, "me");
        assert_eq!(i.updated_at, 1_782_900_000);
    }

    #[test]
    fn nulls_and_missing_fields_read_as_defaults() {
        let v = json!({
            "id": 1, "iid": 1, "project_id": 1,
            "title": null, "epic": null,
            "time_stats": { "human_total_time_spent": null },
            "assignees": null, "labels": null, "updated_at": null,
        });
        let i: Issue = serde_json::from_value(v).unwrap();
        assert_eq!(i.title, "");
        assert_eq!(i.epic, None);
        assert_eq!(i.work_item_type(), "issue");
        assert_eq!(i.total_time(), "");
        assert!(i.assignees.is_empty() && i.labels.is_empty());
        assert_eq!(i.updated_at, 0);

        let bare: Issue = serde_json::from_value(json!({})).unwrap();
        assert!(!bare.is_valid(), "an id-less row is dropped at ingestion");
    }

    /// Stored rows serialize `updated_at` as an integer; reading them back
    /// must go through the same lenient impls.
    #[test]
    fn stored_rows_round_trip() {
        let mr = MergeRequest {
            id: 5,
            iid: 2,
            project_id: 9,
            title: "t".into(),
            labels: vec!["x".into()],
            assignees: vec![UserRef {
                id: 42,
                username: "me".into(),
            }],
            updated_at: 1_700_000_000,
            ..Default::default()
        };
        let back: MergeRequest = serde_json::from_slice(&serde_json::to_vec(&mr).unwrap()).unwrap();
        assert_eq!(back, mr);

        let t = Timelog {
            id: 11,
            spent_at: 100,
            kind: Issuable::MergeRequest,
            project_id: 7,
            iid: 5,
            time_spent: 1800,
            ..Default::default()
        };
        let back: Timelog = serde_json::from_slice(&serde_json::to_vec(&t).unwrap()).unwrap();
        assert_eq!(back, t);
    }

    #[test]
    fn epic_reads_a_gitlab_row() {
        let e: Epic = serde_json::from_value(json!({
            "id": 30, "iid": 5, "group_id": 3, "parent_id": null,
            "title": "Accounts", "state": "opened",
            "web_url": "https://gl/groups/team/-/epics/5",
            "labels": ["roadmap"], "updated_at": "2026-07-01T10:00:00.000Z",
        }))
        .unwrap();
        assert_eq!(e.key(), (3, 5), "keyed by its group");
        assert!(e.is_valid());
        assert_eq!(e.labels, ["roadmap"]);
        assert_eq!(e.updated_at, 1_782_900_000);

        let groupless: Epic = serde_json::from_value(json!({"id": 30, "iid": 5})).unwrap();
        assert!(!groupless.is_valid());
    }

    /// The legacy `id` stays the epic's own next to its work item's.
    #[test]
    fn epic_reads_its_work_item_id() {
        let e: Epic = serde_json::from_value(json!({
            "id": 30, "iid": 5, "group_id": 3, "work_item_id": 9001,
        }))
        .unwrap();
        assert_eq!((e.id, e.work_item_id), (30, 9001));
        let stored = serde_json::to_vec(&e).unwrap();
        assert_eq!(serde_json::from_slice::<Epic>(&stored).unwrap(), e);

        // A row stored before schema 2, and odd values.
        for odd in [
            json!({"id": 30, "iid": 5, "group_id": 3}),
            json!({"id": 30, "iid": 5, "group_id": 3, "work_item_id": null}),
            json!({"id": 30, "iid": 5, "group_id": 3, "work_item_id": "x"}),
        ] {
            let e: Epic = serde_json::from_value(odd.clone()).unwrap();
            assert_eq!(e.work_item_id, 0, "{odd}");
            assert!(e.is_valid(), "{odd}");
        }
    }

    /// An issue's epic as a row stored before schema 2 has it: the link only.
    #[test]
    fn an_old_epic_ref_reads_with_its_link_only() {
        let i: Issue = serde_json::from_value(json!({
            "id": 1, "iid": 1, "project_id": 1, "epic": { "url": "/groups/g/-/epics/1" },
        }))
        .unwrap();
        let epic = i.epic.unwrap();
        assert_eq!((epic.iid, epic.group_id), (0, 0));
        assert_eq!(epic.url, "/groups/g/-/epics/1");
    }

    #[test]
    fn project_reads_its_avatar_url() {
        let p: Project = serde_json::from_value(json!({
            "id": 7, "name": "API", "path_with_namespace": "team/api",
            "avatar_url": "https://gl/uploads/-/system/project/avatar/7/logo.png",
        }))
        .unwrap();
        assert_eq!(
            p.avatar_url,
            "https://gl/uploads/-/system/project/avatar/7/logo.png"
        );
        for none in [json!({"id": 7, "avatar_url": null}), json!({"id": 7})] {
            let p: Project = serde_json::from_value(none).unwrap();
            assert_eq!(p.avatar_url, "");
        }
    }

    #[test]
    fn project_reads_its_archived_flag() {
        let p: Project = serde_json::from_value(json!({
            "id": 7, "name": "API", "path_with_namespace": "team/api",
            "archived": true, "visibility": "private",
        }))
        .unwrap();
        assert!(p.archived);
        // The stored form reads back the same.
        let stored = serde_json::to_vec(&p).unwrap();
        assert_eq!(serde_json::from_slice::<Project>(&stored).unwrap(), p);

        // A row stored before schema 3 has no such field.
        let old: Project = serde_json::from_str(
            r#"{"id":7,"name":"API","path_with_namespace":"team/api","web_url":"","avatar_url":""}"#,
        )
        .unwrap();
        assert!(!old.archived);
        for none in [json!({"id": 7, "archived": null}), json!({"id": 7})] {
            let p: Project = serde_json::from_value(none).unwrap();
            assert!(!p.archived);
        }
    }

    /// gitlab.com's full representation carries both the access levels and
    /// the deprecated flags; the levels decide.
    #[test]
    fn project_reads_its_feature_levels() {
        let full = |issues: &str, mrs: &str, repository: &str| -> Project {
            serde_json::from_value(json!({
                "id": 7, "name": "API", "path_with_namespace": "team/api",
                "issues_enabled": issues != "disabled",
                "merge_requests_enabled": mrs != "disabled",
                "issues_access_level": issues,
                "merge_requests_access_level": mrs,
                "repository_access_level": repository,
                "wiki_access_level": "disabled",
                "builds_access_level": "enabled",
            }))
            .unwrap()
        };
        let on = full("enabled", "private", "enabled");
        assert_eq!(on.issues_access_level, "enabled");
        assert_eq!(on.merge_requests_access_level, "private");
        assert_eq!(on.issues_enabled, Some(true));
        assert!(!on.issues_disabled() && !on.merge_requests_disabled());
        // Members only is still on: the account may be one.
        assert!(!full("private", "private", "private").issues_disabled());

        let no_issues = full("disabled", "enabled", "enabled");
        assert!(no_issues.issues_disabled());
        assert!(!no_issues.merge_requests_disabled());
        let no_mrs = full("enabled", "disabled", "enabled");
        assert!(!no_mrs.issues_disabled());
        assert!(no_mrs.merge_requests_disabled());
        // No repository, no merge requests.
        assert!(full("enabled", "enabled", "disabled").merge_requests_disabled());

        // The stored form reads back the same.
        let stored = serde_json::to_vec(&no_mrs).unwrap();
        assert_eq!(serde_json::from_slice::<Project>(&stored).unwrap(), no_mrs);
    }

    /// An instance from before the access levels only has the flags.
    #[test]
    fn project_reads_the_legacy_feature_flags() {
        let legacy = |issues: bool, mrs: bool| -> Project {
            serde_json::from_value(json!({
                "id": 7, "issues_enabled": issues, "merge_requests_enabled": mrs,
            }))
            .unwrap()
        };
        assert!(legacy(false, true).issues_disabled());
        assert!(!legacy(false, true).merge_requests_disabled());
        assert!(!legacy(true, false).issues_disabled());
        assert!(legacy(true, false).merge_requests_disabled());
        assert!(
            !legacy(true, true).issues_disabled() && !legacy(true, true).merge_requests_disabled()
        );
    }

    /// What the project doesn't say switches nothing off: a row stored
    /// before schema 4, a representation without the fields, nulls, and
    /// values of another type, which must not cost the row either.
    #[test]
    fn unknown_feature_levels_switch_nothing_off() {
        let old: Project = serde_json::from_str(
            r#"{"id":7,"name":"API","path_with_namespace":"team/api","web_url":"","avatar_url":"","archived":true}"#,
        )
        .unwrap();
        assert!(old.archived);
        assert_eq!(old.issues_access_level, "");
        assert_eq!(old.issues_enabled, None);
        assert!(!old.issues_disabled() && !old.merge_requests_disabled());
        for odd in [
            json!({"id": 7}),
            json!({"id": 7, "issues_access_level": null, "issues_enabled": null,
                   "merge_requests_access_level": null, "merge_requests_enabled": null}),
            json!({"id": 7, "issues_access_level": 0, "issues_enabled": "no",
                   "merge_requests_access_level": ["disabled"], "merge_requests_enabled": 0,
                   "repository_access_level": {"level": "disabled"}}),
            json!({"id": 7, "issues_access_level": "someday", "merge_requests_access_level": "public"}),
        ] {
            let p: Project = serde_json::from_value(odd.clone()).unwrap();
            assert!(p.is_valid(), "{odd}");
            assert!(!p.issues_disabled(), "{odd}");
            assert!(!p.merge_requests_disabled(), "{odd}");
        }
    }

    #[test]
    fn membership_events_are_not_activity() {
        let e = |action: &str, project_id| Event {
            id: 1,
            project_id,
            action_name: action.into(),
            ..Default::default()
        };
        for action in [
            "pushed to",
            "pushed new",
            "opened",
            "commented on",
            "accepted",
        ] {
            assert!(e(action, 3).is_activity(), "{action}");
        }
        for action in ["joined", "left", "expired"] {
            assert!(!e(action, 3).is_activity(), "{action}");
        }
        assert!(
            !e("opened", 0).is_activity(),
            "group-level events have no project"
        );
    }

    #[test]
    fn creating_a_project_implies_membership() {
        let e = |action: &str, target_type: Value, project_id| -> Event {
            serde_json::from_value(json!({
                "id": 1, "project_id": project_id, "action_name": action,
                "target_type": target_type, "target_id": 7, "target_iid": 7,
                "target_title": "Api Tests", "created_at": "2026-01-02T03:04:05Z",
            }))
            .unwrap()
        };
        // gitlab.com's shape, and the target-less one of older versions.
        assert!(e("created", json!("Project"), 7).implies_membership());
        assert!(e("created", Value::Null, 7).implies_membership());
        assert!(!e("created", json!("WikiPage::Meta"), 7).implies_membership());
        assert!(!e("opened", json!("Issue"), 7).implies_membership());
        assert!(!e("created", json!("Project"), 0).implies_membership());
    }

    #[test]
    fn events_read_pushes_comments_and_rows_of_the_old_shape() {
        let push: Event = serde_json::from_value(json!({
            "id": 1, "project_id": 7, "action_name": "pushed to",
            "target_type": null, "target_iid": null, "target_title": null,
            "created_at": "2026-01-02T03:04:05Z",
            "push_data": {
                "commit_count": 3, "action": "pushed", "ref_type": "branch",
                "ref": "main", "commit_title": "Fix it",
            },
        }))
        .unwrap();
        assert_eq!(push.target(), ("", 0));
        assert_eq!(push.push_data.git_ref, "main");
        assert_eq!(push.push_data.commit_count, 3);
        assert_eq!(push.push_data.commit_title, "Fix it");
        // The stored form reads back the same.
        let stored = serde_json::to_vec(&push).unwrap();
        assert_eq!(serde_json::from_slice::<Event>(&stored).unwrap(), push);

        let comment: Event = serde_json::from_value(json!({
            "id": 2, "project_id": 7, "action_name": "commented on",
            "target_type": "DiffNote", "target_iid": 9001, "target_title": "Add x",
            "created_at": "2026-01-02T03:04:05Z",
            "note": { "id": 9001, "body": "lgtm", "noteable_type": "MergeRequest", "noteable_iid": 12 },
        }))
        .unwrap();
        assert_eq!(comment.target(), ("MergeRequest", 12));
        assert_eq!(comment.note.body, "lgtm");

        // A push that deleted its branch, and rows stored before schema 2 and 3.
        let deleted: Event = serde_json::from_value(json!({
            "id": 3, "push_data": { "commit_count": 0, "ref": "old", "commit_title": null },
        }))
        .unwrap();
        assert_eq!(deleted.push_data.commit_title, "");
        let old: Event = serde_json::from_str(
            r#"{"id":4,"project_id":7,"action_name":"opened","target_type":"Issue","created_at":5}"#,
        )
        .unwrap();
        assert_eq!(old.target(), ("Issue", 0));
        assert_eq!(old.created_at, 5);
        assert_eq!(old.push_data, PushData::default());
        let old: Event = serde_json::from_str(
            r#"{"id":5,"created_at":5,"note":{"noteable_type":"Issue","noteable_iid":3}}"#,
        )
        .unwrap();
        assert_eq!(old.target(), ("Issue", 3));
        assert_eq!(old.note.body, "");
    }

    #[test]
    fn an_event_without_a_readable_timestamp_is_malformed() {
        let e: Event = serde_json::from_value(json!({"id": 1, "created_at": "yesterday"})).unwrap();
        assert_eq!(e.created_at, 0);
        assert!(!e.is_valid());
    }

    #[test]
    fn a_comment_keeps_only_the_start_of_its_first_line() {
        let note = |body: Value| -> NoteRef {
            serde_json::from_value(json!({ "body": body, "noteable_type": "Issue" })).unwrap()
        };
        assert_eq!(
            note(json!("\n  Looks good \r\n\nbut: the rest")).body,
            "Looks good"
        );
        assert_eq!(note(Value::Null).body, "");
        assert_eq!(note(json!(" \n\n")).body, "");

        // Cut by characters, not bytes: a multi-byte text is not split.
        let exact = "ä".repeat(200);
        assert_eq!(note(json!(exact)).body, exact);
        let long = note(json!(format!("{}\nsecret", "日本".repeat(150)))).body;
        assert_eq!(long, format!("{}日…", "日本".repeat(99)));
        assert_eq!(long.chars().count(), 200);
        // No blank before the mark.
        let spaced = note(json!(format!("x{}", "ab ".repeat(100)))).body;
        assert!(spaced.ends_with("ab…"), "{spaced}");

        // What is stored is the excerpt, and it reads back unchanged.
        let cut = note(json!(format!("{}\nsecret", "x".repeat(300))));
        let stored = serde_json::to_string(&cut).unwrap();
        assert!(!stored.contains("secret") && stored.len() < 300, "{stored}");
        assert_eq!(serde_json::from_str::<NoteRef>(&stored).unwrap(), cut);
    }

    #[test]
    fn board_labels_skip_label_less_lists() {
        let b: Board = serde_json::from_value(json!({
            "id": 1,
            "project": { "id": 9 },
            "lists": [
                { "label": { "name": "Doing" } },
                { "label": null, "list_type": "backlog" },
                { "label": { "name": "Review" } },
            ],
        }))
        .unwrap();
        assert_eq!(b.labels().collect::<Vec<_>>(), ["Doing", "Review"]);
        assert!(!b.is_valid(), "project id is stamped by the job");
    }
}
