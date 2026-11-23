import assert from "node:assert/strict";
import test from "node:test";

import { engineBadgeLabel, nextEngineSwitch } from "./localEngine.ts";

test("engine badge names llama and mistral.rs", () => {
  assert.equal(engineBadgeLabel("llama"), "llama");
  assert.equal(engineBadgeLabel("mistralrs"), "mistral.rs");
});

test("engine switch keeps the selected engine and rejects invalid or unchanged values", () => {
  assert.equal(nextEngineSwitch("llama", "llama"), null);
  assert.equal(nextEngineSwitch("llama", "mistralrs"), "mistralrs");
  assert.equal(nextEngineSwitch("mistralrs", "unknown"), null);
});
