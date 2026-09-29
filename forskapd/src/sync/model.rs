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
/// half is the scan prefix: the project for issues, MRs and boards, the time
/// for timelogs and events.
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

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EpicRef {
    #[serde(default, deserialize_with = "de::nullable")]
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
    /// Epic URL, empty when the issue has no parent.
    pub fn parent_url(&self) -> &str {
        self.epic.as_ref().map_or("", |e| e.url.as_str())
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

/// `GET /projects?simple=true`.
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
    /// Unix seconds.
    #[serde(default, deserialize_with = "de::timestamp")]
    pub created_at: u64,
}

impl Event {
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
                "created" => self.target_type.is_empty(),
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
    const SCHEMA: u32 = 1;
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
    const SCHEMA: u32 = 1;
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

impl Resource for Event {
    const NAME: &'static str = "events";
    const KEYSPACE: &'static str = "gl_events_v1";
    const SCHEMA: u32 = 1;
    fn key(&self) -> RowKey {
        (self.created_at, positive(self.id))
    }
    fn is_valid(&self) -> bool {
        self.id > 0
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
    use serde_json::json;

    #[test]
    fn issue_reads_a_gitlab_row() {
        let v = json!({
            "id": 123, "iid": 7, "project_id": 9,
            "title": "Fix it", "web_url": "https://gl/g/p/-/issues/7", "state": "opened",
            "epic": { "url": "https://gl/epics/1", "id": 3 },
            "time_stats": { "human_total_time_spent": "2h", "time_estimate": 0 },
            "labels": ["bug", 42, null, "high", {"name": "x"}, ["y"], true],
            "assignees": [{ "id": 5, "username": "me", "name": "Me" }],
            "updated_at": "2026-07-01T10:00:00.000Z",
            "description": "ignored",
        });
        let i: Issue = serde_json::from_value(v).unwrap();
        assert_eq!(i.key(), (9, 7));
        assert!(i.is_valid());
        assert_eq!(i.parent_url(), "https://gl/epics/1");
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
        assert_eq!(i.parent_url(), "");
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
