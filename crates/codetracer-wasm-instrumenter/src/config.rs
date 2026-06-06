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
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            host_module_name: hooks::DEFAULT_HOST_MODULE.to_string(),
            instrument_stores: true,
            instrument_imported_calls: true,
            instrument_exported_functions: true,
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
        }
    }

    /// Parse from a TOML string. Used by the CLI's `--config`
    /// flag and by the bundler plugins.
    pub fn from_toml(source: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(source)
    }
}
