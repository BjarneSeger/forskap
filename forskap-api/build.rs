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
    let declared = IDL::try_from(idl.as_str()).unwrap();
    let optional: usize = structs(&declared).map(optional_fields).sum();
    // A field the walk didn't recognise would go out as `null` again.
    assert_eq!(
        omit.fields, optional,
        "the interface declares {optional} optional fields, the generated structs have {} \
         `Option` fields: varlink_generator's output changed, adapt `OmitAbsent`",
        omit.fields
    );

    fs::write(output_path, file.into_token_stream().to_string()).unwrap();
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
        let serialized = item.attrs.iter().any(|a| {
            a.path().is_ident("derive") && a.to_token_stream().to_string().contains("Serialize")
        });
        if let (true, syn::Fields::Named(fields)) = (serialized, &mut item.fields) {
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
/// its methods' arguments and replies.
fn structs<'a>(idl: &'a IDL<'a>) -> impl Iterator<Item = &'a VStruct<'a>> {
    let types = idl.typedefs.values().filter_map(|t| match &t.elt {
        VStructOrEnum::VStruct(s) => Some(&**s),
        VStructOrEnum::VEnum(_) => None,
    });
    let errors = idl.errors.values().map(|e| &e.parm);
    let methods = idl.methods.values().flat_map(|m| [&m.input, &m.output]);
    types.chain(errors).chain(methods)
}

/// The `?` fields of `s` and of the anonymous structs inside it, each of
/// which the generator turns into a struct of its own.
fn optional_fields(s: &VStruct) -> usize {
    fn nested(t: &VTypeExt) -> usize {
        match t {
            VTypeExt::Plain(VType::Struct(s)) => optional_fields(s),
            VTypeExt::Array(t) | VTypeExt::Dict(t) | VTypeExt::Option(t) => nested(t),
            VTypeExt::Plain(_) => 0,
        }
    }
    let field = |a: &varlink_parser::Argument| {
        usize::from(matches!(a.vtype, VTypeExt::Option(_))) + nested(&a.vtype)
    };
    s.elts.iter().map(field).sum()
}
