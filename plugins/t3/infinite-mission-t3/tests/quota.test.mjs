import test, { beforeEach } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createCipheriv, createHash, randomBytes } from "node:crypto";
import {
  QuotaGate,
  credentialFallbackSecret,
  decryptCredentialValue,
  parseQuotaWindow,
  formatResetMs,
} from "../lib/quota.mjs";
import { validateConfig } from "../lib/config.mjs";

const SECRET = "test-secret";
const TOKEN = "tok-" + "x".repeat(40);

function encrypt(value, secret = SECRET) {
  const key = createHash("sha256").update(secret).digest();
  const iv = randomBytes(12);
  const cipher = createCipheriv("aes-256-gcm", key, iv);
  const encrypted = Buffer.concat([cipher.update(value, "utf8"), cipher.final()]);
  return ["enc:v1:", iv.toString("base64url"), ".", cipher.getAuthTag().toString("base64url"), ".", encrypted.toString("base64url")].join("");
}

function quotaBody(pct, resetAt, unit = 3) {
  return { data: { limits: [
    { type: "TIME_LIMIT", unit: 5, percentage: 1, nextResetTime: resetAt + 9_000_000 },
    { type: "TOKENS_LIMIT", unit, percentage: pct, nextResetTime: resetAt },
  ] } };
}

function okResponse(body) {
  return { ok: true, status: 200, json: async () => body };
}

function writeCredentials(entries) {
  const file = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "im-quota-test-")), "credentials.json");
  fs.writeFileSync(file, JSON.stringify(entries));
  return file;
}

function makeGate({ fetchImpl, now, credentialsFile, ...rest } = {}) {
  return new QuotaGate({
    credentialsFile: credentialsFile ?? writeCredentials({ "oauth:bigmodel:access_token": encrypt(TOKEN) }),
    fetchImpl,
    now: now ?? (() => NOW),
    env: { ZCODE_CREDENTIAL_SECRET: SECRET },
    logger: null,
    ...rest,
  });
}

const BASE_NOW = 1_000_000;
let NOW = BASE_NOW;
const RESET_AT = BASE_NOW + 3_600_000;

// Tests mutate the fake clock; every test starts from the same instant.
beforeEach(() => {
  NOW = BASE_NOW;
});

test("decryptCredentialValue round-trips the CLI's enc:v1 scheme and passes plaintext through", () => {
  assert.equal(decryptCredentialValue(encrypt(TOKEN), SECRET), TOKEN);
  assert.equal(decryptCredentialValue("plain-value", SECRET), "plain-value");
  assert.throws(() => decryptCredentialValue("enc:v1:garbage", SECRET), /invalid ciphertext/);
  assert.throws(() => decryptCredentialValue(encrypt(TOKEN, "other-secret"), SECRET), /key mismatch|decrypt/i);
});

test("credentialFallbackSecret prefers the env override, else the machine facts", () => {
  assert.equal(credentialFallbackSecret({ ZCODE_CREDENTIAL_SECRET: " s p " }), "s p");
  assert.equal(
    credentialFallbackSecret({}, { platform: "darwin", homedir: "/Users/x", username: "x" }),
    "zcode-credential-fallback:darwin:/Users/x:x",
  );
});

test("parseQuotaWindow picks the 5h TOKENS_LIMIT window", () => {
  assert.deepEqual(parseQuotaWindow(quotaBody(28, RESET_AT)), { pct: 28, resetAt: RESET_AT });
  assert.equal(parseQuotaWindow({ data: { limits: [] } }), null);
  assert.equal(parseQuotaWindow({ data: { limits: [{ type: "TOKENS_LIMIT", unit: 3, percentage: 5 }] } }), null);
  assert.equal(parseQuotaWindow(null), null);
});

test("formatResetMs renders local time", () => {
  const fixed = new Date("2026-10-10T12:34:00");
  const out = formatResetMs(fixed.getTime());
  assert.match(out, /12:34/);
});

test("exhausted() gates at/over the threshold before reset+buffer", async () => {
  const gate = makeGate({ fetchImpl: async () => okResponse(quotaBody(100, RESET_AT)) });
  const verdict = await gate.exhausted();
  assert.equal(verdict.gated, true);
  assert.equal(verdict.pct, 100);
  assert.equal(verdict.resetAt, RESET_AT);
});

test("exhausted() does not gate below the threshold", async () => {
  const gate = makeGate({ fetchImpl: async () => okResponse(quotaBody(99, RESET_AT)) });
  assert.equal((await gate.exhausted()).gated, false);
});

test("a configurable threshold can pre-empt the window", async () => {
  const gate = makeGate({ fetchImpl: async () => okResponse(quotaBody(28, RESET_AT)), thresholdPct: 20 });
  assert.equal((await gate.exhausted()).gated, true);
});

test("past reset+buffer the cached verdict is stale — one refetch un-gates", async () => {
  let calls = 0;
  const gate = makeGate({
    fetchImpl: async () => { calls += 1; return okResponse(quotaBody(100, RESET_AT)); },
    pollSec: 600, // far beyond the reset — only the staleness bypass may refetch
  });
  assert.equal((await gate.exhausted()).gated, true);
  assert.equal((await gate.exhausted()).gated, true); // still cached, one fetch
  assert.equal(calls, 1);
  NOW = RESET_AT + 61_000; // buffer is 60s
  // The refetch answers with a fresh, healthy window.
  const verdict = await gate.exhausted();
  assert.equal(calls, 2);
  assert.equal(verdict.gated, false);
});

test("fetch failures fail open and retry on a short TTL", async () => {
  let fail = true;
  let calls = 0;
  const gate = makeGate({
    fetchImpl: async () => { calls += 1; if (fail) throw new Error("network down"); return okResponse(quotaBody(100, RESET_AT)); },
    pollSec: 600,
  });
  const verdict = await gate.exhausted();
  assert.equal(verdict.gated, false);
  assert.match(verdict.reason, /network down/);
  NOW += 31_000; // past UNKNOWN_RETRY_MS
  fail = false;
  assert.equal((await gate.exhausted()).gated, true);
  assert.equal(calls, 2);
});

test("non-200 and malformed bodies fail open", async () => {
  const http = makeGate({ fetchImpl: async () => ({ ok: false, status: 401, json: async () => ({}) }) });
  assert.equal((await http.exhausted()).gated, false);
  const malformed = makeGate({ fetchImpl: async () => okResponse({ data: {} }) });
  assert.equal((await malformed.exhausted()).gated, false);
});

test("the Authorization header carries the decrypted credential token", async () => {
  let seenAuth = null;
  const gate = makeGate({ fetchImpl: async (url, options) => {
    seenAuth = options.headers.Authorization;
    return okResponse(quotaBody(10, RESET_AT));
  } });
  await gate.exhausted();
  assert.equal(seenAuth, TOKEN);
});

test("undecryptable credentials fail open", async () => {
  const file = writeCredentials({ "oauth:bigmodel:access_token": encrypt(TOKEN, "wrong-secret") });
  const gate = makeGate({ credentialsFile: file, fetchImpl: async () => { throw new Error("must not fetch"); } });
  const verdict = await gate.exhausted();
  assert.equal(verdict.gated, false);
  assert.match(verdict.reason, /credentials/);
});

test("a missing credentials file fails open", async () => {
  const gate = makeGate({ credentialsFile: "/nonexistent/im-quota-test/credentials.json", fetchImpl: async () => okResponse(quotaBody(100, RESET_AT)) });
  assert.equal((await gate.exhausted()).gated, false);
});

test("concurrent exhausted() calls share one fetch (single flight)", async () => {
  let calls = 0;
  const gate = makeGate({
    fetchImpl: async () => { calls += 1; await new Promise((r) => setTimeout(r, 20)); return okResponse(quotaBody(50, RESET_AT)); },
  });
  const verdicts = await Promise.all([gate.exhausted(), gate.exhausted(), gate.exhausted()]);
  assert.equal(calls, 1);
  assert.ok(verdicts.every((v) => v.gated === false));
});

test("transition logging: first snapshot, exhaustion, and recovery", async () => {
  const lines = [];
  const logger = (tag, message) => lines.push(`${tag}: ${message}`);
  let pct = 30;
  const gate = new QuotaGate({
    credentialsFile: writeCredentials({ "oauth:bigmodel:access_token": encrypt(TOKEN) }),
    fetchImpl: async () => okResponse(quotaBody(pct, RESET_AT)),
    now: () => NOW,
    env: { ZCODE_CREDENTIAL_SECRET: SECRET },
    logger,
  });
  await gate.exhausted(); // healthy start
  pct = 100;
  NOW += 121_000; // past pollSec → refetch
  await gate.exhausted(); // exhaustion onset
  pct = 30;
  NOW += 121_000;
  await gate.exhausted(); // recovery
  assert.equal(lines.filter((l) => l.includes("exhausted")).length, 1);
  assert.equal(lines.filter((l) => l.includes("recovered")).length, 1);
  assert.equal(lines.filter((l) => l.includes("gate open") && !l.includes("recovered")).length, 1);
});

test("config: quotaGate defaults are conservative and the credentials path follows t3.home", () => {
  const result = validateConfig({ members: [] });
  assert.equal(result.ok, true);
  assert.equal(result.config.quotaGate.enabled, false);
  assert.equal(result.config.quotaGate.instances[0], "zcode");
  assert.equal(
    result.config.quotaGate.credentialsFile,
    path.join(result.config.t3.home, "zcode-home", ".zcode", "v2", "credentials.json"),
  );
  const explicit = validateConfig({
    t3: { home: "/tmp/other-t3" },
    quotaGate: { enabled: true, instances: ["zcode", "codex"], thresholdPct: 95, bufferSec: 30, pollSec: 60, credentialsFile: "/abs/creds.json" },
  });
  assert.equal(explicit.ok, true);
  assert.deepEqual(explicit.config.quotaGate.instances, ["zcode", "codex"]);
  assert.equal(explicit.config.quotaGate.credentialsFile, "/abs/creds.json");
  assert.equal(explicit.config.quotaGate.thresholdPct, 95);
  const bad = validateConfig({ quotaGate: { pollSec: 1, thresholdPct: 0, bufferSec: -1, instances: [], credentialKeys: [] } });
  assert.equal(bad.ok, false);
  assert.equal(bad.errors.filter((e) => e.startsWith("quotaGate.")).length, 5);
});
