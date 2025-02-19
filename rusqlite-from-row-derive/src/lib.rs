use darling::{ast::Data, Error, FromDeriveInput, FromField, ToTokens};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Result};
extern crate darling;

/// Calls the fallible entry point and writes any errors to the tokenstream.
#[proc_macro_derive(FromRow, attributes(from_row))]
pub fn derive_from_row(input: TokenStream) -> TokenStream {
    let derive_input = parse_macro_input!(input as DeriveInput);
    match try_derive_from_row(&derive_input) {
        Ok(result) => result,
        Err(err) => err.write_errors().into(),
    }
}

/// Fallible entry point for generating a `FromRow` implementation
fn try_derive_from_row(input: &DeriveInput) -> std::result::Result<TokenStream, Error> {
    let from_row_derive = DeriveFromRow::from_derive_input(input)?;
    Ok(from_row_derive.generate()?)
}

/// Main struct for deriving `FromRow` for a struct.
#[derive(Debug, FromDeriveInput)]
#[darling(
    attributes(from_row),
    forward_attrs(allow, doc, cfg),
    supports(struct_named)
)]
struct DeriveFromRow {
    ident: syn::Ident,
    generics: syn::Generics,
    data: Data<(), FromRowField>,
}

impl DeriveFromRow {
    /// Validates all fields
    fn validate(&self) -> Result<()> {
        for field in self.fields() {
            field.validate()?;
        }

        Ok(())
    }

    /// Generates any additional where clause predicates needed for the fields in this struct.
    fn predicates(&self) -> Result<Vec<TokenStream2>> {
        let mut predicates = Vec::new();

        for field in self.fields() {
            field.add_predicates(&mut predicates)?;
        }

        Ok(predicates)
    }

    /// Provides a slice of this struct's fields.
    fn fields(&self) -> &[FromRowField] {
        match &self.data {
            Data::Struct(fields) => &fields.fields,
            _ => panic!("invalid shape"),
        }
    }

    /// Generate the `FromRow` implementation.
    fn generate(self) -> Result<TokenStream> {
        self.validate()?;

        let ident = &self.ident;

        let (impl_generics, ty_generics, where_clause) = self.generics.split_for_impl();
        let original_predicates = where_clause.clone().map(|w| &w.predicates).into_iter();
        let predicates = self.predicates()?;

        let from_row_fields = self
            .fields()
            .iter()
            .map(|f| f.generate_from_row())
            .collect::<syn::Result<Vec<_>>>()?;

        let try_from_row_fields = self
            .fields()
            .iter()
            .map(|f| f.generate_try_from_row())
            .collect::<syn::Result<Vec<_>>>()?;

        Ok(quote! {
            impl #impl_generics rusqlite_from_row::FromRow for #ident #ty_generics where #(#original_predicates),* #(#predicates),* {

                fn from_row_prefixed(row: &rusqlite_from_row::rusqlite::Row, prefix: &str) -> Self {
                    Self {
                        #(#from_row_fields),*
                    }
                }

                fn try_from_row_prefixed(row: &rusqlite_from_row::rusqlite::Row, prefix: &str) -> std::result::Result<Self, rusqlite_from_row::rusqlite::Error> {
                    Ok(Self {
                        #(#try_from_row_fields),*
                    })
                }
            }
        }
        .into())
    }
}

/// A single field inside of a struct that derives `FromRow`
#[derive(Debug, FromField)]
#[darling(attributes(from_row), forward_attrs(allow, doc, cfg))]
struct FromRowField {
    /// The identifier of this field.
    ident: Option<syn::Ident>,
    /// The type specified in this field.
    ty: syn::Type,
    /// Wether to flatten this field. Flattening means calling the `FromRow` implementation
    /// of `self.ty` instead of extracting it directly from the row.
    #[darling(default)]
    flatten: bool,
    /// Can only be used in combination with flatten. Will prefix all fields of the nested struct
    /// with this string. Can be useful for joins with overlapping names.
    prefix: Option<String>,
    /// Optionaly use this type as the target for `FromRow` or `FromSql`, and then
    /// call `TryFrom::try_from` to convert it the `self.ty`.
    try_from: Option<String>,
    /// Optionaly use this type as the target for `FromRow` or `FromSql`, and then
    /// call `From::from` to convert it the `self.ty`.
    from: Option<String>,
    /// Override the name of the actual sql column instead of using `self.ident`.
    /// Is not compatible with `flatten` since no column is needed there.
    rename: Option<String>,
}

impl FromRowField {
    /// Checks wether this field has a valid combination of attributes
    fn validate(&self) -> Result<()> {
        if self.from.is_some() && self.try_from.is_some() {
            return Err(Error::custom(
                r#"can't combine `#[from_row(from = "..")]` with `#[from_row(try_from = "..")]`"#,
            )
            .into());
        }

        if self.rename.is_some() && self.flatten {
            return Err(Error::custom(
                r#"can't combine `#[from_row(flatten)]` with `#[from_row(rename = "..")]`"#,
            )
            .into());
        }

        Ok(())
    }

    /// Returns a tokenstream of the type that should be returned from either
    /// `FromRow` (when using `flatten`) or `FromSql`.
    fn target_ty(&self) -> Result<TokenStream2> {
        if let Some(from) = &self.from {
            Ok(from.parse()?)
        } else if let Some(try_from) = &self.try_from {
            Ok(try_from.parse()?)
        } else {
            Ok(self.ty.to_token_stream())
        }
    }

    /// Returns the name that maps to the actuall sql column
    /// By default this is the same as the rust field name but can be overwritten by `#[from_row(rename = "..")]`.
    fn column_name(&self) -> String {
        self.rename
            .as_ref()
            .map(Clone::clone)
            .unwrap_or_else(|| self.ident.as_ref().unwrap().to_string())
    }

    /// Pushes the needed where clause predicates for this field.
    ///
    /// By default this is `T: rusqlite::types::FromSql`,
    /// when using `flatten` it's: `T: rusqlite_from_row::FromRow`
    /// and when using either `from` or `try_from` attributes it additionally pushes this bound:
    /// `T: std::convert::From<R>`, where `T` is the type specified in the struct and `R` is the
    /// type specified in the `[try]_from` attribute.
    fn add_predicates(&self, predicates: &mut Vec<TokenStream2>) -> Result<()> {
        let target_ty = &self.target_ty()?;
        let ty = &self.ty;

        predicates.push(if self.flatten {
            quote! (#target_ty: rusqlite_from_row::FromRow)
        } else {
            quote! (#target_ty: rusqlite_from_row::rusqlite::types::FromSql)
        });

        if self.from.is_some() {
            predicates.push(quote!(#ty: std::convert::From<#target_ty>))
        } else if self.try_from.is_some() {
            let try_from = quote!(std::convert::TryFrom<#target_ty>);

            predicates.push(quote!(#ty: #try_from));
            predicates.push(quote!(rusqlite_from_row::rusqlite::Error: std::convert::From<<#ty as #try_from>::Error>));
            predicates.push(quote!(<#ty as #try_from>::Error: std::fmt::Debug));
        }

        Ok(())
    }

    /// Generate the line needed to retrieve this field from a row when calling `from_row`.
    fn generate_from_row(&self) -> Result<TokenStream2> {
        let ident = self.ident.as_ref().unwrap();
        let column_name = self.column_name();
        let field_ty = &self.ty;
        let target_ty = self.target_ty()?;

        let mut base = if self.flatten {
            let prefix = self.prefix.clone().unwrap_or_default();

            quote!(<#target_ty as rusqlite_from_row::FromRow>::from_row_prefixed(row, #prefix))
        } else {
            quote!(rusqlite_from_row::rusqlite::Row::get_unwrap::<&str, #target_ty>(row, &format!("{}{}", prefix, #column_name)))
        };

        if self.from.is_some() {
            base = quote!(<#field_ty as std::convert::From<#target_ty>>::from(#base));
        } else if self.try_from.is_some() {
            base = quote!(<#field_ty as std::convert::TryFrom<#target_ty>>::try_from(#base).expect("could not convert column"));
        };

        Ok(quote!(#ident: #base))
    }

    /// Generate the line needed to retrieve this field from a row when calling `try_from_row`.
    fn generate_try_from_row(&self) -> Result<TokenStream2> {
        let ident = self.ident.as_ref().unwrap();
        let column_name = self.column_name();
        let field_ty = &self.ty;
        let target_ty = self.target_ty()?;

        let mut base = if self.flatten {
            let prefix = self.prefix.clone().unwrap_or_default();

            quote!(<#target_ty as rusqlite_from_row::FromRow>::try_from_row_prefixed(row, #prefix)?)
        } else {
            quote!(rusqlite_from_row::rusqlite::Row::get::<&str, #target_ty>(row, &format!("{}{}", prefix, #column_name))?)
        };

        if self.from.is_some() {
            base = quote!(<#field_ty as std::convert::From<#target_ty>>::from(#base));
        } else if self.try_from.is_some() {
            base = quote!(<#field_ty as std::convert::TryFrom<#target_ty>>::try_from(#base)?);
        };

        Ok(quote!(#ident: #base))
    }
}
