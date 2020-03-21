use crate::code_gen::linker::LinkerError;
use crate::IrDatabase;
use failure::Fail;
use hir::{FileId, RelativePathBuf};
use inkwell::targets::TargetData;
use inkwell::{
    module::{Linkage, Module},
    passes::{PassManager, PassManagerBuilder},
    targets::{CodeModel, FileType, InitializationConfig, RelocMode, Target},
    types::StructType,
    values::{BasicValue, GlobalValue, IntValue, PointerValue, UnnamedAddress},
    AddressSpace, OptimizationLevel,
};
use std::io::{self, Write};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

mod linker;
pub mod symbols;

#[derive(Debug, Fail)]
enum CodeGenerationError {
    #[fail(display = "{}", 0)]
    LinkerError(#[fail(cause)] LinkerError),
    #[fail(display = "unknown target triple: {}", 0)]
    UnknownTargetTriple(String),
    #[fail(display = "error creating target machine")]
    CouldNotCreateTargetMachine,
    #[fail(display = "error creating object file")]
    CouldNotCreateObjectFile(io::Error),
    #[fail(display = "error generating machine code")]
    CodeGenerationError(String),
}

impl From<LinkerError> for CodeGenerationError {
    fn from(e: LinkerError) -> Self {
        CodeGenerationError::LinkerError(e)
    }
}

pub struct ModuleBuilder<'a, D: IrDatabase> {
    db: &'a D,
    file_id: FileId,
    _target: inkwell::targets::Target,
    target_machine: inkwell::targets::TargetMachine,
    assembly_module: Arc<inkwell::module::Module>,
}

impl<'a, D: IrDatabase> ModuleBuilder<'a, D> {
    /// Construct module for the given `hir::FileId` at the specified output file location.
    pub fn new(db: &'a mut D, file_id: FileId) -> Result<Self, failure::Error> {
        let target = db.target();

        // Construct a module for the assembly
        let assembly_module = Arc::new(
            db.context()
                .create_module(db.file_relative_path(file_id).as_str()),
        );

        // Initialize the x86 target
        Target::initialize_x86(&InitializationConfig::default());

        // Retrieve the LLVM target using the specified target.
        let llvm_target = Target::from_triple(&target.llvm_target)
            .map_err(|e| CodeGenerationError::UnknownTargetTriple(e.to_string()))?;
        assembly_module.set_target(&llvm_target);

        // Construct target machine for machine code generation
        let target_machine = llvm_target
            .create_target_machine(
                &target.llvm_target,
                &target.options.cpu,
                &target.options.features,
                db.optimization_lvl(),
                RelocMode::PIC,
                CodeModel::Default,
            )
            .ok_or(CodeGenerationError::CouldNotCreateTargetMachine)?;

        // Initialize the module and target data
        db.set_module(assembly_module.clone());

        Ok(Self {
            db,
            file_id,
            _target: llvm_target,
            target_machine,
            assembly_module,
        })
    }

    /// Construct a shared object at the specified output file location.
    pub fn finalize(&self, out_dir: Option<&Path>) -> Result<PathBuf, failure::Error> {
        // Generate IR for the module and clone it so that we can modify it without modifying the
        // cached value.
        let module = self.db.module_ir(self.file_id);

        // Generate the `get_info` method.
        symbols::gen_reflection_ir(
            self.db,
            &self.assembly_module,
            &module.functions,
            &module.dispatch_table,
            &module.type_table,
        );

        // Optimize the assembly module
        optimize_module(&self.assembly_module, self.db.optimization_lvl());

        // Debug print the IR
        //println!("{}", assembly_module.print_to_string().to_string());

        // Generate object file
        let obj_file = {
            let obj = self
                .target_machine
                .write_to_memory_buffer(&self.assembly_module, FileType::Object)
                .map_err(|e| CodeGenerationError::CodeGenerationError(e.to_string()))?;
            let mut obj_file = tempfile::NamedTempFile::new()
                .map_err(CodeGenerationError::CouldNotCreateObjectFile)?;
            obj_file
                .write(obj.as_slice())
                .map_err(CodeGenerationError::CouldNotCreateObjectFile)?;
            obj_file
        };

        let target = self.db.target();

        // Construct a linker for the target
        let mut linker = linker::create_with_target(&target);
        linker.add_object(obj_file.path())?;

        let output_path = assembly_output_path(self.db, self.file_id, out_dir);

        // Link the object
        linker.build_shared_object(&output_path)?;
        linker.finalize()?;

        Ok(output_path)
    }
}
/// Computes the output path for the assembly of the specified file.
fn assembly_output_path<D: IrDatabase>(db: &D, file_id: FileId, out_dir: Option<&Path>) -> PathBuf {
    let relative_path: RelativePathBuf = db.file_relative_path(file_id);
    let original_filename = Path::new(relative_path.file_name().unwrap());

    // Add the `munlib` suffix to the original filename
    let output_file_name = original_filename.with_extension("munlib");

    // If there is an out dir specified, prepend the output directory
    if let Some(out_dir) = out_dir {
        out_dir.join(output_file_name)
    } else {
        output_file_name
    }
}

/// Optimizes the specified LLVM `Module` using the default passes for the given
/// `OptimizationLevel`.
fn optimize_module(module: &Module, optimization_lvl: OptimizationLevel) {
    let pass_builder = PassManagerBuilder::create();
    pass_builder.set_optimization_level(optimization_lvl);

    let module_pass_manager = PassManager::create(());
    pass_builder.populate_module_pass_manager(&module_pass_manager);
    module_pass_manager.run_on(module);
}

/// Intern a string by constructing a global value. Looks something like this:
/// ```c
/// const char[] GLOBAL_ = "str";
/// ```
pub(crate) fn intern_string(module: &Module, string: &str, name: &str) -> PointerValue {
    let value = module.get_context().const_string(string, true);
    gen_global(module, &value, name).as_pointer_value()
}

/// Construct a global from the specified value
pub(crate) fn gen_global(module: &Module, value: &dyn BasicValue, name: &str) -> GlobalValue {
    let global = module.add_global(value.as_basic_value_enum().get_type(), None, name);
    global.set_linkage(Linkage::Private);
    global.set_constant(true);
    global.set_unnamed_address(UnnamedAddress::Global);
    global.set_initializer(value);
    global
}

/// Generates a global array from the specified list of strings
pub(crate) fn gen_string_array(
    module: &Module,
    strings: impl Iterator<Item = String>,
    name: &str,
) -> PointerValue {
    let str_type = module.get_context().i8_type().ptr_type(AddressSpace::Const);

    let mut strings = strings.peekable();
    if strings.peek().is_none() {
        str_type.ptr_type(AddressSpace::Const).const_null()
    } else {
        let strings = strings
            .map(|s| intern_string(module, &s, name))
            .collect::<Vec<PointerValue>>();

        let strings_ir = str_type.const_array(&strings);
        gen_global(module, &strings_ir, "").as_pointer_value()
    }
}

/// Generates a global array from the specified list of struct pointers
pub(crate) fn gen_struct_ptr_array(
    module: &Module,
    ir_type: StructType,
    ptrs: &[PointerValue],
    name: &str,
) -> PointerValue {
    if ptrs.is_empty() {
        ir_type
            .ptr_type(AddressSpace::Const)
            .ptr_type(AddressSpace::Const)
            .const_null()
    } else {
        let ptr_array_ir = ir_type.ptr_type(AddressSpace::Const).const_array(&ptrs);

        gen_global(module, &ptr_array_ir, name).as_pointer_value()
    }
}

/// Generates a global array from the specified list of integers
pub(crate) fn gen_u16_array(module: &Module, integers: impl Iterator<Item = u64>) -> PointerValue {
    let u16_type = module.get_context().i16_type();

    let mut integers = integers.peekable();
    if integers.peek().is_none() {
        u16_type.ptr_type(AddressSpace::Const).const_null()
    } else {
        let integers = integers
            .map(|i| u16_type.const_int(i, false))
            .collect::<Vec<IntValue>>();

        let array_ir = u16_type.const_array(&integers);
        gen_global(module, &array_ir, "").as_pointer_value()
    }
}

/// Create an inkwell TargetData from the target in the database
pub(crate) fn target_data_query(db: &impl IrDatabase) -> Arc<TargetData> {
    Arc::new(TargetData::create(&db.target().data_layout))
}
