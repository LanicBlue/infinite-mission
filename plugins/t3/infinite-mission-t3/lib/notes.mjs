// Closed-set parser for `im receive <id> --wait` stdout. Exactly four line
// shapes are routed (see infinite-mission src/main.rs print_messages /
// print_notes / the wait loop); anything else is collected as `unknown` and
// dropped — never guessed at — so a future im CLI wording change cannot
// misroute a delivery.

const MISSION_ID = "ms_[0-9a-f]{6,64}";
const RUN_HINT = new RegExp(`^  → Run: im mission show (${MISSION_ID}) --for .+? \\(then`);
const STATION_LINE = /^\[station (.+?)\] (.*)$/;
const ENDED_LINE = new RegExp(`^\\[from .+?\\] \\[(${MISSION_ID})\\] mission ended: `);
const MEMBERSHIP_END = /^\[membership\] you are no longer an active member/;
const TIMEOUT_LINE = /^No new messages \(timed out after \d+s\)\.$/;
// Hint lines the CLI prints under messages/notes ("→ History: …", "→ After
// processing, …"). They carry no routing fact of their own.
const HINT_LINE = /^  →/;

/**
 * @returns {{
 *   arrivals: Array<{ station: string, missionId: string }>,
 *   ended: string[],
 *   membershipEnd: boolean,
 *   timeout: boolean,
 *   unknown: string[],
 * }}
 */
export function parseReceiveOutput(text) {
  const arrivals = [];
  const ended = [];
  const unknown = [];
  let membershipEnd = false;
  let timeout = false;

  const lines = String(text).split("\n");
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (!line.trim()) continue;

    if (MEMBERSHIP_END.test(line)) {
      membershipEnd = true;
      continue;
    }
    if (TIMEOUT_LINE.test(line)) {
      timeout = true;
      continue;
    }
    const endedMatch = line.match(ENDED_LINE);
    if (endedMatch) {
      ended.push(endedMatch[1]);
      continue;
    }
    const stationMatch = line.match(STATION_LINE);
    if (stationMatch) {
      // The mission id lives on the "→ Run: im mission show …" hint printed
      // directly under an arrival note. Without it the note is not routable.
      const runHint = (lines[i + 1] ?? "").match(RUN_HINT);
      if (runHint) {
        arrivals.push({ station: stationMatch[1], missionId: runHint[1] });
      } else {
        unknown.push(line);
      }
      continue;
    }
    if (HINT_LINE.test(line)) continue;

    unknown.push(line);
  }

  return { arrivals, ended, membershipEnd, timeout, unknown };
}
