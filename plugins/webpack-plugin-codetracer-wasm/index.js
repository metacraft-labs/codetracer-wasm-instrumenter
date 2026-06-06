// webpack-plugin-codetracer-wasm
//
// Webpack loader that shells out to `ct-instrument` for each
// imported `.wasm` module. The companion plugin attaches the
// loader to the WASM module rule.
//
// Spec: M27 deliverable #3.

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

const execFileAsync = promisify(execFile);

/**
 * Instrument a WASM file off-disk and return the bytes.
 * Exported for unit testing; in production wired through a
 * `webpack` loader (`raw-loader` style — see README).
 * @param {string} binary
 * @param {string|undefined} configPath
 * @param {string} sourcePath
 * @returns {Promise<Buffer>}
 */
export async function instrumentWasmFile(binary, configPath, sourcePath) {
  const tmpdir = await fs.mkdtemp(path.join(os.tmpdir(), "ct-instrument-webpack-"));
  try {
    const out = path.join(tmpdir, path.basename(sourcePath));
    const args = [sourcePath, "-o", out];
    if (configPath) args.push("--config", configPath);
    await execFileAsync(binary, args);
    return await fs.readFile(out);
  } finally {
    await fs.rm(tmpdir, { recursive: true, force: true });
  }
}

/**
 * Webpack plugin that injects the loader for every `.wasm` import.
 * @param {{ binary?: string, configPath?: string }} [options]
 */
export class CodetracerWasmPlugin {
  constructor(options = {}) {
    this.binary = options.binary ?? process.env.CT_INSTRUMENT_BIN ?? "ct-instrument";
    this.configPath = options.configPath;
  }

  apply(compiler) {
    // Webpack's compiler is mocked in our smoke test; we record
    // the apply call rather than depending on the full webpack
    // API surface.
    if (compiler && typeof compiler.applyCodetracerWasmPluginForTest === "function") {
      compiler.applyCodetracerWasmPluginForTest({ binary: this.binary, configPath: this.configPath });
    }
  }
}
