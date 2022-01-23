use std::convert::TryInto;
use std::fmt;
use std::io;

use ordered_float::OrderedFloat;
use rayon::prelude::*;
use wasmparser::{
    ImportSectionEntryType, NameSectionReader, Naming, Parser, Payload, SectionReader, TypeDef,
};

use crate::highlevel::{
    Code, Data, Element, Function, Global, GlobalOp, ImportOrPresent, Instr, LoadOp, Local,
    LocalOp, Memory, Module, NumericOp, StoreOp, Table,
};
use crate::lowlevel::{CustomSection, NameSection, Offsets, Section, SectionOffset, WithSize};
use crate::{
    BlockType, ElemType, FunctionType, GlobalType, Idx, Label, Limits, Memarg, MemoryType,
    Mutability, RawCustomSection, TableType, Val, ValType,
};

pub fn parse_module_with_offsets<R: io::Read>(
    mut reader: R,
    // TODO once all "benign"/correct cases work, implement proper typed error.
) -> Result<(Module, Offsets), Box<dyn std::error::Error>> {
    // This reads the whole file into a vector first, so it's not streaming in the sense of
    // "can start analysis before all bytes are received", but it is stream in the sense of
    // "not necessary to parse a section fully before going to the next section" (although
    // this is purely because of wasmparser's event-driven design.)
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf)?;

    // The final module to return.
    let mut module = Module::default();

    // State during module parsing.
    let mut types = Types::none();
    let mut imported_function_count = 0;
    let mut current_code_idx = 0;
    let mut section_offsets = Vec::with_capacity(16);
    let mut function_offsets = Vec::new();
    // Put the function bodies in their own vector, such that parallel processing of the
    // code section doesn't require synchronization on the shared `module` variable.
    let mut function_bodies = Vec::new();
    let mut code_entries_count = 0;

    let offset = 0;
    for payload in Parser::new(offset).parse_all(&buf) {
        match payload? {
            Payload::Version { .. } => {
                // The version number is checked by wasmparser to always be 1.
            }
            Payload::TypeSection(mut reader) => {
                // TODO Index the section offsets not by the section's discriminant, but by
                // a new enum `SectionId`, which is just the section name for "normal" sections,
                // and CustomSection(name: String) for custom sections (whose name should be unique).
                let discriminant = std::mem::discriminant(&Section::Type(Default::default()));
                // This is the offset AFTER the section tag and size in bytes,
                // but BEFORE the number of elements in the section.
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                types.set_capacity(count)?;
                for _ in 0..count {
                    let ty = reader.read()?;
                    match ty {
                        TypeDef::Func(ty) => types.add(ty)?,
                        TypeDef::Instance(_) | TypeDef::Module(_) => {
                            Err(UnsupportedError(WasmExtension::ModuleLinking))?
                        }
                    }
                }
            }
            Payload::ImportSection(mut reader) => {
                let discriminant = std::mem::discriminant(&Section::Import(Default::default()));
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                for _ in 0..count {
                    let import = reader.read()?;

                    let import_module = import.module.to_string();
                    let import_name = import
                        .field
                        .ok_or(UnsupportedError(WasmExtension::ModuleLinking))?
                        .to_string();

                    match import.ty {
                        ImportSectionEntryType::Function(ty_i) => {
                            imported_function_count += 1;
                            module.functions.push(Function::new_imported(
                                types.get(ty_i)?,
                                import_module,
                                import_name,
                            ))
                        }
                        ImportSectionEntryType::Global(ty) => module.globals.push(
                            Global::new_imported(convert_global_ty(ty)?, import_module, import_name),
                        ),
                        ImportSectionEntryType::Table(ty) => module.tables.push(
                            Table::new_imported(convert_table_ty(ty)?, import_module, import_name),
                        ),
                        ImportSectionEntryType::Memory(ty) => {
                            module.memories.push(Memory::new_imported(
                                convert_memory_ty(ty)?,
                                import_module,
                                import_name,
                            ))
                        }
                        ImportSectionEntryType::Tag(_) => {
                            Err(UnsupportedError(WasmExtension::ExceptionHandling))?
                        }
                        ImportSectionEntryType::Module(_) | ImportSectionEntryType::Instance(_) => {
                            Err(UnsupportedError(WasmExtension::ModuleLinking))?
                        }
                    }
                }
            }
            Payload::AliasSection(_) => Err(UnsupportedError(WasmExtension::ModuleLinking))?,
            Payload::InstanceSection(_) => Err(UnsupportedError(WasmExtension::ModuleLinking))?,
            Payload::FunctionSection(mut reader) => {
                let discriminant = std::mem::discriminant(&Section::Function(Default::default()));
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                module.functions.reserve(u32_to_usize(count));
                for _ in 0..count {
                    let ty_i = reader.read()?;
                    let type_ = types.get(ty_i)?;
                    // Fill in the code of the function later with the code section.
                    module.functions.push(Function::new(type_, Code::new()));
                }
            }
            Payload::TableSection(mut reader) => {
                let discriminant = std::mem::discriminant(&Section::Table(Default::default()));
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                module.tables.reserve(u32_to_usize(count));
                for _ in 0..count {
                    let type_ = reader.read()?;
                    let type_ = convert_table_ty(type_)?;
                    // Fill in the elements of the table later with the elem section.
                    module.tables.push(Table::new(type_));
                }
            }
            Payload::MemorySection(mut reader) => {
                let discriminant = std::mem::discriminant(&Section::Memory(Default::default()));
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                module.memories.reserve(u32_to_usize(count));
                for _ in 0..count {
                    let type_ = reader.read()?;
                    let type_ = convert_memory_ty(type_)?;
                    // Fill in the data of the memory later with the data section.
                    module.memories.push(Memory::new(type_));
                }
            }
            Payload::TagSection(_) => Err(UnsupportedError(WasmExtension::ExceptionHandling))?,
            Payload::GlobalSection(mut reader) => {
                let discriminant = std::mem::discriminant(&Section::Global(Default::default()));
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                module.globals.reserve(u32_to_usize(count));
                for _ in 0..count {
                    let global = reader.read()?;
                    let type_ = convert_global_ty(global.ty)?;

                    // Most initialization expressions have just a constant and the end instruction.
                    let mut init = Vec::with_capacity(2);
                    for op in global.init_expr.get_operators_reader() {
                        init.push(convert_instr(op?, &types)?)
                    }

                    module.globals.push(Global::new(type_, init))
                }
            }
            Payload::ExportSection(mut reader) => {
                let discriminant = std::mem::discriminant(&Section::Export(Default::default()));
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                for _ in 0..count {
                    let export = reader.read()?;
                    let name = export.field.to_string();
                    let idx = u32_to_usize(export.index);
                    use wasmparser::ExternalKind;
                    match export.kind {
                        ExternalKind::Function => module
                            .functions
                            .get_mut(idx)
                            .ok_or(IndexError::<Function>(idx.into()))?
                            .export
                            .push(name),
                        ExternalKind::Table => module
                            .tables
                            .get_mut(idx)
                            .ok_or(IndexError::<Table>(idx.into()))?
                            .export
                            .push(name),
                        ExternalKind::Memory => module
                            .memories
                            .get_mut(idx)
                            .ok_or(IndexError::<Memory>(idx.into()))?
                            .export
                            .push(name),
                        ExternalKind::Global => module
                            .globals
                            .get_mut(idx)
                            .ok_or(IndexError::<Global>(idx.into()))?
                            .export
                            .push(name),
                        ExternalKind::Tag => {
                            Err(UnsupportedError(WasmExtension::ExceptionHandling))?
                        }
                        ExternalKind::Type => Err(UnsupportedError(WasmExtension::TypeImports))?,
                        ExternalKind::Module | ExternalKind::Instance => {
                            Err(UnsupportedError(WasmExtension::ModuleLinking))?
                        }
                    }
                }
            }
            Payload::StartSection { func, range } => {
                let discriminant =
                    std::mem::discriminant(&Section::Start(WithSize(SectionOffset(0u32.into()))));
                section_offsets.push((discriminant, range.start));

                module.start = Some(func.into())
            }
            Payload::ElementSection(mut reader) => {
                let discriminant = std::mem::discriminant(&Section::Element(Default::default()));
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                for _ in 0..count {
                    let element = reader.read()?;
                    let elem_type = convert_elem_ty(element.ty)?;

                    let items_reader = element.items.get_items_reader()?;
                    let mut items = Vec::with_capacity(u32_to_usize(items_reader.get_count()));
                    for item in items_reader {
                        let item = item?;
                        use wasmparser::ElementItem;
                        items.push(match item {
                            ElementItem::Func(idx) => idx.into(),
                            ElementItem::Expr(_) => Err(UnsupportedError(WasmExtension::ReferenceTypes))?,
                        });
                    }

                    use wasmparser::ElementKind;
                    match element.kind {
                        ElementKind::Active {
                            table_index,
                            init_expr,
                        } => {
                            let table = module
                                .tables
                                .get_mut(u32_to_usize(table_index))
                                .ok_or_else(|| IndexError::<Table>(table_index.into()))?;

                            // TODO I am not sure this is correct.
                            if table.type_.0 != elem_type {
                                Err("type error: table and element not fitting together")?
                            }

                            // Most offset expressions are just a constant and the end instruction.
                            let mut offset = Vec::with_capacity(2);
                            for op in init_expr.get_operators_reader() {
                                offset.push(convert_instr(op?, &types)?)
                            }

                            table.elements.push(Element {
                                offset,
                                functions: items,
                            })
                        }
                        ElementKind::Passive => {
                            Err(UnsupportedError(WasmExtension::BulkMemoryOperations))?
                        }
                        ElementKind::Declared => {
                            Err(UnsupportedError(WasmExtension::ReferenceTypes))?
                        }
                    }
                }
            }
            Payload::DataCountSection { count: _, range: _ } => {
                Err(UnsupportedError(WasmExtension::BulkMemoryOperations))?
            }
            Payload::DataSection(mut reader) => {
                let discriminant = std::mem::discriminant(&Section::Data(Default::default()));
                section_offsets.push((discriminant, reader.range().start));

                let count = reader.get_count();
                for _ in 0..count {
                    let data = reader.read()?;

                    use wasmparser::DataKind;
                    match data.kind {
                        DataKind::Active {
                            memory_index,
                            init_expr,
                        } => {
                            let memory = module
                                .memories
                                .get_mut(u32_to_usize(memory_index))
                                .ok_or(IndexError::<Memory>(memory_index.into()))?;

                            // Most offset expressions are just a constant and the end instruction.
                            let mut offset = Vec::with_capacity(2);
                            for op in init_expr.get_operators_reader() {
                                offset.push(convert_instr(op?, &types)?)
                            }

                            memory.data.push(Data {
                                offset,
                                bytes: data.data.to_vec(),
                            })
                        }
                        DataKind::Passive => {
                            Err(UnsupportedError(WasmExtension::BulkMemoryOperations))?
                        }
                    }
                }
            }
            Payload::CustomSection {
                name: "name",
                data_offset,
                data,
                range,
            } => {
                let discriminant =
                    std::mem::discriminant(&Section::Custom(CustomSection::Name(NameSection {
                        subsections: Vec::new(),
                    })));
                section_offsets.push((discriminant, range.start));

                // TODO if name section cannot be parsed, do not error but warn and save as bytes

                let reader = NameSectionReader::new(data, data_offset)?;
                for name_subsection in reader {
                    let name_subsection = name_subsection?;
                    use wasmparser::Name;
                    match name_subsection {
                        Name::Module(name) => {
                            let prev = module.name.replace(name.get_name()?.to_string());
                            if let Some(_) = prev {
                                Err("duplicate module name")?
                            }
                        }
                        Name::Function(name_map) => {
                            let mut name_map = name_map.get_map()?;
                            for _ in 0..name_map.get_count() {
                                let Naming { index, name } = name_map.read()?;
                                module
                                    .functions
                                    .get_mut(u32_to_usize(index))
                                    .ok_or(IndexError::<Function>(index.into()))?
                                    .name = Some(name.to_string());
                            }
                        }
                        Name::Local(indirect_name_map) => {
                            let mut indirect_name_map = indirect_name_map.get_indirect_map()?;
                            for _ in 0..indirect_name_map.get_indirect_count() {
                                let indirect_naming = indirect_name_map.read()?;

                                let function_idx = indirect_naming.indirect_index;
                                let function = module
                                    .functions
                                    .get_mut(u32_to_usize(function_idx))
                                    .ok_or(IndexError::<Function>(function_idx.into()))?;

                                let mut name_map = indirect_naming.get_map()?;
                                for _ in 0..name_map.get_count() {
                                    let Naming {
                                        index: local_idx,
                                        name,
                                    } = name_map.read()?;
                                    *function.param_or_local_name_mut(local_idx.into()) =
                                        Some(name.to_string());
                                }
                            }
                        }
                        // TODO
                        Name::Label(_)
                        | Name::Type(_)
                        | Name::Table(_)
                        | Name::Memory(_)
                        | Name::Global(_)
                        | Name::Element(_)
                        | Name::Data(_)
                        | Name::Unknown {
                            ty: _,
                            data: _,
                            range: _,
                        } => println!("todo: name section parsing/conversion"),
                    }
                }
            }
            Payload::CustomSection {
                name,
                data_offset: _,
                data,
                range,
            } => {
                let raw_custom_section = RawCustomSection {
                    name: name.to_string(),
                    content: data.to_vec(),
                    after: section_offsets
                        .last()
                        .map(|(section, _offset)| section)
                        .cloned(),
                };

                let discriminant = std::mem::discriminant(&Section::Custom(CustomSection::Raw(
                    RawCustomSection {
                        name: "".into(),
                        content: Vec::new(),
                        after: None,
                    },
                )));
                section_offsets.push((discriminant, range.start));

                module.custom_sections.push(raw_custom_section);
            }
            Payload::CodeSectionStart {
                count,
                range,
                size: _,
            } => {
                let discriminant = std::mem::discriminant(&Section::Code(Default::default()));
                section_offsets.push((discriminant, range.start));

                function_offsets = Vec::with_capacity(u32_to_usize(count));

                code_entries_count = count;
                function_bodies = Vec::with_capacity(u32_to_usize(count));
            }
            Payload::CodeSectionEntry(body) => {
                let func_idx = imported_function_count + current_code_idx;

                function_offsets.push((func_idx.into(), body.range().start));

                function_bodies.push((func_idx, body));

                current_code_idx += 1;

                let last_code_entry = current_code_idx == code_entries_count;
                if last_code_entry {
                    // Parse and convert to high-levl instructions in parallel.
                    let function_bodies: Vec<_> = function_bodies
                        .par_drain(..)
                        .map(|(i, body)| {
                            // FIXME ugly hack to get error Send + Sync.
                            (i, parse_body(body, &types).map_err(|e| e.to_string()))
                        })
                        .collect();
                    for (func_idx, code) in function_bodies {
                        let function = module
                            .functions
                            .get_mut(u32_to_usize(func_idx))
                            .ok_or(IndexError::<Function>(func_idx.into()))?;
                        function.code = ImportOrPresent::Present(code?);
                    }
                }
            }
            Payload::ModuleSectionStart {
                count: _,
                range: _,
                size: _,
            } => Err(UnsupportedError(WasmExtension::ModuleLinking))?,
            Payload::ModuleSectionEntry {
                parser: _,
                range: _,
            } => Err(UnsupportedError(WasmExtension::ModuleLinking))?,
            Payload::UnknownSection {
                id: _,
                contents: _,
                range: _,
            } => Err("unknown section")?,
            Payload::End => {
                // I don't understand what this end marker is for?
                // If the module ended (i.e., the input buffer is exhausted),
                // there is just no more payload following, isn't there?
            }
        }
    }

    let offsets = Offsets {
        sections: section_offsets,
        functions_code: function_offsets,
    };

    Ok((module, offsets))
}

fn parse_body(
    body: wasmparser::FunctionBody,
    types: &Types,
) -> Result<Code, Box<dyn std::error::Error>> {
    let mut locals = Vec::new();
    for local in body.get_locals_reader()? {
        let (count, type_) = local?;
        for _ in 0..count {
            locals.push(Local::new(convert_ty(type_)?));
        }
    }

    // There is roughly one instruction per byte, so reserve space for
    // approximately this many instructions.
    let body_byte_size = body.range().end - body.range().start;
    let mut instrs = Vec::with_capacity(body_byte_size);

    for op in body.get_operators_reader()? {
        instrs.push(convert_instr(op?, &types)?);
    }

    Ok(Code {
        locals,
        body: instrs,
    })
}

#[allow(unused)]
fn convert_instr(
    op: wasmparser::Operator,
    types: &Types,
) -> Result<Instr, Box<dyn std::error::Error>> {
    use crate::highlevel::Instr::*;
    use wasmparser::Operator as wp;
    Ok(match op {
        wp::Unreachable => Unreachable,
        wp::Nop => Nop,

        wp::Block { ty } => Block(convert_block_ty(ty)?),
        wp::Loop { ty } => Loop(convert_block_ty(ty)?),
        wp::If { ty } => If(convert_block_ty(ty)?),
        wp::Else => Else,
        wp::End => End,

        wp::Try { ty: _ }
        | wp::Catch { index: _ }
        | wp::CatchAll
        | wp::Throw { index: _ }
        | wp::Rethrow { relative_depth: _ }
        | wp::Delegate { relative_depth: _ } => {
            Err(UnsupportedError(WasmExtension::ExceptionHandling))?
        }

        wp::Br { relative_depth } => Br(Label(relative_depth)),
        wp::BrIf { relative_depth } => BrIf(Label(relative_depth)),
        wp::BrTable { table } => {
            let default = Label(table.default());
            let mut targets = Vec::with_capacity(u32_to_usize(table.len()));
            for target in table.targets() {
                targets.push(Label(target?))
            }
            BrTable {
                table: targets,
                default,
            }
        }

        wp::Return => Return,
        wp::Call { function_index } => Call(function_index.into()),
        wp::CallIndirect { index, table_index } => {
            CallIndirect(types.get(index)?, table_index.into())
        }

        wp::ReturnCall { function_index: _ }
        | wp::ReturnCallIndirect {
            index: _,
            table_index: _,
        } => Err(UnsupportedError(WasmExtension::TailCalls))?,

        wp::Drop => Drop,
        wp::Select => Select,

        wp::TypedSelect { ty } => Err(UnsupportedError(WasmExtension::ReferenceTypes))?,

        wp::LocalGet { local_index } => Local(LocalOp::Get, local_index.into()),
        wp::LocalSet { local_index } => Local(LocalOp::Set, local_index.into()),
        wp::LocalTee { local_index } => Local(LocalOp::Tee, local_index.into()),
        wp::GlobalGet { global_index } => Global(GlobalOp::Get, global_index.into()),
        wp::GlobalSet { global_index } => Global(GlobalOp::Set, global_index.into()),

        wp::I32Load { memarg } => Load(LoadOp::I32Load, convert_memarg(memarg)?),
        wp::I64Load { memarg } => Load(LoadOp::I64Load, convert_memarg(memarg)?),
        wp::F32Load { memarg } => Load(LoadOp::F32Load, convert_memarg(memarg)?),
        wp::F64Load { memarg } => Load(LoadOp::F64Load, convert_memarg(memarg)?),
        wp::I32Load8S { memarg } => Load(LoadOp::I32Load8S, convert_memarg(memarg)?),
        wp::I32Load8U { memarg } => Load(LoadOp::I32Load8U, convert_memarg(memarg)?),
        wp::I32Load16S { memarg } => Load(LoadOp::I32Load16S, convert_memarg(memarg)?),
        wp::I32Load16U { memarg } => Load(LoadOp::I32Load16U, convert_memarg(memarg)?),
        wp::I64Load8S { memarg } => Load(LoadOp::I64Load8S, convert_memarg(memarg)?),
        wp::I64Load8U { memarg } => Load(LoadOp::I64Load8U, convert_memarg(memarg)?),
        wp::I64Load16S { memarg } => Load(LoadOp::I64Load16S, convert_memarg(memarg)?),
        wp::I64Load16U { memarg } => Load(LoadOp::I64Load16U, convert_memarg(memarg)?),
        wp::I64Load32S { memarg } => Load(LoadOp::I64Load32S, convert_memarg(memarg)?),
        wp::I64Load32U { memarg } => Load(LoadOp::I64Load32U, convert_memarg(memarg)?),

        wp::I32Store { memarg } => Store(StoreOp::I32Store, convert_memarg(memarg)?),
        wp::I64Store { memarg } => Store(StoreOp::I64Store, convert_memarg(memarg)?),
        wp::F32Store { memarg } => Store(StoreOp::F32Store, convert_memarg(memarg)?),
        wp::F64Store { memarg } => Store(StoreOp::F64Store, convert_memarg(memarg)?),
        wp::I32Store8 { memarg } => Store(StoreOp::I32Store8, convert_memarg(memarg)?),
        wp::I32Store16 { memarg } => Store(StoreOp::I32Store16, convert_memarg(memarg)?),
        wp::I64Store8 { memarg } => Store(StoreOp::I64Store8, convert_memarg(memarg)?),
        wp::I64Store16 { memarg } => Store(StoreOp::I64Store16, convert_memarg(memarg)?),
        wp::I64Store32 { memarg } => Store(StoreOp::I64Store32, convert_memarg(memarg)?),

        // This is not well documented in wasmparser: `mem_byte` and `mem` essentially contain
        // the same information, it's just that mem_byte is the original (single) byte that was
        // read from the instruction stream, and mem is it if parsed as a LEB128.
        // I think the variable-length parser is more robust, as it can handle memory indices
        // above 255, so ignore `mem_byte` here.
        wp::MemorySize { mem, mem_byte: _ } => {
            if mem != 0 {
                Err(UnsupportedError(WasmExtension::MultiMemory))?
            }
            MemorySize(0u32.into())
        }
        wp::MemoryGrow { mem, mem_byte: _ } => {
            if mem != 0 {
                Err(UnsupportedError(WasmExtension::MultiMemory))?
            }
            MemoryGrow(0u32.into())
        }

        wp::I32Const { value } => Const(Val::I32(value)),
        wp::I64Const { value } => Const(Val::I64(value)),
        wp::F32Const { value } => Const(Val::F32(OrderedFloat(f32::from_bits(value.bits())))),
        wp::F64Const { value } => Const(Val::F64(OrderedFloat(f64::from_bits(value.bits())))),

        wp::RefNull { ty: _ } | wp::RefIsNull | wp::RefFunc { function_index: _ } => {
            Err(UnsupportedError(WasmExtension::ReferenceTypes))?
        }

        wp::I32Eqz => Numeric(NumericOp::I32Eqz),
        wp::I32Eq => Numeric(NumericOp::I32Eq),
        wp::I32Ne => Numeric(NumericOp::I32Ne),
        wp::I32LtS => Numeric(NumericOp::I32LtS),
        wp::I32LtU => Numeric(NumericOp::I32LtU),
        wp::I32GtS => Numeric(NumericOp::I32GtS),
        wp::I32GtU => Numeric(NumericOp::I32GtU),
        wp::I32LeS => Numeric(NumericOp::I32LeS),
        wp::I32LeU => Numeric(NumericOp::I32LeU),
        wp::I32GeS => Numeric(NumericOp::I32GeS),
        wp::I32GeU => Numeric(NumericOp::I32GeU),
        wp::I64Eqz => Numeric(NumericOp::I64Eqz),
        wp::I64Eq => Numeric(NumericOp::I64Eq),
        wp::I64Ne => Numeric(NumericOp::I64Ne),
        wp::I64LtS => Numeric(NumericOp::I64LtS),
        wp::I64LtU => Numeric(NumericOp::I64LtU),
        wp::I64GtS => Numeric(NumericOp::I64GtS),
        wp::I64GtU => Numeric(NumericOp::I64GtU),
        wp::I64LeS => Numeric(NumericOp::I64LeS),
        wp::I64LeU => Numeric(NumericOp::I64LeU),
        wp::I64GeS => Numeric(NumericOp::I64GeS),
        wp::I64GeU => Numeric(NumericOp::I64GeU),
        wp::F32Eq => Numeric(NumericOp::F32Eq),
        wp::F32Ne => Numeric(NumericOp::F32Ne),
        wp::F32Lt => Numeric(NumericOp::F32Lt),
        wp::F32Gt => Numeric(NumericOp::F32Gt),
        wp::F32Le => Numeric(NumericOp::F32Le),
        wp::F32Ge => Numeric(NumericOp::F32Ge),
        wp::F64Eq => Numeric(NumericOp::F64Eq),
        wp::F64Ne => Numeric(NumericOp::F64Ne),
        wp::F64Lt => Numeric(NumericOp::F64Lt),
        wp::F64Gt => Numeric(NumericOp::F64Gt),
        wp::F64Le => Numeric(NumericOp::F64Le),
        wp::F64Ge => Numeric(NumericOp::F64Ge),
        wp::I32Clz => Numeric(NumericOp::I32Clz),
        wp::I32Ctz => Numeric(NumericOp::I32Ctz),
        wp::I32Popcnt => Numeric(NumericOp::I32Popcnt),
        wp::I32Add => Numeric(NumericOp::I32Add),
        wp::I32Sub => Numeric(NumericOp::I32Sub),
        wp::I32Mul => Numeric(NumericOp::I32Mul),
        wp::I32DivS => Numeric(NumericOp::I32DivS),
        wp::I32DivU => Numeric(NumericOp::I32DivU),
        wp::I32RemS => Numeric(NumericOp::I32RemS),
        wp::I32RemU => Numeric(NumericOp::I32RemU),
        wp::I32And => Numeric(NumericOp::I32And),
        wp::I32Or => Numeric(NumericOp::I32Or),
        wp::I32Xor => Numeric(NumericOp::I32Xor),
        wp::I32Shl => Numeric(NumericOp::I32Shl),
        wp::I32ShrS => Numeric(NumericOp::I32ShrS),
        wp::I32ShrU => Numeric(NumericOp::I32ShrU),
        wp::I32Rotl => Numeric(NumericOp::I32Rotl),
        wp::I32Rotr => Numeric(NumericOp::I32Rotr),
        wp::I64Clz => Numeric(NumericOp::I64Clz),
        wp::I64Ctz => Numeric(NumericOp::I64Ctz),
        wp::I64Popcnt => Numeric(NumericOp::I64Popcnt),
        wp::I64Add => Numeric(NumericOp::I64Add),
        wp::I64Sub => Numeric(NumericOp::I64Sub),
        wp::I64Mul => Numeric(NumericOp::I64Mul),
        wp::I64DivS => Numeric(NumericOp::I64DivS),
        wp::I64DivU => Numeric(NumericOp::I64DivU),
        wp::I64RemS => Numeric(NumericOp::I64RemS),
        wp::I64RemU => Numeric(NumericOp::I64RemU),
        wp::I64And => Numeric(NumericOp::I64And),
        wp::I64Or => Numeric(NumericOp::I64Or),
        wp::I64Xor => Numeric(NumericOp::I64Xor),
        wp::I64Shl => Numeric(NumericOp::I64Shl),
        wp::I64ShrS => Numeric(NumericOp::I64ShrS),
        wp::I64ShrU => Numeric(NumericOp::I64ShrU),
        wp::I64Rotl => Numeric(NumericOp::I64Rotl),
        wp::I64Rotr => Numeric(NumericOp::I64Rotr),
        wp::F32Abs => Numeric(NumericOp::F32Abs),
        wp::F32Neg => Numeric(NumericOp::F32Neg),
        wp::F32Ceil => Numeric(NumericOp::F32Ceil),
        wp::F32Floor => Numeric(NumericOp::F32Floor),
        wp::F32Trunc => Numeric(NumericOp::F32Trunc),
        wp::F32Nearest => Numeric(NumericOp::F32Nearest),
        wp::F32Sqrt => Numeric(NumericOp::F32Sqrt),
        wp::F32Add => Numeric(NumericOp::F32Add),
        wp::F32Sub => Numeric(NumericOp::F32Sub),
        wp::F32Mul => Numeric(NumericOp::F32Mul),
        wp::F32Div => Numeric(NumericOp::F32Div),
        wp::F32Min => Numeric(NumericOp::F32Min),
        wp::F32Max => Numeric(NumericOp::F32Max),
        wp::F32Copysign => Numeric(NumericOp::F32Copysign),
        wp::F64Abs => Numeric(NumericOp::F64Abs),
        wp::F64Neg => Numeric(NumericOp::F64Neg),
        wp::F64Ceil => Numeric(NumericOp::F64Ceil),
        wp::F64Floor => Numeric(NumericOp::F64Floor),
        wp::F64Trunc => Numeric(NumericOp::F64Trunc),
        wp::F64Nearest => Numeric(NumericOp::F64Nearest),
        wp::F64Sqrt => Numeric(NumericOp::F64Sqrt),
        wp::F64Add => Numeric(NumericOp::F64Add),
        wp::F64Sub => Numeric(NumericOp::F64Sub),
        wp::F64Mul => Numeric(NumericOp::F64Mul),
        wp::F64Div => Numeric(NumericOp::F64Div),
        wp::F64Min => Numeric(NumericOp::F64Min),
        wp::F64Max => Numeric(NumericOp::F64Max),
        wp::F64Copysign => Numeric(NumericOp::F64Copysign),
        wp::I32WrapI64 => Numeric(NumericOp::I32WrapI64),
        wp::I32TruncF32S => Numeric(NumericOp::I32TruncF32S),
        wp::I32TruncF32U => Numeric(NumericOp::I32TruncF32U),
        wp::I32TruncF64S => Numeric(NumericOp::I32TruncF64S),
        wp::I32TruncF64U => Numeric(NumericOp::I32TruncF64U),
        wp::I64ExtendI32S => Numeric(NumericOp::I64ExtendI32S),
        wp::I64ExtendI32U => Numeric(NumericOp::I64ExtendI32U),
        wp::I64TruncF32S => Numeric(NumericOp::I64TruncF32S),
        wp::I64TruncF32U => Numeric(NumericOp::I64TruncF32U),
        wp::I64TruncF64S => Numeric(NumericOp::I64TruncF64S),
        wp::I64TruncF64U => Numeric(NumericOp::I64TruncF64U),
        wp::F32ConvertI32S => Numeric(NumericOp::F32ConvertI32S),
        wp::F32ConvertI32U => Numeric(NumericOp::F32ConvertI32U),
        wp::F32ConvertI64S => Numeric(NumericOp::F32ConvertI64S),
        wp::F32ConvertI64U => Numeric(NumericOp::F32ConvertI64U),
        wp::F32DemoteF64 => Numeric(NumericOp::F32DemoteF64),
        wp::F64ConvertI32S => Numeric(NumericOp::F64ConvertI32S),
        wp::F64ConvertI32U => Numeric(NumericOp::F64ConvertI32U),
        wp::F64ConvertI64S => Numeric(NumericOp::F64ConvertI64S),
        wp::F64ConvertI64U => Numeric(NumericOp::F64ConvertI64U),
        wp::F64PromoteF32 => Numeric(NumericOp::F64PromoteF32),
        wp::I32ReinterpretF32 => Numeric(NumericOp::I32ReinterpretF32),
        wp::I64ReinterpretF64 => Numeric(NumericOp::I64ReinterpretF64),
        wp::F32ReinterpretI32 => Numeric(NumericOp::F32ReinterpretI32),
        wp::F64ReinterpretI64 => Numeric(NumericOp::F64ReinterpretI64),

        wp::I32Extend8S
        | wp::I32Extend16S
        | wp::I64Extend8S
        | wp::I64Extend16S
        | wp::I64Extend32S => Err(UnsupportedError(WasmExtension::SignExtensionOps))?,

        wp::I32TruncSatF32S
        | wp::I32TruncSatF32U
        | wp::I32TruncSatF64S
        | wp::I32TruncSatF64U
        | wp::I64TruncSatF32S
        | wp::I64TruncSatF32U
        | wp::I64TruncSatF64S
        | wp::I64TruncSatF64U => Err(UnsupportedError(WasmExtension::NontrappingFloatToInt))?,

        wp::MemoryInit { segment: _, mem: _ }
        | wp::DataDrop { segment: _ }
        | wp::MemoryCopy { src: _, dst: _ }
        | wp::MemoryFill { mem: _ }
        | wp::TableInit {
            segment: _,
            table: _,
        }
        | wp::ElemDrop { segment: _ }
        | wp::TableCopy {
            dst_table: _,
            src_table: _,
        } => Err(UnsupportedError(WasmExtension::BulkMemoryOperations))?,

        wp::TableFill { table: _ } => Err(UnsupportedError(WasmExtension::ReferenceTypes))?,

        wp::TableGet { table: _ }
        | wp::TableSet { table: _ }
        | wp::TableGrow { table: _ }
        | wp::TableSize { table: _ } => Err(UnsupportedError(WasmExtension::ReferenceTypes))?,

        wp::MemoryAtomicNotify { memarg: _ }
        | wp::MemoryAtomicWait32 { memarg: _ }
        | wp::MemoryAtomicWait64 { memarg: _ }
        | wp::AtomicFence { flags: _ }
        | wp::I32AtomicLoad { memarg: _ }
        | wp::I64AtomicLoad { memarg: _ }
        | wp::I32AtomicLoad8U { memarg: _ }
        | wp::I32AtomicLoad16U { memarg: _ }
        | wp::I64AtomicLoad8U { memarg: _ }
        | wp::I64AtomicLoad16U { memarg: _ }
        | wp::I64AtomicLoad32U { memarg: _ }
        | wp::I32AtomicStore { memarg: _ }
        | wp::I64AtomicStore { memarg: _ }
        | wp::I32AtomicStore8 { memarg: _ }
        | wp::I32AtomicStore16 { memarg: _ }
        | wp::I64AtomicStore8 { memarg: _ }
        | wp::I64AtomicStore16 { memarg: _ }
        | wp::I64AtomicStore32 { memarg: _ }
        | wp::I32AtomicRmwAdd { memarg: _ }
        | wp::I64AtomicRmwAdd { memarg: _ }
        | wp::I32AtomicRmw8AddU { memarg: _ }
        | wp::I32AtomicRmw16AddU { memarg: _ }
        | wp::I64AtomicRmw8AddU { memarg: _ }
        | wp::I64AtomicRmw16AddU { memarg: _ }
        | wp::I64AtomicRmw32AddU { memarg: _ }
        | wp::I32AtomicRmwSub { memarg: _ }
        | wp::I64AtomicRmwSub { memarg: _ }
        | wp::I32AtomicRmw8SubU { memarg: _ }
        | wp::I32AtomicRmw16SubU { memarg: _ }
        | wp::I64AtomicRmw8SubU { memarg: _ }
        | wp::I64AtomicRmw16SubU { memarg: _ }
        | wp::I64AtomicRmw32SubU { memarg: _ }
        | wp::I32AtomicRmwAnd { memarg: _ }
        | wp::I64AtomicRmwAnd { memarg: _ }
        | wp::I32AtomicRmw8AndU { memarg: _ }
        | wp::I32AtomicRmw16AndU { memarg: _ }
        | wp::I64AtomicRmw8AndU { memarg: _ }
        | wp::I64AtomicRmw16AndU { memarg: _ }
        | wp::I64AtomicRmw32AndU { memarg: _ }
        | wp::I32AtomicRmwOr { memarg: _ }
        | wp::I64AtomicRmwOr { memarg: _ }
        | wp::I32AtomicRmw8OrU { memarg: _ }
        | wp::I32AtomicRmw16OrU { memarg: _ }
        | wp::I64AtomicRmw8OrU { memarg: _ }
        | wp::I64AtomicRmw16OrU { memarg: _ }
        | wp::I64AtomicRmw32OrU { memarg: _ }
        | wp::I32AtomicRmwXor { memarg: _ }
        | wp::I64AtomicRmwXor { memarg: _ }
        | wp::I32AtomicRmw8XorU { memarg: _ }
        | wp::I32AtomicRmw16XorU { memarg: _ }
        | wp::I64AtomicRmw8XorU { memarg: _ }
        | wp::I64AtomicRmw16XorU { memarg: _ }
        | wp::I64AtomicRmw32XorU { memarg: _ }
        | wp::I32AtomicRmwXchg { memarg: _ }
        | wp::I64AtomicRmwXchg { memarg: _ }
        | wp::I32AtomicRmw8XchgU { memarg: _ }
        | wp::I32AtomicRmw16XchgU { memarg: _ }
        | wp::I64AtomicRmw8XchgU { memarg: _ }
        | wp::I64AtomicRmw16XchgU { memarg: _ }
        | wp::I64AtomicRmw32XchgU { memarg: _ }
        | wp::I32AtomicRmwCmpxchg { memarg: _ }
        | wp::I64AtomicRmwCmpxchg { memarg: _ }
        | wp::I32AtomicRmw8CmpxchgU { memarg: _ }
        | wp::I32AtomicRmw16CmpxchgU { memarg: _ }
        | wp::I64AtomicRmw8CmpxchgU { memarg: _ }
        | wp::I64AtomicRmw16CmpxchgU { memarg: _ }
        | wp::I64AtomicRmw32CmpxchgU { memarg: _ } => {
            Err(UnsupportedError(WasmExtension::ThreadsAtomics))?
        }

        wp::V128Load { memarg: _ }
        | wp::V128Load8x8S { memarg: _ }
        | wp::V128Load8x8U { memarg: _ }
        | wp::V128Load16x4S { memarg: _ }
        | wp::V128Load16x4U { memarg: _ }
        | wp::V128Load32x2S { memarg: _ }
        | wp::V128Load32x2U { memarg: _ }
        | wp::V128Load8Splat { memarg: _ }
        | wp::V128Load16Splat { memarg: _ }
        | wp::V128Load32Splat { memarg: _ }
        | wp::V128Load64Splat { memarg: _ }
        | wp::V128Load32Zero { memarg: _ }
        | wp::V128Load64Zero { memarg: _ }
        | wp::V128Store { memarg: _ }
        | wp::V128Load8Lane { memarg: _, lane: _ }
        | wp::V128Load16Lane { memarg: _, lane: _ }
        | wp::V128Load32Lane { memarg: _, lane: _ }
        | wp::V128Load64Lane { memarg: _, lane: _ }
        | wp::V128Store8Lane { memarg: _, lane: _ }
        | wp::V128Store16Lane { memarg: _, lane: _ }
        | wp::V128Store32Lane { memarg: _, lane: _ }
        | wp::V128Store64Lane { memarg: _, lane: _ }
        | wp::V128Const { value: _ }
        | wp::I8x16Shuffle { lanes: _ }
        | wp::I8x16ExtractLaneS { lane: _ }
        | wp::I8x16ExtractLaneU { lane: _ }
        | wp::I8x16ReplaceLane { lane: _ }
        | wp::I16x8ExtractLaneS { lane: _ }
        | wp::I16x8ExtractLaneU { lane: _ }
        | wp::I16x8ReplaceLane { lane: _ }
        | wp::I32x4ExtractLane { lane: _ }
        | wp::I32x4ReplaceLane { lane: _ }
        | wp::I64x2ExtractLane { lane: _ }
        | wp::I64x2ReplaceLane { lane: _ }
        | wp::F32x4ExtractLane { lane: _ }
        | wp::F32x4ReplaceLane { lane: _ }
        | wp::F64x2ExtractLane { lane: _ }
        | wp::F64x2ReplaceLane { lane: _ }
        | wp::I8x16Swizzle
        | wp::I8x16Splat
        | wp::I16x8Splat
        | wp::I32x4Splat
        | wp::I64x2Splat
        | wp::F32x4Splat
        | wp::F64x2Splat
        | wp::I8x16Eq
        | wp::I8x16Ne
        | wp::I8x16LtS
        | wp::I8x16LtU
        | wp::I8x16GtS
        | wp::I8x16GtU
        | wp::I8x16LeS
        | wp::I8x16LeU
        | wp::I8x16GeS
        | wp::I8x16GeU
        | wp::I16x8Eq
        | wp::I16x8Ne
        | wp::I16x8LtS
        | wp::I16x8LtU
        | wp::I16x8GtS
        | wp::I16x8GtU
        | wp::I16x8LeS
        | wp::I16x8LeU
        | wp::I16x8GeS
        | wp::I16x8GeU
        | wp::I32x4Eq
        | wp::I32x4Ne
        | wp::I32x4LtS
        | wp::I32x4LtU
        | wp::I32x4GtS
        | wp::I32x4GtU
        | wp::I32x4LeS
        | wp::I32x4LeU
        | wp::I32x4GeS
        | wp::I32x4GeU
        | wp::I64x2Eq
        | wp::I64x2Ne
        | wp::I64x2LtS
        | wp::I64x2GtS
        | wp::I64x2LeS
        | wp::I64x2GeS
        | wp::F32x4Eq
        | wp::F32x4Ne
        | wp::F32x4Lt
        | wp::F32x4Gt
        | wp::F32x4Le
        | wp::F32x4Ge
        | wp::F64x2Eq
        | wp::F64x2Ne
        | wp::F64x2Lt
        | wp::F64x2Gt
        | wp::F64x2Le
        | wp::F64x2Ge
        | wp::V128Not
        | wp::V128And
        | wp::V128AndNot
        | wp::V128Or
        | wp::V128Xor
        | wp::V128Bitselect
        | wp::V128AnyTrue
        | wp::I8x16Abs
        | wp::I8x16Neg
        | wp::I8x16Popcnt
        | wp::I8x16AllTrue
        | wp::I8x16Bitmask
        | wp::I8x16NarrowI16x8S
        | wp::I8x16NarrowI16x8U
        | wp::I8x16Shl
        | wp::I8x16ShrS
        | wp::I8x16ShrU
        | wp::I8x16Add
        | wp::I8x16AddSatS
        | wp::I8x16AddSatU
        | wp::I8x16Sub
        | wp::I8x16SubSatS
        | wp::I8x16SubSatU
        | wp::I8x16MinS
        | wp::I8x16MinU
        | wp::I8x16MaxS
        | wp::I8x16MaxU
        | wp::I8x16RoundingAverageU
        | wp::I16x8ExtAddPairwiseI8x16S
        | wp::I16x8ExtAddPairwiseI8x16U
        | wp::I16x8Abs
        | wp::I16x8Neg
        | wp::I16x8Q15MulrSatS
        | wp::I16x8AllTrue
        | wp::I16x8Bitmask
        | wp::I16x8NarrowI32x4S
        | wp::I16x8NarrowI32x4U
        | wp::I16x8ExtendLowI8x16S
        | wp::I16x8ExtendHighI8x16S
        | wp::I16x8ExtendLowI8x16U
        | wp::I16x8ExtendHighI8x16U
        | wp::I16x8Shl
        | wp::I16x8ShrS
        | wp::I16x8ShrU
        | wp::I16x8Add
        | wp::I16x8AddSatS
        | wp::I16x8AddSatU
        | wp::I16x8Sub
        | wp::I16x8SubSatS
        | wp::I16x8SubSatU
        | wp::I16x8Mul
        | wp::I16x8MinS
        | wp::I16x8MinU
        | wp::I16x8MaxS
        | wp::I16x8MaxU
        | wp::I16x8RoundingAverageU
        | wp::I16x8ExtMulLowI8x16S
        | wp::I16x8ExtMulHighI8x16S
        | wp::I16x8ExtMulLowI8x16U
        | wp::I16x8ExtMulHighI8x16U
        | wp::I32x4ExtAddPairwiseI16x8S
        | wp::I32x4ExtAddPairwiseI16x8U
        | wp::I32x4Abs
        | wp::I32x4Neg
        | wp::I32x4AllTrue
        | wp::I32x4Bitmask
        | wp::I32x4ExtendLowI16x8S
        | wp::I32x4ExtendHighI16x8S
        | wp::I32x4ExtendLowI16x8U
        | wp::I32x4ExtendHighI16x8U
        | wp::I32x4Shl
        | wp::I32x4ShrS
        | wp::I32x4ShrU
        | wp::I32x4Add
        | wp::I32x4Sub
        | wp::I32x4Mul
        | wp::I32x4MinS
        | wp::I32x4MinU
        | wp::I32x4MaxS
        | wp::I32x4MaxU
        | wp::I32x4DotI16x8S
        | wp::I32x4ExtMulLowI16x8S
        | wp::I32x4ExtMulHighI16x8S
        | wp::I32x4ExtMulLowI16x8U
        | wp::I32x4ExtMulHighI16x8U
        | wp::I64x2Abs
        | wp::I64x2Neg
        | wp::I64x2AllTrue
        | wp::I64x2Bitmask
        | wp::I64x2ExtendLowI32x4S
        | wp::I64x2ExtendHighI32x4S
        | wp::I64x2ExtendLowI32x4U
        | wp::I64x2ExtendHighI32x4U
        | wp::I64x2Shl
        | wp::I64x2ShrS
        | wp::I64x2ShrU
        | wp::I64x2Add
        | wp::I64x2Sub
        | wp::I64x2Mul
        | wp::I64x2ExtMulLowI32x4S
        | wp::I64x2ExtMulHighI32x4S
        | wp::I64x2ExtMulLowI32x4U
        | wp::I64x2ExtMulHighI32x4U
        | wp::F32x4Ceil
        | wp::F32x4Floor
        | wp::F32x4Trunc
        | wp::F32x4Nearest
        | wp::F32x4Abs
        | wp::F32x4Neg
        | wp::F32x4Sqrt
        | wp::F32x4Add
        | wp::F32x4Sub
        | wp::F32x4Mul
        | wp::F32x4Div
        | wp::F32x4Min
        | wp::F32x4Max
        | wp::F32x4PMin
        | wp::F32x4PMax
        | wp::F64x2Ceil
        | wp::F64x2Floor
        | wp::F64x2Trunc
        | wp::F64x2Nearest
        | wp::F64x2Abs
        | wp::F64x2Neg
        | wp::F64x2Sqrt
        | wp::F64x2Add
        | wp::F64x2Sub
        | wp::F64x2Mul
        | wp::F64x2Div
        | wp::F64x2Min
        | wp::F64x2Max
        | wp::F64x2PMin
        | wp::F64x2PMax
        | wp::I32x4TruncSatF32x4S
        | wp::I32x4TruncSatF32x4U
        | wp::F32x4ConvertI32x4S
        | wp::F32x4ConvertI32x4U
        | wp::I32x4TruncSatF64x2SZero
        | wp::I32x4TruncSatF64x2UZero
        | wp::F64x2ConvertLowI32x4S
        | wp::F64x2ConvertLowI32x4U
        | wp::F32x4DemoteF64x2Zero
        | wp::F64x2PromoteLowF32x4 => Err(UnsupportedError(WasmExtension::Simd))?,
    })
}

fn convert_memarg(memarg: wasmparser::MemoryImmediate) -> Result<Memarg, UnsupportedError> {
    let offset: u32 = memarg
        .offset
        .try_into()
        .map_err(|_| UnsupportedError(WasmExtension::Memory64))?;
    if memarg.memory != 0 {
        Err(UnsupportedError(WasmExtension::MultiMemory))?
    }
    Ok(Memarg {
        alignment_exp: memarg.align,
        offset,
    })
}

fn convert_memory_ty(ty: wasmparser::MemoryType) -> Result<MemoryType, UnsupportedError> {
    if ty.memory64 {
        Err(UnsupportedError(WasmExtension::Memory64))?
    }
    Ok(MemoryType(Limits {
        initial_size: ty
            .initial
            .try_into()
            .expect("guaranteed by wasmparser if !memory64"),
        max_size: ty
            .maximum
            .map(|u| u.try_into().expect("guaranteed by wasmparser if !memory64")),
    }))
}

fn convert_table_ty(ty: wasmparser::TableType) -> Result<TableType, UnsupportedError> {
    Ok(TableType(
        convert_elem_ty(ty.element_type)?,
        Limits {
            initial_size: ty.initial,
            max_size: ty.maximum,
        },
    ))
}

fn convert_elem_ty(ty: wasmparser::Type) -> Result<ElemType, UnsupportedError> {
    use wasmparser::Type::*;
    match ty {
        // TODO replace panic with custom error
        I32 | I64 | F32 | F64 => panic!("only reftypes, not value types are allowed as element types"),
        V128 => panic!("only reftypes, not value types are allowed as element types"),
        FuncRef => Ok(ElemType::Anyfunc),
        ExternRef => Err(UnsupportedError(WasmExtension::ReferenceTypes)),
        ExnRef => Err(UnsupportedError(WasmExtension::ExceptionHandling)),
        Func => panic!("only reftypes, not function types are allowed as element types"),
        EmptyBlockType => panic!("only reftypes, not block types are allowed as element types"),
    }
}

fn convert_block_ty(ty: wasmparser::TypeOrFuncType) -> Result<BlockType, UnsupportedError> {
    use wasmparser::TypeOrFuncType::*;
    match ty {
        Type(wasmparser::Type::EmptyBlockType) => Ok(BlockType(None)),
        Type(ty) => Ok(BlockType(Some(convert_ty(ty)?))),
        FuncType(_) => Err(UnsupportedError(WasmExtension::MultiValue)),
    }
}

fn convert_func_ty(ty: wasmparser::FuncType) -> Result<FunctionType, UnsupportedError> {
    fn convert_tys(tys: &[wasmparser::Type]) -> Result<Box<[ValType]>, UnsupportedError> {
        let vec: Vec<ValType> = tys
            .iter()
            .cloned()
            .map(convert_ty)
            .collect::<Result<_, _>>()?;
        Ok(vec.into())
    }

    Ok(FunctionType {
        params: convert_tys(&ty.params)?,
        results: convert_tys(&ty.returns)?,
    })
}

fn convert_global_ty(ty: wasmparser::GlobalType) -> Result<GlobalType, UnsupportedError> {
    Ok(GlobalType(
        convert_ty(ty.content_type)?,
        if ty.mutable {
            Mutability::Mut
        } else {
            Mutability::Const
        },
    ))
}

fn convert_ty(ty: wasmparser::Type) -> Result<ValType, UnsupportedError> {
    use wasmparser::Type;
    match ty {
        Type::I32 => Ok(ValType::I32),
        Type::I64 => Ok(ValType::I64),
        Type::F32 => Ok(ValType::F32),
        Type::F64 => Ok(ValType::F64),
        Type::V128 => Err(UnsupportedError(WasmExtension::Simd)),
        Type::FuncRef => Err(UnsupportedError(WasmExtension::ReferenceTypes)),
        Type::ExternRef => Err(UnsupportedError(WasmExtension::ReferenceTypes)),
        Type::ExnRef => Err(UnsupportedError(WasmExtension::ExceptionHandling)),
        // TODO replace with custom error
        Type::Func => panic!("function types are not a valid value type"),
        Type::EmptyBlockType => panic!("block types are not a valid value type"),
    }
}

// impl<T> AddErrInfo<T> for Result<T, BinaryReaderError> {
//     fn add_err_info<GrammarElement>(self: Result<T, BinaryReaderError>, offset: usize) -> Result<T, Error> {
//         self.map_err(|err|
//             Error::with_source::<GrammarElement, _>(offset, ErrorKind::Leb128, err))
//     }
// }

#[derive(Debug)]
struct IndexError<T>(Idx<T>);

impl<T: fmt::Debug> std::error::Error for IndexError<T> {}

impl<T> fmt::Display for IndexError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let type_name = std::any::type_name::<T>().split("::").last().unwrap();
        writeln!(
            f,
            "{} index out of bounds: {}",
            type_name,
            self.0.into_inner()
        )
    }
}

// TODO higher level error type that contains:
//     offset: usize,

#[derive(Debug)]
struct UnsupportedError(WasmExtension);

impl std::error::Error for UnsupportedError {}

impl fmt::Display for UnsupportedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "This module uses a WebAssembly extension we don't support yet: {}\n\
            See {} for more information about the extension.",
            self.0.name(),
            self.0.url(),
        )
    }
}

/// See https://webassembly.org/roadmap/ and https://github.com/WebAssembly/proposals.
#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub enum WasmExtension {
    // Extensions that are already standardized and merged into WebAssembly 1.1:
    NontrappingFloatToInt,
    SignExtensionOps,
    MultiValue,
    ReferenceTypes,
    BulkMemoryOperations,

    // Standardized, but not yet merged into the core spec (as of 2021-10):
    Simd,

    // In rough decreasing order of stability (i.e., increasing order of
    // breaking changes):
    ThreadsAtomics,
    Memory64,
    ExceptionHandling,
    TailCalls,
    TypeImports,
    MultiMemory,
    ModuleLinking,
}

impl WasmExtension {
    pub fn name(self) -> &'static str {
        use WasmExtension::*;
        match self {
            NontrappingFloatToInt => "non-trapping float-to-int conversions",
            SignExtensionOps => "sign-extension operators",
            MultiValue => "multiple return/result values",
            ReferenceTypes => "reference types",
            BulkMemoryOperations => "bulk memory operations",

            Simd => "SIMD",

            ThreadsAtomics => "threads and atomics",
            Memory64 => "64-bit memory",
            ExceptionHandling => "exception handling",
            TailCalls => "tail calls",
            TypeImports => "type imports",
            MultiMemory => "multiple memories",
            ModuleLinking => "module linking",
        }
    }

    #[rustfmt::skip]
    pub fn url(self) -> &'static str {
        use WasmExtension::*;
        match self {
            NontrappingFloatToInt => r"https://github.com/WebAssembly/nontrapping-float-to-int-conversions",
            SignExtensionOps => r"https://github.com/WebAssembly/sign-extension-ops",
            MultiValue => r"https://github.com/WebAssembly/multi-value",
            ReferenceTypes => r"https://github.com/WebAssembly/reference-types",
            BulkMemoryOperations => r"https://github.com/WebAssembly/bulk-memory-operations",

            Simd => r"https://github.com/WebAssembly/simd",

            ThreadsAtomics => r"https://github.com/WebAssembly/threads",
            Memory64 => r"https://github.com/WebAssembly/memory64",
            ExceptionHandling => r"https://github.com/WebAssembly/exception-handling",
            TailCalls => r"https://github.com/WebAssembly/tail-call",
            TypeImports => r"https://github.com/WebAssembly/proposal-type-imports",
            MultiMemory => r"https://github.com/WebAssembly/multi-memory",
            ModuleLinking => r"https://github.com/WebAssembly/module-linking",
        }
    }
}

// Wrapper for type map, to offer some convenience like:
// - u32 indices (which we get from wasmparser) instead of usize (which Vec expects)
// - checking that type section was present and type index is occupied
struct Types(Option<Vec<FunctionType>>);

impl Types {
    /// Initial state, where the type section has not been parsed yet.
    pub fn none() -> Self {
        Types(None)
    }

    /// Next state, where the number of type entries is known, but nothing filled yet.
    // TODO use own parseerror, not Box dyn Error.
    pub fn set_capacity(&mut self, count: u32) -> Result<(), Box<dyn std::error::Error>> {
        let prev_state = self.0.replace(Vec::with_capacity(u32_to_usize(count)));
        match prev_state {
            Some(_) => Err("duplicate type section".into()),
            None => Ok(()),
        }
    }

    // TODO use own parseerror, not Box dyn Error.
    pub fn add(&mut self, ty: wasmparser::FuncType) -> Result<(), Box<dyn std::error::Error>> {
        self.0
            .as_mut()
            .ok_or("missing type section")?
            .push(convert_func_ty(ty)?);
        Ok(())
    }

    // TODO use own parseerror, not Box dyn Error.
    pub fn get(&self, idx: u32) -> Result<FunctionType, Box<dyn std::error::Error>> {
        Ok(self
            .0
            .as_ref()
            // TODO typed error
            .ok_or("missing type section")?
            .get(u32_to_usize(idx))
            .cloned()
            .ok_or_else(|| IndexError::<FunctionType>(idx.into()))?)
    }
}

fn u32_to_usize(u: u32) -> usize {
    u.try_into().expect("u32 to usize should always succeed")
}