//! Desktop-search query grammar, mirrored from the noctalia plugin
//! (`forskap/launcher.luau`) so both launchers read the same shorthand.
//!
//! Plain text searches projects: a launcher's job is mostly "open that repo",
//! and the issues and MRs would bury the project rows. A leading kind word
//! (`i`, `mr`, `e`, `p`, `g`) or sigil (`#42`, `!42`, `&42`) picks another
//! kind, `all` (or `*`) every kind; the empty query is the frequently opened
//! view across kinds.
//!
//! Pure functions; the shapes are easiest to see in the tests.

use std::fmt::Write as _;

use crate::cli::SearchKind;

/// Freedesktop icon name present in both Adwaita and Breeze.
pub fn icon(kind: SearchKind) -> &'static str {
    match kind {
        SearchKind::Issues => "emblem-important-symbolic",
        SearchKind::Mrs => "emblem-synchronizing-symbolic",
        SearchKind::Projects => "folder-symbolic",
        SearchKind::Groups => "system-users-symbolic",
        SearchKind::Epics => "user-bookmarks-symbolic",
    }
}

/// The kinds a search asks `Search` for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kinds {
    /// Every kind: `all oauth`, or the empty query's frequently opened view.
    All,
    Only(SearchKind),
}

impl Kinds {
    /// The filter as `wire_filter` takes it: empty for every kind.
    pub fn as_slice(&self) -> &[SearchKind] {
        match self {
            Kinds::All => &[],
            Kinds::Only(kind) => std::slice::from_ref(kind),
        }
    }
}

/// The kinds a leading word of the shorthand stands for.
fn kinds_of(word: &str) -> Option<Kinds> {
    Some(match word.to_ascii_lowercase().as_str() {
        "i" | "issue" | "issues" => Kinds::Only(SearchKind::Issues),
        "mr" | "mrs" => Kinds::Only(SearchKind::Mrs),
        "p" | "project" | "projects" => Kinds::Only(SearchKind::Projects),
        "g" | "group" | "groups" => Kinds::Only(SearchKind::Groups),
        "e" | "epic" | "epics" => Kinds::Only(SearchKind::Epics),
        "all" | "*" => Kinds::All,
        _ => return None,
    })
}

/// What the user typed, split into the kinds they named and the text for
/// `Search`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The kind word or sigil typed, `None` when the text was plain.
    pub typed: Option<Kinds>,
    pub query: String,
    /// The query was a bare `#42` / `!42` / `&42`: the iid match is the exact
    /// hit.
    pub exact: bool,
}

impl Parsed {
    /// What to search: the kinds typed, else projects for text and every kind
    /// for the empty query (the frequently opened issues, MRs and epics).
    pub fn kinds(&self) -> Kinds {
        self.typed.unwrap_or(if self.query.is_empty() {
            Kinds::All
        } else {
            Kinds::Only(SearchKind::Projects)
        })
    }
}

/// Strip a configured trigger word: `Some(rest)` when `text` is the word alone
/// or the word followed by whitespace, `None` when the search is not for us.
pub fn strip_trigger<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let text = text.trim_start();
    let rest = text.strip_prefix(word)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest)
    } else {
        None
    }
}

/// Split the search text into the kinds typed and the query; [`Parsed::kinds`]
/// fills in the default:
///
/// ```text
/// "oauth"      → None,           "oauth"   (searched as Projects)
/// ""           → None,           ""        (searched as All: frequently opened)
/// "all oauth"  → All,            "oauth"
/// "mr oauth"   → MergeRequests,  "oauth"
/// "mr"         → MergeRequests,  ""        (frequently opened MRs)
/// "!42"        → MergeRequests,  "#42"     (`Search` only knows the # form)
/// "#42"        → Issues,         "#42"
/// "&42"        → Epics,          "&42"
/// ```
pub fn parse(text: &str) -> Parsed {
    let text = text.trim();
    let (word, rest) = match text.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (text, ""),
    };
    if let Some(kinds) = kinds_of(word) {
        return Parsed {
            typed: Some(kinds),
            query: rest.to_string(),
            exact: false,
        };
    }
    if let Some(n) = text.strip_prefix('!').filter(|n| is_number(n)) {
        return Parsed {
            typed: Some(Kinds::Only(SearchKind::Mrs)),
            query: format!("#{n}"),
            exact: true,
        };
    }
    if text.strip_prefix('#').is_some_and(is_number) {
        return Parsed {
            typed: Some(Kinds::Only(SearchKind::Issues)),
            query: text.to_string(),
            exact: true,
        };
    }
    if text.strip_prefix('&').is_some_and(is_number) {
        return Parsed {
            typed: Some(Kinds::Only(SearchKind::Epics)),
            query: text.to_string(),
            exact: true,
        };
    }
    Parsed {
        typed: None,
        query: text.to_string(),
        exact: false,
    }
}

/// What a desktop search asks of us, or `None` when it is not for us: with a
/// trigger word configured, anything not starting with it; without one, a
/// bare single character (the shells send every keystroke, and one letter
/// would match most of the projects).
pub fn interpret(text: &str, trigger: Option<&str>) -> Option<Parsed> {
    match trigger {
        Some(word) => Some(parse(strip_trigger(text, word)?)),
        None => Some(parse(text)).filter(|p| !too_short(p)),
    }
}

fn too_short(p: &Parsed) -> bool {
    p.typed.is_none() && p.query.chars().count() < 2
}

fn is_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Percent-encode a query for use in a URL query string (RFC 3986 unreserved
/// characters pass through).
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => write!(out, "%{b:02X}").expect("writing to a String cannot fail"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn p(typed: Option<Kinds>, query: &str, exact: bool) -> Parsed {
        Parsed {
            typed,
            query: query.to_string(),
            exact,
        }
    }

    fn only(kind: SearchKind) -> Option<Kinds> {
        Some(Kinds::Only(kind))
    }

    #[test]
    fn mirrors_the_luau_table() {
        assert_eq!(parse("oauth"), p(None, "oauth", false));
        assert_eq!(parse("all oauth"), p(Some(Kinds::All), "oauth", false));
        assert_eq!(parse("* oauth"), p(Some(Kinds::All), "oauth", false));
        assert_eq!(parse("ALL"), p(Some(Kinds::All), "", false));
        assert_eq!(parse("mr oauth"), p(only(SearchKind::Mrs), "oauth", false));
        assert_eq!(parse("mr"), p(only(SearchKind::Mrs), "", false));
        assert_eq!(parse("!42"), p(only(SearchKind::Mrs), "#42", true));
        assert_eq!(parse("#42"), p(only(SearchKind::Issues), "#42", true));
        assert_eq!(parse("&42"), p(only(SearchKind::Epics), "&42", true));
        assert_eq!(
            parse("e roadmap"),
            p(only(SearchKind::Epics), "roadmap", false)
        );
        assert_eq!(
            parse("  Issues  login  "),
            p(only(SearchKind::Issues), "login", false)
        );
        assert_eq!(
            parse("g infra"),
            p(only(SearchKind::Groups), "infra", false)
        );
        assert_eq!(parse("p api"), p(only(SearchKind::Projects), "api", false));
    }

    #[test]
    fn plain_text_searches_projects_and_the_empty_query_everything() {
        assert_eq!(parse("oauth").kinds(), Kinds::Only(SearchKind::Projects));
        assert_eq!(parse("team/api").kinds(), Kinds::Only(SearchKind::Projects));
        assert_eq!(parse("").kinds(), Kinds::All);
        assert_eq!(parse("   ").kinds(), Kinds::All);
        // A typed kind wins over both defaults.
        assert_eq!(parse("all oauth").kinds(), Kinds::All);
        assert_eq!(parse("mr").kinds(), Kinds::Only(SearchKind::Mrs));
        assert_eq!(parse("p").kinds(), Kinds::Only(SearchKind::Projects));
        assert_eq!(Kinds::All.as_slice(), &[] as &[SearchKind]);
        assert_eq!(
            Kinds::Only(SearchKind::Issues).as_slice(),
            &[SearchKind::Issues]
        );
    }

    #[test]
    fn sigil_without_number_is_plain_text() {
        assert_eq!(parse("#abc"), p(None, "#abc", false));
        assert_eq!(parse("!"), p(None, "!", false));
        assert_eq!(parse("&amp"), p(None, "&amp", false));
        // `*` is a kind word only as a word of its own.
        assert_eq!(parse("*oauth"), p(None, "*oauth", false));
    }

    #[test]
    fn trigger_word_must_stand_alone() {
        assert_eq!(strip_trigger("gl oauth", "gl"), Some(" oauth"));
        assert_eq!(strip_trigger("gl", "gl"), Some(""));
        assert_eq!(strip_trigger("  gl", "gl"), Some(""));
        assert_eq!(strip_trigger("glob", "gl"), None);
        assert_eq!(strip_trigger("oauth", "gl"), None);
    }

    #[test]
    fn without_trigger_short_queries_need_a_kind() {
        assert_eq!(interpret("o", None), None);
        assert_eq!(interpret("", None), None);
        assert_eq!(interpret("oa", None), Some(p(None, "oa", false)));
        assert_eq!(
            interpret("mr", None),
            Some(p(only(SearchKind::Mrs), "", false))
        );
        assert_eq!(
            interpret("i o", None),
            Some(p(only(SearchKind::Issues), "o", false))
        );
        assert_eq!(
            interpret("all o", None),
            Some(p(Some(Kinds::All), "o", false))
        );
    }

    #[test]
    fn with_trigger_the_word_gates_and_the_rest_is_free() {
        assert_eq!(interpret("oauth", Some("gl")), None);
        assert_eq!(interpret("glob", Some("gl")), None);
        // `gl` alone is the frequently-opened view, like `/gl` in noctalia.
        let alone = interpret("gl", Some("gl")).unwrap();
        assert_eq!(alone, p(None, "", false));
        assert_eq!(alone.kinds(), Kinds::All);
        let text = interpret("gl o", Some("gl")).unwrap();
        assert_eq!(text, p(None, "o", false));
        assert_eq!(text.kinds(), Kinds::Only(SearchKind::Projects));
        assert_eq!(
            interpret("gl !42", Some("gl")),
            Some(p(only(SearchKind::Mrs), "#42", true))
        );
    }

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(percent_encode("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(percent_encode("safe-_.~"), "safe-_.~");
        assert_eq!(percent_encode("ü"), "%C3%BC");
    }

    proptest! {
        #[test]
        fn never_panics_and_output_is_normalised(text in "\\PC{0,40}") {
            let parsed = parse(&text);
            prop_assert_eq!(parsed.query.trim(), parsed.query.as_str());
            if parsed.exact {
                let sigil = if parsed.typed == Some(Kinds::Only(SearchKind::Epics)) { '&' } else { '#' };
                prop_assert!(parsed.query.starts_with(sigil));
                prop_assert!(matches!(
                    parsed.typed,
                    Some(Kinds::Only(SearchKind::Issues | SearchKind::Mrs | SearchKind::Epics))
                ));
            }
            // Only a typed kind narrows the empty query away from the
            // frequently opened view; plain text always has a kind.
            if parsed.typed.is_none() {
                prop_assert_eq!(parsed.kinds() == Kinds::All, parsed.query.is_empty());
            }
            prop_assert_eq!(strip_trigger(&text, "gl").is_some(), {
                let t = text.trim_start();
                t == "gl" || t.starts_with("gl ") || t.starts_with("gl\t")
            });
        }
    }
}
