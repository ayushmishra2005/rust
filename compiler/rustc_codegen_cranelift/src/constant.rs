//! Handling of `static`s, `const`s and promoted allocations

use rustc_span::DUMMY_SP;

use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_errors::ErrorReported;
use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
use rustc_middle::mir::interpret::{
    read_target_uint, AllocId, Allocation, ConstValue, ErrorHandled, GlobalAlloc, Pointer, Scalar,
};
use rustc_middle::ty::ConstKind;

use cranelift_codegen::ir::GlobalValueData;
use cranelift_module::*;

use crate::prelude::*;

pub(crate) struct ConstantCx {
    todo: Vec<TodoItem>,
    done: FxHashSet<DataId>,
    anon_allocs: FxHashMap<AllocId, DataId>,
}

#[derive(Copy, Clone, Debug)]
enum TodoItem {
    Alloc(AllocId),
    Static(DefId),
}

impl ConstantCx {
    pub(crate) fn new() -> Self {
        ConstantCx { todo: vec![], done: FxHashSet::default(), anon_allocs: FxHashMap::default() }
    }

    pub(crate) fn finalize(mut self, tcx: TyCtxt<'_>, module: &mut dyn Module) {
        //println!("todo {:?}", self.todo);
        define_all_allocs(tcx, module, &mut self);
        //println!("done {:?}", self.done);
        self.done.clear();
    }
}

pub(crate) fn check_constants(fx: &mut FunctionCx<'_, '_, '_>) -> bool {
    let mut all_constants_ok = true;
    for constant in &fx.mir.required_consts {
        let const_ = match fx.monomorphize(constant.literal) {
            ConstantKind::Ty(ct) => ct,
            ConstantKind::Val(..) => continue,
        };
        match const_.val {
            ConstKind::Value(_) => {}
            ConstKind::Unevaluated(unevaluated) => {
                if let Err(err) =
                    fx.tcx.const_eval_resolve(ParamEnv::reveal_all(), unevaluated, None)
                {
                    all_constants_ok = false;
                    match err {
                        ErrorHandled::Reported(ErrorReported) | ErrorHandled::Linted => {
                            fx.tcx.sess.span_err(constant.span, "erroneous constant encountered");
                        }
                        ErrorHandled::TooGeneric => {
                            span_bug!(
                                constant.span,
                                "codgen encountered polymorphic constant: {:?}",
                                err
                            );
                        }
                    }
                }
            }
            ConstKind::Param(_)
            | ConstKind::Infer(_)
            | ConstKind::Bound(_, _)
            | ConstKind::Placeholder(_)
            | ConstKind::Error(_) => unreachable!("{:?}", const_),
        }
    }
    all_constants_ok
}

pub(crate) fn codegen_static(tcx: TyCtxt<'_>, module: &mut dyn Module, def_id: DefId) {
    let mut constants_cx = ConstantCx::new();
    constants_cx.todo.push(TodoItem::Static(def_id));
    constants_cx.finalize(tcx, module);
}

pub(crate) fn codegen_tls_ref<'tcx>(
    fx: &mut FunctionCx<'_, '_, 'tcx>,
    def_id: DefId,
    layout: TyAndLayout<'tcx>,
) -> CValue<'tcx> {
    let data_id = data_id_for_static(fx.tcx, fx.module, def_id, false);
    let local_data_id = fx.module.declare_data_in_func(data_id, &mut fx.bcx.func);
    if fx.clif_comments.enabled() {
        fx.add_comment(local_data_id, format!("tls {:?}", def_id));
    }
    let tls_ptr = fx.bcx.ins().tls_value(fx.pointer_type, local_data_id);
    CValue::by_val(tls_ptr, layout)
}

fn codegen_static_ref<'tcx>(
    fx: &mut FunctionCx<'_, '_, 'tcx>,
    def_id: DefId,
    layout: TyAndLayout<'tcx>,
) -> CPlace<'tcx> {
    let data_id = data_id_for_static(fx.tcx, fx.module, def_id, false);
    let local_data_id = fx.module.declare_data_in_func(data_id, &mut fx.bcx.func);
    if fx.clif_comments.enabled() {
        fx.add_comment(local_data_id, format!("{:?}", def_id));
    }
    let global_ptr = fx.bcx.ins().global_value(fx.pointer_type, local_data_id);
    assert!(!layout.is_unsized(), "unsized statics aren't supported");
    assert!(
        matches!(
            fx.bcx.func.global_values[local_data_id],
            GlobalValueData::Symbol { tls: false, .. }
        ),
        "tls static referenced without Rvalue::ThreadLocalRef"
    );
    CPlace::for_ptr(crate::pointer::Pointer::new(global_ptr), layout)
}

pub(crate) fn codegen_constant<'tcx>(
    fx: &mut FunctionCx<'_, '_, 'tcx>,
    constant: &Constant<'tcx>,
) -> CValue<'tcx> {
    let const_ = match fx.monomorphize(constant.literal) {
        ConstantKind::Ty(ct) => ct,
        ConstantKind::Val(val, ty) => return codegen_const_value(fx, val, ty),
    };
    let const_val = match const_.val {
        ConstKind::Value(const_val) => const_val,
        ConstKind::Unevaluated(ty::Unevaluated { def, substs, promoted })
            if fx.tcx.is_static(def.did) =>
        {
            assert!(substs.is_empty());
            assert!(promoted.is_none());

            return codegen_static_ref(fx, def.did, fx.layout_of(const_.ty)).to_cvalue(fx);
        }
        ConstKind::Unevaluated(unevaluated) => {
            match fx.tcx.const_eval_resolve(ParamEnv::reveal_all(), unevaluated, None) {
                Ok(const_val) => const_val,
                Err(_) => {
                    span_bug!(constant.span, "erroneous constant not captured by required_consts");
                }
            }
        }
        ConstKind::Param(_)
        | ConstKind::Infer(_)
        | ConstKind::Bound(_, _)
        | ConstKind::Placeholder(_)
        | ConstKind::Error(_) => unreachable!("{:?}", const_),
    };

    codegen_const_value(fx, const_val, const_.ty)
}

pub(crate) fn codegen_const_value<'tcx>(
    fx: &mut FunctionCx<'_, '_, 'tcx>,
    const_val: ConstValue<'tcx>,
    ty: Ty<'tcx>,
) -> CValue<'tcx> {
    let layout = fx.layout_of(ty);
    assert!(!layout.is_unsized(), "sized const value");

    if layout.is_zst() {
        return CValue::by_ref(crate::Pointer::dangling(layout.align.pref), layout);
    }

    match const_val {
        ConstValue::Scalar(x) => {
            if fx.clif_type(layout.ty).is_none() {
                let (size, align) = (layout.size, layout.align.pref);
                let mut alloc = Allocation::from_bytes(
                    std::iter::repeat(0).take(size.bytes_usize()).collect::<Vec<u8>>(),
                    align,
                );
                let ptr = Pointer::new(AllocId(!0), Size::ZERO); // The alloc id is never used
                alloc.write_scalar(fx, ptr, x.into(), size).unwrap();
                let alloc = fx.tcx.intern_const_alloc(alloc);
                return CValue::by_ref(pointer_for_allocation(fx, alloc), layout);
            }

            match x {
                Scalar::Int(int) => CValue::const_val(fx, layout, int),
                Scalar::Ptr(ptr) => {
                    let alloc_kind = fx.tcx.get_global_alloc(ptr.alloc_id);
                    let base_addr = match alloc_kind {
                        Some(GlobalAlloc::Memory(alloc)) => {
                            fx.constants_cx.todo.push(TodoItem::Alloc(ptr.alloc_id));
                            let data_id = data_id_for_alloc_id(
                                &mut fx.constants_cx,
                                fx.module,
                                ptr.alloc_id,
                                alloc.mutability,
                            );
                            let local_data_id =
                                fx.module.declare_data_in_func(data_id, &mut fx.bcx.func);
                            if fx.clif_comments.enabled() {
                                fx.add_comment(local_data_id, format!("{:?}", ptr.alloc_id));
                            }
                            fx.bcx.ins().global_value(fx.pointer_type, local_data_id)
                        }
                        Some(GlobalAlloc::Function(instance)) => {
                            let func_id = crate::abi::import_function(fx.tcx, fx.module, instance);
                            let local_func_id =
                                fx.module.declare_func_in_func(func_id, &mut fx.bcx.func);
                            fx.bcx.ins().func_addr(fx.pointer_type, local_func_id)
                        }
                        Some(GlobalAlloc::Static(def_id)) => {
                            assert!(fx.tcx.is_static(def_id));
                            let data_id = data_id_for_static(fx.tcx, fx.module, def_id, false);
                            let local_data_id =
                                fx.module.declare_data_in_func(data_id, &mut fx.bcx.func);
                            if fx.clif_comments.enabled() {
                                fx.add_comment(local_data_id, format!("{:?}", def_id));
                            }
                            fx.bcx.ins().global_value(fx.pointer_type, local_data_id)
                        }
                        None => bug!("missing allocation {:?}", ptr.alloc_id),
                    };
                    let val = if ptr.offset.bytes() != 0 {
                        fx.bcx.ins().iadd_imm(base_addr, i64::try_from(ptr.offset.bytes()).unwrap())
                    } else {
                        base_addr
                    };
                    CValue::by_val(val, layout)
                }
            }
        }
        ConstValue::ByRef { alloc, offset } => CValue::by_ref(
            pointer_for_allocation(fx, alloc)
                .offset_i64(fx, i64::try_from(offset.bytes()).unwrap()),
            layout,
        ),
        ConstValue::Slice { data, start, end } => {
            let ptr = pointer_for_allocation(fx, data)
                .offset_i64(fx, i64::try_from(start).unwrap())
                .get_addr(fx);
            let len = fx
                .bcx
                .ins()
                .iconst(fx.pointer_type, i64::try_from(end.checked_sub(start).unwrap()).unwrap());
            CValue::by_val_pair(ptr, len, layout)
        }
    }
}

fn pointer_for_allocation<'tcx>(
    fx: &mut FunctionCx<'_, '_, 'tcx>,
    alloc: &'tcx Allocation,
) -> crate::pointer::Pointer {
    let alloc_id = fx.tcx.create_memory_alloc(alloc);
    fx.constants_cx.todo.push(TodoItem::Alloc(alloc_id));
    let data_id =
        data_id_for_alloc_id(&mut fx.constants_cx, &mut *fx.module, alloc_id, alloc.mutability);

    let local_data_id = fx.module.declare_data_in_func(data_id, &mut fx.bcx.func);
    if fx.clif_comments.enabled() {
        fx.add_comment(local_data_id, format!("{:?}", alloc_id));
    }
    let global_ptr = fx.bcx.ins().global_value(fx.pointer_type, local_data_id);
    crate::pointer::Pointer::new(global_ptr)
}

fn data_id_for_alloc_id(
    cx: &mut ConstantCx,
    module: &mut dyn Module,
    alloc_id: AllocId,
    mutability: rustc_hir::Mutability,
) -> DataId {
    *cx.anon_allocs.entry(alloc_id).or_insert_with(|| {
        module.declare_anonymous_data(mutability == rustc_hir::Mutability::Mut, false).unwrap()
    })
}

fn data_id_for_static(
    tcx: TyCtxt<'_>,
    module: &mut dyn Module,
    def_id: DefId,
    definition: bool,
) -> DataId {
    let rlinkage = tcx.codegen_fn_attrs(def_id).linkage;
    let linkage = if definition {
        crate::linkage::get_static_linkage(tcx, def_id)
    } else if rlinkage == Some(rustc_middle::mir::mono::Linkage::ExternalWeak)
        || rlinkage == Some(rustc_middle::mir::mono::Linkage::WeakAny)
    {
        Linkage::Preemptible
    } else {
        Linkage::Import
    };

    let instance = Instance::mono(tcx, def_id).polymorphize(tcx);
    let symbol_name = tcx.symbol_name(instance).name;
    let ty = instance.ty(tcx, ParamEnv::reveal_all());
    let is_mutable = if tcx.is_mutable_static(def_id) {
        true
    } else {
        !ty.is_freeze(tcx.at(DUMMY_SP), ParamEnv::reveal_all())
    };
    let align = tcx.layout_of(ParamEnv::reveal_all().and(ty)).unwrap().align.pref.bytes();

    let attrs = tcx.codegen_fn_attrs(def_id);

    let data_id = module
        .declare_data(
            &*symbol_name,
            linkage,
            is_mutable,
            attrs.flags.contains(CodegenFnAttrFlags::THREAD_LOCAL),
        )
        .unwrap();

    if rlinkage.is_some() {
        // Comment copied from https://github.com/rust-lang/rust/blob/45060c2a66dfd667f88bd8b94261b28a58d85bd5/src/librustc_codegen_llvm/consts.rs#L141
        // Declare an internal global `extern_with_linkage_foo` which
        // is initialized with the address of `foo`.  If `foo` is
        // discarded during linking (for example, if `foo` has weak
        // linkage and there are no definitions), then
        // `extern_with_linkage_foo` will instead be initialized to
        // zero.

        let ref_name = format!("_rust_extern_with_linkage_{}", symbol_name);
        let ref_data_id = module.declare_data(&ref_name, Linkage::Local, false, false).unwrap();
        let mut data_ctx = DataContext::new();
        data_ctx.set_align(align);
        let data = module.declare_data_in_data(data_id, &mut data_ctx);
        data_ctx.define(std::iter::repeat(0).take(pointer_ty(tcx).bytes() as usize).collect());
        data_ctx.write_data_addr(0, data, 0);
        match module.define_data(ref_data_id, &data_ctx) {
            // Every time the static is referenced there will be another definition of this global,
            // so duplicate definitions are expected and allowed.
            Err(ModuleError::DuplicateDefinition(_)) => {}
            res => res.unwrap(),
        }
        ref_data_id
    } else {
        data_id
    }
}

fn define_all_allocs(tcx: TyCtxt<'_>, module: &mut dyn Module, cx: &mut ConstantCx) {
    while let Some(todo_item) = cx.todo.pop() {
        let (data_id, alloc, section_name) = match todo_item {
            TodoItem::Alloc(alloc_id) => {
                //println!("alloc_id {}", alloc_id);
                let alloc = match tcx.get_global_alloc(alloc_id).unwrap() {
                    GlobalAlloc::Memory(alloc) => alloc,
                    GlobalAlloc::Function(_) | GlobalAlloc::Static(_) => unreachable!(),
                };
                let data_id = data_id_for_alloc_id(cx, module, alloc_id, alloc.mutability);
                (data_id, alloc, None)
            }
            TodoItem::Static(def_id) => {
                //println!("static {:?}", def_id);

                let section_name = tcx.codegen_fn_attrs(def_id).link_section.map(|s| s.as_str());

                let alloc = tcx.eval_static_initializer(def_id).unwrap();

                let data_id = data_id_for_static(tcx, module, def_id, true);
                (data_id, alloc, section_name)
            }
        };

        //("data_id {}", data_id);
        if cx.done.contains(&data_id) {
            continue;
        }

        let mut data_ctx = DataContext::new();
        data_ctx.set_align(alloc.align.bytes());

        if let Some(section_name) = section_name {
            // FIXME set correct segment for Mach-O files
            data_ctx.set_segment_section("", &*section_name);
        }

        let bytes = alloc.inspect_with_uninit_and_ptr_outside_interpreter(0..alloc.len()).to_vec();
        data_ctx.define(bytes.into_boxed_slice());

        for &(offset, (_tag, reloc)) in alloc.relocations().iter() {
            let addend = {
                let endianness = tcx.data_layout.endian;
                let offset = offset.bytes() as usize;
                let ptr_size = tcx.data_layout.pointer_size;
                let bytes = &alloc.inspect_with_uninit_and_ptr_outside_interpreter(
                    offset..offset + ptr_size.bytes() as usize,
                );
                read_target_uint(endianness, bytes).unwrap()
            };

            let reloc_target_alloc = tcx.get_global_alloc(reloc).unwrap();
            let data_id = match reloc_target_alloc {
                GlobalAlloc::Function(instance) => {
                    assert_eq!(addend, 0);
                    let func_id = crate::abi::import_function(tcx, module, instance);
                    let local_func_id = module.declare_func_in_data(func_id, &mut data_ctx);
                    data_ctx.write_function_addr(offset.bytes() as u32, local_func_id);
                    continue;
                }
                GlobalAlloc::Memory(target_alloc) => {
                    cx.todo.push(TodoItem::Alloc(reloc));
                    data_id_for_alloc_id(cx, module, reloc, target_alloc.mutability)
                }
                GlobalAlloc::Static(def_id) => {
                    if tcx.codegen_fn_attrs(def_id).flags.contains(CodegenFnAttrFlags::THREAD_LOCAL)
                    {
                        tcx.sess.fatal(&format!(
                            "Allocation {:?} contains reference to TLS value {:?}",
                            alloc, def_id
                        ));
                    }

                    // Don't push a `TodoItem::Static` here, as it will cause statics used by
                    // multiple crates to be duplicated between them. It isn't necessary anyway,
                    // as it will get pushed by `codegen_static` when necessary.
                    data_id_for_static(tcx, module, def_id, false)
                }
            };

            let global_value = module.declare_data_in_data(data_id, &mut data_ctx);
            data_ctx.write_data_addr(offset.bytes() as u32, global_value, addend as i64);
        }

        module.define_data(data_id, &data_ctx).unwrap();
        cx.done.insert(data_id);
    }

    assert!(cx.todo.is_empty(), "{:?}", cx.todo);
}

pub(crate) fn mir_operand_get_const_val<'tcx>(
    fx: &FunctionCx<'_, '_, 'tcx>,
    operand: &Operand<'tcx>,
) -> Option<ConstValue<'tcx>> {
    match operand {
        Operand::Copy(_) | Operand::Move(_) => None,
        Operand::Constant(const_) => match const_.literal {
            ConstantKind::Ty(const_) => {
                fx.monomorphize(const_).eval(fx.tcx, ParamEnv::reveal_all()).val.try_to_value()
            }
            ConstantKind::Val(val, _) => Some(val),
        },
    }
}
