//! Pipeline configuration.

use serde::{Deserialize, Serialize};

use crate::hooks;

/// Per-run configuration of the instrumenter.
///
/// Defaults are the **boundary-only** model of
/// `Recording-Backends/WASM-Instrumentation-Layer.md` §§ 1–3: both
/// boundary passes on, values captured, the interior store pass off,
/// and the host module `__codetracer`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// Name of the host module that supplies the
    /// `__ct_emit_*` imports. Defaults to `"__codetracer"`.
    pub host_module_name: String,

    /// Whether to instrument `*.store` instructions — the withdrawn
    /// V1 interior model.
    ///
    /// **Off by default and not part of any production path.** Spec
    /// §§ 2 and 11 withdraw the interior model on two independent
    /// grounds: it cannot be complete (locals and operand-stack
    /// values have no address, so a store hook cannot see them, and
    /// *which* values reach memory at all is decided by the
    /// optimiser), and it was measured at **+2955 %** runtime with
    /// recording hooks against **+11 %** for boundary capture.
    ///
    /// The pass itself is correct and stays reachable behind this
    /// flag for experiments. Before turning it on, re-read spec
    /// § 2.2: 33 ns/event is the WASM→JS call boundary itself, so no
    /// amount of making the hook cheaper recovers the cost.
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
            // Boundary-only (spec §§ 2, 11). See the field docs.
            instrument_stores: false,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// M36 / spec § 11: the default rewrite is boundary-only.
    ///
    /// This is a guard against a silent regression, not a restatement
    /// of the constructor. "Instrument the stores" *looks* like the
    /// direct route to a trace, so the withdrawn model has already
    /// been re-specified once; the spec's § 11 anti-drift section
    /// exists for the same reason this assertion does. Flipping the
    /// default back would turn every default rewrite into the model
    /// measured at +2955 % runtime — and would do it invisibly,
    /// because a store-instrumented module still runs and still
    /// records *something*.
    #[test]
    fn verify_default_config_is_boundary_only() {
        let config = PipelineConfig::default();

        assert!(
            !config.instrument_stores,
            "the interior store pass is withdrawn (spec §§ 2, 11) and must stay off by default"
        );

        // The other half of "boundary-only": the boundary passes are
        // what carries the recording, so an over-eager retirement that
        // switched everything off would leave a module reporting
        // nothing at all. Asserted here so the two halves cannot drift
        // apart.
        assert!(
            config.instrument_imported_calls,
            "the import edge is the recording's non-determinism (spec § 3.2)"
        );
        assert!(
            config.instrument_exported_functions,
            "the export edge is the recording's entry point (spec § 3.1)"
        );
        assert!(
            config.capture_boundary_values,
            "a boundary log without values is not a re-execution input (spec § 6)"
        );
    }

    /// Turning the experiment back on stays a one-field change: the
    /// pass is retired from the default path, not deleted (spec § 11).
    #[test]
    fn the_store_pass_remains_reachable_behind_the_flag() {
        let config = PipelineConfig {
            instrument_stores: true,
            ..PipelineConfig::default()
        };
        assert!(config.instrument_stores);
    }
}
