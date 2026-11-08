import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { test } from "node:test";
import { deflateRawSync } from "node:zlib";
import { APALACHE_VERSION, compileQuint, readZipEntry } from "../quint.ts";

// A zip with one entry per [name, data, deflate], laid out as jar tools write it.
function zip(entries: [string, string, boolean][]) {
  const locals: Buffer[] = [];
  const centrals: Buffer[] = [];
  let offset = 0;
  for (const [name, text, deflate] of entries) {
    const data = deflate ? deflateRawSync(Buffer.from(text)) : Buffer.from(text);
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(deflate ? 8 : 0, 8);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(text.length, 22);
    local.writeUInt16LE(name.length, 26);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(deflate ? 8 : 0, 10);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(text.length, 24);
    central.writeUInt16LE(name.length, 28);
    central.writeUInt32LE(offset, 42);
    locals.push(local, Buffer.from(name), data);
    centrals.push(central, Buffer.from(name));
    offset += 30 + name.length + data.length;
  }
  const directory = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, directory, end]);
}

test("readZipEntry reads stored and deflated entries", () => {
  const jar = zip([
    ["a.txt", "stored text", false],
    ["b/c.tla", "---- MODULE c ----\n====\n", true],
  ]);
  assert.equal(readZipEntry(jar, "a.txt")?.toString(), "stored text");
  assert.equal(readZipEntry(jar, "b/c.tla")?.toString(), "---- MODULE c ----\n====\n");
  assert.equal(readZipEntry(jar, "missing"), undefined);
});

test("compileQuint keeps the module after Apalache's log and adds the modules it extends", async () => {
  const home = fs.mkdtempSync(path.join(os.tmpdir(), "quint-home-"));
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "quint-out-"));
  const previous = process.env.QUINT_HOME;
  process.env.QUINT_HOME = home;
  try {
    const lib = path.join(home, `apalache-dist-${APALACHE_VERSION}`, "apalache", "lib");
    fs.mkdirSync(lib, { recursive: true });
    fs.writeFileSync(
      path.join(lib, "apalache.jar"),
      zip([
        ["tla2sany/StandardModules/Apalache.tla", "---- MODULE Apalache ----\n====\n", true],
        ["tla2sany/StandardModules/Variants.tla", "---- MODULE Variants ----\n====\n", false],
      ]),
    );
    const module = "------ MODULE counter ------\nEXTENDS Apalache, Variants\n====\n";
    const calls: string[][] = [];
    const file = await compileQuint("counter.qnt", dir, "counter", async (args) => {
      calls.push(args);
      return { status: 0, stdout: `# APALACHE version: ${APALACHE_VERSION}\n${module}`, stderr: "" };
    });
    assert.deepEqual(calls[0].slice(0, 6), [
      "compile",
      "--target",
      "tlaplus",
      "--apalache-version",
      APALACHE_VERSION,
      "--main",
    ]);
    assert.equal(path.basename(file), "counter.tla");
    assert.equal(fs.readFileSync(file, "utf8"), module);
    const folder = path.dirname(file);
    assert.deepEqual(fs.readdirSync(folder).sort(), ["Apalache.tla", "Variants.tla", "counter.tla"]);

    await assert.rejects(
      compileQuint("bad.qnt", dir, undefined, async () => ({
        status: 1,
        stdout: "",
        stderr: "Error [QNT000]: Couldn't unify int and str",
      })),
      /Quint could not compile bad\.qnt to TLA\+ \(exit 1\):\nError \[QNT000\]/,
    );
  } finally {
    if (previous === undefined) delete process.env.QUINT_HOME;
    else process.env.QUINT_HOME = previous;
    fs.rmSync(home, { recursive: true, force: true });
    fs.rmSync(dir, { recursive: true, force: true });
  }
});
