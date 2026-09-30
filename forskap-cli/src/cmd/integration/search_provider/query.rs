//! Desktop-search query grammar, mirrored from the noctalia plugin
//! (`forskap/launcher.luau`) so both launchers read the same shorthand.
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

/// The kind a leading word of the shorthand stands for.
fn kind_of(word: &str) -> Option<SearchKind> {
    Some(match word.to_ascii_lowercase().as_str() {
        "i" | "issue" | "issues" => SearchKind::Issues,
        "mr" | "mrs" => SearchKind::Mrs,
        "p" | "project" | "projects" => SearchKind::Projects,
        "g" | "group" | "groups" => SearchKind::Groups,
        "e" | "epic" | "epics" => SearchKind::Epics,
        _ => return None,
    })
}

/// What the user typed, split into the kind filter and the text for `Search`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub kind: Option<SearchKind>,
    pub query: String,
    /// The query was a bare `#42` / `!42` / `&42`: the iid match is the exact
    /// hit.
    pub exact: bool,
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

/// Split the search text into an optional kind filter and the query:
///
/// ```text
/// "oauth"     → None,           "oauth"
/// "mr oauth"  → MergeRequests,  "oauth"
/// "mr"        → MergeRequests,  ""        (frequently opened MRs)
/// "!42"       → MergeRequests,  "#42"     (`Search` only knows the # form)
/// "#42"       → Issues,         "#42"
/// "&42"       → Epics,          "&42"
/// ```
pub fn parse(text: &str) -> Parsed {
    let text = text.trim();
    let (word, rest) = match text.split_once(char::is_whitespace) {
        Some((w, r)) => (w, r.trim()),
        None => (text, ""),
    };
    if let Some(kind) = kind_of(word) {
        return Parsed {
            kind: Some(kind),
            query: rest.to_string(),
            exact: false,
        };
    }
    if let Some(n) = text.strip_prefix('!').filter(|n| is_number(n)) {
        return Parsed {
            kind: Some(SearchKind::Mrs),
            query: format!("#{n}"),
            exact: true,
        };
    }
    if text.strip_prefix('#').is_some_and(is_number) {
        return Parsed {
            kind: Some(SearchKind::Issues),
            query: text.to_string(),
            exact: true,
        };
    }
    if text.strip_prefix('&').is_some_and(is_number) {
        return Parsed {
            kind: Some(SearchKind::Epics),
            query: text.to_string(),
            exact: true,
        };
    }
    Parsed {
        kind: None,
        query: text.to_string(),
        exact: false,
    }
}

/// What a desktop search asks of us, or `None` when it is not for us: with a
/// trigger word configured, anything not starting with it; without one, a
/// bare single character (the shells send every keystroke, and one letter
/// would match most of the corpus).
pub fn interpret(text: &str, trigger: Option<&str>) -> Option<Parsed> {
    match trigger {
        Some(word) => Some(parse(strip_trigger(text, word)?)),
        None => Some(parse(text)).filter(|p| !too_short(p)),
    }
}

fn too_short(p: &Parsed) -> bool {
    p.kind.is_none() && p.query.chars().count() < 2
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

    fn p(kind: Option<SearchKind>, query: &str, exact: bool) -> Parsed {
        Parsed {
            kind,
            query: query.to_string(),
            exact,
        }
    }

    #[test]
    fn mirrors_the_luau_table() {
        assert_eq!(parse("oauth"), p(None, "oauth", false));
        assert_eq!(parse("mr oauth"), p(Some(SearchKind::Mrs), "oauth", false));
        assert_eq!(parse("mr"), p(Some(SearchKind::Mrs), "", false));
        assert_eq!(parse("!42"), p(Some(SearchKind::Mrs), "#42", true));
        assert_eq!(parse("#42"), p(Some(SearchKind::Issues), "#42", true));
        assert_eq!(parse("&42"), p(Some(SearchKind::Epics), "&42", true));
        assert_eq!(
            parse("e roadmap"),
            p(Some(SearchKind::Epics), "roadmap", false)
        );
        assert_eq!(
            parse("  Issues  login  "),
            p(Some(SearchKind::Issues), "login", false)
        );
        assert_eq!(
            parse("g infra"),
            p(Some(SearchKind::Groups), "infra", false)
        );
        assert_eq!(parse("p api"), p(Some(SearchKind::Projects), "api", false));
    }

    #[test]
    fn sigil_without_number_is_plain_text() {
        assert_eq!(parse("#abc"), p(None, "#abc", false));
        assert_eq!(parse("!"), p(None, "!", false));
        assert_eq!(parse("&amp"), p(None, "&amp", false));
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
            Some(p(Some(SearchKind::Mrs), "", false))
        );
        assert_eq!(
            interpret("i o", None),
            Some(p(Some(SearchKind::Issues), "o", false))
        );
    }

    #[test]
    fn with_trigger_the_word_gates_and_the_rest_is_free() {
        assert_eq!(interpret("oauth", Some("gl")), None);
        assert_eq!(interpret("glob", Some("gl")), None);
        // `gl` alone is the frequently-opened view, like `/gl` in noctalia.
        assert_eq!(interpret("gl", Some("gl")), Some(p(None, "", false)));
        assert_eq!(interpret("gl o", Some("gl")), Some(p(None, "o", false)));
        assert_eq!(
            interpret("gl !42", Some("gl")),
            Some(p(Some(SearchKind::Mrs), "#42", true))
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
                let sigil = if parsed.kind == Some(SearchKind::Epics) { '&' } else { '#' };
                prop_assert!(parsed.query.starts_with(sigil));
                prop_assert!(matches!(
                    parsed.kind,
                    Some(SearchKind::Issues | SearchKind::Mrs | SearchKind::Epics)
                ));
            }
            prop_assert_eq!(strip_trigger(&text, "gl").is_some(), {
                let t = text.trim_start();
                t == "gl" || t.starts_with("gl ") || t.starts_with("gl\t")
            });
        }
    }
}
