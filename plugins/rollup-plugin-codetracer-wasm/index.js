// rollup-plugin-codetracer-wasm
//
// Rollup plugin. Mirrors the Vite shape (Vite is built on
// Rollup); the differences are cosmetic.
//
// Spec: M27 deliverable #3.

import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import * as path from "node:path";
import * as os from "node:os";

const execFileAsync = promisify(execFile);

export async function instrumentWasmFile(binary, configPath, sourcePath) {
  const tmpdir = await fs.mkdtemp(path.join(os.tmpdir(), "ct-instrument-rollup-"));
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
 * @param {{ binary?: string, configPath?: string }} [options]
 */
export default function codetracerWasm(options = {}) {
  const binary = options.binary ?? process.env.CT_INSTRUMENT_BIN ?? "ct-instrument";
  const configPath = options.configPath;
  return {
    name: "rollup-plugin-codetracer-wasm",
    async load(id) {
      if (!/\.wasm$/.test(id)) return null;
      const bytes = await instrumentWasmFile(binary, configPath, id);
      const b64 = bytes.toString("base64");
      return (
        "export default Uint8Array.from(atob(" +
        JSON.stringify(b64) +
        "), c => c.charCodeAt(0));"
      );
    },
  };
}
