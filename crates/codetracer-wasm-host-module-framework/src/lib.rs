//! Generic host-module instrumentation framework (M27 deliverable
//! #7).
//!
//! Replaces the hard-coded Stylus stubs in
//! `codetracer-wasm-recorder/internal/stylus/` with a
//! configuration-driven path. Given a target host module's import
//! list, produces a *pass-through host module description* that
//! logs every call (with arguments and return value) before
//! forwarding to the real module. M28 consumes this for the
//! Stylus migration.
//!
//! The framework outputs a structured [`PassThroughPlan`] that
//! embedders translate to their target language: the wazero
//! recorder generates Go stubs from the plan; a wasmtime / wasmi
//! embedder generates Rust closures; the browser embedder
//! generates JS shims.
//!
//! ## Configuration model
//!
//! The plan is driven by a small TOML schema:
//!
//! ```toml
//! [host_module]
//! name = "vm_hooks"
//! auto_correlation_markers = true
//!
//! [[function]]
//! name    = "read_args"
//! params  = ["i32"]
//! results = []
//! note    = "Stylus calldata read"
//! ```
//!
//! Each `[[function]]` entry describes one import. Param and result
//! types use the canonical WASM names: `i32`, `i64`, `f32`, `f64`,
//! `v128`. The framework validates the spec, then synthesises one
//! [`PassThroughFunction`] per entry with a deterministic "log call,
//! forward, log return" template.
//!
//! ## Correlation markers
//!
//! When `auto_correlation_markers = true` (per spec § 14.5), each
//! pass-through entry is decorated with a marker descriptor whose
//! `boundary_id` is `<module>.<function>` and whose key is the
//! arguments. Embedders feed these descriptors back into the M25
//! correlation-index machinery — no protocol-specific shim is
//! introduced.

#![deny(rust_2018_idioms, unused_must_use)]
#![warn(missing_docs)]

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Top-level pass-through plan: one host module's worth of
/// generated stubs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassThroughPlan {
    /// Host module name (e.g. `"vm_hooks"`, `"env"`).
    pub module: String,
    /// One entry per imported function.
    pub functions: Vec<PassThroughFunction>,
    /// Whether the framework should also emit M25 correlation
    /// markers at each crossing.
    #[serde(default)]
    pub auto_correlation_markers: bool,
}

/// One pass-through function entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PassThroughFunction {
    /// Function name as exposed by the import.
    pub name: String,
    /// Parameter types in WASM canonical form (`i32`, `i64`, …).
    pub params: Vec<WasmType>,
    /// Result types in WASM canonical form.
    pub results: Vec<WasmType>,
    /// Free-form annotation copied through verbatim.
    #[serde(default)]
    pub note: Option<String>,
    /// `boundary_id` for the auto-correlation marker (only set when
    /// `auto_correlation_markers = true` in the parent plan).
    #[serde(default)]
    pub boundary_id: Option<String>,
}

/// Canonical WASM value type names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WasmType {
    /// 32-bit integer.
    I32,
    /// 64-bit integer.
    I64,
    /// 32-bit float.
    F32,
    /// 64-bit float.
    F64,
    /// 128-bit SIMD vector.
    V128,
}

impl WasmType {
    /// Returns the canonical WASM type name string.
    pub fn as_str(&self) -> &'static str {
        match self {
            WasmType::I32 => "i32",
            WasmType::I64 => "i64",
            WasmType::F32 => "f32",
            WasmType::F64 => "f64",
            WasmType::V128 => "v128",
        }
    }
}

/// Input schema for the TOML driver.
#[derive(Debug, Clone, Deserialize)]
struct PlanSpec {
    host_module: HostModuleSection,
    #[serde(rename = "function", default)]
    functions: Vec<FunctionSection>,
}

#[derive(Debug, Clone, Deserialize)]
struct HostModuleSection {
    name: String,
    #[serde(default)]
    auto_correlation_markers: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct FunctionSection {
    name: String,
    #[serde(default)]
    params: Vec<String>,
    #[serde(default)]
    results: Vec<String>,
    #[serde(default)]
    note: Option<String>,
}

/// Parse a TOML config into a [`PassThroughPlan`].
pub fn plan_from_toml(source: &str) -> Result<PassThroughPlan> {
    let spec: PlanSpec = toml::from_str(source)?;
    let mut functions = Vec::with_capacity(spec.functions.len());
    for f in spec.functions {
        let params = parse_types(&f.params)?;
        let results = parse_types(&f.results)?;
        let boundary_id = if spec.host_module.auto_correlation_markers {
            Some(format!("{}.{}", spec.host_module.name, f.name))
        } else {
            None
        };
        functions.push(PassThroughFunction {
            name: f.name,
            params,
            results,
            note: f.note,
            boundary_id,
        });
    }
    Ok(PassThroughPlan {
        module: spec.host_module.name,
        functions,
        auto_correlation_markers: spec.host_module.auto_correlation_markers,
    })
}

fn parse_types(input: &[String]) -> Result<Vec<WasmType>> {
    input
        .iter()
        .map(|s| match s.as_str() {
            "i32" => Ok(WasmType::I32),
            "i64" => Ok(WasmType::I64),
            "f32" => Ok(WasmType::F32),
            "f64" => Ok(WasmType::F64),
            "v128" => Ok(WasmType::V128),
            other => Err(anyhow!("unknown WASM value type: {other}")),
        })
        .collect()
}

/// Embedder-side helper: render the plan as a deterministic
/// human-readable summary the recorder records for audit.
pub fn render_plan_summary(plan: &PassThroughPlan) -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
    let _ = writeln!(
        buf,
        "host-module: {} (auto_correlation_markers = {})",
        plan.module, plan.auto_correlation_markers
    );
    for f in &plan.functions {
        let params: Vec<&str> = f.params.iter().map(|t| t.as_str()).collect();
        let results: Vec<&str> = f.results.iter().map(|t| t.as_str()).collect();
        let _ = writeln!(
            buf,
            "  {} ({}) -> ({}){}{}",
            f.name,
            params.join(", "),
            results.join(", "),
            f.note
                .as_ref()
                .map(|n| format!("  ;; {n}"))
                .unwrap_or_default(),
            f.boundary_id
                .as_ref()
                .map(|b| format!("  ;; marker={b}"))
                .unwrap_or_default(),
        );
    }
    buf
}

/// Embedder-side helper: format a single in-process call as the
/// log line that `codetracer-wasm-recorder` would record. The
/// embedder feeds this into its trace writer; the test suite uses
/// it to assert the per-call format.
pub fn format_call_log(plan: &PassThroughPlan, function: &str, args: &[CallValue]) -> String {
    use std::fmt::Write as _;
    let mut buf = String::new();
    let _ = write!(buf, "call {}.{}", plan.module, function);
    let _ = write!(buf, "(");
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            let _ = write!(buf, ", ");
        }
        let _ = write!(buf, "{a}");
    }
    let _ = write!(buf, ")");
    buf
}

/// Value passed through the pass-through. Used by the unit tests
/// to confirm the framework's log format is stable across changes.
#[derive(Debug, Clone, PartialEq)]
#[allow(missing_docs)]
pub enum CallValue {
    I32(i32),
    I64(i64),
    F32(f32),
    F64(f64),
    V128([u8; 16]),
}

impl std::fmt::Display for CallValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallValue::I32(v) => write!(f, "i32:{v}"),
            CallValue::I64(v) => write!(f, "i64:{v}"),
            CallValue::F32(v) => write!(f, "f32:{v}"),
            CallValue::F64(v) => write!(f, "f64:{v}"),
            CallValue::V128(bs) => {
                write!(f, "v128:0x")?;
                for b in bs {
                    write!(f, "{b:02x}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_from_toml_minimal() {
        let src = r#"
            [host_module]
            name = "env"

            [[function]]
            name = "log"
            params = ["i32"]
            results = []
        "#;
        let plan = plan_from_toml(src).unwrap();
        assert_eq!(plan.module, "env");
        assert!(!plan.auto_correlation_markers);
        assert_eq!(plan.functions.len(), 1);
        assert_eq!(plan.functions[0].name, "log");
        assert_eq!(plan.functions[0].params, vec![WasmType::I32]);
        assert!(plan.functions[0].boundary_id.is_none());
    }

    #[test]
    fn plan_from_toml_with_auto_markers() {
        let src = r#"
            [host_module]
            name = "vm_hooks"
            auto_correlation_markers = true

            [[function]]
            name = "read_args"
            params = ["i32"]
            results = []

            [[function]]
            name = "storage_load_bytes32"
            params = ["i32", "i32"]
            results = ["i32"]
            note  = "EVM SLOAD"
        "#;
        let plan = plan_from_toml(src).unwrap();
        assert!(plan.auto_correlation_markers);
        assert_eq!(
            plan.functions[0].boundary_id.as_deref(),
            Some("vm_hooks.read_args")
        );
        assert_eq!(
            plan.functions[1].boundary_id.as_deref(),
            Some("vm_hooks.storage_load_bytes32")
        );
        assert_eq!(plan.functions[1].note.as_deref(), Some("EVM SLOAD"));
    }

    #[test]
    fn unknown_value_type_rejected() {
        let src = r#"
            [host_module]
            name = "env"

            [[function]]
            name = "log"
            params = ["i128"]
            results = []
        "#;
        let err = plan_from_toml(src).unwrap_err();
        assert!(err.to_string().contains("unknown WASM value type"));
    }

    #[test]
    fn render_plan_summary_is_stable() {
        let plan = PassThroughPlan {
            module: "env".into(),
            auto_correlation_markers: true,
            functions: vec![PassThroughFunction {
                name: "log".into(),
                params: vec![WasmType::I32],
                results: vec![],
                note: None,
                boundary_id: Some("env.log".into()),
            }],
        };
        let s = render_plan_summary(&plan);
        assert!(s.contains("host-module: env"));
        assert!(s.contains("marker=env.log"));
    }

    #[test]
    fn format_call_log_records_each_arg() {
        let plan = PassThroughPlan {
            module: "vm_hooks".into(),
            auto_correlation_markers: false,
            functions: vec![],
        };
        let s = format_call_log(
            &plan,
            "read_args",
            &[CallValue::I32(0x1000), CallValue::I32(32)],
        );
        assert_eq!(s, "call vm_hooks.read_args(i32:4096, i32:32)");
    }
}
