// vite-plugin-codetracer-wasm
//
// Routes every imported `.wasm` module through `ct-instrument`
// (the codetracer-wasm-instrumenter CLI). The plugin is a thin
// wrapper around the shared core; the real work happens in the
// Rust crate. HMR re-instrumentation works automatically because
// Vite's transform pipeline re-runs the plugin on file change.
//
// Spec: codetracer-specs/GUI/Debugging-Features/Value-Origin-Tracking.md
//       § 14.5; M27 deliverable #3.

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

const execFileAsync = promisify(execFile);

/** @typedef {Object} CodetracerWasmPluginOptions
 *  @property {string} [binary]   Path to the `ct-instrument` binary.
 *                                Falls back to the `CT_INSTRUMENT_BIN`
 *                                env var, then to `ct-instrument` on
 *                                $PATH.
 *  @property {string} [configPath] Path to a TOML config file.
 *  @property {RegExp} [include]   Path regex to instrument; defaults
 *                                 to `/\\.wasm$/`.
 */

/**
 * Vite plugin factory.
 * @param {CodetracerWasmPluginOptions} [options]
 */
export default function codetracerWasm(options = {}) {
  const binary =
    options.binary ?? process.env.CT_INSTRUMENT_BIN ?? "ct-instrument";
  const include = options.include ?? /\.wasm$/;
  const configPath = options.configPath;

  return {
    name: "vite-plugin-codetracer-wasm",
    enforce: "pre",
    async load(id) {
      if (!include.test(id)) {
        return null;
      }
      return await instrumentWasmFile(binary, configPath, id);
    },
  };
}

/**
 * Instrument a WASM file by shelling out to `ct-instrument`.
 * Exported for the unit test.
 * @param {string} binary
 * @param {string|undefined} configPath
 * @param {string} sourcePath
 * @returns {Promise<{ code: string }>}
 */
export async function instrumentWasmFile(binary, configPath, sourcePath) {
  const tmpdir = await fs.mkdtemp(path.join(os.tmpdir(), "ct-instrument-"));
  const out = path.join(tmpdir, path.basename(sourcePath));
  const args = [sourcePath, "-o", out];
  if (configPath) args.push("--config", configPath);
  await execFileAsync(binary, args);
  const bytes = await fs.readFile(out);
  await fs.rm(tmpdir, { recursive: true, force: true });
  // Vite expects a JS module that exports the bytes; this matches
  // the default WASM loader contract.
  const base64 = bytes.toString("base64");
  return {
    code:
      "export default Uint8Array.from(atob(" +
      JSON.stringify(base64) +
      "), c => c.charCodeAt(0));",
  };
}
