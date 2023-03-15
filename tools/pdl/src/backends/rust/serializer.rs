use crate::analyzer::ast as analyzer_ast;
use crate::backends::rust::{mask_bits, types};
use crate::{ast, lint};
use heck::ToUpperCamelCase;
use quote::{format_ident, quote};

/// A single bit-field value.
struct BitField {
    value: proc_macro2::TokenStream, // An expression which produces a value.
    field_type: types::Integer,      // The type of the value.
    shift: usize,                    // A bit-shift to apply to `value`.
}

pub struct FieldSerializer<'a> {
    scope: &'a lint::Scope<'a>,
    endianness: ast::EndiannessValue,
    packet_name: &'a str,
    span: &'a proc_macro2::Ident,
    chunk: Vec<BitField>,
    code: Vec<proc_macro2::TokenStream>,
    shift: usize,
}

impl<'a> FieldSerializer<'a> {
    pub fn new(
        scope: &'a lint::Scope<'a>,
        endianness: ast::EndiannessValue,
        packet_name: &'a str,
        span: &'a proc_macro2::Ident,
    ) -> FieldSerializer<'a> {
        FieldSerializer {
            scope,
            endianness,
            packet_name,
            span,
            chunk: Vec::new(),
            code: Vec::new(),
            shift: 0,
        }
    }

    pub fn add(&mut self, field: &analyzer_ast::Field) {
        match &field.desc {
            _ if self.scope.is_bitfield(field) => self.add_bit_field(field),
            ast::FieldDesc::Array { id, width, .. } => {
                self.add_array_field(id, *width, self.scope.get_field_declaration(field))
            }
            ast::FieldDesc::Typedef { id, type_id } => {
                self.add_typedef_field(id, type_id);
            }
            ast::FieldDesc::Payload { .. } | ast::FieldDesc::Body { .. } => {
                self.add_payload_field()
            }
            _ => todo!("Cannot yet serialize {field:?}"),
        }
    }

    fn add_bit_field(&mut self, field: &analyzer_ast::Field) {
        let width = self.scope.get_field_width(field, false).unwrap();
        let shift = self.shift;

        match &field.desc {
            ast::FieldDesc::Scalar { id, width } => {
                let field_name = format_ident!("{id}");
                let field_type = types::Integer::new(*width);
                if field_type.width > *width {
                    let packet_name = &self.packet_name;
                    let max_value = mask_bits(*width, "u64");
                    self.code.push(quote! {
                        if self.#field_name > #max_value {
                            panic!(
                                "Invalid value for {}::{}: {} > {}",
                                #packet_name, #id, self.#field_name, #max_value
                            );
                        }
                    });
                }
                self.chunk.push(BitField { value: quote!(self.#field_name), field_type, shift });
            }
            ast::FieldDesc::FixedEnum { enum_id, tag_id, .. } => {
                let field_type = types::Integer::new(width);
                let enum_id = format_ident!("{enum_id}");
                let tag_id = format_ident!("{}", tag_id.to_upper_camel_case());
                self.chunk.push(BitField { value: quote!(#enum_id::#tag_id), field_type, shift });
            }
            ast::FieldDesc::FixedScalar { value, .. } => {
                let field_type = types::Integer::new(width);
                let value = proc_macro2::Literal::usize_unsuffixed(*value);
                self.chunk.push(BitField { value: quote!(#value), field_type, shift });
            }
            ast::FieldDesc::Typedef { id, .. } => {
                let field_name = format_ident!("{id}");
                let field_type = types::Integer::new(width);
                let to_u = format_ident!("to_u{}", field_type.width);
                // TODO(mgeisler): remove `unwrap` and return error to
                // caller in generated code.
                self.chunk.push(BitField {
                    value: quote!(self.#field_name.#to_u().unwrap()),
                    field_type,
                    shift,
                });
            }
            ast::FieldDesc::Reserved { .. } => {
                // Nothing to do here.
            }
            ast::FieldDesc::Size { field_id, width, .. } => {
                let packet_name = &self.packet_name;
                let max_value = mask_bits(*width, "usize");

                let decl = self.scope.typedef.get(self.packet_name).unwrap();
                let scope = self.scope.scopes.get(decl).unwrap();
                let value_field = scope.get_packet_field(field_id).unwrap();

                let field_name = format_ident!("{field_id}");
                let field_type = types::Integer::new(*width);
                // TODO: size modifier

                let value_field_decl = self.scope.get_field_declaration(value_field);

                let field_size_name = format_ident!("{field_id}_size");
                let array_size = match (&value_field.desc, value_field_decl.map(|decl| &decl.desc))
                {
                    (ast::FieldDesc::Payload { .. } | ast::FieldDesc::Body { .. }, _) => {
                        if let ast::DeclDesc::Packet { .. } = &decl.desc {
                            quote! { self.child.get_total_size() }
                        } else {
                            quote! { self.payload.len() }
                        }
                    }
                    (ast::FieldDesc::Array { width: Some(width), .. }, _)
                    | (ast::FieldDesc::Array { .. }, Some(ast::DeclDesc::Enum { width, .. })) => {
                        let byte_width = syn::Index::from(width / 8);
                        if byte_width.index == 1 {
                            quote! { self.#field_name.len() }
                        } else {
                            quote! { (self.#field_name.len() * #byte_width) }
                        }
                    }
                    (ast::FieldDesc::Array { .. }, _) => {
                        self.code.push(quote! {
                            let #field_size_name = self.#field_name
                                .iter()
                                .map(|elem| elem.get_size())
                                .sum::<usize>();
                        });
                        quote! { #field_size_name }
                    }
                    _ => panic!("Unexpected size field: {field:?}"),
                };

                self.code.push(quote! {
                    if #array_size > #max_value {
                        panic!(
                            "Invalid length for {}::{}: {} > {}",
                            #packet_name, #field_id, #array_size, #max_value
                        );
                    }
                });

                self.chunk.push(BitField {
                    value: quote!(#array_size as #field_type),
                    field_type,
                    shift,
                });
            }
            ast::FieldDesc::Count { field_id, width, .. } => {
                let field_name = format_ident!("{field_id}");
                let field_type = types::Integer::new(*width);
                if field_type.width > *width {
                    let packet_name = &self.packet_name;
                    let max_value = mask_bits(*width, "usize");
                    self.code.push(quote! {
                        if self.#field_name.len() > #max_value {
                            panic!(
                                "Invalid length for {}::{}: {} > {}",
                                #packet_name, #field_id, self.#field_name.len(), #max_value
                            );
                        }
                    });
                }
                self.chunk.push(BitField {
                    value: quote!(self.#field_name.len() as #field_type),
                    field_type,
                    shift,
                });
            }
            _ => todo!("{field:?}"),
        }

        self.shift += width;
        if self.shift % 8 == 0 {
            self.pack_bit_fields()
        }
    }

    fn pack_bit_fields(&mut self) {
        assert_eq!(self.shift % 8, 0);
        let chunk_type = types::Integer::new(self.shift);
        let values = self
            .chunk
            .drain(..)
            .map(|BitField { mut value, field_type, shift }| {
                if field_type.width != chunk_type.width {
                    // We will be combining values with `|`, so we
                    // need to cast them first.
                    value = quote! { (#value as #chunk_type) };
                }
                if shift > 0 {
                    let op = quote!(<<);
                    let shift = proc_macro2::Literal::usize_unsuffixed(shift);
                    value = quote! { (#value #op #shift) };
                }
                value
            })
            .collect::<Vec<_>>();

        match values.as_slice() {
            [] => {
                let span = format_ident!("{}", self.span);
                let count = syn::Index::from(self.shift / 8);
                self.code.push(quote! {
                    #span.put_bytes(0, #count);
                });
            }
            [value] => {
                let put = types::put_uint(self.endianness, value, self.shift, self.span);
                self.code.push(quote! {
                    #put;
                });
            }
            _ => {
                let put = types::put_uint(self.endianness, &quote!(value), self.shift, self.span);
                self.code.push(quote! {
                    let value = #(#values)|*;
                    #put;
                });
            }
        }

        self.shift = 0;
    }

    fn add_array_field(
        &mut self,
        id: &str,
        width: Option<usize>,
        decl: Option<&analyzer_ast::Decl>,
    ) {
        // TODO: padding

        let serialize = match width {
            Some(width) => {
                let value = quote!(*elem);
                types::put_uint(self.endianness, &value, width, self.span)
            }
            None => {
                if let Some(ast::DeclDesc::Enum { width, .. }) = decl.map(|decl| &decl.desc) {
                    let field_type = types::Integer::new(*width);
                    let to_u = format_ident!("to_u{}", field_type.width);
                    types::put_uint(
                        self.endianness,
                        &quote!(elem.#to_u().unwrap()),
                        *width,
                        self.span,
                    )
                } else {
                    let span = format_ident!("{}", self.span);
                    quote! {
                        elem.write_to(#span)
                    }
                }
            }
        };

        let id = format_ident!("{id}");
        self.code.push(quote! {
            for elem in &self.#id {
                #serialize;
            }
        });
    }

    fn add_typedef_field(&mut self, id: &str, type_id: &str) {
        assert_eq!(self.shift, 0, "Typedef field does not start on an octet boundary");
        let decl = self.scope.typedef[type_id];
        if let ast::DeclDesc::Struct { parent_id: Some(_), .. } = &decl.desc {
            panic!("Derived struct used in typedef field");
        }

        let id = format_ident!("{id}");
        let span = format_ident!("{}", self.span);
        self.code.push(quote! {
            self.#id.write_to(#span);
        });
    }

    fn add_payload_field(&mut self) {
        if self.shift != 0 && self.endianness == ast::EndiannessValue::BigEndian {
            panic!("Payload field does not start on an octet boundary");
        }

        let decl = self.scope.typedef[self.packet_name];
        let is_packet = matches!(&decl.desc, ast::DeclDesc::Packet { .. });

        let child_ids = self
            .scope
            .iter_children(self.packet_name)
            .map(|child| format_ident!("{}", child.id().unwrap()))
            .collect::<Vec<_>>();

        let span = format_ident!("{}", self.span);
        if self.shift == 0 {
            if is_packet {
                let packet_data_child = format_ident!("{}DataChild", self.packet_name);
                self.code.push(quote! {
                    match &self.child {
                        #(#packet_data_child::#child_ids(child) => child.write_to(#span),)*
                        #packet_data_child::Payload(payload) => #span.put_slice(payload),
                        #packet_data_child::None => {},
                    }
                })
            } else {
                self.code.push(quote! {
                    #span.put_slice(&self.payload);
                });
            }
        } else {
            todo!("Shifted payloads");
        }
    }
}

impl quote::ToTokens for FieldSerializer<'_> {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let code = &self.code;
        tokens.extend(quote! {
            #(#code)*
        });
    }
}
