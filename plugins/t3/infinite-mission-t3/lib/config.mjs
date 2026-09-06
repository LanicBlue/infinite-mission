import fs from "node:fs";
import os from "node:os";
import path from "node:path";

export const RUNTIME_MODES = ["approval-required", "auto-accept-edits", "auto", "full-access"];

// im member ids become filenames under .im/sessions/ — keep them filename-safe.
const MEMBER_ID_RE = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

export function defaultConfigPath() {
  return process.env.IM_T3_BRIDGE_CONFIG ?? path.join(os.homedir(), ".im", "t3-bridge.json");
}

export function expandTilde(value) {
  if (typeof value !== "string") return value;
  if (value === "~") return os.homedir();
  if (value.startsWith("~/")) return path.join(os.homedir(), value.slice(2));
  return value;
}

const trim = (value) => (typeof value === "string" ? value.trim() : "");

/**
 * Validate a raw config object. Returns { ok: true, config } or
 * { ok: false, errors: string[] } with every problem listed — never throws.
 */
export function validateConfig(raw) {
  if (raw === null || typeof raw !== "object" || Array.isArray(raw)) {
    return { ok: false, errors: ["config root must be a JSON object"] };
  }
  const errors = [];
  const add = (where, what) => errors.push(`${where}: ${what}`);

  const imBin = trim(raw.imBin) || "im";

  const receiveTimeoutSec = raw.receiveTimeoutSec ?? 600;
  if (!Number.isInteger(receiveTimeoutSec) || receiveTimeoutSec < 1) {
    add("receiveTimeoutSec", "must be an integer >= 1 (seconds)");
  }
  const rescanSec = raw.rescanSec ?? 60;
  if (!Number.isInteger(rescanSec) || rescanSec < 1) {
    add("rescanSec", "must be an integer >= 1 (seconds)");
  }
  const maxDeliverAttempts = raw.maxDeliverAttempts ?? 10;
  if (!Number.isInteger(maxDeliverAttempts) || maxDeliverAttempts < 1) {
    add("maxDeliverAttempts", "must be an integer >= 1");
  }

  const t3raw = raw.t3 ?? {};
  if (t3raw === null || typeof t3raw !== "object" || Array.isArray(t3raw)) {
    add("t3", "must be an object");
  }
  let t3 = {};
  if (t3raw && typeof t3raw === "object" && !Array.isArray(t3raw)) {
    const bin = t3raw.bin ?? "t3";
    const binOk =
      (typeof bin === "string" && bin.trim().length > 0) ||
      (Array.isArray(bin) && bin.length > 0 && bin.every((part) => typeof part === "string" && part.length > 0));
    if (!binOk) {
      add("t3.bin", "must be a command string or a non-empty array of strings");
    }
    const origin = trim(t3raw.origin);
    if (origin && !/^https?:\/\//.test(origin)) {
      add("t3.origin", "must be an http(s) URL");
    }
    t3 = {
      bin: binOk ? bin : "t3",
      home: expandTilde(trim(t3raw.home) || path.join(os.homedir(), ".t3")),
      origin: origin || null,
      label: trim(t3raw.label) || "im-t3-bridge",
      tokenTtl: trim(t3raw.tokenTtl) || "30d",
    };
  }

  let workspaces = null;
  if (raw.workspaces !== undefined && raw.workspaces !== null) {
    if (!Array.isArray(raw.workspaces)) {
      add("workspaces", "must be an array of absolute paths (omit to follow `im workspaces`)");
    } else {
      workspaces = [];
      raw.workspaces.forEach((entry, index) => {
        if (typeof entry !== "string" || !entry.startsWith("/") || !path.isAbsolute(entry)) {
          add(`workspaces[${index}]`, "must be an absolute path");
        } else {
          workspaces.push(entry);
        }
      });
    }
  }

  const members = [];
  if (raw.members !== undefined && !Array.isArray(raw.members)) {
    add("members", "must be an array");
  } else {
    const list = raw.members ?? [];
    const seen = new Set();
    list.forEach((entry, index) => {
      const where = `members[${index}]`;
      if (entry === null || typeof entry !== "object" || Array.isArray(entry)) {
        add(where, "must be an object");
        return;
      }
      const id = trim(entry.id);
      if (!MEMBER_ID_RE.test(id)) {
        add(`${where}.id`, "must match [A-Za-z0-9][A-Za-z0-9._-]{0,63} (it becomes a filename in .im/sessions/)");
      } else if (seen.has(id)) {
        add(`${where}.id`, `duplicate member id "${id}"`);
      } else {
        seen.add(id);
      }
      const instance = trim(entry.instance);
      if (!instance) add(`${where}.instance`, "is required (a T3 provider instance id)");
      const model = trim(entry.model);
      if (!model) add(`${where}.model`, "is required (see T3's model picker for valid slugs)");
      const runtimeMode = entry.runtimeMode ?? "full-access";
      if (!RUNTIME_MODES.includes(runtimeMode)) {
        add(`${where}.runtimeMode`, `must be one of ${RUNTIME_MODES.join(" | ")}`);
      }
      let options = undefined;
      if (entry.options !== undefined) {
        if (entry.options === null || typeof entry.options !== "object" || Array.isArray(entry.options)) {
          add(`${where}.options`, "must be an object (passed through to T3 modelSelection.options)");
        } else {
          options = entry.options;
        }
      }
      const enabled = entry.enabled ?? true;
      if (typeof enabled !== "boolean") add(`${where}.enabled`, "must be a boolean");
      members.push({ id: id || `member-${index}`, instance, model, options, runtimeMode, enabled });
    });
  }


  if (errors.length) return { ok: false, errors };

  return {
    ok: true,
    config: { imBin, receiveTimeoutSec, rescanSec, maxDeliverAttempts, t3, workspaces, members },
  };
}

/** Read + JSON.parse + validate a config file. Never throws. */
export function readConfigFile(filePath) {
  let text;
  try {
    text = fs.readFileSync(filePath, "utf8");
  } catch (err) {
    return { ok: false, errors: [`cannot read ${filePath}: ${err.message}`] };
  }
  let raw;
  try {
    raw = JSON.parse(text);
  } catch (err) {
    return { ok: false, errors: [`invalid JSON in ${filePath}: ${err.message}`] };
  }
  const result = validateConfig(raw);
  if (!result.ok) return { ok: false, errors: result.errors.map((line) => `${filePath}: ${line}`) };
  return result;
}

/**
 * The member table lives in the T3 fork's settings.json (`imBridge.members`)
 * and is edited by T3's own settings UI. Returns the mapped member list, or
 * null when that key is absent (then the bridge config file's own members
 * list applies — the migration fallback).
 */
export function readT3ImMembers(t3Home) {
  let settings;
  try {
    settings = JSON.parse(fs.readFileSync(path.join(t3Home, "userdata", "settings.json"), "utf8"));
  } catch {
    return null;
  }
  const section = settings?.imBridge;
  if (section === undefined || section === null) return null;
  const members = [];
  const seen = new Set();
  for (const entry of Array.isArray(section.members) ? section.members : []) {
    const id = trim(entry?.id);
    const instance = trim(entry?.instanceId);
    const model = trim(entry?.model);
    if (!MEMBER_ID_RE.test(id) || !instance || !model || seen.has(id)) continue;
    seen.add(id);
    const runtimeMode = RUNTIME_MODES.includes(entry.runtimeMode) ? entry.runtimeMode : "full-access";
    members.push({
      id,
      instance,
      model,
      options: entry.options && typeof entry.options === "object" && !Array.isArray(entry.options) ? entry.options : undefined,
      runtimeMode,
      enabled: entry.enabled ?? true,
    });
  }
  return members;
}
