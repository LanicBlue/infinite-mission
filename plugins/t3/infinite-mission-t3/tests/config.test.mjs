import test from "node:test";
import assert from "node:assert/strict";
import os from "node:os";
import path from "node:path";
import { validateConfig, expandTilde } from "../lib/config.mjs";

test("defaults apply and a minimal config validates", () => {
  const result = validateConfig({ members: [{ id: "t3-codex", instance: "codex", model: "gpt-5.6-luna" }] });
  assert.equal(result.ok, true);
  const { config } = result;
  assert.equal(config.imBin, "im");
  assert.equal(config.receiveTimeoutSec, 600);
  assert.equal(config.rescanSec, 60);
  assert.equal(config.workspaces, null);
  assert.equal(config.members[0].runtimeMode, "full-access");
  assert.equal(config.members[0].enabled, true);
  assert.equal(config.t3.bin, "t3");
  assert.equal(config.t3.home, path.join(os.homedir(), ".t3"));
  assert.equal(config.t3.label, "im-t3-bridge");
});

test("member fields are enforced", () => {
  const base = { instance: "codex", model: "m" };
  assert.equal(validateConfig({ members: [{ ...base, id: "has space" }] }).ok, false);
  assert.equal(validateConfig({ members: [{ ...base, id: "../evil" }] }).ok, false);
  assert.equal(validateConfig({ members: [{ ...base, id: "ok", runtimeMode: "yolo" }] }).ok, false);
  assert.equal(validateConfig({ members: [{ ...base, id: "ok", options: [1] }] }).ok, false);
  const dup = validateConfig({
    members: [
      { ...base, id: "t3-codex" },
      { ...base, id: "t3-codex" },
    ],
  });
  assert.equal(dup.ok, false);
  assert.match(dup.errors.join("; "), /duplicate member id/);
  const missingModel = validateConfig({ members: [{ id: "t3-codex", instance: "codex" }] });
  assert.equal(missingModel.ok, false);
  assert.match(missingModel.errors.join("; "), /model/);
});

test("workspace entries must be absolute; null follows the im registry", () => {
  const result = validateConfig({
    members: [{ id: "a", instance: "codex", model: "m" }],
    workspaces: ["/Users/x/projects", "relative/path"],
  });
  assert.equal(result.ok, false);
  assert.match(result.errors.join("; "), /workspaces\[1\]/);

  const follows = validateConfig({
    members: [{ id: "a", instance: "codex", model: "m" }],
    workspaces: null,
  });
  assert.equal(follows.ok, true);
  assert.equal(follows.config.workspaces, null);
});

test("t3.bin accepts a command array (node + script path)", () => {
  const result = validateConfig({
    t3: { bin: ["node", "/opt/homebrew/lib/node_modules/t3/dist/bin.mjs"] },
    members: [{ id: "a", instance: "codex", model: "m" }],
  });
  assert.equal(result.ok, true);
  assert.deepEqual(result.config.t3.bin, ["node", "/opt/homebrew/lib/node_modules/t3/dist/bin.mjs"]);
});

test("non-object root is rejected without throwing", () => {
  assert.equal(validateConfig(null).ok, false);
  assert.equal(validateConfig("nope").ok, false);
  assert.equal(validateConfig([1]).ok, false);
});

test("expandTilde", () => {
  assert.equal(expandTilde("~"), os.homedir());
  assert.equal(expandTilde("~/.t3"), path.join(os.homedir(), ".t3"));
  assert.equal(expandTilde("/abs"), "/abs");
});
