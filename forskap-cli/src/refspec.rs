//! GitLab-style issuable references on the command line.
//!
//! `forskap time log` acts on either kind, so it takes its target positionally as
//! `42`, `#42`, or `!42` — the sigils GitLab itself uses: `#` for issues, `!`
//! for merge requests. A bare number is an issue unless the `--mr` flag says
//! otherwise; a sigil that contradicts `--mr` is an error rather than a silent
//! guess. (`forskap issue` / `forskap mr` know the kind from the command group.)
//!
//! Shell note (surfaced in the clap help texts): `!` triggers history
//! expansion in interactive bash/zsh and `#` starts a comment, so those forms
//! need quoting there (`forskap time log '!42' 1h`); fish needs none, and
//! `42 --mr` avoids the issue entirely.

use anyhow::{Result, bail};
use forskap_api::{IssuableKind, SearchKind};
use serde::{Deserialize, Serialize};

/// Which kind of issuable a ref denotes. The client-side counterpart of the
/// wire `IssuableKind`, where an issue is a work item; also persisted inside
/// [`crate::state::LastIssue`] (default `Issue` keeps pre-MR state files
/// readable).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    #[default]
    Issue,
    #[serde(rename = "merge_request")]
    Mr,
}

/// A parsed positional reference: the per-project iid plus the kind its
/// sigil pinned (`None` for a bare number).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IssuableRef {
    pub iid: i64,
    pub kind: Option<RefKind>,
}

/// Parse `"42"`, `"#42"`, or `"!42"`. Anything else — other sigils, empty or
/// non-numeric digits, a non-positive number — is an eager error.
pub fn parse(s: &str) -> Result<IssuableRef> {
    let trimmed = s.trim();
    let (kind, digits) = match (trimmed.strip_prefix('#'), trimmed.strip_prefix('!')) {
        (Some(d), _) => (Some(RefKind::Issue), d),
        (_, Some(d)) => (Some(RefKind::Mr), d),
        _ => (None, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!("invalid issue/MR reference {s:?} — expected `42`, `#42`, or `!42`");
    }
    let iid: i64 = digits
        .parse()
        .map_err(|_| anyhow::anyhow!("issue/MR number out of range in {s:?}"))?;
    if iid == 0 {
        bail!("invalid issue/MR reference {s:?} — iids start at 1");
    }
    Ok(IssuableRef { iid, kind })
}

/// Combine a parsed ref with the `--mr` flag. `--mr` on a bare number selects
/// the MR kind; on `!42` it's redundant but consistent; on `#42` it's a
/// contradiction and errors instead of guessing.
pub fn resolve_kind(r: IssuableRef, mr_flag: bool) -> Result<RefKind> {
    match (r.kind, mr_flag) {
        (Some(RefKind::Issue), true) => {
            bail!("'#{}' is an issue reference, but --mr was given", r.iid)
        }
        (Some(kind), _) => Ok(kind),
        (None, true) => Ok(RefKind::Mr),
        (None, false) => Ok(RefKind::Issue),
    }
}

/// The wire enum value for a ref kind.
pub fn wire(kind: RefKind) -> IssuableKind {
    match kind {
        RefKind::Issue => IssuableKind::work_item,
        RefKind::Mr => IssuableKind::merge_request,
    }
}

/// The ref kind of a wire enum value.
pub fn from_wire(kind: &IssuableKind) -> RefKind {
    match kind {
        IssuableKind::work_item => RefKind::Issue,
        IssuableKind::merge_request => RefKind::Mr,
    }
}

/// The `Search` kind holding this kind's rows; an issue's are the work
/// items, epics included.
pub fn search_kind(kind: RefKind) -> SearchKind {
    match kind {
        RefKind::Issue => SearchKind::work_items,
        RefKind::Mr => SearchKind::merge_requests,
    }
}

pub fn noun(kind: RefKind) -> &'static str {
    match kind {
        RefKind::Issue => "issue",
        RefKind::Mr => "merge request",
    }
}

/// The GitLab display sigil: `#42` / `!42`.
pub fn sigil(kind: RefKind) -> char {
    match kind {
        RefKind::Issue => '#',
        RefKind::Mr => '!',
    }
}

/// [`sigil`] of a wire kind.
pub fn wire_sigil(kind: &IssuableKind) -> char {
    sigil(from_wire(kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_the_three_ref_forms() {
        assert_eq!(
            parse("42").unwrap(),
            IssuableRef {
                iid: 42,
                kind: None
            }
        );
        assert_eq!(
            parse("#42").unwrap(),
            IssuableRef {
                iid: 42,
                kind: Some(RefKind::Issue)
            }
        );
        assert_eq!(
            parse("!42").unwrap(),
            IssuableRef {
                iid: 42,
                kind: Some(RefKind::Mr)
            }
        );
        assert_eq!(parse(" !7 ").unwrap().iid, 7, "whitespace tolerated");
    }

    #[test]
    fn parse_rejects_malformed_refs() {
        for bad in ["", "#", "!", "abc", "#4a", "!!42", "#-3", "-3", "0", "#0"] {
            assert!(parse(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn resolve_kind_combines_sigil_and_flag() {
        let bare = parse("42").unwrap();
        let issue = parse("#42").unwrap();
        let mr = parse("!42").unwrap();

        assert_eq!(resolve_kind(bare, false).unwrap(), RefKind::Issue);
        assert_eq!(resolve_kind(bare, true).unwrap(), RefKind::Mr);
        assert_eq!(resolve_kind(issue, false).unwrap(), RefKind::Issue);
        assert_eq!(resolve_kind(mr, false).unwrap(), RefKind::Mr);
        assert_eq!(
            resolve_kind(mr, true).unwrap(),
            RefKind::Mr,
            "redundant --mr ok"
        );
        assert!(
            resolve_kind(issue, true).is_err(),
            "'#42' + --mr is a contradiction"
        );
    }

    /// State files hold these names, so they stay what they were before the
    /// wire called an issue a work item.
    #[test]
    fn ref_kind_keeps_its_persisted_names() {
        assert_eq!(serde_json::to_string(&RefKind::Issue).unwrap(), "\"issue\"");
        assert_eq!(
            serde_json::to_string(&RefKind::Mr).unwrap(),
            "\"merge_request\""
        );
    }
}
