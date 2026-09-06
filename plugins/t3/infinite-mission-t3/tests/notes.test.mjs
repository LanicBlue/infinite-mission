import test from "node:test";
import assert from "node:assert/strict";
import { parseReceiveOutput } from "../lib/notes.mjs";

test("parses an arrival note (station line + Run hint)", () => {
  const stdout = [
    "[station build] mission ms_abc123def456 arrived (iteration 1)",
    "  → Run: im mission show ms_abc123def456 --for t3-codex (then im missions t3-codex)",
    "  → After processing, run `im receive t3-codex --wait` to continue listening.",
    "",
  ].join("\n");
  const notes = parseReceiveOutput(stdout);
  assert.deepEqual(notes.arrivals, [{ station: "build", missionId: "ms_abc123def456" }]);
  assert.equal(notes.membershipEnd, false);
  assert.equal(notes.timeout, false);
  assert.deepEqual(notes.unknown, []);
});

test("parses a batch: two arrivals plus a mission-ended message", () => {
  const stdout = [
    "[from workspace] [ms_111222333444555] mission ended: deleted (outcome)",
    "  → History: im mission events ms_111222333444555",
    "[station design] round returned (iteration 2)",
    "  → Run: im mission show ms_aaaabbbbccccdddd --for t3-codex (then im missions t3-codex)",
    "[station review] mission ms_0000111122223333 arrived (iteration 1)",
    "  → Run: im mission show ms_0000111122223333 --for t3-codex (then im missions t3-codex)",
  ].join("\n");
  const notes = parseReceiveOutput(stdout);
  assert.deepEqual(notes.ended, ["ms_111222333444555"]);
  assert.deepEqual(notes.arrivals, [
    { station: "design", missionId: "ms_aaaabbbbccccdddd" },
    { station: "review", missionId: "ms_0000111122223333" },
  ]);
});

test("parses membership-end and timeout", () => {
  const notes = parseReceiveOutput(
    "[membership] you are no longer an active member (removed or archived) — stopping the listener.\n",
  );
  assert.equal(notes.membershipEnd, true);

  const timed = parseReceiveOutput("No new messages (timed out after 600s).\n");
  assert.equal(timed.timeout, true);
  assert.equal(timed.membershipEnd, false);
});

test("station note without a Run hint is unroutable → unknown, never guessed", () => {
  const notes = parseReceiveOutput("[station build] something without a mission id\n  → After processing, run `im receive x --wait` to continue listening.\n");
  assert.deepEqual(notes.arrivals, []);
  assert.equal(notes.unknown.length, 1);
});

test("unrecognized shapes land in unknown and are dropped", () => {
  const stdout = [
    "Joined as t3-codex.",
    "[from workspace] some future wording we do not know yet",
    "  → some new hint shape",
  ].join("\n");
  const notes = parseReceiveOutput(stdout);
  assert.deepEqual(notes.arrivals, []);
  assert.deepEqual(notes.ended, []);
  assert.equal(notes.membershipEnd, false);
  // the "[from …]" line is not a mission-ended shape → unknown
  assert.equal(notes.unknown.length, 2);
});

test("empty output is a no-op", () => {
  const notes = parseReceiveOutput("");
  assert.deepEqual(notes, { arrivals: [], ended: [], membershipEnd: false, timeout: false, unknown: [] });
});
