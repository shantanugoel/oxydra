use proc_macro::TokenStream;

use quote::{format_ident, quote};
use syn::{Attribute, Error, FnArg, ItemFn, Pat, Type, parse_macro_input, spanned::Spanned};

#[proc_macro_attribute]
pub fn tool(args: TokenStream, input: TokenStream) -> TokenStream {
    if !args.is_empty() {
        return Error::new(
            proc_macro2::Span::call_site(),
            "#[tool] does not accept attribute arguments",
        )
        .to_compile_error()
        .into();
    }

    let function = parse_macro_input!(input as ItemFn);
    match expand_tool(function) {
        Ok(expanded) => expanded.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

fn expand_tool(function: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    if function.sig.asyncness.is_none() {
        return Err(Error::new(
            function.sig.span(),
            "#[tool] can only be applied to async functions",
        ));
    }
    if !function.sig.generics.params.is_empty() {
        return Err(Error::new(
            function.sig.generics.span(),
            "#[tool] does not support generic functions",
        ));
    }

    let function_name = function.sig.ident.to_string();
    let schema_function_name = format_ident!("__tool_function_decl_{}", function.sig.ident);
    let description = extract_doc_comment(&function.attrs);

    let mut property_insertions = Vec::new();
    let mut required_parameters = Vec::new();

    for input in &function.sig.inputs {
        let argument = match input {
            FnArg::Typed(argument) => argument,
            FnArg::Receiver(receiver) => {
                return Err(Error::new(
                    receiver.span(),
                    "#[tool] does not support methods with self receivers",
                ));
            }
        };

        let parameter_name = match argument.pat.as_ref() {
            Pat::Ident(identifier) => identifier.ident.to_string(),
            pattern => {
                return Err(Error::new(
                    pattern.span(),
                    "#[tool] parameters must use identifier bindings",
                ));
            }
        };

        let schema_type = map_parameter_type_to_schema_type(argument.ty.as_ref())?;
        property_insertions.push(quote! {
            properties.insert(#parameter_name.to_owned(), ::types::JsonSchema::new(#schema_type));
        });
        required_parameters.push(parameter_name);
    }

    let description_tokens = match description {
        Some(description) => quote! { Some(#description.to_owned()) },
        None => quote! { None },
    };
    let required_tokens = required_parameters
        .iter()
        .map(|parameter| quote! { #parameter.to_owned() });

    Ok(quote! {
        #function

        #[doc(hidden)]
        pub fn #schema_function_name() -> ::types::FunctionDecl {
            let mut properties = ::std::collections::BTreeMap::new();
            #(#property_insertions)*
            ::types::FunctionDecl::new(
                #function_name,
                #description_tokens,
                ::types::JsonSchema::object(properties, vec![#(#required_tokens),*]),
            )
        }
    })
}

fn extract_doc_comment(attributes: &[Attribute]) -> Option<String> {
    let lines = attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("doc"))
        .filter_map(|attribute| match &attribute.meta {
            syn::Meta::NameValue(name_value) => match &name_value.value {
                syn::Expr::Lit(expression) => match &expression.lit {
                    syn::Lit::Str(value) => {
                        let line = value.value();
                        let trimmed = line.trim();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_owned())
                        }
                    }
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();

    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

fn map_parameter_type_to_schema_type(
    parameter_type: &Type,
) -> syn::Result<proc_macro2::TokenStream> {
    if is_path_type(parameter_type, "String") || is_reference_to_str(parameter_type) {
        return Ok(quote!(::types::JsonSchemaType::String));
    }
    if is_path_type(parameter_type, "bool") {
        return Ok(quote!(::types::JsonSchemaType::Boolean));
    }
    if matches_path_type(
        parameter_type,
        &[
            "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
        ],
    ) {
        return Ok(quote!(::types::JsonSchemaType::Integer));
    }
    if matches_path_type(parameter_type, &["f32", "f64"]) {
        return Ok(quote!(::types::JsonSchemaType::Number));
    }

    Err(Error::new(
        parameter_type.span(),
        "#[tool] only supports String, &str, bool, integer, and float arguments in this phase",
    ))
}

fn is_path_type(parameter_type: &Type, expected: &str) -> bool {
    matches!(
        parameter_type,
        Type::Path(type_path) if type_path.qself.is_none() && type_path.path.is_ident(expected)
    )
}

fn matches_path_type(parameter_type: &Type, expected: &[&str]) -> bool {
    expected
        .iter()
        .any(|candidate| is_path_type(parameter_type, candidate))
}

fn is_reference_to_str(parameter_type: &Type) -> bool {
    match parameter_type {
        Type::Reference(reference) => matches!(
            reference.elem.as_ref(),
            Type::Path(type_path)
                if type_path.qself.is_none() && type_path.path.is_ident("str")
        ),
        _ => false,
    }
}
