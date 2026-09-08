import fs from "node:fs";
import path from "node:path";
import { runCapture } from "./spawn.mjs";

// Refresh a token this long before its stated expiry.
const REFRESH_MARGIN_MS = 5 * 60 * 1000;

/**
 * Minimal client for the T3 orchestration HTTP API: snapshot reads plus
 * command dispatch. The bearer token is issued offline through the t3 CLI
 * (`auth session issue`) and re-issued on 401, so a rotated/expired token
 * never wedges the bridge.
 */
export class HttpT3Client {
  #fetchImpl;
  #origin = null;
  #token = null;
  #expiresAtMs = 0;
  #onIssue = null;

  constructor({
    origin = null,
    bin = "t3",
    home,
    label = "im-t3-bridge",
    tokenTtl = "30d",
    fetchImpl = globalThis.fetch?.bind(globalThis),
    onIssue = null,
  } = {}) {
    this.configOrigin = origin;
    this.bin = Array.isArray(bin) ? bin : [bin];
    this.home = home;
    this.label = label;
    this.tokenTtl = tokenTtl;
    this.#fetchImpl = fetchImpl;
    this.#onIssue = onIssue;
  }

  origin() {
    if (this.#origin) return this.#origin;
    if (this.configOrigin) {
      this.#origin = this.configOrigin.replace(/\/+$/, "");
      return this.#origin;
    }
    const runtimePath = path.join(this.home, "userdata", "server-runtime.json");
    let runtime;
    try {
      runtime = JSON.parse(fs.readFileSync(runtimePath, "utf8"));
    } catch (err) {
      throw new Error(`cannot read ${runtimePath} (${err.message}) — is the T3 server running? Set t3.origin in the bridge config to override discovery.`);
    }
    if (!runtime.origin) throw new Error(`no origin field in ${runtimePath}`);
    this.#origin = runtime.origin.replace(/\/+$/, "");
    return this.#origin;
  }

  async #issueToken() {
    const args = [
      ...this.bin,
      "auth",
      "session",
      "issue",
      "--ttl",
      this.tokenTtl,
      "--label",
      this.label,
      "--json",
    ];
    const res = await runCapture(args[0], args.slice(1), { cwd: process.cwd() });
    if (res.code !== 0) {
      throw new Error(`t3 auth session issue failed (exit ${res.code}): ${res.stderr.trim().slice(0, 300)}`);
    }
    const start = res.stdout.indexOf("{");
    if (start < 0) throw new Error("t3 auth session issue produced no JSON output");
    let issued;
    try {
      issued = JSON.parse(res.stdout.slice(start));
    } catch (err) {
      throw new Error(`cannot parse issued session JSON: ${err.message}`);
    }
    if (!issued.token) throw new Error("issued session JSON has no token field");
    // Tokens are long-lived (30d) and nothing sweeps bearer sessions, so every
    // restart and 401-reissue would otherwise pile another `im-t3-bridge | bot`
    // row into the T3 session list. Revoke the previously issued session (best
    // effort — a missing/expired id just means nothing to clean) and remember
    // the new one.
    const issuedId = issued.sessionId ?? issued.id ?? null;
    const previousId = this.#readPreviousSessionId();
    if (previousId && previousId !== issuedId) {
      await this.#revokeSession(previousId);
    }
    if (issuedId) this.#storeSessionId(issuedId);
    this.#token = issued.token;
    this.#expiresAtMs = Date.parse(issued.expiresAt ?? "") || 0;
    return this.#token;
  }

  #statePath() {
    let home = this.home ?? path.join(process.env.HOME ?? ".", ".t3");
    // Config files may carry an unexpanded `~` prefix; write next to the T3
    // state, not into a literal `~` directory.
    if (home === "~") home = process.env.HOME ?? ".";
    else if (home.startsWith("~/")) home = path.join(process.env.HOME ?? ".", home.slice(2));
    return path.join(home, `t3-bridge-session-${this.label}.json`);
  }

  #readPreviousSessionId() {
    try {
      const state = JSON.parse(fs.readFileSync(this.#statePath(), "utf8"));
      return typeof state.sessionId === "string" ? state.sessionId : null;
    } catch {
      return null;
    }
  }

  #storeSessionId(sessionId) {
    try {
      fs.writeFileSync(this.#statePath(), JSON.stringify({ sessionId }, null, 2));
    } catch {
      // State persistence is advisory: without it the bridge just stops
      // revoking predecessors.
    }
  }

  async #revokeSession(sessionId) {
    const args = [...this.bin, "auth", "session", "revoke", sessionId];
    const res = await runCapture(args[0], args.slice(1), { cwd: process.cwd() });
    if (res.code !== 0) return; // already revoked/expired — nothing to clean
    this.#onIssue?.(`revoked superseded bridge session ${sessionId.slice(0, 8)}`);
  }

  async token(force = false) {
    const fresh = this.#token && (!this.#expiresAtMs || Date.now() < this.#expiresAtMs - REFRESH_MARGIN_MS);
    if (!force && fresh) return this.#token;
    return this.#issueToken();
  }

  async #request(method, urlPath, body, retried = false) {
    const token = await this.token();
    let res;
    try {
      res = await this.#fetchImpl(this.origin() + urlPath, {
        method,
        headers: {
          authorization: `Bearer ${token}`,
          ...(body !== undefined ? { "content-type": "application/json" } : {}),
        },
        body: body !== undefined ? JSON.stringify(body) : undefined,
      });
    } catch (err) {
      throw new Error(`${method} ${urlPath}: ${err.message}`);
    }
    if (res.status === 401 && !retried) {
      await this.token(true);
      return this.#request(method, urlPath, body, true);
    }
    return res;
  }

  async shell() {
    const res = await this.#request("GET", "/api/orchestration/shell");
    if (!res.ok) throw new Error(`shell snapshot failed: HTTP ${res.status}`);
    return res.json();
  }

  /** @returns the thread detail snapshot, or null when the thread does not exist (404). */
  async threadDetail(threadId) {
    const res = await this.#request("GET", `/api/orchestration/threads/${encodeURIComponent(threadId)}`);
    if (res.status === 404) return null;
    if (!res.ok) throw new Error(`thread snapshot for ${threadId} failed: HTTP ${res.status}`);
    return res.json();
  }

  async dispatch(command) {
    const res = await this.#request("POST", "/api/orchestration/dispatch", command);
    if (!res.ok) {
      const text = await res.text().catch(() => "");
      throw new Error(`dispatch ${command.type} failed: HTTP ${res.status} ${text.slice(0, 300)}`);
    }
    return res.json();
  }
}
