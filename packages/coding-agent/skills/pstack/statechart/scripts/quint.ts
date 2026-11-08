import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { inflateRawSync } from "node:zlib";

// Quint compiles to TLA+ through an Apalache server, so it needs Java. Its output EXTENDS the
// Apalache and Variants modules, which TLC lacks; they are read from the Apalache jar that Quint
// downloaded, so they always match the operators Quint wrote.
export const APALACHE_VERSION = "0.62.4";
const APALACHE_MODULES = ["Apalache", "Variants"];

// One stored or deflated entry of a zip file, found through its central directory.
export function readZipEntry(zip: Buffer, name: string) {
  const end = zip.lastIndexOf(Buffer.from([0x50, 0x4b, 0x05, 0x06]));
  if (end === -1) throw new Error("not a zip file");
  const count = zip.readUInt16LE(end + 10);
  let at = zip.readUInt32LE(end + 16);
  for (let i = 0; i < count; i++) {
    const method = zip.readUInt16LE(at + 10);
    const size = zip.readUInt32LE(at + 20);
    const nameLength = zip.readUInt16LE(at + 28);
    const skip = nameLength + zip.readUInt16LE(at + 30) + zip.readUInt16LE(at + 32);
    const local = zip.readUInt32LE(at + 42);
    if (zip.toString("utf8", at + 46, at + 46 + nameLength) === name) {
      const start = local + 30 + zip.readUInt16LE(local + 26) + zip.readUInt16LE(local + 28);
      const data = zip.subarray(start, start + size);
      if (method === 0) return data;
      if (method === 8) return inflateRawSync(data);
      throw new Error(`${name} uses zip method ${method}`);
    }
    at += 46 + skip;
  }
  return undefined;
}

const MODULE_LINE = /^-{4,}\s*MODULE\s+(\w+)\s*-{4,}\s*$/m;

// Writes the TLA+ module that Quint compiles SPEC.qnt to into `dir`, with the Apalache modules it
// extends, and returns its path. `run` runs Quint with the given arguments in `cwd`.
export async function compileQuint(
  specFile: string,
  dir: string,
  main: string | undefined,
  run: (args: string[], cwd: string) => Promise<{ status: number | null; stdout: string; stderr: string }>,
) {
  const work = fs.mkdtempSync(path.join(dir, "quint-"));
  const r = await run(
    [
      "compile",
      "--target",
      "tlaplus",
      "--apalache-version",
      APALACHE_VERSION,
      ...(main ? ["--main", main] : []),
      path.resolve(specFile),
    ],
    work,
  );
  // Apalache's server log shares stdout with the module, so the module starts at its header.
  const start = r.stdout.search(MODULE_LINE);
  if (r.status !== 0 || start === -1)
    throw new Error(
      `Quint could not compile ${specFile} to TLA+ (exit ${r.status}):\n${(r.stderr + r.stdout).trim().split("\n").slice(-30).join("\n")}`,
    );
  const text = r.stdout.slice(start);
  const module = (MODULE_LINE.exec(text) as RegExpExecArray)[1];
  const out = path.join(dir, "quint-tla");
  fs.mkdirSync(out);
  const home = process.env.QUINT_HOME || path.join(os.homedir(), ".quint");
  const jarFile = path.join(home, `apalache-dist-${APALACHE_VERSION}`, "apalache", "lib", "apalache.jar");
  if (!fs.existsSync(jarFile))
    throw new Error(
      `Cannot find ${jarFile}, which holds the Apalache and Variants modules of Quint's TLA+. Quint downloads it when no Apalache server runs on port 8822; stop that server and run again.`,
    );
  const jar = fs.readFileSync(jarFile);
  for (const name of APALACHE_MODULES) {
    const entry = readZipEntry(jar, `tla2sany/StandardModules/${name}.tla`);
    if (!entry) throw new Error(`${jarFile} has no ${name}.tla.`);
    fs.writeFileSync(path.join(out, `${name}.tla`), entry);
  }
  const file = path.join(out, `${module}.tla`);
  fs.writeFileSync(file, text);
  return file;
}
