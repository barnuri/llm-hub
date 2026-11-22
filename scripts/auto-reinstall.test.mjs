import assert from "node:assert/strict";
import { copyFileSync, mkdirSync, mkdtempSync, readFileSync, rmSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { test } from "node:test";

function fixture(t, { healthy = false, newer = false, installFails = false, buildFails = false } = {}) {
  const root = mkdtempSync(join(tmpdir(), "llm-hub-reinstall-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  for (const dir of ["scripts", "src", "ui-src/src", "target/release"]) {
    mkdirSync(join(root, dir), { recursive: true });
  }
  copyFileSync(new URL("./auto-reinstall.sh", import.meta.url), join(root, "scripts/auto-reinstall.sh"));
  const executable = (path, body) => writeFileSync(join(root, path), `#!/bin/bash\n${body}\n`, { mode: 0o755 });
  executable("target/release/llm-hub", `
echo install >> "$TEST_ROOT/calls"
${installFails ? "exit 1" : ': > "$TEST_ROOT/healthy"'}
`);
  writeFileSync(join(root, "commands.sh"), `
curl() { test -f "$TEST_ROOT/healthy"; }
cargo() { echo build >> "$TEST_ROOT/calls"; return ${buildFails ? 1 : 0}; }
npm() { echo ui-build >> "$TEST_ROOT/calls"; }
sleep() { :; }
`);
  writeFileSync(join(root, "src/lib.rs"), "");
  utimesSync(join(root, "src/lib.rs"), 10, newer ? 30 : 10);
  utimesSync(join(root, "target/release/llm-hub"), 20, 20);
  if (healthy) {
    writeFileSync(join(root, "healthy"), "");
  }
  const run = () => spawnSync("bash", [join(root, "scripts/auto-reinstall.sh")], {
    env: { ...process.env, BASH_ENV: join(root, "commands.sh"), TEST_ROOT: root, AUTO_REINSTALL_DRY_RUN: "" },
    encoding: "utf8",
    timeout: 10000,
  });
  const log = () => readFileSync(join(root, "logs/auto-reinstall.log"), "utf8");
  const calls = () => readFileSync(join(root, "calls"), "utf8");
  return { run, log, calls, root };
}

test("healthy unchanged service needs no reinstall", (t) => {
  const f = fixture(t, { healthy: true });
  assert.equal(f.run().status, 0);
  assert.throws(f.calls, { code: "ENOENT" });
});

test("unhealthy unchanged service reinstalls without rebuilding", (t) => {
  const f = fixture(t);
  assert.equal(f.run().status, 0);
  assert.equal(f.calls(), "install\n");
  assert.match(f.log(), /healthz 200/);
});

test("changed sources rebuild before reinstalling", (t) => {
  const f = fixture(t, { healthy: true, newer: true });
  assert.equal(f.run().status, 0);
  assert.equal(f.calls(), "build\ninstall\n");
});

test("failed build does not reinstall", (t) => {
  const f = fixture(t, { healthy: true, newer: true, buildFails: true });
  assert.equal(f.run().status, 1);
  assert.equal(f.calls(), "build\n");
  assert.match(f.log(), /cargo build failed/);
});

test("failed install does not report healthy", (t) => {
  const f = fixture(t, { installFails: true });
  assert.equal(f.run().status, 1);
  assert.match(f.log(), /service install failed/);
  assert.doesNotMatch(f.log(), /healthz 200/);
});
