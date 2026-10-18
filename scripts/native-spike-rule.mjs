// scripts/native-spike-rule.mjs — what one `pnpm native:spike` probe process adds to the table.
//
// A probe passes only on what it finished: a timeout, a signal or a non-zero exit is an ERROR row even
// after PASS lines (the plugin teardown runs after the report), and so is a clean exit that printed no
// verdict. Pure: scripts/native-spike.mjs hands in what the process printed and how it ended.
// Guard: verify/guards/native-spike-rule.mjs.

/**
 * @param {object} run
 * @param {{ pass: boolean, id: string, rest: string }[]} run.verdicts its PASS/FAIL lines
 * @param {string | null} run.invalid its INVALID line, if any
 * @param {number | null} run.exit its exit code (null when a signal ended it)
 * @param {string | null} run.signal the signal that ended it
 * @param {number | null} run.timedOutS the timeout that killed it, in seconds; null when it ended by itself
 * @param {string} where the table's "where" column
 * @returns {{ id: string, where: string, pass: boolean, value: string, bar: string }[]}
 */
export function spikeRunRows({ verdicts, invalid, exit, signal, timedOutS }, where) {
  const rows = [];
  const error = (value) => rows.push({ id: 'ERROR', where, pass: false, value, bar: 'the probe runs to its end and exits 0' });
  if (invalid) rows.push({ id: 'INVALID', where, pass: false, value: invalid.replace(/^INVALID /, ''), bar: 'xcorr >= 0.8 (setup fault: fix and rerun)' });
  if (timedOutS != null) error(`killed after its ${timedOutS} s timeout`);
  else if (signal) error(`killed by ${signal}`);
  else if (exit !== 0) error(`exit ${exit}`);
  else if (!verdicts.length && !invalid) error('exit 0 without a verdict');
  for (const v of verdicts) rows.push({ id: v.id, where, pass: v.pass, value: v.rest, bar: '' });
  return rows;
}
