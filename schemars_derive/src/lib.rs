#[macro_use]
extern crate quote;
#[macro_use]
extern crate syn;
extern crate proc_macro;

mod preprocess;

use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use serde_derive_internals::ast::{Container, Data, Field, Style, Variant};
use serde_derive_internals::attr::{self, Default as SerdeDefault, EnumTag};
use serde_derive_internals::{Ctxt, Derive};
use syn::spanned::Spanned;

#[proc_macro_derive(JsonSchema, attributes(schemars, serde))]
pub fn derive_json_schema(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let mut input = parse_macro_input!(input as syn::DeriveInput);

    preprocess::add_trait_bounds(&mut input.generics);
    if let Err(e) = preprocess::process_serde_attrs(&mut input) {
        return compile_error(input.span(), e).into();
    }

    let ctxt = Ctxt::new();
    let cont = Container::from_ast(&ctxt, &input, Derive::Deserialize);
    if let Err(e) = ctxt.check() {
        return compile_error(input.span(), e).into();
    }

    let schema = match cont.data {
        Data::Struct(Style::Unit, _) => schema_for_unit_struct(),
        Data::Struct(Style::Newtype, ref fields) => schema_for_newtype_struct(&fields[0]),
        Data::Struct(Style::Tuple, ref fields) => schema_for_tuple_struct(fields),
        Data::Struct(Style::Struct, ref fields) => schema_for_struct(fields, &cont.attrs),
        Data::Enum(ref variants) => schema_for_enum(variants, &cont.attrs),
    };

    let type_name = cont.ident;
    let type_params: Vec<_> = cont.generics.type_params().map(|ty| &ty.ident).collect();

    let schema_base_name = cont.attrs.name().deserialize_name();
    let schema_name = if type_params.is_empty() {
        quote! {
            #schema_base_name.to_owned()
        }
    } else if type_name == schema_base_name {
        let mut schema_name_fmt = schema_base_name;
        schema_name_fmt.push_str("_For_{}");
        schema_name_fmt.push_str(&"_And_{}".repeat(type_params.len() - 1));
        quote! {
            format!(#schema_name_fmt #(,#type_params::schema_name())*)
        }
    } else {
        let mut schema_name_fmt = schema_base_name;
        for tp in &type_params {
            schema_name_fmt.push_str(&format!("{{{}:.0}}", tp));
        }
        let fmt_param_names = &type_params;
        let type_params = &type_params;
        quote! {
            format!(#schema_name_fmt #(,#fmt_param_names=#type_params::schema_name())*)
        }
    };

    let (impl_generics, ty_generics, where_clause) = cont.generics.split_for_impl();

    let impl_block = quote! {
        #[automatically_derived]
        impl #impl_generics schemars::JsonSchema for #type_name #ty_generics #where_clause {
            fn schema_name() -> String {
                #schema_name
            }

            fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::Result {
                Ok(#schema)
            }
        };
    };
    proc_macro::TokenStream::from(impl_block)
}

fn wrap_schema_fields(schema_contents: TokenStream) -> TokenStream {
    quote! {
        schemars::schema::Schema::Object(
            schemars::schema::SchemaObject {
            #schema_contents
            ..Default::default()
        })
    }
}

fn compile_error(span: Span, message: String) -> TokenStream {
    quote_spanned! {span=>
        compile_error!(#message);
    }
}

fn is_unit_variant(v: &Variant) -> bool {
    match v.style {
        Style::Unit => true,
        _ => false,
    }
}

fn schema_for_enum(variants: &[Variant], cattrs: &attr::Container) -> TokenStream {
    match cattrs.tag() {
        EnumTag::External => schema_for_external_tagged_enum(variants, cattrs),
        EnumTag::None => schema_for_untagged_enum(variants, cattrs),
        EnumTag::Internal { tag } => schema_for_internal_tagged_enum(variants, cattrs, tag),
        EnumTag::Adjacent { .. } => unimplemented!("Adjacent tagged enums not yet supported."),
    }
}

fn schema_for_external_tagged_enum(variants: &[Variant], cattrs: &attr::Container) -> TokenStream {
    let (unit_variants, complex_variants): (Vec<_>, Vec<_>) =
        variants.iter().partition(|v| is_unit_variant(v));
    let unit_count = unit_variants.len();

    let unit_names = unit_variants
        .into_iter()
        .map(|v| v.attrs.name().deserialize_name());
    let unit_schema = wrap_schema_fields(quote! {
        enum_values: Some(vec![#(#unit_names.into()),*]),
    });

    if complex_variants.is_empty() {
        return unit_schema;
    }

    let mut schemas = Vec::new();
    if unit_count > 0 {
        schemas.push(unit_schema);
    }

    schemas.extend(complex_variants.into_iter().map(|variant| {
        let name = variant.attrs.name().deserialize_name();
        let sub_schema = schema_for_untagged_enum_variant(variant, cattrs);
        wrap_schema_fields(quote! {
            instance_type: Some(schemars::schema::InstanceType::Object.into()),
            properties: {
                let mut props = schemars::Map::new();
                props.insert(#name.to_owned(), #sub_schema);
                props
            },
            required: {
                let mut required = schemars::Set::new();
                required.insert(#name.to_owned());
                required
            },
        })
    }));

    wrap_schema_fields(quote! {
        any_of: Some(vec![#(#schemas),*]),
    })
}

fn schema_for_internal_tagged_enum(
    variants: &[Variant],
    cattrs: &attr::Container,
    tag_name: &str,
) -> TokenStream {
    let schemas = variants.iter().map(|variant| {
        let name = variant.attrs.name().deserialize_name();
        let type_schema = wrap_schema_fields(quote! {
            instance_type: Some(schemars::schema::InstanceType::String.into()),
            enum_values: Some(vec![#name.into()]),
        });
        let schema = wrap_schema_fields(quote! {
            instance_type: Some(schemars::schema::InstanceType::Object.into()),
            properties: {
                let mut props = schemars::Map::new();
                props.insert(#tag_name.to_owned(), #type_schema);
                props
            },
            required: {
                let mut required = schemars::Set::new();
                required.insert(#tag_name.to_owned());
                required
            },
        });
        if is_unit_variant(&variant) {
            schema
        } else {
            let sub_schema = schema_for_untagged_enum_variant(variant, cattrs);
            quote! {
                #schema.flatten(#sub_schema)?
            }
        }
    });

    wrap_schema_fields(quote! {
        any_of: Some(vec![#(#schemas),*]),
    })
}

fn schema_for_untagged_enum(variants: &[Variant], cattrs: &attr::Container) -> TokenStream {
    let schemas = variants
        .iter()
        .map(|v| schema_for_untagged_enum_variant(v, cattrs));

    wrap_schema_fields(quote! {
        any_of: Some(vec![#(#schemas),*]),
    })
}

fn schema_for_untagged_enum_variant(variant: &Variant, cattrs: &attr::Container) -> TokenStream {
    match variant.style {
        Style::Unit => schema_for_unit_struct(),
        Style::Newtype => schema_for_newtype_struct(&variant.fields[0]),
        Style::Tuple => schema_for_tuple_struct(&variant.fields),
        Style::Struct => schema_for_struct(&variant.fields, cattrs),
    }
}

fn schema_for_unit_struct() -> TokenStream {
    quote! {
        gen.subschema_for::<()>()?
    }
}

fn schema_for_newtype_struct(field: &Field) -> TokenStream {
    let ty = get_json_schema_type(field);
    quote_spanned! {field.original.span()=>
        gen.subschema_for::<#ty>()?
    }
}

fn schema_for_tuple_struct(fields: &[Field]) -> TokenStream {
    let types = fields.iter().map(get_json_schema_type);
    quote! {
        gen.subschema_for::<(#(#types),*)>()?
    }
}

fn schema_for_struct(fields: &[Field], cattrs: &attr::Container) -> TokenStream {
    let (nested, flat): (Vec<_>, Vec<_>) = fields.iter().partition(|f| !f.attrs.flatten());
    let container_has_default = has_default(cattrs.default());
    let mut required = Vec::new();
    let recurse = nested.iter().map(|field| {
        let name = field.attrs.name().deserialize_name();
        if !container_has_default && !has_default(field.attrs.default()) {
            required.push(name.clone());
        }
        let ty = get_json_schema_type(field);
        quote_spanned! {field.original.span()=>
            props.insert(#name.to_owned(), gen.subschema_for::<#ty>()?);
        }
    });

    let schema = wrap_schema_fields(quote! {
        instance_type: Some(schemars::schema::InstanceType::Object.into()),
        properties: {
            let mut props = schemars::Map::new();
            #(#recurse)*
            props
        },
        required: {
            let mut required = schemars::Set::new();
            #(required.insert(#required.to_owned());)*
            required
        },
    });

    let flattens = flat.iter().map(|field| {
        let ty = get_json_schema_type(field);
        quote_spanned! {field.original.span()=>
            .flatten(<#ty>::json_schema(gen)?)?
        }
    });

    quote! {
        #schema #(#flattens)*
    }
}

fn has_default(d: &SerdeDefault) -> bool {
    match d {
        SerdeDefault::None => false,
        _ => true,
    }
}

fn get_json_schema_type(field: &Field) -> Box<dyn ToTokens> {
    // TODO it would probably be simpler to parse attributes manually here, instead of
    // using the serde-parsed attributes
    let de_with_segments = without_last_element(field.attrs.deserialize_with(), "deserialize");
    let se_with_segments = without_last_element(field.attrs.serialize_with(), "serialize");
    if de_with_segments == se_with_segments {
        if let Some(expr_path) = de_with_segments {
            return Box::from(expr_path);
        }
    }
    Box::from(field.ty.clone())
}

fn without_last_element(path: Option<&syn::ExprPath>, last: &str) -> Option<syn::ExprPath> {
    match path {
        Some(expr_path)
            if expr_path
                .path
                .segments
                .last()
                .map(|p| p.value().ident == last)
                .unwrap_or(false) =>
        {
            let mut expr_path = expr_path.clone();
            expr_path.path.segments.pop();
            if let Some(segment) = expr_path.path.segments.pop() {
                expr_path.path.segments.push(segment.into_value())
            }
            Some(expr_path)
        }
        _ => None,
    }
}
