// scripts/native-kill.mjs — stop a `tauri dev` run: every `app` and `cargo` process plus whatever
// listens on port 1420 (the dev Vite). Never every node process: the agent session may be one. A probe
// run sweeps with `cargo: false`: its own tree kill already took its cargo, and a cargo running now is
// someone else's build.
//
//   pnpm native:kill
//
// Windows node only (from WSL the pnpm wrapper runs it there). `native-probe.mjs` imports `killNative`,
// `appRunning` and `closeAppWindow`.

import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const PS = `
$targets = @(Get-Process app,cargo -ErrorAction SilentlyContinue)
$owners = (Get-NetTCPConnection -LocalPort 1420 -State Listen -ErrorAction SilentlyContinue).OwningProcess
foreach ($id in ($owners | Select-Object -Unique)) { $targets += Get-Process -Id $id -ErrorAction SilentlyContinue }
$targets | Sort-Object Id -Unique | ForEach-Object {
  Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
  "$($_.ProcessName) $($_.Id)"
}`;

export function assertWindows(command) {
  if (process.platform === 'win32') return;
  console.error(`${command} runs on Windows node only (the PC; from WSL, run it through pnpm on a /mnt/c checkout).`);
  process.exit(1);
}

/** Stops the processes; returns their "name pid" lines. `cargo: false` leaves cargo running. */
export function killNative({ cargo = true } = {}) {
  const script = cargo ? PS : PS.replace('app,cargo', 'app');
  const out = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8' });
  return out.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
}

/** Lists what `killNative` would stop, without stopping it. */
export function nativeRunning() {
  const probe = PS.replace(/Stop-Process[^\n]*\n/, '');
  const out = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', probe], { encoding: 'utf8' });
  return out.split(/\r?\n/).map((l) => l.trim()).filter(Boolean);
}

/** Whether an `app` process runs (a phased native probe waits for the app to quit by itself). */
export function appRunning() {
  const count = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', '@(Get-Process app -ErrorAction SilentlyContinue).Count'], { encoding: 'utf8' });
  return Number(count.trim()) > 0;
}

/** Sends the app's main window WM_CLOSE, as its close button does; returns whether one was sent. */
export function closeAppWindow() {
  const count = execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', '@(Get-Process app -ErrorAction SilentlyContinue | Where-Object { $_.CloseMainWindow() }).Count'], { encoding: 'utf8' });
  return Number(count.trim()) > 0;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  assertWindows('native:kill');
  const killed = killNative();
  console.log(killed.length ? `native:kill: stopped ${killed.join(', ')}` : 'native:kill: nothing running');
}
