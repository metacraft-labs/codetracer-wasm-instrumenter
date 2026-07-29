//! Instrumentation manifest for an instrumented WASM module.
//!
//! The runtime hooks the instrumenter injects report execution as bare
//! integers: `__ct_emit_call(fn_kind, fn_index)` names a function only
//! by its index in the export (or import) table. Something has to map
//! those indices back to human-meaningful names and source locations,
//! and the recording daemon has no access to the `.wasm` bytes — so the
//! instrumenter emits that table here, as a sidecar the embedder ships
//! to the daemon alongside the events.
//!
//! # Why the JS manifest shape
//!
//! The JSON shape below is deliberately identical to the one the
//! JavaScript instrumenter emits (`paths` / `functions` / `sites`, all
//! `camelCase`). A browser page that runs both instrumented JS and an
//! instrumented WASM module produces two recordings, and both are
//! consumed by the same decoder in
//! `codetracer/src/backend-manager/src/browser_stream_host.rs`
//! (`InstrumentationManifest`). One shape means one decoder, and it
//! means a WASM recording is an ordinary CodeTracer recording rather
//! than a special case that every downstream consumer has to learn.
//!
//! # Source attribution
//!
//! Function *names* are recovered automatically from the module's export
//! table (and, where present, the custom `name` section), so they need no
//! configuration. Source *paths* cannot be: a `.wasm` module built by a
//! release-profile toolchain generally carries no DWARF, and inventing a
//! path would be worse than admitting we do not have one. The embedder
//! therefore passes the source path it built the module from — the same
//! kind of build-time input as the output path. When it is omitted the
//! manifest falls back to the module's own filename, which still gives
//! the debugger a stable, non-fabricated identity to show.

use serde::Serialize;
use walrus::{FunctionId, Module, ValType};

use crate::dwarf::LineTable;
use crate::hooks::{FUNC_KIND_EXPORT, FUNC_KIND_IMPORT};

/// A WASM value type the typed `__ct_emit_*` hooks can carry.
///
/// Deliberately *not* `walrus::ValType`: the point of the type is to
/// name the subset that has a value hook, so that a signature holding
/// anything else is representable as "not capturable" rather than
/// silently truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ScalarType {
    /// 32-bit integer — `__ct_emit_i32`.
    I32,
    /// 64-bit integer — `__ct_emit_i64`.
    I64,
    /// 32-bit float — `__ct_emit_f32`.
    F32,
    /// 64-bit float — `__ct_emit_f64`.
    F64,
}

impl ScalarType {
    /// The matching walrus value type.
    pub fn val_type(self) -> ValType {
        match self {
            ScalarType::I32 => ValType::I32,
            ScalarType::I64 => ValType::I64,
            ScalarType::F32 => ValType::F32,
            ScalarType::F64 => ValType::F64,
        }
    }

    /// `Some(scalar)` for a capturable type, `None` otherwise.
    pub fn from_val_type(ty: ValType) -> Option<Self> {
        match ty {
            ValType::I32 => Some(ScalarType::I32),
            ValType::I64 => Some(ScalarType::I64),
            ValType::F32 => Some(ScalarType::F32),
            ValType::F64 => Some(ScalarType::F64),
            _ => None,
        }
    }

    /// Dense index used to key the per-type scratch-local pool.
    pub(crate) fn pool_slot(self) -> usize {
        match self {
            ScalarType::I32 => 0,
            ScalarType::I64 => 1,
            ScalarType::F32 => 2,
            ScalarType::F64 => 3,
        }
    }

    /// Inverse of [`Self::pool_slot`].
    pub(crate) fn from_pool_slot(slot: usize) -> Self {
        match slot {
            0 => ScalarType::I32,
            1 => ScalarType::I64,
            2 => ScalarType::F32,
            _ => ScalarType::F64,
        }
    }
}

/// The parameter and result types of one boundary function.
///
/// This is what makes the flat `__ct_emit_<t>(slot, value)` stream
/// decodable. The replayer of spec § 6 receives values as a sequence
/// of typed slots with no signature attached; without the arity it
/// cannot tell where the argument tuple ends and the result tuple
/// begins, and without the types it cannot rebuild an `f32` from an
/// `f64`-shaped JSON number. Recording the signature here means the
/// replayer never has to re-parse the `.wasm` to find out.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoundarySignature {
    /// Parameter types in declaration order. Empty when
    /// [`Self::unrepresentable`] is set.
    pub params: Vec<ScalarType>,
    /// Result types in declaration order. Empty when
    /// [`Self::unrepresentable`] is set.
    pub results: Vec<ScalarType>,
    /// `Some(type_name)` when the signature mentions a type no value
    /// hook can carry (`externref`, `funcref`, `v128`).
    ///
    /// The scalar vectors are then left *empty* rather than holding
    /// the capturable subset: a partial tuple would shift every slot
    /// index after the gap, which is worse than admitting the
    /// boundary cannot be recorded. Spec § 8 turns this into a hard
    /// rejection at instrumentation time.
    pub unrepresentable: Option<&'static str>,
}

impl BoundarySignature {
    /// Read the signature of `func_id` out of the module's type table.
    pub fn of_function(module: &Module, func_id: FunctionId) -> Self {
        let (params, results) = module.types.params_results(module.funcs.get(func_id).ty());
        let mut out = Self::default();
        for ty in params.iter().chain(results.iter()) {
            if ScalarType::from_val_type(*ty).is_none() {
                out.unrepresentable = Some(val_type_name(*ty));
                return out;
            }
        }
        out.params = params
            .iter()
            .filter_map(|t| ScalarType::from_val_type(*t))
            .collect();
        out.results = results
            .iter()
            .filter_map(|t| ScalarType::from_val_type(*t))
            .collect();
        out
    }
}

fn val_type_name(ty: ValType) -> &'static str {
    match ty {
        ValType::I32 => "i32",
        ValType::I64 => "i64",
        ValType::F32 => "f32",
        ValType::F64 => "f64",
        ValType::V128 => "v128",
        // Every reference type collapses to one diagnostic: the
        // distinction between `externref`, `funcref` and the GC heap
        // types does not change the answer, which is that no value
        // hook can transport a reference (spec § 8).
        ValType::Ref(_) => "a reference type",
    }
}

/// One entry of the manifest's `boundaries` table: the signature of a
/// single import or export edge, keyed by the same `(fn_kind,
/// fn_index)` pair the runtime hooks report.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestBoundary {
    /// `0` for an imported function, `1` for an exported one — the
    /// same vocabulary as the `fn_kind` hook argument.
    pub fn_kind: i32,
    /// Index within the import section (`fn_kind = 0`) or the export
    /// section (`fn_kind = 1`).
    pub fn_index: u32,
    /// Import field name, or export name.
    pub name: String,
    /// Import module name; empty for exports.
    pub module: String,
    /// Argument types, in the order their slots are emitted.
    pub params: Vec<ScalarType>,
    /// Result types, in the order their slots are emitted.
    pub results: Vec<ScalarType>,
    /// Set when the signature mentions a type the hooks cannot carry,
    /// in which case `params` and `results` are empty. Instrumenting
    /// such a module with value capture on is a hard error; the field
    /// exists so a manifest produced with capture *off* still says
    /// why the boundary has no signature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unsupported_type: Option<String>,
}

/// One entry of the manifest's `functions` table.
///
/// Field names mirror the JS instrumenter's `FunctionEntry`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestFunction {
    /// Human-readable function name, taken from the export name.
    pub name: String,
    /// Index into [`ModuleManifest::paths`].
    pub path_index: usize,
    /// 1-based source line, or `0` when no source attribution exists.
    pub line: i64,
    /// 0-based source column, or `0` when unknown.
    pub col: i64,
}

/// One entry of the manifest's `sites` table.
///
/// The instrumenter emits exactly one step site per instrumented export,
/// at the function's entry, so `sites[i]` describes the same function as
/// `functions[i]`. Keeping the arrays parallel is what lets the runtime
/// pass a single `export_index` as both the `siteId` of the `Step` event
/// and the `fnId` of the `Call` event.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManifestSite {
    /// Always `"step"` for the entry sites this instrumenter emits.
    pub kind: String,
    /// Index into [`ModuleManifest::paths`].
    pub path_index: usize,
    /// 1-based source line, or `0` when no source attribution exists.
    pub line: i64,
    /// 0-based source column, or `0` when unknown.
    pub col: i64,
    /// Index into [`ModuleManifest::functions`].
    pub fn_id: usize,
}

/// The sidecar manifest for one instrumented module.
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct ModuleManifest {
    /// Source paths referenced by the tables below.
    pub paths: Vec<String>,
    /// Functions, indexed by export index.
    pub functions: Vec<ManifestFunction>,
    /// Step sites, parallel to [`Self::functions`].
    pub sites: Vec<ManifestSite>,
    /// Signature of every import and export edge, so the replayer can
    /// decode the flat `__ct_emit_<t>(slot, value)` stream without
    /// re-parsing the `.wasm`.
    ///
    /// Unlike `functions` / `sites` this table is *not* index-parallel
    /// with anything — an entry is looked up by its `(fnKind,
    /// fnIndex)` pair, because the import and export index spaces are
    /// disjoint and both are sparse in it.
    pub boundaries: Vec<ManifestBoundary>,
}

impl ModuleManifest {
    /// Build the manifest for `module`.
    ///
    /// `source_path` is the source file the module was compiled from. It
    /// is recorded verbatim as the manifest's single path entry; pass
    /// `None` to fall back to `module_name`.
    ///
    /// The function table is indexed by **export index**, matching the
    /// `fn_index` argument the injected `__ct_emit_call` /
    /// `__ct_emit_realm_boundary` hooks pass at runtime for
    /// `fn_kind == FUNC_KIND_EXPORT`. Non-function exports still occupy
    /// their index — they are emitted with an empty name rather than
    /// skipped — because the runtime indexes into this array directly and
    /// a compacted table would silently misattribute every function after
    /// the first memory or table export.
    pub fn from_module(
        module: &Module,
        original_bytes: &[u8],
        source_path: Option<&str>,
        module_name: &str,
    ) -> Self {
        // Real source locations when the module carries DWARF; the
        // caller-supplied path otherwise. Reading DWARF matters because
        // without it every recorded WASM frame lands on line 0, which
        // makes the module addressable by function but not by
        // statement — an origin chain crossing into it has nowhere to
        // point.
        let (line_table, declarations) = LineTable::parse_all(original_bytes);
        // Say how much source attribution was recovered. A module built
        // without debug info records every frame at line 0, and the
        // symptom (a chain that reaches the module but points nowhere)
        // is otherwise indistinguishable from a bug in the recorder.
        if line_table.is_empty() && declarations.is_empty() {
            eprintln!(
                "ct-instrument: no DWARF line information in this module — \
                 recorded frames will name functions but not source lines"
            );
        } else {
            eprintln!(
                "ct-instrument: recovered {} DWARF line rows, {} function declarations",
                line_table.len(),
                declarations.len()
            );
        }

        let mut paths: Vec<String> = Vec::new();
        let path_index_of = |paths: &mut Vec<String>, candidate: &str| -> usize {
            if let Some(existing) = paths.iter().position(|p| p == candidate) {
                return existing;
            }
            paths.push(candidate.to_string());
            paths.len() - 1
        };
        // Slot 0 is always the fallback path, so a function DWARF says
        // nothing about still resolves somewhere real.
        let fallback = source_path.unwrap_or(module_name).to_string();
        path_index_of(&mut paths, &fallback);

        let mut functions = Vec::new();
        let mut sites = Vec::new();

        for export in module.exports.iter() {
            let (name, location) = match export.item {
                walrus::ExportItem::Function(func_id) => {
                    let func = module.funcs.get(func_id);
                    // Prefer the custom `name` section entry when the
                    // toolchain kept one (it carries the fully-qualified
                    // Rust path, e.g. `balance_calc::compute_balance`);
                    // fall back to the export name otherwise.
                    let name = func.name.clone().unwrap_or_else(|| export.name.clone());
                    // A function's declaration site is the right thing
                    // to show in a frame; the line table is the
                    // fallback, and reports whatever the first
                    // instruction belongs to — often an inlined callee
                    // in someone else's file.
                    let location = declarations
                        .lookup(&name)
                        .cloned()
                        .or_else(|| first_instruction_location(module, func_id, &line_table));
                    (name, location)
                }
                // Placeholder so indices stay aligned with the export
                // table the runtime reports against.
                _ => (String::new(), None),
            };

            let (path_index, line, col) = match location {
                Some(loc) => (path_index_of(&mut paths, &loc.file), loc.line, loc.column),
                None => (0, 0, 0),
            };

            let fn_id = functions.len();
            functions.push(ManifestFunction {
                name,
                path_index,
                line,
                col,
            });
            sites.push(ManifestSite {
                kind: "step".to_string(),
                path_index,
                line,
                col,
                fn_id,
            });
        }

        Self {
            paths,
            functions,
            sites,
            boundaries: collect_boundaries(module),
        }
    }

    /// Serialise to the JSON document the embedder ships to the daemon.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }
}

/// Build the boundary-signature table.
///
/// Import indices count *every* function import, including any the
/// instrumenter itself would add, so the numbering matches what the
/// runtime hooks report. The manifest is built before the hook imports
/// are registered, so in practice nothing is skipped here — the
/// filter exists for the case of a module that was already
/// instrumented once.
fn collect_boundaries(module: &Module) -> Vec<ManifestBoundary> {
    let mut out = Vec::new();
    let mut import_index = 0u32;
    for import in module.imports.iter() {
        if let walrus::ImportKind::Function(func_id) = import.kind {
            let is_hook = import.name.starts_with("__ct_emit_")
                || import.name == crate::hooks::HOOK_CORRELATION_TOKEN;
            if !is_hook {
                out.push(boundary_entry(
                    FUNC_KIND_IMPORT,
                    import_index,
                    import.name.clone(),
                    import.module.clone(),
                    BoundarySignature::of_function(module, func_id),
                ));
            }
            import_index += 1;
        }
    }
    for (index, export) in module.exports.iter().enumerate() {
        if let walrus::ExportItem::Function(func_id) = export.item {
            out.push(boundary_entry(
                FUNC_KIND_EXPORT,
                index as u32,
                export.name.clone(),
                String::new(),
                BoundarySignature::of_function(module, func_id),
            ));
        }
    }
    out
}

fn boundary_entry(
    fn_kind: i32,
    fn_index: u32,
    name: String,
    module: String,
    sig: BoundarySignature,
) -> ManifestBoundary {
    ManifestBoundary {
        fn_kind,
        fn_index,
        name,
        module,
        params: sig.params,
        results: sig.results,
        unsupported_type: sig.unrepresentable.map(str::to_string),
    }
}

/// Source position of a function's first instruction, per DWARF.
///
/// `InstrLocId` carries the instruction's offset in the original
/// bytecode, which is the address space DWARF's line program indexes —
/// so the first instruction that has one identifies where the function
/// body begins in the source.
fn first_instruction_location(
    module: &Module,
    func_id: walrus::FunctionId,
    line_table: &LineTable,
) -> Option<crate::dwarf::SourceLocation> {
    if line_table.is_empty() {
        return None;
    }
    let local = match &module.funcs.get(func_id).kind {
        walrus::FunctionKind::Local(local) => local,
        _ => return None,
    };
    let block = local.block(local.entry_block());
    for (_, loc) in block.instrs.iter() {
        if loc.is_default() {
            continue;
        }
        if let Some(found) = line_table.lookup(u64::from(loc.data())) {
            return Some(found.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A module with a non-function export between two function exports
    /// must keep the function indices aligned with the export table —
    /// otherwise every runtime `fn_index` after the memory export would
    /// resolve to the wrong function.
    #[test]
    fn manifest_indices_track_the_export_table_including_non_functions() {
        let wat = r#"
            (module
              (func $first (result i32) i32.const 1)
              (memory 1)
              (func $second (result i32) i32.const 2)
              (export "first" (func $first))
              (export "mem" (memory 0))
              (export "second" (func $second)))
        "#;
        let bytes = wat::parse_str(wat).expect("valid wat");
        let module = Module::from_buffer(&bytes).expect("parses");
        let manifest = ModuleManifest::from_module(&module, &bytes, Some("src/lib.rs"), "mod.wasm");

        assert_eq!(manifest.paths, vec!["src/lib.rs".to_string()]);
        assert_eq!(manifest.functions.len(), 3, "one entry per export");
        assert_eq!(manifest.functions[0].name, "first");
        assert_eq!(
            manifest.functions[1].name, "",
            "the memory export holds its slot with an empty name"
        );
        assert_eq!(manifest.functions[2].name, "second");
        // Sites stay parallel so one index serves as both siteId and fnId.
        assert_eq!(manifest.sites.len(), manifest.functions.len());
        assert_eq!(manifest.sites[2].fn_id, 2);
    }

    /// A module with no debug info must still produce a usable
    /// manifest — names without lines, rather than no manifest.
    #[test]
    fn manifest_without_dwarf_reports_functions_at_line_zero() {
        let wat = r#"
            (module
              (func $only (result i32) i32.const 7)
              (export "only" (func $only)))
        "#;
        let bytes = wat::parse_str(wat).expect("valid wat");
        let module = Module::from_buffer(&bytes).expect("parses");
        let manifest = ModuleManifest::from_module(&module, &bytes, Some("src/lib.rs"), "mod.wasm");
        assert_eq!(manifest.functions.len(), 1);
        assert_eq!(manifest.functions[0].name, "only");
        // No DWARF in a `wat`-assembled module, so the fallback path
        // applies: the caller-supplied source path, and no line.
        assert_eq!(manifest.paths, vec!["src/lib.rs".to_string()]);
        assert_eq!(manifest.functions[0].line, 0);
    }

    #[test]
    fn manifest_falls_back_to_the_module_name_without_a_source_path() {
        let bytes =
            wat::parse_str("(module (func $f) (export \"f\" (func $f)))").expect("valid wat");
        let module = Module::from_buffer(&bytes).expect("parses");
        let manifest = ModuleManifest::from_module(&module, &bytes, None, "balance_calc.wasm");
        assert_eq!(manifest.paths, vec!["balance_calc.wasm".to_string()]);
    }
}
