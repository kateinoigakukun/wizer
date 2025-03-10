//! Wizer: the WebAssembly pre-initializer!
//!
//! See the [`Wizer`] struct for details.

#![deny(missing_docs)]

#[cfg(fuzzing)]
pub mod dummy;
#[cfg(not(fuzzing))]
mod dummy;

mod info;
mod instrument;
mod parse;
mod rewrite;
mod snapshot;
mod stack_ext;
mod translate;

use anyhow::Context;
use dummy::dummy_imports;
use std::collections::{HashMap, HashSet};
use std::fmt::Display;
use std::path::PathBuf;
#[cfg(feature = "structopt")]
use structopt::StructOpt;
use wasmtime::Extern;
use wasmtime_wasi::WasiCtx;

const DEFAULT_INHERIT_STDIO: bool = true;
const DEFAULT_INHERIT_ENV: bool = false;
const DEFAULT_WASM_MULTI_VALUE: bool = true;
const DEFAULT_WASM_MULTI_MEMORY: bool = true;
const DEFAULT_WASM_MODULE_LINKING: bool = false;

/// We only ever use `Store<T>` with a fixed `T` that is our optional WASI
/// context.
pub(crate) type Store = wasmtime::Store<Option<WasiCtx>>;

/// We only ever use `Linker<T>` with a fixed `T` that is our optional WASI
/// context.
pub(crate) type Linker = wasmtime::Linker<Option<WasiCtx>>;

/// Wizer: the WebAssembly pre-initializer!
///
/// Don't wait for your Wasm module to initialize itself, pre-initialize it!
/// Wizer instantiates your WebAssembly module, executes its initialization
/// function, and then serializes the instance's initialized state out into a
/// new WebAssembly module. Now you can use this new, pre-initialized
/// WebAssembly module to hit the ground running, without making your users wait
/// for that first-time set up code to complete.
///
/// ## Caveats
///
/// * The initialization function may not call any imported functions. Doing so
///   will trigger a trap and `wizer` will exit.
///
/// * The Wasm module may not import globals, tables, or memories.
///
/// * Reference types are not supported yet. This is tricky because it would
///   allow the Wasm module to mutate tables, and we would need to be able to
///   snapshot the new table state, but funcrefs and externrefs don't have
///   identity and aren't comparable in the Wasm spec, which makes snapshotting
///   difficult.
#[cfg_attr(feature = "structopt", derive(StructOpt))]
#[derive(Clone, Debug)]
pub struct Wizer {
    /// The Wasm export name of the function that should be executed to
    /// initialize the Wasm module.
    #[cfg_attr(
        feature = "structopt",
        structopt(short = "f", long = "init-func", default_value = "wizer.initialize")
    )]
    init_func: String,

    /// Any function renamings to perform.
    ///
    /// A renaming specification `dst=src` renames a function export `src` to
    /// `dst`, overwriting any previous `dst` export.
    ///
    /// Multiple renamings can be specified. It is an error to specify more than
    /// one source to rename to a destination name, or to specify more than one
    /// renaming destination for one source.
    ///
    /// This option can be used, for example, to replace a `_start` entry point
    /// in an initialized module with an alternate entry point.
    ///
    /// When module linking is enabled, these renames are only applied to the
    /// outermost module.
    #[cfg_attr(
        feature = "structopt",
        structopt(
            short = "r",
            long = "rename-func",
            alias = "func-rename",
            value_name = "dst=src"
        )
    )]
    func_renames: Vec<String>,

    /// Allow WASI imports to be called during initialization.
    ///
    /// This can introduce diverging semantics because the initialization can
    /// observe nondeterminism that might have gone a different way at runtime
    /// than it did at initialization time.
    ///
    /// If your Wasm module uses WASI's `get_random` to add randomness to
    /// something as a security mitigation (e.g. something akin to ASLR or the
    /// way Rust's hash maps incorporate a random nonce) then note that, if the
    /// randomization is added during initialization time and you don't ever
    /// re-randomize at runtime, then that randomization will become per-module
    /// rather than per-instance.
    #[cfg_attr(feature = "structopt", structopt(long = "allow-wasi"))]
    allow_wasi: bool,

    /// When using WASI during initialization, should `stdin`, `stderr`, and
    /// `stdout` be inherited?
    ///
    /// This is true by default.
    #[cfg_attr(
        feature = "structopt",
        structopt(long = "inherit-stdio", value_name = "true|false")
    )]
    inherit_stdio: Option<bool>,

    /// When using WASI during initialization, should environment variables be
    /// inherited?
    ///
    /// This is false by default.
    #[cfg_attr(
        feature = "structopt",
        structopt(long = "inherit-env", value_name = "true|false")
    )]
    inherit_env: Option<bool>,

    /// When using WASI during initialization, which file system directories
    /// should be made available?
    ///
    /// None are available by default.
    #[cfg_attr(
        feature = "structopt",
        structopt(long = "dir", parse(from_os_str), value_name = "directory")
    )]
    dirs: Vec<PathBuf>,

    /// Enable or disable Wasm multi-memory proposal.
    ///
    /// Enabled by default.
    #[cfg_attr(feature = "structopt", structopt(long, value_name = "true|false"))]
    wasm_multi_memory: Option<bool>,

    /// Enable or disable the Wasm multi-value proposal.
    ///
    /// Enabled by default.
    #[cfg_attr(feature = "structopt", structopt(long, value_name = "true|false"))]
    wasm_multi_value: Option<bool>,

    /// Enable or disable the Wasm module-linking proposal.
    ///
    /// Disabled by default.
    #[cfg_attr(feature = "structopt", structopt(long, value_name = "true|false"))]
    wasm_module_linking: Option<bool>,
}

struct FuncRenames {
    /// For a given export name that we encounter in the original module, a map
    /// to a new name, if any, to emit in the output module.
    rename_src_to_dst: HashMap<String, String>,
    /// A set of export names that we ignore in the original module (because
    /// they are overwritten by renamings).
    rename_dsts: HashSet<String>,
}

impl FuncRenames {
    fn parse(renames: &Vec<String>) -> anyhow::Result<FuncRenames> {
        let mut ret = FuncRenames {
            rename_src_to_dst: HashMap::new(),
            rename_dsts: HashSet::new(),
        };
        if renames.is_empty() {
            return Ok(ret);
        }

        for rename_spec in renames {
            let equal = rename_spec
                .trim()
                .find('=')
                .ok_or_else(|| anyhow::anyhow!("Invalid function rename part: {}", rename_spec))?;
            // TODO: use .split_off() when the API is stabilized.
            let dst = rename_spec[..equal].to_owned();
            let src = rename_spec[equal + 1..].to_owned();
            if ret.rename_dsts.contains(&dst) {
                anyhow::bail!("Duplicated function rename dst {}", dst);
            }
            if ret.rename_src_to_dst.contains_key(&src) {
                anyhow::bail!("Duplicated function rename src {}", src);
            }
            ret.rename_dsts.insert(dst.clone());
            ret.rename_src_to_dst.insert(src, dst);
        }

        Ok(ret)
    }
}

impl Wizer {
    /// Construct a new `Wizer` builder.
    pub fn new() -> Self {
        Wizer {
            init_func: "wizer.initialize".into(),
            func_renames: vec![],
            allow_wasi: false,
            inherit_stdio: None,
            inherit_env: None,
            dirs: vec![],
            wasm_multi_memory: None,
            wasm_multi_value: None,
            wasm_module_linking: None,
        }
    }

    /// The export name of the initializer function.
    ///
    /// Defaults to `"wizer.initialize"`.
    pub fn init_func(&mut self, init_func: impl Into<String>) -> &mut Self {
        self.init_func = init_func.into();
        self
    }

    /// Add a function rename to perform.
    pub fn func_rename(&mut self, new_name: impl Display, old_name: impl Display) -> &mut Self {
        self.func_renames.push(format!("{}={}", new_name, old_name));
        self
    }

    /// Allow WASI imports to be called during initialization?
    ///
    /// This can introduce diverging semantics because the initialization can
    /// observe nondeterminism that might have gone a different way at runtime
    /// than it did at initialization time.
    ///
    /// If your Wasm module uses WASI's `get_random` to add randomness to
    /// something as a security mitigation (e.g. something akin to ASLR or the
    /// way Rust's hash maps incorporate a random nonce) then note that, if the
    /// randomization is added during initialization time and you don't ever
    /// re-randomize at runtime, then that randomization will become per-module
    /// rather than per-instance.
    ///
    /// Defaults to `false`.
    pub fn allow_wasi(&mut self, allow: bool) -> &mut Self {
        self.allow_wasi = allow;
        self
    }

    /// When using WASI during initialization, should `stdin`, `stdout`, and
    /// `stderr` be inherited?
    ///
    /// Defaults to `true`.
    pub fn inherit_stdio(&mut self, inherit: bool) -> &mut Self {
        self.inherit_stdio = Some(inherit);
        self
    }

    /// When using WASI during initialization, should the environment variables
    /// be inherited?
    ///
    /// Defaults to `false`.
    pub fn inherit_env(&mut self, inherit: bool) -> &mut Self {
        self.inherit_env = Some(inherit);
        self
    }

    /// When using WASI during initialization, which file system directories
    /// should be made available?
    ///
    /// None are available by default.
    pub fn dir(&mut self, directory: impl Into<PathBuf>) -> &mut Self {
        self.dirs.push(directory.into());
        self
    }

    /// Enable or disable the Wasm multi-memory proposal.
    ///
    /// Defaults to `true`.
    pub fn wasm_multi_memory(&mut self, enable: bool) -> &mut Self {
        self.wasm_multi_memory = Some(enable);
        self
    }

    /// Enable or disable the Wasm multi-value proposal.
    ///
    /// Defaults to `true`.
    pub fn wasm_multi_value(&mut self, enable: bool) -> &mut Self {
        self.wasm_multi_value = Some(enable);
        self
    }

    /// Enable or disable the Wasm module-linking proposal.
    ///
    /// Defaults to `false`.
    pub fn wasm_module_linking(&mut self, enable: bool) -> &mut Self {
        self.wasm_module_linking = Some(enable);
        self
    }

    /// Initialize the given Wasm, snapshot it, and return the serialized
    /// snapshot as a new, pre-initialized Wasm module.
    pub fn run(&self, wasm: &[u8]) -> anyhow::Result<Vec<u8>> {
        // Parse rename spec.
        let renames = FuncRenames::parse(&self.func_renames)?;

        // Make sure we're given valid Wasm from the get go.
        self.wasm_validate(&wasm)?;

        let mut cx = parse::parse(wasm)?;
        let instrumented_wasm = instrument::instrument(&cx);

        if cfg!(debug_assertions) {
            if let Err(error) = self.wasm_validate(&instrumented_wasm) {
                #[cfg(feature = "wasmprinter")]
                let wat = wasmprinter::print_bytes(&wasm)
                    .unwrap_or_else(|e| format!("Disassembling to WAT failed: {}", e));
                #[cfg(not(feature = "wasmprinter"))]
                let wat = "`wasmprinter` cargo feature is not enabled".to_string();
                panic!(
                    "instrumented Wasm is not valid: {:?}\n\nWAT:\n{}",
                    error, wat
                );
            }
        }

        let config = self.wasmtime_config()?;
        let engine = wasmtime::Engine::new(&config)?;
        let wasi_ctx = self.wasi_context()?;
        let mut store = wasmtime::Store::new(&engine, wasi_ctx);
        let module = wasmtime::Module::new(&engine, &instrumented_wasm)
            .context("failed to compile the Wasm module")?;
        self.validate_init_func(&module)?;

        let (instance, has_wasi_initialize) = self.initialize(&mut store, &module)?;
        let snapshot = snapshot::snapshot(&mut store, &instance);
        let rewritten_wasm = self.rewrite(
            &mut cx,
            &mut store,
            &snapshot,
            &renames,
            has_wasi_initialize,
        );

        if cfg!(debug_assertions) {
            if let Err(error) = self.wasm_validate(&rewritten_wasm) {
                #[cfg(feature = "wasmprinter")]
                let wat = wasmprinter::print_bytes(&wasm)
                    .unwrap_or_else(|e| format!("Disassembling to WAT failed: {}", e));
                #[cfg(not(feature = "wasmprinter"))]
                let wat = "`wasmprinter` cargo feature is not enabled".to_string();
                panic!("rewritten Wasm is not valid: {:?}\n\nWAT:\n{}", error, wat);
            }
        }

        Ok(rewritten_wasm)
    }

    // NB: keep this in sync with the wasmparser features.
    fn wasmtime_config(&self) -> anyhow::Result<wasmtime::Config> {
        let mut config = wasmtime::Config::new();

        // Enable Wasmtime's code cache. This makes it so that repeated
        // wizenings of the same Wasm module (e.g. with different WASI inputs)
        // doesn't require re-compiling the Wasm to native code every time.
        config.cache_config_load_default()?;

        // Proposals we support.
        config.wasm_multi_memory(self.wasm_multi_memory.unwrap_or(DEFAULT_WASM_MULTI_MEMORY));
        config.wasm_multi_value(self.wasm_multi_value.unwrap_or(DEFAULT_WASM_MULTI_VALUE));
        config.wasm_module_linking(
            self.wasm_module_linking
                .unwrap_or(DEFAULT_WASM_MODULE_LINKING),
        );

        // Proposoals that we should add support for.
        config.wasm_reference_types(false);
        config.wasm_simd(false);
        config.wasm_threads(false);
        config.wasm_bulk_memory(false);

        Ok(config)
    }

    // NB: keep this in sync with the Wasmtime config.
    fn wasm_features(&self) -> wasmparser::WasmFeatures {
        wasmparser::WasmFeatures {
            // Proposals that we support.
            multi_memory: self.wasm_multi_memory.unwrap_or(DEFAULT_WASM_MULTI_MEMORY),
            multi_value: self.wasm_multi_value.unwrap_or(DEFAULT_WASM_MULTI_VALUE),
            module_linking: self
                .wasm_module_linking
                .unwrap_or(DEFAULT_WASM_MODULE_LINKING),

            // Proposals that we should add support for.
            reference_types: false,
            simd: false,
            threads: false,
            tail_call: false,
            memory64: false,
            exceptions: false,

            // XXX: Though we don't actually support bulk memory yet, we
            // unconditionally turn it on.
            //
            // Many parsers, notably our own `wasmparser`, assume that which
            // Wasm features are enabled or disabled cannot affect parsing, only
            // validation. That assumption is incorrect when it comes to data
            // segments, the multi-memory proposal, and the bulk memory
            // proposal. A `0x01` prefix of a data segment can either mean "this
            // is a passive segment" if bulk memory is enabled or "this segment
            // is referring to memory index 1" if both bulk memory is disabled
            // and multi-memory is enabled. `wasmparser` fails to handle this
            // edge case, which means that everything built on top of it, like
            // Wasmtime, also fail to handle this edge case. However, because
            // bulk memory is merged into the spec proper and is no longer
            // technically a "proposal", and because a fix would require
            // significant refactoring and API changes to give a
            // `wasmparser::Parser` a `wasmparser::WasmFeatures`, we won't ever
            // resolve this discrepancy in `wasmparser`.
            //
            // So we enable bulk memory during parsing, validation, and
            // execution, but we add our own custom validation pass to ensure
            // that no table-mutating instructions exist in our input modules
            // until we *actually* support bulk memory.
            bulk_memory: true,

            // We will never want to enable this.
            deterministic_only: false,
        }
    }

    fn wasm_validate(&self, wasm: &[u8]) -> anyhow::Result<()> {
        log::debug!("Validating input Wasm");

        let mut validator = wasmparser::Validator::new();
        validator.wasm_features(self.wasm_features());
        validator.validate_all(wasm)?;

        // Reject bulk memory stuff that manipulates state we don't
        // snapshot. See the comment inside `wasm_features`.
        let mut wasm = wasm;
        let mut parsers = vec![wasmparser::Parser::new(0)];
        while !parsers.is_empty() {
            let payload = match parsers.last_mut().unwrap().parse(wasm, true).unwrap() {
                wasmparser::Chunk::NeedMoreData(_) => unreachable!(),
                wasmparser::Chunk::Parsed { consumed, payload } => {
                    wasm = &wasm[consumed..];
                    payload
                }
            };
            match payload {
                wasmparser::Payload::CodeSectionEntry(code) => {
                    let mut ops = code.get_operators_reader().unwrap();
                    while !ops.eof() {
                        match ops.read().unwrap() {
                            wasmparser::Operator::TableCopy { .. } => {
                                anyhow::bail!("unsupported `table.copy` instruction")
                            }
                            wasmparser::Operator::TableInit { .. } => {
                                anyhow::bail!("unsupported `table.init` instruction")
                            }
                            wasmparser::Operator::ElemDrop { .. } => {
                                anyhow::bail!("unsupported `elem.drop` instruction")
                            }
                            wasmparser::Operator::DataDrop { .. } => {
                                anyhow::bail!("unsupported `data.drop` instruction")
                            }
                            wasmparser::Operator::TableSet { .. } => {
                                unreachable!("part of reference types")
                            }
                            _ => continue,
                        }
                    }
                }
                wasmparser::Payload::ModuleSectionEntry { parser, .. } => {
                    parsers.push(parser);
                }
                wasmparser::Payload::DataSection(mut data) => {
                    let count = data.get_count();
                    for _ in 0..count {
                        if let wasmparser::DataKind::Passive = data.read().unwrap().kind {
                            anyhow::bail!("unsupported passive data segment");
                        }
                    }
                }
                wasmparser::Payload::End => {
                    parsers.pop();
                }
                _ => continue,
            }
        }

        Ok(())
    }

    /// Check that the module exports an initialization function, and that the
    /// function has the correct type.
    fn validate_init_func(&self, module: &wasmtime::Module) -> anyhow::Result<()> {
        log::debug!("Validating the exported initialization function");
        match module.get_export(&self.init_func) {
            Some(wasmtime::ExternType::Func(func_ty)) => {
                if func_ty.params().len() != 0 || func_ty.results().len() != 0 {
                    anyhow::bail!(
                        "the Wasm module's `{}` function export does not have type `[] -> []`",
                        &self.init_func
                    );
                }
            }
            Some(_) => anyhow::bail!(
                "the Wasm module's `{}` export is not a function",
                &self.init_func
            ),
            None => anyhow::bail!(
                "the Wasm module does not have a `{}` export",
                &self.init_func
            ),
        }
        Ok(())
    }

    fn wasi_context(&self) -> anyhow::Result<Option<WasiCtx>> {
        if !self.allow_wasi {
            return Ok(None);
        }

        let mut ctx = wasi_cap_std_sync::WasiCtxBuilder::new();
        if self.inherit_stdio.unwrap_or(DEFAULT_INHERIT_STDIO) {
            ctx = ctx.inherit_stdio();
        }
        if self.inherit_env.unwrap_or(DEFAULT_INHERIT_ENV) {
            ctx = ctx.inherit_env()?;
        }
        for dir in &self.dirs {
            log::debug!("Preopening directory: {}", dir.display());
            let preopened = wasmtime_wasi::sync::Dir::open_ambient_dir(
                dir,
                wasmtime_wasi::sync::ambient_authority(),
            )
            .with_context(|| format!("failed to open directory: {}", dir.display()))?;
            ctx = ctx.preopened_dir(preopened, dir)?;
        }
        Ok(Some(ctx.build()))
    }

    /// Instantiate the module and call its initialization function.
    fn initialize(
        &self,
        store: &mut Store,
        module: &wasmtime::Module,
    ) -> anyhow::Result<(wasmtime::Instance, bool)> {
        log::debug!("Calling the initialization function");

        let mut linker = wasmtime::Linker::new(store.engine());

        if self.allow_wasi {
            wasmtime_wasi::add_to_linker(&mut linker, |ctx: &mut Option<WasiCtx>| {
                ctx.as_mut().unwrap()
            })?;
        }

        dummy_imports(&mut *store, &module, &mut linker)?;

        let instance = linker
            .instantiate(&mut *store, module)
            .context("failed to instantiate the Wasm module")?;

        let mut has_wasi_initialize = false;

        if let Some(export) = instance.get_export(&mut *store, "_initialize") {
            if let Extern::Func(func) = export {
                func.typed::<(), (), _>(&store)
                    .and_then(|f| {
                        has_wasi_initialize = true;
                        f.call(&mut *store, ()).map_err(Into::into)
                    })
                    .context("calling the Reactor initialization function")?;
            }
        }

        let init_func = instance
            .get_typed_func::<(), (), _>(&mut *store, &self.init_func)
            .expect("checked by `validate_init_func`");
        init_func
            .call(&mut *store, ())
            .with_context(|| format!("the `{}` function trapped", self.init_func))?;

        Ok((instance, has_wasi_initialize))
    }
}
