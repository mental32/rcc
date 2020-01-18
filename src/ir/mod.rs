mod expr;
mod static_init;
mod stmt;

use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;

use cranelift::codegen::{
    self,
    ir::{
        entities::StackSlot,
        function::Function,
        stackslot::{StackSlotData, StackSlotKind},
        ExternalName, InstBuilder, MemFlags,
    },
    isa,
    settings::{self, Configurable},
};
use cranelift::frontend::Switch;
use cranelift::prelude::{Ebb, FunctionBuilder, FunctionBuilderContext, Signature};
use cranelift_module::{self, DataId, FuncId, Linkage, Module as CraneliftModule};
use cranelift_object::{ObjectBackend, ObjectBuilder, ObjectProduct, ObjectTrapCollection};

use crate::arch::TARGET;
use crate::data::{prelude::*, types::FunctionType, Initializer, Scope, StorageClass};
use crate::utils;

type Module = CraneliftModule<ObjectBackend>;

enum Id {
    Function(FuncId),
    Global(DataId),
    Local(StackSlot),
}

struct Compiler {
    module: Module,
    scope: Scope<InternedStr, Id>,
    debug: bool,
    // if false, we last saw a switch
    last_saw_loop: bool,
    strings: HashMap<InternedStr, DataId>,
    loops: Vec<(Ebb, Ebb)>,
    // switch, default, end
    // if default is empty once we get to the end of a switch body,
    // we didn't see a default case
    switches: Vec<(Switch, Option<Ebb>, Ebb)>,
    labels: HashMap<InternedStr, Ebb>,
    error_handler: ErrorHandler,
}

/// Compile a program from a high level IR to a Cranelift Module
pub(crate) fn compile(
    program: Vec<Locatable<Declaration>>,
    debug: bool,
) -> (
    Result<ObjectProduct, CompileError>,
    VecDeque<CompileWarning>,
) {
    let name = program.first().map_or_else(
        || "<empty>".to_string(),
        |decl| decl.location.filename.resolve_and_clone(),
    );
    // really we'd like to have all errors but that requires a refactor
    let mut err = None;
    let mut compiler = Compiler::new(name, debug);
    for decl in program {
        let current = match (decl.data.symbol.ctype.clone(), decl.data.init) {
            (Type::Function(func_type), None) => compiler
                .declare_func(
                    decl.data.symbol.id,
                    &func_type.signature(compiler.module.isa()),
                    decl.data.symbol.storage_class,
                    false,
                )
                .map(|_| ()),
            (Type::Void, _) => unreachable!("parser let an incomplete type through"),
            (Type::Function(func_type), Some(Initializer::FunctionBody(stmts))) => compiler
                .compile_func(
                    decl.data.symbol.id,
                    func_type,
                    decl.data.symbol.storage_class,
                    stmts,
                    decl.location,
                ),
            (_, Some(Initializer::FunctionBody(_))) => {
                unreachable!("only functions should have a function body")
            }
            (_, init) => compiler.store_static(decl.data.symbol, init, decl.location),
        };
        if let Err(e) = current {
            err = Some(e);
            break;
        }
    }
    let warns = compiler.error_handler.warnings;
    if let Some(err) = err {
        (Err(err), warns)
    } else {
        (Ok(compiler.module.finish()), warns)
    }
}

impl Compiler {
    fn new(name: String, debug: bool) -> Compiler {
        let mut flags_builder = settings::builder();
        // allow creating shared libraries
        flags_builder
            .enable("is_pic")
            .expect("is_pic should be a valid option");
        // use debug assertions
        flags_builder
            .enable("enable_verifier")
            .expect("enable_verifier should be a valid option");
        // minimal optimizations
        flags_builder
            .set("opt_level", "speed")
            .expect("opt_level: speed should be a valid option");
        // don't emit call to __cranelift_probestack
        flags_builder
            .set("enable_probestack", "false")
            .expect("enable_probestack should be a valid option");

        let isa = isa::lookup(TARGET)
            .unwrap_or_else(|_| utils::fatal(format!("platform not supported: {}", TARGET), 5))
            .finish(settings::Flags::new(flags_builder));

        let builder = ObjectBuilder::new(
            isa,
            name,
            ObjectTrapCollection::Disabled,
            cranelift_module::default_libcall_names(),
        )
        .expect("unknown error creating module");

        Compiler {
            module: Module::new(builder),
            scope: Scope::new(),
            loops: Vec::new(),
            switches: Vec::new(),
            labels: HashMap::new(),
            // the initial value doesn't really matter
            last_saw_loop: true,
            strings: Default::default(),
            error_handler: Default::default(),
            debug,
        }
    }
    // we have to consider the following cases:
    // 1. declaration before definition
    // 2. 2nd declaration before definition
    // 3. definition
    // 4. declaration after definition

    // 1. should declare `id` a import unless specified as `static`.
    // 3. should always declare `id` as export or local.
    // 2. and 4. should be a no-op.
    fn declare_func(
        &mut self,
        id: InternedStr,
        signature: &Signature,
        sc: StorageClass,
        is_definition: bool,
    ) -> CompileResult<FuncId> {
        use crate::get_str;
        if !is_definition {
            // case 2 and 4
            if let Some(Id::Function(func_id)) = self.scope.get(&id) {
                return Ok(*func_id);
            }
        }
        let linkage = match sc {
            StorageClass::Auto | StorageClass::Extern if is_definition => Linkage::Export,
            StorageClass::Auto | StorageClass::Extern => Linkage::Import,
            StorageClass::Static => Linkage::Local,
            StorageClass::Register | StorageClass::Typedef => unreachable!(),
        };
        let func_id = self
            .module
            .declare_function(get_str!(id), linkage, &signature)
            .unwrap_or_else(|err| utils::fatal(err, 6));
        self.scope.insert(id, Id::Function(func_id));
        Ok(func_id)
    }
    /// declare an object on the stack
    fn declare_stack(
        &mut self,
        decl: Declaration,
        location: Location,
        builder: &mut FunctionBuilder,
    ) -> CompileResult<()> {
        if let Type::Function(ftype) = decl.symbol.ctype {
            self.declare_func(
                decl.symbol.id,
                &ftype.signature(self.module.isa()),
                decl.symbol.storage_class,
                false,
            )?;
            return Ok(());
        }
        let u64_size = match decl.symbol.ctype.sizeof() {
            Ok(size) => size,
            Err(err) => {
                return Err(CompileError::semantic(Locatable {
                    data: err.into(),
                    location,
                }))
            }
        };
        let kind = StackSlotKind::ExplicitSlot;
        let size = match u32::try_from(u64_size) {
            Ok(size) => size,
            Err(_) => return Err(CompileError::semantic(Locatable {
                data: "cannot store items on the stack that are more than 4 GB, it will overflow the stack".into(),
                location,
            }))
        };
        let data = StackSlotData {
            kind,
            size,
            offset: None,
        };
        let stack_slot = builder.create_stack_slot(data);
        self.scope.insert(decl.symbol.id, Id::Local(stack_slot));
        if let Some(init) = decl.init {
            self.store_stack(init, stack_slot, builder)?;
        }
        Ok(())
    }
    fn store_stack(
        &mut self,
        init: Initializer,
        stack_slot: StackSlot,
        builder: &mut FunctionBuilder,
    ) -> CompileResult<()> {
        match init {
            Initializer::Scalar(expr) => {
                let val = self.compile_expr(*expr, builder)?;
                // TODO: replace with `builder.ins().stack_store(val.ir_val, stack_slot, 0);`
                // when Cranelift implements stack_store for i8 and i16
                let addr = builder.ins().stack_addr(Type::ptr_type(), stack_slot, 0);
                builder.ins().store(MemFlags::new(), val.ir_val, addr, 0);
            }
            Initializer::InitializerList(_) => unimplemented!("aggregate dynamic initialization"),
            Initializer::FunctionBody(_) => unreachable!("functions can't be stored on the stack"),
        }
        Ok(())
    }
    // TODO: this is grossly inefficient, ask Cranelift devs if
    // there's an easier way to make parameters modifiable.
    fn store_stack_params(
        &mut self,
        params: Vec<Symbol>,
        func_start: Ebb,
        location: &Location,
        builder: &mut FunctionBuilder,
    ) -> CompileResult<()> {
        // Cranelift requires that all EBB params are declared up front
        let ir_vals: Vec<_> = params
            .iter()
            .map(|param| {
                let ir_type = param.ctype.as_ir_type();
                Ok(builder.append_ebb_param(func_start, ir_type))
            })
            .collect::<CompileResult<_>>()?;
        for (param, ir_val) in params.into_iter().zip(ir_vals) {
            let u64_size = match param.ctype.sizeof() {
                Err(data) => semantic_err!(data.into(), *location),
                Ok(size) => size,
            };
            let u32_size = match u32::try_from(u64_size) {
                Err(_) => semantic_err!(
                    format!(
                        "size {} is too large for stack (can only handle 32-bit values)",
                        u64_size
                    ),
                    *location
                ),
                Ok(size) => size,
            };
            let stack_data = StackSlotData {
                kind: StackSlotKind::ExplicitSlot,
                size: u32_size,
                offset: None,
            };
            let slot = builder.create_stack_slot(stack_data);
            // TODO: need to take the address before storing until Cranelift implements
            // stores for i8 and i16
            // then this can be replaced with `builder.ins().stack_store(ir_val, slot, 0);`
            // See https://github.com/CraneStation/cranelift/issues/433
            let addr = builder.ins().stack_addr(Type::ptr_type(), slot, 0);
            builder.ins().store(MemFlags::new(), ir_val, addr, 0);
            self.scope.insert(param.id, Id::Local(slot));
        }
        Ok(())
    }
    fn compile_func(
        &mut self,
        id: InternedStr,
        func_type: FunctionType,
        sc: StorageClass,
        stmts: Vec<Stmt>,
        location: Location,
    ) -> CompileResult<()> {
        let signature = func_type.signature(self.module.isa());
        let func_id = self.declare_func(id.clone(), &signature, sc, true)?;
        // external name is meant to be a lookup in a symbol table,
        // but we just give it garbage values
        let mut func = Function::with_name_signature(ExternalName::user(0, 0), signature);

        // this context is just boiler plate
        let mut ctx = FunctionBuilderContext::new();
        let mut builder = FunctionBuilder::new(&mut func, &mut ctx);

        let func_start = builder.create_ebb();
        builder.switch_to_block(func_start);

        let should_ret = func_type.should_return();
        if func_type.has_params() {
            self.store_stack_params(func_type.params, func_start, &location, &mut builder)?;
        }
        self.compile_all(stmts, &mut builder)?;
        if !builder.is_filled() {
            if id == InternedStr::get_or_intern("main") {
                let ir_int = func_type.return_type.as_ir_type();
                let zero = [builder.ins().iconst(ir_int, 0)];
                builder.ins().return_(&zero);
            } else if should_ret {
                semantic_err!(
                    format!(
                        "expected a return statement before end of function '{}' returning {}",
                        id, func_type.return_type
                    ),
                    location
                );
            } else {
                // void function, return nothing
                builder.ins().return_(&[]);
            }
        }
        builder.seal_all_blocks();
        builder.finalize();

        let flags = settings::Flags::new(settings::builder());

        if self.debug {
            println!("{}", func);
        }

        if let Err(err) = codegen::verify_function(&func, &flags) {
            println!("{}", func);
            utils::fatal(err, 3);
        }

        let mut ctx = codegen::Context::for_function(func);
        if let Err(err) = self.module.define_function(func_id, &mut ctx) {
            println!("{}", ctx.func);
            utils::fatal(err, 4);
        }

        Ok(())
    }
}
