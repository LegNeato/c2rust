#![deny(missing_docs)]
//! This module provides translation for bitfield structs and operations on them. Generated code
//! requires the use of the c2rust-bitfields crate.

use std::collections::HashSet;
use std::ops::Index;

use super::TranslationError;
use crate::c_ast::{BinOp, CDeclId, CDeclKind, CExprId, CRecordId, CTypeId};
use crate::translator::{ExprContext, Translation, PADDING_SUFFIX};
use crate::with_stmts::WithStmts;
use c2rust_ast_builder::mk;
use c2rust_ast_printer::pprust;
use syn::{
    self, AttrStyle, BinOp as RBinOp, Expr, Meta,
    NestedMeta, Stmt, Field, Type,
    ExprBlock, ExprAssign, ExprAssignOp, ExprBinary, ExprUnary, ExprMethodCall, ExprCast,
};
use syntax_pos::DUMMY_SP;

use itertools::EitherOrBoth::{Both, Right};
use itertools::Itertools;

#[derive(Debug)]
enum FieldType {
    BitfieldGroup {
        start_bit: u64,
        field_name: String,
        bytes: u64,
        attrs: Vec<(String, Box<Type>, String)>,
    },
    Padding {
        bytes: u64,
    },
    ComputedPadding {
        ident: String,
    },
    Regular {
        name: String,
        ctype: CTypeId,
        field: Field,
        use_inner_type: bool,
    },
}

fn contains_block(expr_kind: &Expr) -> bool {
    match expr_kind {
        Expr::Block(..) => true,
        Expr::Assign(ExprAssign {left, right, ..}) => contains_block(&left) || contains_block(&right),
        Expr::AssignOp(ExprAssignOp {left, right, ..}) => contains_block(&left) || contains_block(&right),
        Expr::Binary(ExprBinary {left, right, ..}) => contains_block(&left) || contains_block(&right),
        Expr::Unary(ExprUnary {expr, ..}) => contains_block(&expr),
        Expr::MethodCall(ExprMethodCall {args, ..}) => args.iter().map(|e| contains_block(&e)).any(|b| b),
        Expr::Cast(ExprCast {expr, ..}) => contains_block(&expr),
        _ => false,
    }
}

fn assignment_metaitem(lhs: &str, rhs: &str) -> NestedMeta {
    use c2rust_ast_builder::Make;
    let token = rhs.make(&mk());
    let meta_item = Meta::NameValue(syn::MetaNameValue {
        path: mk().path(lhs),
        eq_token: Default::default(),
        lit: token,
    });

    NestedMeta::Meta(meta_item)
}

impl<'a> Translation<'a> {
    /// This method aggregates bitfield struct field information by way of:
    /// 1. Collecting consecutive bytes of bitfields into a single FieldType::BitfieldGroup
    /// 2. Summing up the number of padding bytes between fields (or at the end of a struct)
    ///    into a FieldType::Padding
    /// 3. A standard field into a FieldType::Regular
    fn get_field_types(
        &self,
        record_id: CRecordId,
        field_ids: &[CDeclId],
        platform_byte_size: u64,
    ) -> Result<Vec<FieldType>, TranslationError> {
        let mut reorganized_fields = Vec::new();
        let mut last_bitfield_group: Option<FieldType> = None;
        let mut next_byte_pos = 0;
        let mut encountered_bytes = HashSet::new();

        for field_id in field_ids {
            if let CDeclKind::Field {
                typ,
                bitfield_width,
                platform_bit_offset,
                platform_type_bitwidth,
                ..
            } = self.ast_context.index(*field_id).kind
            {
                let field_name = self
                    .type_converter
                    .borrow()
                    .resolve_field_name(Some(record_id), *field_id)
                    .unwrap();

                let ctype = typ.ctype;
                let mut ty = self.convert_type(ctype)?;
                let bitfield_width = match bitfield_width {
                    // Bitfield widths of 0 should just be markers for clang,
                    // we shouldn't need to explicitly handle it ourselves
                    Some(0) => {
                        // Hit non bitfield group so existing one is all set
                        if let Some(field_group) = last_bitfield_group.take() {
                            reorganized_fields.push(field_group);
                        }

                        continue;
                    }
                    None => {
                        // Hit non bitfield group so existing one is all set
                        if let Some(field_group) = last_bitfield_group.take() {
                            reorganized_fields.push(field_group);

                            // Need to add padding first
                            if (platform_bit_offset / 8) > next_byte_pos {
                                let bytes = (platform_bit_offset / 8) - next_byte_pos;
                                reorganized_fields.push(FieldType::Padding { bytes });
                            }
                        }

                        let mut use_inner_type = false;
                        let mut extra_fields = vec![];
                        if self.ast_context.is_packed_struct_decl(record_id)
                            && self.ast_context.is_aligned_struct_type(ctype)
                        {
                            // If we're embedding an aligned structure inside a packed one,
                            // we need to use the `_Inner` version and add padding
                            let decl_id = self
                                .ast_context
                                .resolve_type(ctype)
                                .kind
                                .as_underlying_decl()
                                .unwrap();

                            let inner_name = self.resolve_decl_inner_name(decl_id);
                            ty = mk().path_ty(mk().path(vec![inner_name]));

                            use_inner_type = true;

                            // Add the padding field
                            let padding_name = self
                                .type_converter
                                .borrow_mut()
                                .resolve_decl_suffix_name(decl_id, PADDING_SUFFIX)
                                .to_owned();
                            extra_fields.push(FieldType::ComputedPadding {
                                ident: padding_name,
                            })
                        }

                        let field = mk().pub_().struct_field(field_name.clone(), ty);

                        reorganized_fields.push(FieldType::Regular {
                            name: field_name,
                            ctype,
                            field,
                            use_inner_type,
                        });
                        reorganized_fields.extend(extra_fields.into_iter());

                        next_byte_pos = (platform_bit_offset + platform_type_bitwidth) / 8;

                        continue;
                    }
                    Some(bw) => bw,
                };

                // Ensure we aren't looking at overlapping bits in the same byte
                if (platform_bit_offset / 8) > next_byte_pos {
                    let bytes = (platform_bit_offset / 8) - next_byte_pos;
                    reorganized_fields.push(FieldType::Padding { bytes });
                }

                match last_bitfield_group {
                    Some(FieldType::BitfieldGroup {
                        start_bit,
                        field_name: ref mut name,
                        ref mut bytes,
                        ref mut attrs,
                    }) => {
                        name.push('_');
                        name.push_str(&field_name);

                        let end_bit = platform_bit_offset + bitfield_width;

                        // Add to the total byte size of the bitfield group only if
                        // we have not already enountered this byte
                        for bit in platform_bit_offset..end_bit {
                            let byte = bit / 8;

                            if !encountered_bytes.contains(&byte) {
                                *bytes += 1;
                                encountered_bytes.insert(byte);
                            }
                        }

                        let bit_start = platform_bit_offset - start_bit;
                        let bit_end = bit_start + bitfield_width - 1;
                        let bit_range = format!("{}..={}", bit_start, bit_end);

                        attrs.push((field_name.clone(), ty, bit_range));
                    }
                    Some(_) => unreachable!("Found last bitfield group which is not a group"),
                    None => {
                        let mut bytes = 0;
                        let end_bit = platform_bit_offset + bitfield_width;

                        // Add to the total byte size of the bitfield group only if
                        // we have not already enountered this byte
                        for bit in platform_bit_offset..end_bit {
                            let byte = bit / 8;

                            if !encountered_bytes.contains(&byte) {
                                bytes += 1;
                                encountered_bytes.insert(byte);
                            }
                        }

                        let bit_range = format!("0..={}", bitfield_width - 1);
                        let attrs = vec![(field_name.clone(), ty, bit_range)];

                        last_bitfield_group = Some(FieldType::BitfieldGroup {
                            start_bit: platform_bit_offset,
                            field_name,
                            bytes,
                            attrs,
                        });
                    }
                }

                next_byte_pos = (platform_bit_offset + bitfield_width - 1) / 8 + 1;
            }
        }

        // Find leftover bitfield group at end: it's all set
        if let Some(field_group) = last_bitfield_group.take() {
            reorganized_fields.push(field_group);

            // Packed structs can cause platform_byte_size < next_byte_pos
            if platform_byte_size > next_byte_pos {
                let bytes = platform_byte_size - next_byte_pos;

                // Need to add padding to end if we haven't hit the expected total byte size
                reorganized_fields.push(FieldType::Padding { bytes });
            }
        }

        Ok(reorganized_fields)
    }

    /// Here we output a struct derive to generate bitfield data that looks like this:
    ///
    /// ```no_run
    /// #[derive(BitfieldStruct, Clone, Copy)]
    /// #[repr(C, align(2))]
    /// struct Foo {
    ///     #[bitfield(name = "bf1", ty = "libc::c_char", bits = "0..=9")]
    ///     #[bitfield(name = "bf2", ty = "libc::c_uchar",bits = "10..=15")]
    ///     bf1_bf2: [u8; 2],
    ///     non_bf: u64,
    ///     _pad: [u8; 2],
    /// }
    /// ```
    pub fn convert_struct_fields(
        &self,
        struct_id: CRecordId,
        field_ids: &[CDeclId],
        platform_byte_size: u64,
    ) -> Result<Vec<Field>, TranslationError> {
        let mut field_entries = Vec::with_capacity(field_ids.len());
        // We need to clobber bitfields in consecutive bytes together (leaving
        // regular fields alone) and add in padding as necessary
        let reorganized_fields = self.get_field_types(struct_id, field_ids, platform_byte_size)?;

        let mut padding_count = 0;
        let mut next_padding_field = || {
            let field_name = self
                .type_converter
                .borrow_mut()
                .declare_padding(struct_id, padding_count);
            padding_count += 1;
            field_name
        };

        for field_type in reorganized_fields {
            match field_type {
                FieldType::BitfieldGroup {
                    start_bit: _,
                    field_name,
                    bytes,
                    attrs,
                } => {
                    let ty = mk().array_ty(
                        mk().ident_ty("u8"),
                        mk().lit_expr(mk().int_unsuffixed_lit(bytes.into())),
                    );
                    let mut field = mk();
                    let field_attrs = attrs.iter().map(|attr| {
                            TyKind::Path(_, path) => pprust::path_to_string(path),
                        let ty_str = match &*attr.1 {
                            _ => unreachable!("Found type other than path"),
                        };
                        let field_attr_items = vec![
                            assigment_metaitem("name", &attr.0),
                            assigment_metaitem("ty", &ty_str),
                            assigment_metaitem("bits", &attr.2),
                        ];

                        mk().meta_list("bitfield", field_attr_items)
                    });

                    for field_attr in field_attrs {
                        field = field.meta_item_attr(AttrStyle::Outer, field_attr);
                    }

                    field_entries.push(field.pub_().struct_field(field_name, ty));
                }
                FieldType::Padding { bytes } => {
                    let field_name = next_padding_field();
                    let ty = mk().array_ty(
                        mk().ident_ty("u8"),
                        mk().lit_expr(mk().int_unsuffixed_lit(bytes.into())),
                    );

                    // Mark it with `#[bitfield(padding)]`
                    let field_padding_inner = NestedMeta::Meta(mk().meta_path("padding"));
                    let field_padding_inner =
                        vec![mk().nested_meta_item(field_padding_inner)];
                    let field_padding_outer =
                        mk().meta_list("bitfield", field_padding_inner);
                    let field = mk()
                        .meta_item_attr(AttrStyle::Outer, field_padding_outer)
                        .pub_()
                        .struct_field(field_name, ty);

                    field_entries.push(field);
                }
                FieldType::ComputedPadding { ident } => {
                    let field_name = next_padding_field();
                    let ty = mk().array_ty(mk().ident_ty("u8"), mk().ident_expr(ident));

                    // TODO: disable cross-checks on this field
                    let field = mk().pub_().struct_field(field_name, ty);

                    field_entries.push(field);
                }
                FieldType::Regular { field, .. } => field_entries.push(field),
            }
        }
        Ok(field_entries)
    }

    /// Here we output a block to generate a struct literal initializer in.
    /// It looks like this in locals and (sectioned) statics:
    ///
    /// ```no_run
    /// {
    ///     let mut init = Foo {
    ///         bf1_bf2: [0; 2],
    ///         non_bf: 32,
    ///         _pad: [0; 2],
    ///     };
    ///     init.set_bf1(-12);
    ///     init.set_bf2(34);
    ///     init
    /// }
    /// ```
    pub fn convert_struct_literal(
        &self,
        ctx: ExprContext,
        struct_id: CRecordId,
        field_expr_ids: &[CExprId],
    ) -> Result<WithStmts<Box<Expr>>, TranslationError> {
        let name = self.resolve_decl_inner_name(struct_id);

        let (field_decl_ids, platform_byte_size) = match self.ast_context.index(struct_id).kind {
            CDeclKind::Struct {
                fields: Some(ref fields),
                platform_byte_size,
                ..
            } => (fields, platform_byte_size),

            CDeclKind::Struct { fields: None, .. } => {
                return Err(TranslationError::generic(
                    "Attempted to zero-initialize forward-declared struct",
                ))
            }

            _ => panic!("Struct literal declaration mismatch"),
        };

        let mut fields = Vec::with_capacity(field_decl_ids.len());
        let reorganized_fields =
            self.get_field_types(struct_id, field_decl_ids, platform_byte_size)?;
        let local_pat = mk().mutbl().ident_pat("init");
        let mut padding_count = 0;
        let mut next_padding_field = || {
            let field_name = self
                .type_converter
                .borrow_mut()
                .declare_padding(struct_id, padding_count);
            padding_count += 1;
            field_name
        };

        // Add in zero inits for both padding as well as bitfield groups
        for field_type in reorganized_fields {
            match field_type {
                FieldType::BitfieldGroup {
                    field_name, bytes, ..
                } => {
                    let array_expr = mk().repeat_expr(
                        mk().lit_expr(mk().int_unsuffixed_lit(0)),
                        mk().lit_expr(mk().int_unsuffixed_lit(bytes.into())),
                    );
                    let field = mk().field(field_name, array_expr);

                    fields.push(WithStmts::new_val(field));
                }
                FieldType::Padding { bytes } => {
                    let field_name = next_padding_field();
                    let array_expr = mk().repeat_expr(
                        mk().lit_expr(mk().int_unsuffixed_lit(0)),
                        mk().lit_expr(mk().int_unsuffixed_lit(bytes.into())),
                    );
                    let field = mk().field(field_name, array_expr);

                    fields.push(WithStmts::new_val(field));
                }
                FieldType::ComputedPadding { ident } => {
                    let field_name = next_padding_field();
                    let array_expr = mk().repeat_expr(
                        mk().lit_expr(mk().int_unsuffixed_lit(0)),
                        mk().ident_expr(ident),
                    );
                    let field = mk().field(field_name, array_expr);

                    fields.push(WithStmts::new_val(field));
                }
                _ => {}
            }
        }

        // Bitfield widths of 0 should just be markers for clang,
        // we shouldn't need to explicitly handle it ourselves
        let is_packed = self.ast_context.is_packed_struct_decl(struct_id);
        let field_info_iter = field_decl_ids.iter().filter_map(|field_id| {
            match self.ast_context.index(*field_id).kind {
                CDeclKind::Field {
                    bitfield_width: Some(0),
                    ..
                } => None,
                CDeclKind::Field {
                    typ,
                    bitfield_width,
                    ..
                } => {
                    let field_name = self
                        .type_converter
                        .borrow()
                        .resolve_field_name(None, *field_id)
                        .unwrap();

                    let use_inner_type =
                        is_packed && self.ast_context.is_aligned_struct_type(typ.ctype);
                    Some((field_name, typ, bitfield_width, use_inner_type))
                }
                _ => None,
            }
        });
        let zipped_iter = field_expr_ids.iter().zip_longest(field_info_iter);
        let mut bitfield_inits = Vec::new();

        // Specified record fields which are not bitfields need to be added
        for item in zipped_iter {
            match item {
                Right((field_name, ty, bitfield_width, use_inner_type)) => {
                    if bitfield_width.is_some() {
                        continue;
                    }

                    let mut init = self.implicit_default_expr(ty.ctype, ctx.is_static)?;
                    if !init.is_pure() {
                        return Err(TranslationError::generic(
                            "Expected no statements in field expression",
                        ));
                    }
                    if use_inner_type {
                        // Small hack: we need a value of the inner type,
                        // but `implicit_default_expr` produced a value
                        // of the outer type, so unwrap it manually
                        init = init.map(|fi| mk().anon_field_expr(fi, 0));
                    }
                    let field = init.map(|init| mk().field(field_name, init));
                    fields.push(field);
                }
                Both(field_id, (field_name, _, bitfield_width, use_inner_type)) => {
                    let mut expr = self.convert_expr(ctx.used(), *field_id)?;

                    if !expr.is_pure() {
                        return Err(TranslationError::generic(
                            "Expected no statements in field expression",
                        ));
                    }

                    if use_inner_type {
                        // See comment above
                        expr = expr.map(|fi| mk().anon_field_expr(fi, 0));
                    }

                    if bitfield_width.is_some() {
                        bitfield_inits.push((field_name, expr));

                        continue;
                    }

                    fields.push(expr.map(|expr| mk().field(field_name, expr)));
                }
                _ => unreachable!(),
            }
        }

        fields
            .into_iter()
            .collect::<WithStmts<Vec<syn::FieldValue>>>()
            .and_then(|fields| {
                let struct_expr = mk().struct_expr(name.as_str(), fields);
                let local_variable =
                    Box::new(mk().local(local_pat, None as Option<Box<Type>>, Some(struct_expr)));

                let mut is_unsafe = false;
                let mut stmts = vec![mk().local_stmt(local_variable)];

                // Now we must use the bitfield methods to initialize bitfields
                for (field_name, val) in bitfield_inits {
                    let field_name_setter = format!("set_{}", field_name);
                    let struct_ident = mk().ident_expr("init");
                    is_unsafe |= val.is_unsafe();
                    let val = val
                        .to_pure_expr()
                        .expect("Expected no statements in bitfield initializer");
                    let expr = mk().method_call_expr(struct_ident, field_name_setter, vec![val]);

                    stmts.push(mk().expr_stmt(expr));
                }

                let struct_ident = mk().ident_expr("init");

                stmts.push(mk().expr_stmt(struct_ident));

                let val = mk().block_expr(mk().block(stmts));

                if is_unsafe {
                    Ok(WithStmts::new_unsafe_val(val))
                } else {
                    Ok(WithStmts::new_val(val))
                }
            })
    }

    /// This method handles zero-initializing bitfield structs including bitfields
    /// & padding fields
    pub fn convert_struct_zero_initializer(
        &self,
        name: String,
        struct_id: CRecordId,
        field_ids: &[CDeclId],
        platform_byte_size: u64,
        is_static: bool,
    ) -> Result<WithStmts<Box<Expr>>, TranslationError> {
        let reorganized_fields = self.get_field_types(struct_id, field_ids, platform_byte_size)?;
        let mut fields = Vec::with_capacity(reorganized_fields.len());

        let mut padding_count = 0;
        let mut next_padding_field = || {
            let field_name = self
                .type_converter
                .borrow_mut()
                .declare_padding(struct_id, padding_count);
            padding_count += 1;
            field_name
        };

        for field_type in reorganized_fields {
            match field_type {
                FieldType::BitfieldGroup {
                    field_name, bytes, ..
                } => {
                    let array_expr = mk().repeat_expr(
                        mk().lit_expr(mk().int_unsuffixed_lit(0)),
                        mk().lit_expr(mk().int_unsuffixed_lit(bytes.into())),
                    );
                    let field = mk().field(field_name, array_expr);

                    fields.push(WithStmts::new_val(field));
                }
                FieldType::Padding { bytes } => {
                    let field_name = next_padding_field();
                    let array_expr = mk().repeat_expr(
                        mk().lit_expr(mk().int_unsuffixed_lit(0)),
                        mk().lit_expr(mk().int_unsuffixed_lit(bytes.into())),
                    );
                    let field = mk().field(field_name, array_expr);

                    fields.push(WithStmts::new_val(field));
                }
                FieldType::ComputedPadding { ident } => {
                    let field_name = next_padding_field();
                    let array_expr = mk().repeat_expr(
                        mk().lit_expr(mk().int_unsuffixed_lit(0)),
                        mk().ident_expr(ident),
                    );
                    let field = mk().field(field_name, array_expr);

                    fields.push(WithStmts::new_val(field));
                }
                FieldType::Regular {
                    ctype,
                    name,
                    use_inner_type,
                    ..
                } => {
                    let mut field_init = self.implicit_default_expr(ctype, is_static)?;
                    if !field_init.is_pure() {
                        return Err(TranslationError::generic(
                            "Expected no statements in field expression",
                        ));
                    }
                    if use_inner_type {
                        // See comment above
                        field_init = field_init.map(|fi| mk().anon_field_expr(fi, 0));
                    }
                    fields.push(field_init.map(|init| mk().field(name, init)))
                }
            }
        }

        Ok(fields
            .into_iter()
            .collect::<WithStmts<Vec<syn::FieldValue>>>()
            .map(|fields| mk().struct_expr(name.as_str(), fields)))
    }

    /// This method handles conversion of assignment operators on bitfields.
    /// Regular fields would look like this:
    /// A) bf.a = 1;
    /// B) bf.a += 1;
    ///
    /// However, since we need to call methods for read and write, we generate this:
    /// A) bf.set_a(1);
    /// B) bf.set_a(bf.a() + 1);
    ///
    /// Note that B) requires NLL to be valid rust
    pub fn convert_bitfield_assignment_op_with_rhs(
        &self,
        ctx: ExprContext,
        op: BinOp,
        lhs: CExprId,
        rhs_expr: Box<Expr>,
        field_id: CDeclId,
    ) -> Result<WithStmts<Box<Expr>>, TranslationError> {
        let ctx = ctx.set_bitfield_write(true);
        let named_reference = self.name_reference_write_read(ctx, lhs)?;
        named_reference.and_then(|named_reference| {
            let lhs_expr = named_reference.0;
            let field_name = self
                .type_converter
                .borrow()
                .resolve_field_name(None, field_id)
                .ok_or("Could not find bitfield name")?;
            let setter_name = format!("set_{}", field_name);
            let lhs_expr_read =
                mk().method_call_expr(lhs_expr.clone(), field_name, Vec::<Box<Expr>>::new());
            // Allow the value of this assignment to be used as the RHS of other assignments
            let val = lhs_expr_read.clone();
            let param_expr = match op {
                BinOp::AssignAdd => mk().binary_expr(RBinOp::Add(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::AssignSubtract => mk().binary_expr(RBinOp::Sub(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::AssignMultiply => mk().binary_expr(RBinOp::Mul(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::AssignDivide => mk().binary_expr(RBinOp::Div(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::AssignModulus => mk().binary_expr(RBinOp::Rem(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::AssignBitXor => mk().binary_expr(RBinOp::BitXor(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::AssignShiftLeft => mk().binary_expr(RBinOp::Shl(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::AssignShiftRight => {
                    mk().binary_expr(RBinOp::Shr(Default::default()), lhs_expr_read, rhs_expr)
                }
                BinOp::AssignBitOr => mk().binary_expr(RBinOp::BitOr(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::AssignBitAnd => mk().binary_expr(RBinOp::BitAnd(Default::default()), lhs_expr_read, rhs_expr),
                BinOp::Assign => rhs_expr,
                _ => panic!("Cannot convert non-assignment operator"),
            };

            let mut stmts = vec![];

            // If there's just one statement we should be able to be able to fit it into one line without issue
            // If there's a block we can flatten it into the current scope, and if the expr contains a block it's
            // likely complex enough to warrant putting it into a temporary variable to avoid borrowing issues
            match *param_expr {
                Expr::Block(ExprBlock{ block, ..}) => {
                    let last = block.stmts.len() - 1;

                    for (i, stmt) in block.stmts.iter().enumerate() {
                        if i == last {
                            break;
                        }

                        stmts.push(stmt.clone());
                    }

                    let last_expr = match block.stmts[last] {
                        Stmt::Expr(ref expr) => expr.clone(),
                        _ => return Err(TranslationError::generic("Expected Expr Stmt")),
                    };
                    let method_call = mk().method_call_expr(lhs_expr, setter_name, vec![Box::new(last_expr)]);

                    stmts.push(mk().expr_stmt(method_call));
                }
                _ if contains_block(&param_expr) => {
                    let name = self.renamer.borrow_mut().pick_name("rhs");
                    let name_ident = mk().mutbl().ident_pat(name.clone());
                    let temporary_stmt =
                        mk().local(name_ident, None as Option<Box<Type>>, Some(param_expr.clone()));
                    let assignment_expr =
                        mk().method_call_expr(lhs_expr, setter_name, vec![mk().ident_expr(name)]);

                    stmts.push(mk().local_stmt(Box::new(temporary_stmt)));
                    stmts.push(mk().semi_stmt(assignment_expr));
                }
                _ => {
                    let assignment_expr =
                        mk().method_call_expr(lhs_expr, setter_name, vec![param_expr.clone()]);

                    stmts.push(mk().expr_stmt(assignment_expr));
                }
            };

            return Ok(WithStmts::new(stmts, val));
        })
    }
}
