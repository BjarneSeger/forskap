//! API for interacting with forskapd
#![allow(non_camel_case_types)]
// The generated code takes a method's arguments one by one.
#![allow(clippy::too_many_arguments)]

// Every `Option` field of it is left out when absent rather than sent as
// `null`: see build.rs.
include!(concat!(env!("OUT_DIR"), "/org.thehoster.forskapd.rs"));

/// Raw varlink interface description, suitable for the daemon's
/// `org.varlink.service.GetInterfaceDescription` reply.
pub const VARLINK_INTERFACE_DESCRIPTION: &str =
    include_str!("../varlink/org.thehoster.forskapd.varlink");

/// The version of the interface above: the daemon reports it in `GetStatus`,
/// a client compares it with the one it was built against ([`compatible`]).
pub const API_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Whether a daemon whose `GetStatus` says `api_version` speaks an interface
/// a client built against [`API_VERSION`] can use. Before 1.0 that takes the
/// same minor version, as every interface change bumps it (a patch is a fix to
/// a binding alone); from 1.0 on the same major and a minor at least this
/// one's. A version that doesn't parse is not compatible.
pub fn compatible(api_version: &str) -> bool {
    compatible_with(api_version, API_VERSION)
}

fn compatible_with(daemon: &str, client: &str) -> bool {
    match (major_minor(daemon), major_minor(client)) {
        (Some((0, d)), Some((0, c))) => d == c,
        (Some((dm, d)), Some((cm, c))) => dm == cm && d >= c,
        _ => false,
    }
}

/// `MAJOR.MINOR.PATCH`, the patch (and anything after it) unchecked.
fn major_minor(version: &str) -> Option<(u64, u64)> {
    // Digits only: `parse` would take a sign.
    let number = |s: &str| {
        let digits = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        digits.then(|| s.parse().ok()).flatten()
    };
    let mut parts = version.splitn(3, '.');
    let major = number(parts.next()?)?;
    let minor = number(parts.next()?)?;
    parts.next()?;
    Some((major, minor))
}

const SOCKET_NAME: &str = "forskapd.socket";

/// The socket the daemon listens on unless it is told another one, and where
/// a client finds it: `$XDG_RUNTIME_DIR/forskapd.socket`, or without a
/// runtime directory (macOS has none) `forskapd/forskapd.socket` in the
/// user's data directory. Never a shared directory, where another user could
/// put a socket of their own in its place. `None` without a home directory.
pub fn default_socket() -> Option<std::path::PathBuf> {
    socket_in(dirs::runtime_dir(), dirs::data_local_dir())
}

fn socket_in(
    runtime: Option<std::path::PathBuf>,
    data: Option<std::path::PathBuf>,
) -> Option<std::path::PathBuf> {
    runtime
        .map(|dir| dir.join(SOCKET_NAME))
        .or_else(|| data.map(|dir| dir.join("forskapd").join(SOCKET_NAME)))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        API_VERSION, SyncJob, SyncJobStatus, WorkItemRef, compatible, compatible_with, socket_in,
    };

    #[test]
    fn a_daemon_is_compatible_by_minor_before_1_0_and_by_major_after() {
        for (daemon, client, expected) in [
            ("0.32.0", "0.32.0", true),
            ("0.32.1", "0.32.0", true),
            ("0.32.0", "0.32.1", true),
            ("0.33.0", "0.32.0", false),
            ("0.31.0", "0.32.0", false),
            ("1.0.0", "0.32.0", false),
            ("1.0.0", "1.0.0", true),
            ("1.3.0", "1.2.5", true),
            ("1.2.0", "1.3.0", false),
            ("2.0.0", "1.0.0", false),
            ("1.4.0-rc.1", "1.2.0", true),
            ("", "0.32.0", false),
            ("0.32", "0.32.0", false),
            ("v0.32.0", "0.32.0", false),
            ("+0.32.0", "0.32.0", false),
            ("0.x.0", "0.32.0", false),
            ("0.32.0", "", false),
        ] {
            assert_eq!(
                compatible_with(daemon, client),
                expected,
                "{daemon} against {client}"
            );
        }
        assert!(compatible(API_VERSION));
    }

    #[test]
    fn the_default_socket_is_in_the_runtime_dir_or_else_the_data_dir() {
        let runtime = || Some(PathBuf::from("/run/user/1000"));
        let data = || Some(PathBuf::from("/home/me/.local/share"));
        assert_eq!(
            socket_in(runtime(), data()),
            Some("/run/user/1000/forskapd.socket".into())
        );
        assert_eq!(
            socket_in(None, data()),
            Some("/home/me/.local/share/forskapd/forskapd.socket".into())
        );
        assert_eq!(socket_in(None, None), None);
    }

    #[test]
    fn absent_optional_fields_are_left_out() {
        let parent = WorkItemRef {
            project_id: None,
            group_id: None,
            iid: 5,
            r#type: None,
            title: None,
            web_url: None,
        };
        let json = serde_json::to_value(&parent).unwrap();
        assert_eq!(json, serde_json::json!({"iid": 5}));
    }

    /// Absent or `null` (what a daemon before forskap-api 0.32.0 sends):
    /// either reads as `None`.
    #[test]
    fn absent_and_null_optional_fields_read_as_none() {
        for json in [
            serde_json::json!({"key": "events", "status": "due", "failures": 0}),
            serde_json::json!({
                "key": "events", "status": "due", "failures": 0,
                "last_ok": null, "next_due": null, "running_since": null, "last_error": null,
                "unavailable": null, "full": null, "fetched": null, "expected": null,
            }),
        ] {
            let job: SyncJob = serde_json::from_value(json).unwrap();
            assert_eq!(job.status, SyncJobStatus::due);
            assert_eq!(
                (job.last_ok, job.unavailable, job.expected),
                (None, None, None)
            );
        }
    }

    /// Guards build.rs: a struct the generator adds can't send `null` again
    /// unnoticed.
    #[test]
    fn every_generated_option_field_is_skipped_when_absent() {
        let generated = include_str!(concat!(env!("OUT_DIR"), "/org.thehoster.forskapd.rs"));
        let file = syn::parse_file(generated).unwrap();
        let says = |attrs: &[syn::Attribute], name: &str, word: &str| {
            attrs.iter().any(|a| {
                a.path().is_ident(name)
                    && a.meta
                        .require_list()
                        .is_ok_and(|l| l.tokens.to_string().contains(word))
            })
        };
        let mut optional = 0;
        for item in &file.items {
            let syn::Item::Struct(item) = item else {
                continue;
            };
            if !says(&item.attrs, "derive", "Serialize") {
                continue;
            }
            for field in &item.fields {
                let syn::Type::Path(ty) = &field.ty else {
                    continue;
                };
                if ty.path.segments.last().is_none_or(|s| s.ident != "Option") {
                    continue;
                }
                optional += 1;
                assert!(
                    says(&field.attrs, "serde", "skip_serializing_if"),
                    "{}.{:?} would be sent as null",
                    item.ident,
                    field.ident
                );
            }
        }
        assert!(optional > 40, "only {optional} Option fields found");
    }
}
