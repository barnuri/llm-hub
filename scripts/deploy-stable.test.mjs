import assert from "node:assert/strict";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readlinkSync, readdirSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { spawnSync } from "node:child_process";
import { test } from "node:test";

const git = (cwd, ...args) => {
  const r = spawnSync("git", args, { cwd, encoding: "utf8" });
  assert.equal(r.status, 0, r.stderr);
  return r.stdout.trim();
};

function fixture(t, { checks = "success" } = {}) {
  const root = mkdtempSync(join(tmpdir(), "llm-hub-deploy-test-"));
  t.after(() => rmSync(root, { recursive: true, force: true }));
  const origin = join(root, "origin.git");
  const seed = join(root, "seed");
  const clone = join(root, "clone");
  const releases = join(root, "data/releases");
  mkdirSync(seed);
  git(root, "init", "-q", "--bare", "-b", "master", origin);
  git(seed, "init", "-q", "-b", "master");
  git(seed, "config", "user.email", "t@example.com");
  git(seed, "config", "user.name", "t");
  writeFileSync(join(seed, "a"), "1");
  git(seed, "add", ".");
  git(seed, "commit", "-qm", "one");
  git(seed, "remote", "add", "origin", origin);
  git(seed, "push", "-q", "origin", "master");
  git(root, "clone", "-q", origin, clone);
  const commit = (name) => {
    writeFileSync(join(seed, name), name);
    git(seed, "add", ".");
    git(seed, "commit", "-qm", name);
    git(seed, "push", "-q", "origin", "master");
  };
  writeFileSync(join(root, "commands.sh"), `
gh() { echo "$CHECKS"; }
cargo() {
  echo cargo >> "$TEST_ROOT/calls"
  mkdir -p target/release
  cat > target/release/llm-hub <<'BIN'
#!/bin/bash
: > "$TEST_ROOT/installed"
if [ -f "$TEST_ROOT/unhealthy" ]; then rm -f "$TEST_ROOT/healthy"; else : > "$TEST_ROOT/healthy"; fi
BIN
  chmod +x target/release/llm-hub
}
npm() { echo npm >> "$TEST_ROOT/calls"; }
curl() { test -f "$TEST_ROOT/healthy"; }
sleep() { :; }
`);
  const run = (...args) => spawnSync("bash", [new URL("./deploy-stable.sh", import.meta.url).pathname, "--clone", clone, "--releases", releases, ...args], {
    env: { ...process.env, BASH_ENV: join(root, "commands.sh"), TEST_ROOT: root, CHECKS: checks, LLM_HUB_HEALTH_WAIT: "2", CARGO_TARGET_DIR: "" },
    encoding: "utf8",
    timeout: 20000,
  });
  const calls = () => (existsSync(join(root, "calls")) ? readFileSync(join(root, "calls"), "utf8") : "");
  const sha = () => git(seed, "rev-parse", "HEAD");
  return { run, calls, commit, sha, releases, root };
}

test("dry run changes nothing and prints the plan", (t) => {
  const f = fixture(t);
  const r = f.run("--dry-run");
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /would run: GIT_SHA=.* cargo build --release --locked/);
  assert.equal(f.calls(), "");
  assert.equal(existsSync(join(f.releases, "current")), false);
});

test("deploys a green ref, links current and installs", (t) => {
  const f = fixture(t);
  const r = f.run();
  assert.equal(r.status, 0, r.stdout + r.stderr);
  assert.equal(readlinkSync(join(f.releases, "current")), join(f.releases, f.sha()));
  assert.ok(existsSync(join(f.releases, f.sha(), "llm-hub")));
  assert.ok(existsSync(join(f.root, "installed")));
  assert.match(r.stdout, /healthz 200/);
});

test("skips a sha that is already deployed", (t) => {
  const f = fixture(t);
  assert.equal(f.run().status, 0);
  const r = f.run();
  assert.equal(r.status, 0);
  assert.match(r.stdout, /already deployed/);
  assert.equal(f.calls().match(/cargo/g).length, 1);
});

test("refuses a ref whose checks are not green, unless --skip-ci", (t) => {
  const f = fixture(t, { checks: "success,failure" });
  const r = f.run();
  assert.equal(r.status, 1);
  assert.match(r.stdout, /not green/);
  assert.equal(f.calls(), "");
  assert.equal(f.run("--skip-ci").status, 0);
});

test("rolls back to the previous release when healthz fails", (t) => {
  const f = fixture(t);
  assert.equal(f.run().status, 0);
  const first = f.sha();
  f.commit("b");
  writeFileSync(join(f.root, "unhealthy"), "");
  rmSync(join(f.root, "healthy"));
  const r = f.run();
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.equal(readlinkSync(join(f.releases, "current")), realpathSync(join(f.releases, first)));
  assert.match(r.stdout, /rolled back/);
});

test("keeps the last 3 releases", (t) => {
  const f = fixture(t);
  for (const name of ["b", "c", "d", "e"]) {
    assert.equal(f.run().status, 0);
    f.commit(name);
  }
  assert.equal(f.run().status, 0);
  const kept = readdirSync(f.releases).filter((n) => /^[0-9a-f]{40}$/.test(n));
  assert.ok(kept.length <= 3, kept.join(","));
  assert.ok(kept.includes(f.sha()));
});

test("refuses without --clone", () => {
  const r = spawnSync("bash", [new URL("./deploy-stable.sh", import.meta.url).pathname], { encoding: "utf8" });
  assert.equal(r.status, 2);
});
