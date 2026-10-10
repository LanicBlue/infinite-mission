// GLM coding-plan 5h-window quota state for zcode-family members.
//
// The fork CLI's in-turn transparent wait on 429/1308 left with the
// official-CLI swap (mixed architecture, 2026-10-10). This module gives the
// bridge the same primitive at delivery granularity: poll the account quota
// monitor endpoint and answer one question — "is the 5h window exhausted,
// and when does it reset?". The reconcile tick stays the only scheduler.
//
// Fail-open by contract: a fetch, file, or decrypt problem reads as
// "unknown", and the caller must treat unknown as not-exhausted — a broken
// probe may never stall missions.
//
// Token: a zcode credential-store entry (default the bigmodel OAuth access
// token), decrypted with the CLI's own scheme — AES-256-GCM, key
// sha256(ZCODE_CREDENTIAL_SECRET || machine fallback), payload
// "enc:v1:<iv>.<authTag>.<cipher>" in base64url.

import fs from "node:fs";
import { createDecipheriv, createHash } from "node:crypto";
import os from "node:os";

export const QUOTA_ENDPOINT = "https://open.bigmodel.cn/api/monitor/usage/quota/limit";

const FIVE_HOUR_WINDOW_UNIT = 3;
const CIPHER_PREFIX = "enc:v1:";
const UNKNOWN_RETRY_MS = 30_000;
const FETCH_TIMEOUT_MS = 20_000;

export function formatResetMs(resetAt, now = Date.now()) {
  const d = new Date(resetAt);
  const pad = (n) => String(n).padStart(2, "0");
  const day = d.toLocaleDateString();
  return `${day} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/** Same secret resolution the CLI performs (credential-cipher.ts). */
export function credentialFallbackSecret(env = process.env, facts = null) {
  const configured = env?.ZCODE_CREDENTIAL_SECRET?.trim();
  if (configured) return configured;
  const platform = facts?.platform ?? os.platform();
  const homedir = facts?.homedir ?? os.homedir();
  let username = "unknown";
  try {
    username = facts?.username ?? os.userInfo().username;
  } catch {
    // Packaged or sandboxed runtimes may not resolve OS user info.
  }
  return `zcode-credential-fallback:${platform}:${homedir}:${username}`;
}

export function decryptCredentialValue(value, secret) {
  if (typeof value !== "string" || !value.startsWith(CIPHER_PREFIX)) return value;
  const [ivRaw, authTagRaw, cipherRaw] = value.slice(CIPHER_PREFIX.length).split(".");
  if (!ivRaw || !authTagRaw || !cipherRaw) {
    throw new Error("credential decrypt: invalid ciphertext format");
  }
  const key = createHash("sha256").update(secret).digest();
  let decipher;
  try {
    decipher = createDecipheriv("aes-256-gcm", key, Buffer.from(ivRaw, "base64url"));
    decipher.setAuthTag(Buffer.from(authTagRaw, "base64url"));
    return Buffer.concat([
      decipher.update(Buffer.from(cipherRaw, "base64url")),
      decipher.final(),
    ]).toString("utf8");
  } catch {
    throw new Error("credential decrypt: key mismatch or corrupted ciphertext");
  }
}

/** Pull the 5h TOKENS_LIMIT window out of the monitor endpoint's body. */
export function parseQuotaWindow(body, unit = FIVE_HOUR_WINDOW_UNIT) {
  const limits = body?.data?.limits;
  if (!Array.isArray(limits)) return null;
  const tokens = limits.filter((entry) => entry?.type === "TOKENS_LIMIT");
  const window = tokens.find((entry) => entry.unit === unit) ?? tokens[0];
  if (
    !window ||
    typeof window.percentage !== "number" ||
    typeof window.nextResetTime !== "number" ||
    !Number.isFinite(window.nextResetTime)
  ) {
    return null;
  }
  return { pct: window.percentage, resetAt: window.nextResetTime };
}

/**
 * Reads the credential store and the monitor endpoint, caching one verdict.
 *
 * exhausted() → { gated: true, pct, resetAt } while the window is at/over
 * the threshold and the reset (+buffer) has not passed; any other reachable
 * state returns { gated: false, ... } — including every failure mode.
 */
export class QuotaGate {
  #options;
  #cached = null;
  #inflight = null;
  #loggedState = "init";

  constructor(options = {}) {
    this.#options = {
      credentialsFile: options.credentialsFile,
      credentialKeys: options.credentialKeys?.length ? options.credentialKeys : ["oauth:bigmodel:access_token"],
      pollSec: options.pollSec ?? 120,
      thresholdPct: options.thresholdPct ?? 100,
      bufferSec: options.bufferSec ?? 60,
      fetchImpl: options.fetchImpl ?? globalThis.fetch?.bind(globalThis),
      now: options.now ?? Date.now,
      env: options.env ?? process.env,
      logger: options.logger ?? null,
    };
  }

  lastSnapshot() {
    if (!this.#cached || this.#cached.status !== "ok") return this.#cached;
    const { pct, resetAt, fetchedAt } = this.#cached;
    return {
      status: "ok",
      pct,
      resetAt,
      resets: formatResetMs(resetAt),
      fetchedAt,
      gated: this.#verdictFrom(this.#cached).gated,
    };
  }

  async exhausted() {
    const snapshot = await this.#snapshot();
    return this.#verdictFrom(snapshot);
  }

  #verdictFrom(snapshot) {
    if (snapshot.status !== "ok") {
      return { gated: false, reason: snapshot.reason };
    }
    const { pct, resetAt } = snapshot;
    const gated =
      pct >= this.#options.thresholdPct &&
      this.#options.now() < resetAt + this.#options.bufferSec * 1000;
    return gated ? { gated: true, pct, resetAt } : { gated: false, pct, resetAt };
  }

  async #snapshot() {
    const valid = this.#cached !== null && this.#fresh(this.#cached);
    if (this.#cached && valid) return this.#cached;
    if (this.#inflight) return this.#inflight;
    this.#inflight = this.#fetchSnapshot()
      .then((snapshot) => {
        this.#cached = snapshot;
        this.#logTransition(snapshot);
        return snapshot;
      })
      .finally(() => {
        this.#inflight = null;
      });
    return this.#inflight;
  }

  #fresh(snapshot) {
    const now = this.#options.now();
    if (snapshot.status === "ok") {
      // Past reset+buffer the cached percentage is stale by definition —
      // force one refetch instead of un-gating on old data.
      if (now >= snapshot.resetAt + this.#options.bufferSec * 1000) return false;
      return now - snapshot.fetchedAt < this.#options.pollSec * 1000;
    }
    return now - snapshot.fetchedAt < UNKNOWN_RETRY_MS;
  }

  async #fetchSnapshot() {
    const now = this.#options.now();
    let token;
    try {
      token = this.#resolveToken();
    } catch (err) {
      return { status: "unknown", reason: `credentials: ${err.message}`, fetchedAt: now };
    }
    if (!this.#options.fetchImpl) {
      return { status: "unknown", reason: "no fetch implementation", fetchedAt: now };
    }
    let res;
    try {
      res = await this.#options.fetchImpl(QUOTA_ENDPOINT, {
        headers: { Authorization: token },
        signal: AbortSignal.timeout(FETCH_TIMEOUT_MS),
      });
    } catch (err) {
      return { status: "unknown", reason: `fetch: ${err.message}`, fetchedAt: now };
    }
    if (!res.ok) {
      return { status: "unknown", reason: `HTTP ${res.status}`, fetchedAt: now };
    }
    let body;
    try {
      body = await res.json();
    } catch (err) {
      return { status: "unknown", reason: `json: ${err.message}`, fetchedAt: now };
    }
    const window = parseQuotaWindow(body);
    if (!window) {
      return { status: "unknown", reason: "no TOKENS_LIMIT window in response", fetchedAt: now };
    }
    return { status: "ok", pct: window.pct, resetAt: window.resetAt, fetchedAt: now };
  }

  #resolveToken() {
    const raw = JSON.parse(fs.readFileSync(this.#options.credentialsFile, "utf8"));
    const secret = credentialFallbackSecret(this.#options.env);
    const errors = [];
    for (const key of this.#options.credentialKeys) {
      const value = raw?.[key];
      if (typeof value !== "string" || value.length === 0) continue;
      try {
        return decryptCredentialValue(value, secret);
      } catch (err) {
        errors.push(`${key}: ${err.message}`);
      }
    }
    throw new Error(errors.length ? errors.join("; ") : `no credential key present (${this.#options.credentialKeys.join(", ")})`);
  }

  #logTransition(snapshot) {
    if (!this.#options.logger) return;
    const state =
      snapshot.status !== "ok"
        ? "unknown"
        : this.#verdictFrom(snapshot).gated
          ? "exhausted"
          : "ok";
    if (state === this.#loggedState) return;
    const previous = this.#loggedState;
    this.#loggedState = state;
    if (state === "ok" && previous === "init") {
      this.#options.logger(
        "quota",
        `5h window at ${snapshot.pct}% (resets ${formatResetMs(snapshot.resetAt)}) — gate open`,
      );
    } else if (state === "ok") {
      this.#options.logger("quota", `5h window recovered (${snapshot.pct}%) — gate open`);
    } else if (state === "exhausted") {
      this.#options.logger(
        "quota",
        `5h window exhausted (${snapshot.pct}%, resets ${formatResetMs(snapshot.resetAt)}) — deliveries to gated instances are held`,
      );
    } else if (previous === "exhausted") {
      this.#options.logger("quota", `quota probe lost (${snapshot.reason}) — failing open`);
    } else {
      this.#options.logger("quota", `quota probe unavailable (${snapshot.reason}) — failing open`);
    }
  }
}
