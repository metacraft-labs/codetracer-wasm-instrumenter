//! A second *real* WASM host, this one being V8.
//!
//! [`crate::runtime`] executes modules under `wasmi`, and for
//! everything it can run it is the better oracle: it is in-process,
//! it carries floats as exact bit patterns end to end, and it needs
//! no external program. It has one hard limit, and M35 ran into it.
//! `wasmi` 0.31 hard-codes `exceptions: false` in its engine
//! configuration and exposes no setter, so a module using the
//! exception-handling proposal is refused before it runs — with
//! "exceptions proposal not enabled" — and the whole class of modules
//! `-fwasm-exceptions` produces was therefore untestable.
//!
//! V8 has shipped the final exception-handling proposal (`try_table`,
//! `throw_ref`) and WasmGC (`br_on_cast`, `br_on_null`) for some
//! years, and the dev shell already carries `nodejs_22` for the
//! `recorder-runtime` and bundler-plugin suites. So this module
//! reaches the same `RuntimeRecording` through the same host surface,
//! by running the module under `node`. It is the *production* host
//! shape as well: spec § 1's whole point is that an instrumented
//! module runs inside an unmodified V8 in a browser page, which makes
//! this oracle closer to the deployment target than `wasmi` is, not
//! further from it.
//!
//! ## What it is and is not exact about
//!
//! The values the *hooks* observe are exact for every scalar type,
//! including NaN payloads and negative zero: the float hooks carry
//! IEEE bit patterns as integers (`__ct_emit_f32_bits` /
//! `__ct_emit_f64_bits`), never JS `Number`s, so nothing is
//! reinterpreted on the way out.
//!
//! The *export call's own* arguments and results do cross the
//! JavaScript boundary as `Number`s, and the WebAssembly JS API leaves
//! a NaN payload implementation-defined across that conversion — the
//! same limitation `recorder-runtime/host_runtime.js` documents for a
//! browser host. Every other value, including both signed zeros and
//! both infinities, survives. Tests that care about NaN payloads
//! belong on the `wasmi` oracle; tests that need exceptions or GC
//! belong here.
//!
//! ## Why shelling out is acceptable here
//!
//! `node` is declared in this repo's `flake.nix` and is what
//! `just test-runtime` and `just test-plugins` already run, so it is
//! not an ambient assumption about the machine. If it is missing,
//! [`run_module_under_v8`] returns an error naming it; it never
//! degrades into a skip, because a silently skipped oracle is how the
//! exception-handling gap survived a milestone in the first place.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use walrus::ValType;

use codetracer_wasm_instrumenter::hooks;

use crate::runtime::{ImportStub, RecordedValue, RuntimeEvent, RuntimeRecording};

/// The JavaScript host, embedded so the crate carries no path
/// assumptions about its own source tree.
const HOST_JS: &str = include_str!("v8_host.mjs");

// ---------------------------------------------------------------------------
// Wire types shared with `v8_host.mjs`
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "t")]
enum WireValue {
    #[serde(rename = "i32")]
    I32 { v: i32 },
    /// Decimal string: JSON has no 64-bit integer.
    #[serde(rename = "i64")]
    I64 { v: String },
    /// IEEE-754 bits.
    #[serde(rename = "f32")]
    F32 { v: u32 },
    /// IEEE-754 bits, as a decimal string.
    #[serde(rename = "f64")]
    F64 { v: String },
}

impl WireValue {
    fn from_recorded(v: RecordedValue) -> Self {
        match v {
            RecordedValue::I32(v) => WireValue::I32 { v },
            RecordedValue::I64(v) => WireValue::I64 { v: v.to_string() },
            RecordedValue::F32Bits(bits) => WireValue::F32 { v: bits },
            RecordedValue::F64Bits(bits) => WireValue::F64 {
                v: bits.to_string(),
            },
        }
    }

    fn into_recorded(self) -> Result<RecordedValue> {
        Ok(match self {
            WireValue::I32 { v } => RecordedValue::I32(v),
            WireValue::I64 { v } => RecordedValue::I64(
                v.parse::<i64>()
                    .with_context(|| format!("host returned a malformed i64: {v}"))?,
            ),
            WireValue::F32 { v } => RecordedValue::F32Bits(v),
            WireValue::F64 { v } => RecordedValue::F64Bits(
                v.parse::<u64>()
                    .with_context(|| format!("host returned malformed f64 bits: {v}"))?,
            ),
        })
    }
}

#[derive(Serialize, Debug)]
struct WireStub {
    module: String,
    name: String,
    results: Vec<Vec<WireValue>>,
    reentry: Option<WireReentry>,
}

#[derive(Serialize, Debug)]
struct WireReentry {
    #[serde(rename = "export")]
    export_name: String,
    args: Vec<WireValue>,
}

#[derive(Serialize, Debug)]
struct RunSpec {
    #[serde(rename = "hostModule")]
    host_module: String,
    #[serde(rename = "export")]
    export_name: String,
    args: Vec<WireValue>,
    #[serde(rename = "resultTypes")]
    result_types: Vec<String>,
    stubs: Vec<WireStub>,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "e")]
enum WireEvent {
    #[serde(rename = "call")]
    Call { fn_kind: i32, fn_index: u32 },
    #[serde(rename = "return")]
    Return { fn_kind: i32, fn_index: u32 },
    #[serde(rename = "realm")]
    Realm {
        direction: i32,
        fn_kind: i32,
        fn_index: u32,
        token: String,
    },
    #[serde(rename = "value")]
    Value { slot: i32, v: WireValue },
}

#[derive(Deserialize, Debug)]
struct RunOutput {
    ok: bool,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    events: Vec<WireEvent>,
    #[serde(default)]
    results: Vec<WireValue>,
    #[serde(default)]
    memory: Option<String>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Instantiate `wasm` under the host V8, call `export` with `args`,
/// and return everything observed — the same [`RuntimeRecording`]
/// [`crate::runtime::run_module`] returns, so the two oracles are
/// interchangeable in a test and can be compared against each other.
///
/// Works on instrumented and un-instrumented modules alike: the hook
/// imports are defined unconditionally and go unused when the module
/// does not import them, which is what lets the parity property ("the
/// instrumented module computes identically") be checked here too.
///
/// Errors rather than panics when `node` is missing or the module
/// traps, so a caller can assert on the failure.
pub fn run_module_under_v8(
    wasm: &[u8],
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) -> Result<RuntimeRecording> {
    match run_under_v8(wasm, export, args, stubs)? {
        Ok(recording) => Ok(recording),
        Err(failure) => bail!("V8 refused or trapped on the module: {}", failure.error),
    }
}

/// What a run produced when the module did **not** return normally:
/// the engine's own message, and the events that had already been
/// recorded when it stopped.
#[derive(Debug, Clone)]
pub struct FailedRun {
    /// V8's message — a trap, a validation refusal, or an escaping
    /// `WebAssembly.Exception`.
    pub error: String,
    /// The partial event stream. This is the interesting artefact when
    /// an exception unwinds past an open crossing: it is exactly what a
    /// replayer would be handed, and per spec § 6 it must be *refused*
    /// rather than replayed past.
    pub events: Vec<RuntimeEvent>,
}

/// Run `export` expecting it not to return normally, and hand back
/// V8's message together with the events recorded before it stopped.
///
/// Errors (rather than returning a [`FailedRun`]) if the module runs
/// to completion, so a test cannot pass by accident when the behaviour
/// it pins goes away.
pub fn run_module_under_v8_expecting_failure(
    wasm: &[u8],
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) -> Result<FailedRun> {
    match run_under_v8(wasm, export, args, stubs)? {
        Ok(recording) => bail!(
            "`{export}` was expected not to return normally, but it returned {:?}",
            recording.results
        ),
        Err(failure) => Ok(failure),
    }
}

/// The shared implementation: `Ok(Ok(_))` for a normal return,
/// `Ok(Err(_))` for a module V8 refused or that stopped early, and
/// `Err(_)` only for a failure of the harness itself.
fn run_under_v8(
    wasm: &[u8],
    export: &str,
    args: &[RecordedValue],
    stubs: &[ImportStub],
) -> Result<std::result::Result<RuntimeRecording, FailedRun>> {
    let result_types = export_result_types(wasm, export)?;

    let spec = RunSpec {
        host_module: hooks::DEFAULT_HOST_MODULE.to_string(),
        export_name: export.to_string(),
        args: args.iter().copied().map(WireValue::from_recorded).collect(),
        result_types,
        stubs: stubs
            .iter()
            .map(|s| WireStub {
                module: s.module.clone(),
                name: s.name.clone(),
                results: s
                    .results
                    .iter()
                    .map(|tuple| {
                        tuple
                            .iter()
                            .copied()
                            .map(WireValue::from_recorded)
                            .collect()
                    })
                    .collect(),
                reentry: s.reentry.as_ref().map(|(export, args)| WireReentry {
                    export_name: export.clone(),
                    args: args.iter().copied().map(WireValue::from_recorded).collect(),
                }),
            })
            .collect(),
    };

    let dir = scratch_dir()?;
    let wasm_path = dir.join("module.wasm");
    let spec_path = dir.join("run.json");
    let host_path = dir.join("v8_host.mjs");
    write_file(&wasm_path, wasm)?;
    write_file(&spec_path, serde_json::to_string(&spec)?.as_bytes())?;
    write_file(&host_path, HOST_JS.as_bytes())?;

    let node = std::env::var("NODE").unwrap_or_else(|_| "node".to_string());
    let output = Command::new(&node)
        .arg(&host_path)
        .arg(&wasm_path)
        .arg(&spec_path)
        .output()
        .with_context(|| {
            format!(
                "could not run `{node}`. The V8 oracle needs the `nodejs` this repo's \
                 flake.nix declares; enter the dev shell, or set NODE to an interpreter."
            )
        })?;

    let _ = std::fs::remove_dir_all(&dir);

    if !output.status.success() {
        bail!(
            "`{node}` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8(output.stdout).context("host printed non-UTF-8")?;
    let parsed: RunOutput = serde_json::from_str(&stdout)
        .with_context(|| format!("host printed something that is not a run result: {stdout}"))?;
    let ok = parsed.ok;
    let error = parsed.error.clone().unwrap_or_default();

    let events = parsed
        .events
        .into_iter()
        .map(|e| {
            Ok(match e {
                WireEvent::Call { fn_kind, fn_index } => RuntimeEvent::Call { fn_kind, fn_index },
                WireEvent::Return { fn_kind, fn_index } => {
                    RuntimeEvent::Return { fn_kind, fn_index }
                }
                WireEvent::Realm {
                    direction,
                    fn_kind,
                    fn_index,
                    token,
                } => RuntimeEvent::RealmBoundary {
                    direction,
                    fn_kind,
                    fn_index,
                    token: token
                        .parse::<i64>()
                        .with_context(|| format!("malformed correlation token: {token}"))?,
                },
                WireEvent::Value { slot, v } => RuntimeEvent::Value {
                    slot,
                    value: v.into_recorded()?,
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;

    if !ok {
        return Ok(Err(FailedRun { error, events }));
    }

    let results = parsed
        .results
        .into_iter()
        .map(WireValue::into_recorded)
        .collect::<Result<Vec<_>>>()?;

    let memory = parsed.memory.map(|hex| decode_hex(&hex)).transpose()?;

    Ok(Ok(RuntimeRecording {
        events,
        results,
        memory,
    }))
}

/// The declared result types of `export`, which the host needs in
/// order to give a JS `Number` back its WASM type.
fn export_result_types(wasm: &[u8], export: &str) -> Result<Vec<String>> {
    let module = walrus::Module::from_buffer(wasm).context("walrus could not parse the module")?;
    let func = module
        .exports
        .iter()
        .find(|e| e.name == export)
        .and_then(|e| match e.item {
            walrus::ExportItem::Function(id) => Some(id),
            _ => None,
        })
        .ok_or_else(|| anyhow!("module has no exported function `{export}`"))?;
    module
        .types
        .results(module.funcs.get(func).ty())
        .iter()
        .map(|ty| {
            Ok(match ty {
                ValType::I32 => "i32".to_string(),
                ValType::I64 => "i64".to_string(),
                ValType::F32 => "f32".to_string(),
                ValType::F64 => "f64".to_string(),
                other => bail!("`{export}` returns {other:?}, which this oracle cannot carry"),
            })
        })
        .collect()
}

fn scratch_dir() -> Result<PathBuf> {
    // A process-and-counter suffix rather than a `tempfile` dependency:
    // cargo runs test binaries concurrently, so the name has to be
    // unique per call, but nothing here needs the crate's other
    // guarantees.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ct-wasm-v8-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create scratch dir {}", dir.display()))?;
    Ok(dir)
}

fn write_file(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let mut f = std::fs::File::create(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

fn decode_hex(hex: &str) -> Result<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        bail!("odd-length memory dump");
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&hex[i..i + 2], 16).context("malformed byte in the memory dump")
        })
        .collect()
}
