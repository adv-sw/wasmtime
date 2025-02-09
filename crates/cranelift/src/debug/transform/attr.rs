use super::address_transform::AddressTransform;
use super::expression::{compile_expression, CompiledExpression, FunctionFrameInfo, write_expr_addr};
use super::range_info_builder::RangeInfoBuilder;
use super::refs::{PendingDebugInfoRefs, PendingUnitRefs};
use super::{DebugInputContext, Reader, TransformError};
use anyhow::{bail, Error};
use cranelift_codegen::isa::TargetIsa;
use gimli::{
    write, AttributeValue, DebugLineOffset, DebugLineStr, DebugStr, DebugStrOffsets,
    DebuggingInformationEntry, Unit,
};


#[derive(Debug)]
pub(crate) enum FileAttributeContext<'a> {
    Root(Option<DebugLineOffset>),
    Children {
        file_map: &'a [write::FileId],
        file_index_base: u64,
        frame_base: Option<&'a CompiledExpression>,
    },
}

fn is_exprloc_to_loclist_allowed(attr_name: gimli::constants::DwAt) -> bool {
    match attr_name {
        gimli::DW_AT_location
        | gimli::DW_AT_string_length
        | gimli::DW_AT_return_addr
        | gimli::DW_AT_data_member_location
        | gimli::DW_AT_frame_base
        | gimli::DW_AT_segment
        | gimli::DW_AT_static_link
        | gimli::DW_AT_use_location
        | gimli::DW_AT_vtable_elem_location => true,
        _ => false,
    }
}

pub(crate) fn clone_die_attributes<'a, R>(
    dwarf: &gimli::Dwarf<R>,
    unit: &Unit<R, R::Offset>,
    entry: &DebuggingInformationEntry<R>,
    context: &DebugInputContext<R>,
    addr_tr: &'a AddressTransform,
    frame_info: Option<&FunctionFrameInfo>,
    out_unit: &mut write::Unit,
    current_scope_id: write::UnitEntryId,
    subprogram_range_builder: Option<RangeInfoBuilder>,
    scope_ranges: Option<&Vec<(u64, u64)>>,
    cu_low_pc: u64,
    out_strings: &mut write::StringTable,
    pending_die_refs: &mut PendingUnitRefs,
    pending_di_refs: &mut PendingDebugInfoRefs,
    file_context: FileAttributeContext<'a>,
    isa: &dyn TargetIsa,
) -> Result<(), Error>
where
    R: Reader,
{
    let unit_encoding = unit.encoding();

    let range_info = if let Some(subprogram_range_builder) = subprogram_range_builder {
        subprogram_range_builder
    } else {
        // FIXME for CU: currently address_transform operate on a single
        // function range, and when CU spans multiple ranges the
        // transformation may be incomplete.
        RangeInfoBuilder::from(dwarf, unit, entry, context, cu_low_pc)?
    };
    range_info.build(addr_tr, out_unit, current_scope_id);

    let mut attrs = entry.attrs();
    while let Some(attr) = attrs.next()? {
        let attr_value = match attr.value() {
            AttributeValue::Addr(_) | AttributeValue::DebugAddrIndex(_)
                if attr.name() == gimli::DW_AT_low_pc =>
            {
                continue;
            }
            AttributeValue::Udata(_) if attr.name() == gimli::DW_AT_high_pc => {
                continue;
            }
            AttributeValue::RangeListsRef(_) if attr.name() == gimli::DW_AT_ranges => {
                continue;
            }
            AttributeValue::Exprloc(_) if attr.name() == gimli::DW_AT_frame_base => {
                continue;
            }
            AttributeValue::DebugAddrBase(_) | AttributeValue::DebugStrOffsetsBase(_) => {
                continue;
            }

            AttributeValue::Addr(u) => {
                let addr = addr_tr.translate(u).unwrap_or(write::Address::Constant(0));
                write::AttributeValue::Address(addr)
            }
            AttributeValue::DebugAddrIndex(i) => {
                let u = context.debug_addr.get_address(4, unit.addr_base, i)?;
                let addr = addr_tr.translate(u).unwrap_or(write::Address::Constant(0));
                write::AttributeValue::Address(addr)
            }
            AttributeValue::Udata(u) => write::AttributeValue::Udata(u),
            AttributeValue::Data1(d) => write::AttributeValue::Data1(d),
            AttributeValue::Data2(d) => write::AttributeValue::Data2(d),
            AttributeValue::Data4(d) => write::AttributeValue::Data4(d),
            AttributeValue::Sdata(d) => write::AttributeValue::Sdata(d),
            AttributeValue::Flag(f) => write::AttributeValue::Flag(f),
            AttributeValue::DebugLineRef(line_program_offset) => {
                if let FileAttributeContext::Root(o) = file_context {
                    if o != Some(line_program_offset) {
                        return Err(TransformError("invalid debug_line offset").into());
                    }
                    write::AttributeValue::LineProgramRef
                } else {
                    return Err(TransformError("unexpected debug_line index attribute").into());
                }
            }
            AttributeValue::FileIndex(i) => {
                if let FileAttributeContext::Children {
                    file_map,
                    file_index_base,
                    ..
                } = file_context
                {
                    write::AttributeValue::FileIndex(Some(file_map[(i - file_index_base) as usize]))
                } else {
                    return Err(TransformError("unexpected file index attribute").into());
                }
            }
            AttributeValue::DebugStrRef(str_offset) => {
                let s = context.debug_str.get_str(str_offset)?.to_slice()?.to_vec();
                write::AttributeValue::StringRef(out_strings.add(s))
            }
            AttributeValue::DebugStrOffsetsIndex(i) => {
                let str_offset = context.debug_str_offsets.get_str_offset(
                    gimli::Format::Dwarf32,
                    unit.str_offsets_base,
                    i,
                )?;
                let s = context.debug_str.get_str(str_offset)?.to_slice()?.to_vec();
                write::AttributeValue::StringRef(out_strings.add(s))
            }
            AttributeValue::RangeListsRef(r) => {
                let r = dwarf.ranges_offset_from_raw(unit, r);
                let range_info = RangeInfoBuilder::from_ranges_ref(unit, r, context, cu_low_pc)?;
                let range_list_id = range_info.build_ranges(addr_tr, &mut out_unit.ranges);
                write::AttributeValue::RangeListRef(range_list_id)
            }
            AttributeValue::LocationListsRef(r) => {
                let low_pc = 0;
                let mut locs = context.loclists.locations(
                    r,
                    unit_encoding,
                    low_pc,
                    &context.debug_addr,
                    unit.addr_base,
                )?;
                let frame_base =
                    if let FileAttributeContext::Children { frame_base, .. } = file_context {
                        frame_base
                    } else {
                        None
                    };

                let mut result: Option<Vec<_>> = None;
                while let Some(loc) = locs.next()? {
                    if let Some(expr) = compile_expression(&loc.data, unit_encoding, frame_base)? {
                        let chunk = expr
                            .build_with_locals(
                                &[(loc.range.begin, loc.range.end)],
                                addr_tr,
                                frame_info,
                                isa,
                            )
                            .filter(|i| {
                                // Ignore empty range
                                if let Ok((_, 0, _)) = i {
                                    false
                                } else {
                                    true
                                }
                            })
                            .map(|i| {
                                i.map(|(start, len, expr)| write::Location::StartLength {
                                    begin: start,
                                    length: len,
                                    data: expr,
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        match &mut result {
                            Some(r) => r.extend(chunk),
                            x @ None => *x = Some(chunk),
                        }
                    } else {
                        // FIXME _expr contains invalid expression
                        continue; // ignore entry
                    }
                }
                if result.is_none() {
                    continue; // no valid locations
                }
                let list_id = out_unit.locations.add(write::LocationList(result.unwrap()));
                write::AttributeValue::LocationListRef(list_id)
            }

            AttributeValue::Exprloc(ref expr) => {

                let frame_base =
                    if let FileAttributeContext::Children { frame_base, .. } = file_context {
                        frame_base
                    } else {
                        None
                    };
                if let Some(expr) = compile_expression(expr, unit_encoding, frame_base)? {
                    if expr.is_simple() {
                        if let Some(expr) = expr.build() {
                            write::AttributeValue::Exprloc(expr)
                        } else {
                            continue;
                        }
                    } else {
                        // Conversion to loclist is required.
                        if let Some(scope_ranges) = scope_ranges {
                            let exprs = expr
                                .build_with_locals(scope_ranges, addr_tr, frame_info, isa)
                                .collect::<Result<Vec<_>, _>>()?;
                            if exprs.is_empty() {
                                continue;
                            }
                            let found_single_expr = {
                                // Micro-optimization all expressions alike, use one exprloc.
                                let mut found_expr: Option<write::Expression> = None;
                                for (_, _, expr) in &exprs {
                                    if let Some(ref prev_expr) = found_expr {
                                        if expr == prev_expr {
                                            continue; // the same expression
                                        }
                                        found_expr = None;
                                        break;
                                    }
                                    found_expr = Some(expr.clone())
                                }
                                found_expr
                            };
                            if let Some(expr) = found_single_expr {
                                write::AttributeValue::Exprloc(expr)
                            } else if is_exprloc_to_loclist_allowed(attr.name()) {
                                // Converting exprloc to loclist.
                                let mut locs = Vec::new();
                                for (begin, length, data) in exprs {
                                    if length == 0 {
                                        // Ignore empty range
                                        continue;
                                    }
                                    locs.push(write::Location::StartLength {
                                        begin,
                                        length,
                                        data,
                                    });
                                }
                                let list_id = out_unit.locations.add(write::LocationList(locs));
                                write::AttributeValue::LocationListRef(list_id)
                            } else {
                                continue;
                            }
                        } else {
                            continue;
                        }
                    }
                } else {

                   // compile_expression doesn't support global variable DW_AT_location expressions.
                   // Hence we detect & process here instead.

                   // Export static/global variables. WIP - first pass solution.
                   // Limitations: Value is delivered to debug interface as regular pointer to be reinterpreted by debugger wasmtime filter.
                   // That works - as far as has been tested but isn't ideal, as we're passing around potentially invalid data.
                   // A complete solution would either implement WASMPtr style wrappers (not a pointer, a virtual address)
                   // or implement virtual addresses in DWARF & pass directly via that mechanism. 
                   // DWARF Proposal: DW_OP_vaddr on DW_AT_location.

                   if attr.name() == gimli::DW_AT_location  
                   {
                      let buf = expr.0.to_slice()?;

                      // Ignore unless supported op.
                      if buf[0] != (gimli::DW_OP_addr.0 as u8) { continue; } 

                      // TODO: Detect 64 bit wasm & form source address accordingly.
                      let addr_wasm = u32::from_le_bytes([buf[1],buf[2],buf[3],buf[4]]) as u64;
               
                      // Translate wasm address to target.
                      let write_expr = write_expr_addr(addr_wasm);
                      write::AttributeValue::Exprloc(write_expr)
                   }
                   else
                   {
                       println!("Unsupported expression ignored."); // We probably want to know, so log somehow.
                       continue; // Ignore expression
                   }
               }
            }
            AttributeValue::Encoding(e) => write::AttributeValue::Encoding(e),
            AttributeValue::DecimalSign(e) => write::AttributeValue::DecimalSign(e),
            AttributeValue::Endianity(e) => write::AttributeValue::Endianity(e),
            AttributeValue::Accessibility(e) => write::AttributeValue::Accessibility(e),
            AttributeValue::Visibility(e) => write::AttributeValue::Visibility(e),
            AttributeValue::Virtuality(e) => write::AttributeValue::Virtuality(e),
            AttributeValue::Language(e) => write::AttributeValue::Language(e),
            AttributeValue::AddressClass(e) => write::AttributeValue::AddressClass(e),
            AttributeValue::IdentifierCase(e) => write::AttributeValue::IdentifierCase(e),
            AttributeValue::CallingConvention(e) => write::AttributeValue::CallingConvention(e),
            AttributeValue::Inline(e) => write::AttributeValue::Inline(e),
            AttributeValue::Ordering(e) => write::AttributeValue::Ordering(e),
            AttributeValue::UnitRef(offset) => {
                pending_die_refs.insert(current_scope_id, attr.name(), offset);
                continue;
            }
            AttributeValue::DebugInfoRef(offset) => {
                pending_di_refs.insert(current_scope_id, attr.name(), offset);
                continue;
            }
            a => bail!("Unexpected attribute: {:?}", a),
        };
        let current_scope = out_unit.get_mut(current_scope_id);
        current_scope.set(attr.name(), attr_value);
    }
    Ok(())
}

pub(crate) fn clone_attr_string<R>(
    attr_value: &AttributeValue<R>,
    form: gimli::DwForm,
    unit: &Unit<R, R::Offset>,
    debug_str: &DebugStr<R>,
    debug_str_offsets: &DebugStrOffsets<R>,
    debug_line_str: &DebugLineStr<R>,
    out_strings: &mut write::StringTable,
) -> Result<write::LineString, Error>
where
    R: Reader,
{
    let content = match attr_value {
        AttributeValue::DebugStrRef(str_offset) => {
            debug_str.get_str(*str_offset)?.to_slice()?.to_vec()
        }
        AttributeValue::DebugStrOffsetsIndex(i) => {
            let str_offset = debug_str_offsets.get_str_offset(
                gimli::Format::Dwarf32,
                unit.str_offsets_base,
                *i,
            )?;
            debug_str.get_str(str_offset)?.to_slice()?.to_vec()
        }
        AttributeValue::DebugLineStrRef(str_offset) => {
            debug_line_str.get_str(*str_offset)?.to_slice()?.to_vec()
        }
        AttributeValue::String(b) => b.to_slice()?.to_vec(),
        v => bail!("Unexpected attribute value: {:?}", v),
    };
    Ok(match form {
        gimli::DW_FORM_strp => {
            let id = out_strings.add(content);
            write::LineString::StringRef(id)
        }
        gimli::DW_FORM_string => write::LineString::String(content),
        _ => bail!("DW_FORM_line_strp or other not supported"),
    })
}
