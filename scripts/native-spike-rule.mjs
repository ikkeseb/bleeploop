// scripts/native-spike-rule.mjs — what one `pnpm native:spike` probe process adds to the table.
//
// A probe passes only on what it finished: a timeout, a signal or a non-zero exit is an ERROR row even
// after PASS lines (the plugin teardown runs after the report), and so is a clean exit that printed no
// verdict, and a run that names its final verdict (`--probe-share all`: `S1`) fails unless that line
// says PASS: its controls passing does not make it pass. Pure: scripts/native-spike.mjs hands in what the process printed and how it ended.
// Guard: verify/guards/native-spike-rule.mjs.

/**
 * A probe line's verdict (the text after its `[engine-spike]`/`[share-probe]` tag): PASS, FAIL or
 * UNKNOWN (too few measuring periods), which does not pass; null for any other line.
 * @param {string} body
 * @returns {{ pass: boolean, id: string, rest: string } | null}
 */
export function spikeVerdict(body) {
  const m = body.match(/^(PASS|FAIL|UNKNOWN) (\S+) (.*)$/);
  return m ? { pass: m[1] === 'PASS', id: m[2], rest: m[3] } : null;
}

/**
 * @param {object} run
 * @param {{ pass: boolean, id: string, rest: string }[]} run.verdicts its PASS, FAIL and UNKNOWN lines (an UNKNOWN is not a pass)
 * @param {string | null} run.invalid its INVALID line, if any
 * @param {number | null} run.exit its exit code (null when a signal ended it)
 * @param {string | null} run.signal the signal that ended it
 * @param {number | null} run.timedOutS the timeout that killed it, in seconds; null when it ended by itself
 * @param {string} where the table's "where" column
 * @param {string} [final] the id of the run's final verdict, which must be printed and PASS
 * @returns {{ id: string, where: string, pass: boolean, value: string, bar: string }[]}
 */
export function spikeRunRows({ verdicts, invalid, exit, signal, timedOutS }, where, final) {
  const rows = [];
  const error = (value) => rows.push({ id: 'ERROR', where, pass: false, value, bar: 'the probe runs to its end and exits 0' });
  if (invalid) rows.push({ id: 'INVALID', where, pass: false, value: invalid.replace(/^INVALID /, ''), bar: 'xcorr >= 0.8 (setup fault: fix and rerun)' });
  if (timedOutS != null) error(`killed after its ${timedOutS} s timeout`);
  else if (signal) error(`killed by ${signal}`);
  else if (exit !== 0) error(`exit ${exit}`);
  else if (!verdicts.length && !invalid) error('exit 0 without a verdict');
  for (const v of verdicts) rows.push({ id: v.id, where, pass: v.pass, value: v.rest, bar: '' });
  if (final && !invalid && !verdicts.some((v) => v.id === final)) {
    rows.push({ id: final, where, pass: false, value: 'no final verdict printed', bar: `a PASS ${final} line` });
  }
  return rows;
}
