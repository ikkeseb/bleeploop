// verify/harness/run-probe.mjs — run and judge one browser probe (verify/run-probes.mjs runs the set;
// verify/guards/run-probes.mjs holds the judgement to fake probes).

import { spawn } from 'node:child_process';

/** Run one probe file as its own Node process and judge it: it passes when it exits 0 before
 * `timeoutMs` and printed a `=== RESULT: … passed ===` line. A probe still running at the timeout
 * fails, even if it then exits 0 (the probe helper prints its RESULT line before `browser.close()`,
 * verify/harness/probe.ts); one that ignores the first kill is SIGKILLed `killDelayMs` later. */
export function runProbe(file, args = [], { timeoutMs, killDelayMs = 3000, stream = false }) {
  return new Promise((resolve) => {
    const started = Date.now();
    const child = spawn(process.execPath, [file, ...args], {
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let out = '';
    let timedOut = false;
    let killTimer;
    const collect = (sink) => (chunk) => {
      out += chunk;
      if (stream) sink.write(chunk);
    };
    child.stdout.on('data', collect(process.stdout));
    child.stderr.on('data', collect(process.stderr));
    const timer = setTimeout(() => {
      timedOut = true;
      out += `\n[run-probes] timed out after ${timeoutMs / 1000} s\n`;
      child.kill();
      killTimer = setTimeout(() => child.kill('SIGKILL'), killDelayMs);
    }, timeoutMs);
    child.on('error', (error) => { out += `${error}\n`; });
    child.on('close', (code) => {
      clearTimeout(timer);
      clearTimeout(killTimer);
      const result = out.match(/=== RESULT: .*===/g)?.at(-1) ?? '';
      const ok = !timedOut && code === 0 && /passed/.test(result) && !/FAILED/.test(result);
      resolve({ ok, code, timedOut, seconds: (Date.now() - started) / 1000, out });
    });
  });
}
