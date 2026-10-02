//! Holds `org.thehoster.forskapd` to its compatibility rules. Every released
//! `major.minor` of the definition is kept in `varlink/snapshots/` and never
//! edited: the definition must be the snapshot of its own version, and
//! compatible with every snapshot of its major version.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use varlink_parser::{IDL, VStruct, VStructOrEnum, VType, VTypeExt};

use super::{API_VERSION, VARLINK_INTERFACE_DESCRIPTION, major_minor};

const SNAPSHOTS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/varlink/snapshots");

/// Each snapshot's text by its `(major, minor)`.
type Snapshots = BTreeMap<(u64, u64), String>;

fn snapshot_name((major, minor): (u64, u64)) -> String {
    format!("org.thehoster.forskapd-{major}.{minor}.varlink")
}

fn snapshots() -> Snapshots {
    let mut found = Snapshots::new();
    for entry in fs::read_dir(SNAPSHOTS).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        let version = name
            .strip_prefix("org.thehoster.forskapd-")
            .and_then(|n| n.strip_suffix(".varlink"))
            .and_then(|v| v.split_once('.'))
            .and_then(|(major, minor)| Some((major.parse().ok()?, minor.parse().ok()?)))
            .filter(|&version| snapshot_name(version) == name)
            .unwrap_or_else(|| {
                panic!(
                    "varlink/snapshots/{name}: not named \
                     org.thehoster.forskapd-<major>.<minor>.varlink"
                )
            });
        found.insert(version, fs::read_to_string(&path).unwrap());
    }
    found
}

fn parse(idl: &str) -> IDL<'_> {
    IDL::try_from(idl).unwrap_or_else(|e| panic!("{e}"))
}

#[test]
fn the_definition_is_the_snapshot_of_its_version() {
    let version = major_minor(API_VERSION).unwrap();
    let unlike = unlike_snapshot(version, &parse(VARLINK_INTERFACE_DESCRIPTION), &snapshots());
    assert!(
        unlike.is_empty(),
        "the definition is not the snapshot of forskap-api {API_VERSION}. A released snapshot \
         is never edited: an interface change bumps the minor version and adds its snapshot.\n{}",
        unlike.join("\n")
    );
}

#[test]
fn the_definition_breaks_no_snapshot_of_its_major_version() {
    let version = major_minor(API_VERSION).unwrap();
    let breaks = breaking(version, &parse(VARLINK_INTERFACE_DESCRIPTION), &snapshots());
    assert!(
        breaks.is_empty(),
        "the definition breaks clients of these versions:\n{}",
        breaks.join("\n")
    );
}

/// Why `definition` is not the snapshot of `version`.
fn unlike_snapshot(version: (u64, u64), definition: &IDL, snapshots: &Snapshots) -> Vec<String> {
    let Some(snapshot) = snapshots.get(&version) else {
        let (major, minor) = version;
        return vec![format!(
            "no snapshot of {major}.{minor}: copy varlink/org.thehoster.forskapd.varlink to \
             varlink/snapshots/{}",
            snapshot_name(version)
        )];
    };
    let (was, is) = (shape(&parse(snapshot)), shape(definition));
    let names: BTreeSet<_> = was.keys().chain(is.keys()).collect();
    let differs = names
        .into_iter()
        .filter_map(|name| match (was.get(name), is.get(name)) {
            (Some(_), None) => Some(format!("{name}: only in the snapshot")),
            (None, Some(_)) => Some(format!("{name}: not in the snapshot")),
            (Some(was), Some(is)) if was != is => {
                Some(format!("{name}: {was} in the snapshot, {is} now"))
            }
            _ => None,
        });
    differs.collect()
}

/// Each declaration by its kind and name, its body as written: the order of
/// its fields counts, comments, whitespace and the order of the declarations
/// don't.
fn shape(idl: &IDL) -> BTreeMap<String, String> {
    let name = std::iter::once(("interface".to_string(), idl.name.to_string()));
    let types = idl
        .typedefs
        .values()
        .map(|t| (format!("type {}", t.name), t.elt.to_string()));
    let methods = idl.methods.values().map(|m| {
        let body = format!("{} -> {}", m.input, m.output);
        (format!("method {}", m.name), body)
    });
    let errors = idl
        .errors
        .values()
        .map(|e| (format!("error {}", e.name), e.parm.to_string()));
    name.chain(types).chain(methods).chain(errors).collect()
}

/// What in `definition` breaks a client of a snapshot of its major version.
fn breaking(version: (u64, u64), definition: &IDL, snapshots: &Snapshots) -> Vec<String> {
    let (major, minor) = version;
    let mut found = Vec::new();
    for (&(_, released), snapshot) in snapshots.range((major, 0)..=(major, u64::MAX)) {
        if released > minor {
            found.push(format!(
                "{major}.{released}: a snapshot newer than {major}.{minor}"
            ));
            continue;
        }
        let breaks = breaks(&parse(snapshot), definition);
        found.extend(
            breaks
                .into_iter()
                .map(|b| format!("{major}.{released}: {b}")),
        );
    }
    found
}

/// What breaks a client built against `old` when the daemon serves `new`.
fn breaks(old: &IDL, new: &IDL) -> Vec<String> {
    let mut found = Vec::new();
    if old.name != new.name {
        found.push(format!(
            "interface renamed from {} to {}",
            old.name, new.name
        ));
    }
    for (name, method) in &old.methods {
        let at = format!("method {name}");
        match new.methods.get(name) {
            Some(now) => {
                found.extend(kept(&at, "argument", &method.input, &now.input));
                found.extend(kept(&at, "reply field", &method.output, &now.output));
            }
            None => found.push(format!("{at}: removed")),
        }
    }
    for (name, error) in &old.errors {
        let at = format!("error {name}");
        match new.errors.get(name) {
            Some(now) => found.extend(kept(&at, "parameter", &error.parm, &now.parm)),
            None => found.push(format!("{at}: removed")),
        }
    }
    let replied = replied(new);
    for (name, typedef) in &old.typedefs {
        let at = format!("type {name}");
        match (&typedef.elt, new.typedefs.get(name).map(|t| &t.elt)) {
            (VStructOrEnum::VStruct(was), Some(VStructOrEnum::VStruct(is))) => {
                found.extend(kept(&at, "field", was, is));
            }
            (VStructOrEnum::VEnum(was), Some(VStructOrEnum::VEnum(is))) => {
                let removed = was.elts.iter().filter(|v| !is.elts.contains(v));
                found.extend(removed.map(|v| format!("{at}: variant \"{v}\" removed")));
                if replied.contains(name) {
                    let added = is.elts.iter().filter(|v| !was.elts.contains(v));
                    found.extend(added.map(|v| {
                        format!(
                            "{at}: new variant \"{v}\", but a reply or an error can carry {name}"
                        )
                    }));
                }
            }
            (was, Some(is)) => {
                found.push(format!("{at}: changed from {} to {}", kind(was), kind(is)));
            }
            (_, None) => found.push(format!("{at}: removed")),
        }
    }
    found
}

fn kind(elt: &VStructOrEnum) -> &'static str {
    match elt {
        VStructOrEnum::VStruct(_) => "a struct",
        VStructOrEnum::VEnum(_) => "an enum",
    }
}

/// The fields of `old` that `new` drops or retypes, and the required ones
/// only `new` has.
fn kept(at: &str, field: &str, old: &VStruct, new: &VStruct) -> Vec<String> {
    let mut found = Vec::new();
    for was in &old.elts {
        match new.elts.iter().find(|f| f.name == was.name) {
            None => found.push(format!("{at}: {field} \"{}\" removed", was.name)),
            // The rendering is exact: the same name or builtin under the
            // same `?`, `[]` and `[string]`, an anonymous type field by field.
            Some(is) if is.vtype.to_string() != was.vtype.to_string() => found.push(format!(
                "{at}: {field} \"{}\" changed from {} to {}",
                was.name, was.vtype, is.vtype
            )),
            Some(_) => {}
        }
    }
    for is in &new.elts {
        let added = !old.elts.iter().any(|f| f.name == is.name);
        if added && !matches!(is.vtype, VTypeExt::Option(_)) {
            found.push(format!(
                "{at}: new {field} \"{}\" is required ({})",
                is.name, is.vtype
            ));
        }
    }
    found
}

/// The named types a reply or an error can carry, followed through the
/// fields of structs: an enum among them can't gain a variant, which a
/// client built before couldn't read.
fn replied<'a>(idl: &'a IDL<'a>) -> BTreeSet<&'a str> {
    let mut found = BTreeSet::new();
    let replies = idl.methods.values().map(|m| &m.output);
    let mut todo: Vec<&VStruct> = replies
        .chain(idl.errors.values().map(|e| &e.parm))
        .collect();
    while let Some(s) = todo.pop() {
        for field in &s.elts {
            let mut t = &field.vtype;
            while let VTypeExt::Array(inner) | VTypeExt::Dict(inner) | VTypeExt::Option(inner) = t {
                t = inner;
            }
            match t {
                VTypeExt::Plain(VType::Struct(s)) => todo.push(s),
                VTypeExt::Plain(VType::Typename(name)) => {
                    if !found.insert(*name) {
                        continue;
                    }
                    if let Some(VStructOrEnum::VStruct(s)) = idl.typedefs.get(name).map(|t| &t.elt)
                    {
                        todo.push(s);
                    }
                }
                _ => {}
            }
        }
    }
    found
}

/// A definition of `org.example` with these declarations.
fn example(declarations: &str) -> String {
    format!("interface org.example\n{declarations}\n")
}

fn breaking_change(old: &str, new: &str) -> Vec<String> {
    breaks(&parse(&example(old)), &parse(&example(new)))
}

#[test]
fn a_removed_method_breaks() {
    let old = "method A() -> ()\nmethod B() -> ()";
    assert_eq!(
        breaking_change(old, "method A() -> ()"),
        ["method B: removed"]
    );
}

#[test]
fn a_removed_or_changed_argument_breaks() {
    let old = "method Search(query: string, limit: ?int, kinds: []string) -> ()";
    let new = "method Search(query: ?string, limit: int, kinds: ?[]string) -> ()";
    assert_eq!(
        breaking_change(old, new),
        [
            r#"method Search: argument "query" changed from string to ?string"#,
            r#"method Search: argument "limit" changed from ?int to int"#,
            r#"method Search: argument "kinds" changed from []string to ?[]string"#,
        ]
    );
    let new = "method Search(query: string, kinds: [string]string) -> ()";
    assert_eq!(
        breaking_change(old, new),
        [
            r#"method Search: argument "limit" removed"#,
            r#"method Search: argument "kinds" changed from []string to [string]string"#,
        ]
    );
}

#[test]
fn a_new_required_argument_breaks() {
    let old = "method Search(query: string) -> ()";
    let new = "method Search(query: string, limit: int) -> ()";
    assert_eq!(
        breaking_change(old, new),
        [r#"method Search: new argument "limit" is required (int)"#]
    );
}

#[test]
fn a_removed_or_changed_reply_field_breaks() {
    let old = "method Get() -> (id: int, name: ?string, item: Item)";
    let new = "method Get() -> (name: string, item: Other)";
    assert_eq!(
        breaking_change(old, new),
        [
            r#"method Get: reply field "id" removed"#,
            r#"method Get: reply field "name" changed from ?string to string"#,
            r#"method Get: reply field "item" changed from Item to Other"#,
        ]
    );
}

#[test]
fn a_new_required_reply_field_breaks() {
    let old = "method Get() -> (id: int)";
    let new = "method Get() -> (id: int, name: string)";
    assert_eq!(
        breaking_change(old, new),
        [r#"method Get: new reply field "name" is required (string)"#]
    );
}

#[test]
fn a_removed_error_or_a_changed_parameter_breaks() {
    let old = "error Failed (message: string, status: ?int)\nerror Gone ()";
    let new = "error Failed (message: []string, code: int)";
    assert_eq!(
        breaking_change(old, new),
        [
            r#"error Failed: parameter "message" changed from string to []string"#,
            r#"error Failed: parameter "status" removed"#,
            r#"error Failed: new parameter "code" is required (int)"#,
            "error Gone: removed",
        ]
    );
}

#[test]
fn a_removed_type_or_one_of_another_kind_breaks() {
    let old = "type A (x: int)\ntype B (x, y)\ntype C (x: int)";
    let new = "type A (x, y)\ntype B (x: int)";
    assert_eq!(
        breaking_change(old, new),
        [
            "type A: changed from a struct to an enum",
            "type B: changed from an enum to a struct",
            "type C: removed",
        ]
    );
}

#[test]
fn a_removed_or_changed_struct_field_breaks() {
    let old = "type Item (id: int, tags: []string, inner: (a: int))";
    let new = "type Item (id: ?int, inner: (a: int, b: ?int), title: string)";
    assert_eq!(
        breaking_change(old, new),
        [
            r#"type Item: field "id" changed from int to ?int"#,
            r#"type Item: field "tags" removed"#,
            r#"type Item: field "inner" changed from (a: int) to (a: int, b: ?int)"#,
            r#"type Item: new field "title" is required (string)"#,
        ]
    );
}

#[test]
fn a_removed_variant_breaks() {
    let old = "type Kind (a, b)\nmethod Set(kind: Kind) -> ()";
    let new = "type Kind (a)\nmethod Set(kind: Kind) -> ()";
    assert_eq!(
        breaking_change(old, new),
        [r#"type Kind: variant "b" removed"#]
    );
}

#[test]
fn a_new_variant_of_an_enum_a_reply_or_an_error_carries_breaks() {
    for carrier in [
        "method Get() -> (kind: Kind)",
        "method Set(kind: Kind) -> (kind: ?Kind)",
        "type Item (kind: ?Kind)\nmethod Get() -> (items: []Item)",
        "type Inner (kinds: []Kind)\ntype Outer (inner: [string]Inner)\nmethod Get() -> (outer: ?Outer)",
        "method Get() -> (item: ?(kind: Kind))",
        "type Item (kind: Kind)\nerror Failed (item: ?Item)",
    ] {
        let old = format!("type Kind (a)\n{carrier}");
        let new = format!("type Kind (a, b)\n{carrier}");
        assert_eq!(
            breaking_change(&old, &new),
            [r#"type Kind: new variant "b", but a reply or an error can carry Kind"#],
            "{carrier}"
        );
    }
}

#[test]
fn a_renamed_interface_breaks() {
    let old = example("method A() -> ()");
    let new = old.replace("org.example", "org.example2");
    let (old, new) = (parse(&old), parse(&new));
    assert_eq!(
        breaks(&old, &new),
        ["interface renamed from org.example to org.example2"]
    );
}

#[test]
fn additions_an_older_client_does_not_see_are_compatible() {
    let old = [
        "type Kind (a)",
        "type Filter (kind: ?Kind)",
        "type Item (id: int)",
        "method Get(filter: ?Filter) -> (items: []Item)",
        "error Failed (message: string)",
    ];
    for declaration in [
        "method Count() -> (count: int)",
        "type Extra (count: int)",
        "error Gone (message: string)",
        "method Get(filter: ?Filter, since: ?int) -> (items: []Item)",
        "method Get(filter: ?Filter) -> (items: []Item, total: ?int)",
        "type Item (id: int, title: ?string)",
        "type Filter (kind: ?Kind, limit: ?int)",
        "error Failed (message: string, status: ?int)",
        // Only arguments carry it: an older daemon refuses the new variant.
        "type Kind (a, b)",
    ] {
        // Replaces the declaration of that name, else adds it.
        fn head(declaration: &str) -> Vec<&str> {
            declaration.split(['(', ' ']).take(2).collect()
        }
        let mut new: Vec<_> = old
            .into_iter()
            .filter(|d| head(d) != head(declaration))
            .collect();
        new.push(declaration);
        assert_eq!(
            breaking_change(&old.join("\n"), &new.join("\n")),
            Vec::<String>::new(),
            "{declaration}"
        );
    }
}

#[test]
fn a_definition_is_its_snapshot_whatever_its_comments_and_order() {
    let snapshot = example("type Item (id: int, title: ?string)\nmethod Get() -> (items: []Item)");
    let snapshots = Snapshots::from([((1, 0), snapshot)]);
    let definition = "
# The example.
interface org.example

# Gets them.
method Get() -> (
  # All of them.
  items: []Item
)

type Item (
  id:int,
  title: ?string
)
";
    assert!(unlike_snapshot((1, 0), &parse(definition), &snapshots).is_empty());
}

#[test]
fn a_definition_unlike_its_snapshot_says_how() {
    let snapshot = example("type Item (id: int, title: ?string)\nerror Gone ()");
    let snapshots = Snapshots::from([((1, 0), snapshot)]);
    let definition = example("type Item (title: ?string, id: int)\nmethod Get() -> ()");
    let definition = parse(&definition);
    assert_eq!(
        unlike_snapshot((1, 0), &definition, &snapshots),
        [
            "error Gone: only in the snapshot",
            "method Get: not in the snapshot",
            "type Item: (id: int, title: ?string) in the snapshot, (title: ?string, id: int) now",
        ]
    );
    assert_eq!(
        unlike_snapshot((1, 1), &definition, &snapshots),
        [
            "no snapshot of 1.1: copy varlink/org.thehoster.forskapd.varlink to \
             varlink/snapshots/org.thehoster.forskapd-1.1.varlink"
        ]
    );
}

#[test]
fn every_snapshot_of_the_major_version_and_none_other_is_checked() {
    let snapshots = Snapshots::from([
        ((0, 9), example("method Old() -> ()")),
        ((1, 0), example("method A() -> ()")),
        ((1, 1), example("method A() -> ()\nmethod B() -> ()")),
        ((1, 3), example("method A() -> ()")),
        ((2, 0), example("method New() -> ()")),
    ]);
    let definition = example("method A() -> ()\nmethod C() -> ()");
    let definition = parse(&definition);
    assert_eq!(
        breaking((1, 2), &definition, &snapshots),
        ["1.1: method B: removed", "1.3: a snapshot newer than 1.2"]
    );
}
