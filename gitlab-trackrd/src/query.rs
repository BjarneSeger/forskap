//! Pure matching and derivation helpers shared by the read handlers.

/// The group namespace an issue belongs to, parsed from its `web_url`
/// (`https://host/<namespace>/-/issues/<iid>`). Returns `""` when there is no
/// namespace to parse — such issues still show in `tt issue list`, they just don't
/// match any `tt issue list --group` filter.
pub fn namespace_of(web_url: &str) -> String {
    web_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(web_url)
        .split_once('/')
        .map(|(_, path)| path)
        .unwrap_or("")
        .split("/-/")
        .next()
        .unwrap_or("")
        .trim_matches('/')
        .to_string()
}

/// Whether `namespace` falls under `group`, matching GitLab's subgroup-inclusive
/// `.group(g)` filter: an exact match or a `group/…` descendant.
pub fn in_group(namespace: &str, group: &str) -> bool {
    let group = group.trim_matches('/');
    !group.is_empty() && (namespace == group || namespace.starts_with(&format!("{group}/")))
}

/// Parse an issue/MR-reference query: `"#123"` → `Some(123)`. Anything else —
/// no leading `#`, non-digits, empty — is not a reference query.
pub fn parse_iid_query(query: &str) -> Option<i64> {
    let digits = query.trim().strip_prefix('#')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Case-insensitive substring match. The needle must already be lowercased —
/// callers lowercase the query once, not per entry.
pub fn text_matches(needle_lower: &str, hay: &str) -> bool {
    hay.to_lowercase().contains(needle_lower)
}

/// The board-derived `graph_status` for an issue: the first of its labels that
/// appears in the project's board lists, the issue's state when none matches,
/// or empty when the board labels are unknown.
pub fn graph_status_from(
    board_labels: Option<&[String]>,
    labels: &[String],
    state: &str,
) -> String {
    match board_labels {
        Some(board) => labels
            .iter()
            .find(|l| board.iter().any(|b| b == *l))
            .cloned()
            .unwrap_or_else(|| state.to_string()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn namespace_of_empty_without_path() {
        assert_eq!(namespace_of("https://gl"), "");
        assert_eq!(namespace_of(""), "");
    }

    proptest! {
        #[test]
        fn namespace_of_roundtrips_any_constructed_issue_url(
            ns in "[a-z0-9]{1,8}(/[a-z0-9]{1,8}){0,3}",
            iid in 1u64..100_000,
        ) {
            prop_assert_eq!(namespace_of(&format!("https://gl/{ns}/-/issues/{iid}")), ns);
        }

        #[test]
        fn in_group_matches_the_group_and_descendants_on_segment_boundaries(
            group in "[a-z]{1,6}(/[a-z]{1,6}){0,2}",
            child in "[a-z]{1,6}",
        ) {
            prop_assert!(in_group(&group, &group), "exact match");
            prop_assert!(in_group(&format!("{group}/{child}"), &group), "descendant");
            prop_assert!(
                !in_group(&format!("{group}{child}"), &group),
                "shared prefix without a segment boundary is not a descendant"
            );
            prop_assert!(!in_group(&group, ""), "empty filter matches nothing");
        }
    }

    #[test]
    fn parse_iid_query_rejects_a_bare_hash() {
        assert_eq!(parse_iid_query("#"), None);
    }

    proptest! {
        #[test]
        fn parse_iid_query_roundtrips_any_padded_reference(
            n in 0..=i64::MAX,
            pad_left in " {0,3}",
            pad_right in " {0,3}",
        ) {
            prop_assert_eq!(parse_iid_query(&format!("{pad_left}#{n}{pad_right}")), Some(n));
        }

        #[test]
        fn parse_iid_query_rejects_anything_without_a_leading_hash(s in "[^#]*") {
            prop_assert_eq!(parse_iid_query(&s), None);
        }

        #[test]
        fn parse_iid_query_rejects_non_digit_tails(
            digits in "[0-9]{0,4}",
            junk in "[a-z#-]{1,3}",
            more in "[0-9]{0,3}",
        ) {
            prop_assert_eq!(parse_iid_query(&format!("#{digits}{junk}{more}")), None);
        }

        #[test]
        fn text_matches_finds_an_inserted_needle_in_any_case(
            needle in "[a-zA-Z]{1,6}",
            prefix in "[a-zA-Z0-9 ]{0,8}",
            suffix in "[a-zA-Z0-9 ]{0,8}",
        ) {
            let needle_lower = needle.to_lowercase();
            let hay = format!("{prefix}{needle}{suffix}");
            prop_assert!(text_matches(&needle_lower, &hay));
            prop_assert!(text_matches(&needle_lower, &hay.to_uppercase()));
            prop_assert!(text_matches(&needle_lower, &hay.to_lowercase()));
        }

        #[test]
        fn text_matches_rejects_a_needle_absent_from_the_haystack(
            needle in "[a-z]{2,6}",
            hay in "[0-9 ]{0,10}",
        ) {
            prop_assert!(!text_matches(&needle, &hay));
        }
    }
}
