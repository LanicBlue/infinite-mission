import { spawn } from "node:child_process";

/**
 * Run a command, capture stdout/stderr, resolve on exit. Never rejects —
 * spawn errors (ENOENT) and aborts come back as { code: -1 } so callers
 * decide what is fatal. Partial stdout captured before an abort is kept:
 * the caller treats it as a normal cycle.
 */
export function runCapture(command, args, { cwd, signal } = {}) {
  return new Promise((resolve) => {
    let child;
    try {
      child = spawn(command, args, { cwd, signal });
    } catch (err) {
      resolve({ code: -1, stdout: "", stderr: String(err?.message ?? err) });
      return;
    }
    let stdout = "";
    let stderr = "";
    child.stdout?.on("data", (chunk) => (stdout += chunk));
    child.stderr?.on("data", (chunk) => (stderr += chunk));
    child.on("error", (err) => {
      resolve({ code: -1, stdout, stderr: stderr + String(err?.message ?? err) });
    });
    child.on("close", (code) => {
      resolve({ code: code ?? -1, stdout, stderr });
    });
  });
}

/** Sleep that returns early when the signal fires (callers re-check aborted). */
export function sleep(ms, signal) {
  return new Promise((resolve) => {
    if (signal?.aborted) {
      resolve();
      return;
    }
    const timer = setTimeout(done, ms);
    function done() {
      signal?.removeEventListener("abort", done);
      clearTimeout(timer);
      resolve();
    }
    signal?.addEventListener("abort", done, { once: true });
  });
}
