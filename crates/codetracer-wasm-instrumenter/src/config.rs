//! Pipeline configuration.

use serde::{Deserialize, Serialize};

use crate::hooks;

/// Per-run configuration of the instrumenter.
///
/// Defaults reflect the V1 behaviour described in
/// `Recording-Backends/WASM-Instrumentation-Layer.md`: all four
/// instrumentation passes are on, the host module is `__codetracer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Name of the host module that supplies the
    /// `__ct_emit_*` imports. Defaults to `"__codetracer"`.
    pub host_module_name: String,

    /// Whether to instrument `*.store` instructions.
    pub instrument_stores: bool,

    /// Whether to wrap calls to imported functions.
    pub instrument_imported_calls: bool,

    /// Whether to wrap entry/exit of exported functions.
    pub instrument_exported_functions: bool,

    /// Whether the boundary passes also capture the *values* that
    /// cross the boundary — every argument and every result, through
    /// the typed `__ct_emit_{i32,i64,f32,f64}` hooks.
    ///
    /// On by default, because a boundary log without values is not a
    /// re-execution input: the replayer cannot supply an import's
    /// return value it was never told (spec §§ 3.2, 6). The flag
    /// exists so a consumer that only wants the call *shape* (the
    /// M27 V1 event vocabulary) can ask for it, and so the two
    /// halves can be tested independently.
    #[serde(default = "default_capture_boundary_values")]
    pub capture_boundary_values: bool,
}

fn default_capture_boundary_values() -> bool {
    true
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            host_module_name: hooks::DEFAULT_HOST_MODULE.to_string(),
            instrument_stores: true,
            instrument_imported_calls: true,
            instrument_exported_functions: true,
            capture_boundary_values: true,
        }
    }
}

impl PipelineConfig {
    /// Convenience: turn off every instrumentation pass.
    pub fn none() -> Self {
        Self {
            host_module_name: hooks::DEFAULT_HOST_MODULE.to_string(),
            instrument_stores: false,
            instrument_imported_calls: false,
            instrument_exported_functions: false,
            capture_boundary_values: false,
        }
    }

    /// Parse from a TOML string. Used by the CLI's `--config`
    /// flag and by the bundler plugins.
    pub fn from_toml(source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(source)
    }
}
