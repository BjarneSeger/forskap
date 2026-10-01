use std::env;
use std::fs;
use std::path::PathBuf;

use quote::ToTokens as _;
use syn::visit_mut::{self, VisitMut};
use varlink_parser::{IDL, VStruct, VStructOrEnum, VType, VTypeExt};

const INTERFACE: &str = "varlink/org.thehoster.forskapd.varlink";

fn main() {
    let out_dir: PathBuf = env::var_os("OUT_DIR").unwrap().into();
    let output_path = out_dir.join("org.thehoster.forskapd.rs");

    let idl = fs::read_to_string(INTERFACE).unwrap();
    let mut generated = Vec::new();
    varlink_generator::generate_with_options(
        &mut idl.as_bytes(),
        &mut generated,
        &varlink_generator::GeneratorOptions {
            generate_async: true,
            ..Default::default()
        },
        false, // tosource: false for include!() usage
    )
    .unwrap();

    let generated = String::from_utf8(generated).unwrap();
    let mut file = syn::parse_file(&generated).expect("varlink_generator's output parses");
    let mut omit = OmitAbsent::default();
    omit.visit_file_mut(&mut file);
    let mut default = DeriveDefault::default();
    default.visit_file_mut(&mut file);
    let declared = IDL::try_from(idl.as_str()).unwrap();
    let optional: usize = structs(&declared).iter().map(|s| optional_fields(s)).sum();
    // A field the walk didn't recognise would go out as `null` again.
    assert_eq!(
        omit.fields, optional,
        "the interface declares {optional} optional fields, the generated structs have {} \
         `Option` fields: varlink_generator's output changed, adapt `OmitAbsent`",
        omit.fields
    );
    let defaultable = structs(&declared)
        .iter()
        .filter(|s| optional_fields(s) == s.elts.len())
        .count();
    assert_eq!(
        default.structs, defaultable,
        "the interface declares {defaultable} structs without a required field, {} generated \
         structs derive `Default`: varlink_generator's output changed, adapt `DeriveDefault`",
        default.structs
    );

    fs::write(output_path, file.into_token_stream().to_string()).unwrap();
}

/// Derives `Default` for every serialized struct without a required field,
/// so a caller can write `SearchOptions { limit: Some(5),
/// ..Default::default() }` and keeps compiling when a field is added.
#[derive(Default)]
struct DeriveDefault {
    structs: usize,
}

impl VisitMut for DeriveDefault {
    fn visit_item_struct_mut(&mut self, item: &mut syn::ItemStruct) {
        if serialized(item) && item.fields.iter().all(|f| is_option(&f.ty)) {
            self.structs += 1;
            item.attrs.push(syn::parse_quote!(#[derive(Default)]));
        }
        visit_mut::visit_item_struct_mut(self, item);
    }
}

fn serialized(item: &syn::ItemStruct) -> bool {
    item.attrs.iter().any(|a| {
        a.path().is_ident("derive") && a.to_token_stream().to_string().contains("Serialize")
    })
}

/// Gives every `Option` field of a serialized struct the
/// `skip_serializing_if` the generator puts only on argument, reply and error
/// fields, so an absent field is left out of the JSON rather than sent as
/// `null`, wherever the struct is.
#[derive(Default)]
struct OmitAbsent {
    fields: usize,
}

impl VisitMut for OmitAbsent {
    fn visit_item_struct_mut(&mut self, item: &mut syn::ItemStruct) {
        if let (true, syn::Fields::Named(fields)) = (serialized(item), &mut item.fields) {
            for field in fields.named.iter_mut().filter(|f| is_option(&f.ty)) {
                self.fields += 1;
                let skipped = field.attrs.iter().any(|a| {
                    a.path().is_ident("serde")
                        && a.to_token_stream()
                            .to_string()
                            .contains("skip_serializing_if")
                });
                if !skipped {
                    field
                        .attrs
                        .push(syn::parse_quote!(#[serde(skip_serializing_if = "Option::is_none")]));
                }
            }
        }
        visit_mut::visit_item_struct_mut(self, item);
    }
}

fn is_option(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else {
        return false;
    };
    path.path
        .segments
        .last()
        .is_some_and(|s| s.ident == "Option")
}

/// Every struct the interface declares: its types, its errors' parameters,
/// its methods' arguments and replies, and the anonymous structs inside
/// them, each of which the generator turns into a struct of its own.
fn structs<'a>(idl: &'a IDL<'a>) -> Vec<&'a VStruct<'a>> {
    fn with_nested<'a>(s: &'a VStruct<'a>, all: &mut Vec<&'a VStruct<'a>>) {
        all.push(s);
        for field in &s.elts {
            let mut t = &field.vtype;
            while let VTypeExt::Array(inner) | VTypeExt::Dict(inner) | VTypeExt::Option(inner) = t {
                t = inner;
            }
            if let VTypeExt::Plain(VType::Struct(s)) = t {
                with_nested(s, all);
            }
        }
    }
    let types = idl.typedefs.values().filter_map(|t| match &t.elt {
        VStructOrEnum::VStruct(s) => Some(&**s),
        VStructOrEnum::VEnum(_) => None,
    });
    let errors = idl.errors.values().map(|e| &e.parm);
    let methods = idl.methods.values().flat_map(|m| [&m.input, &m.output]);
    let mut all = Vec::new();
    for s in types.chain(errors).chain(methods) {
        with_nested(s, &mut all);
    }
    all
}

/// The `?` fields of `s` itself.
fn optional_fields(s: &VStruct) -> usize {
    let optional = |a: &&varlink_parser::Argument| matches!(a.vtype, VTypeExt::Option(_));
    s.elts.iter().filter(optional).count()
}
