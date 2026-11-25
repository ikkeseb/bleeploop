/**
 * DEV probe: does a `localStorage.setItem` survive the host process dying shortly after it? The rig
 * restore's crash marker (`lf.rigInFlight`, `src/ui/state/rig-recall.ts`) is written that way just before
 * each plugin add; if WebView2 has not kept the write when a plugin crashes app.exe milliseconds later,
 * the marker cannot catch a fast crash. WebView2 keeps localStorage in its own browser process
 * (msedgewebview2.exe), not app.exe: the runner counts this profile's WebView2 processes at app.exe's
 * death and waits for them to go before the next launch, so a read never reuses the dead launch's browser
 * process. `pnpm native:storage-kill` launches the app once per phase (`VITE_LF_PROBE_PHASE`):
 *   abort-<ms>  prints `armed:`, waits 1.5 s (so that line reaches the log), writes a stamp unique to the
 *               launch under its own key, then after <ms> (0: in the same task, right after the write)
 *               calls `debug_abort` (`std::process::abort` in app.exe, as a plugin crash would end it);
 *               Rust prints the lag from the write to the abort
 *   kill        writes its stamp and prints `set:` with the write's `Date.now()`; the runner kills
 *               app.exe alone on that line (`taskkill /F`) and prints the lag from the write
 *   read-<way>  reads that way's key: `survived=yes` only when it holds the stamp the runner handed over;
 *               removes the key and closes
 * The probe measures; every outcome is a clean run. Keys are this probe's own, in a profile of its own
 * (`scripts/storage-probe.tauri.json`). The dev origin (http://localhost:1420) is not the release one,
 * but the same WebView2 storage serves both. Nothing plays: the device opens on WASAPI and stays silent.
 *
 * Trigger: `VITE_LF_PROBE=storage-kill` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_PHASE`   the phase (set by the runner)
 *   `VITE_LF_PROBE_EXPECT`  the die phase's `armed:`/`set:` line, handed over by the runner (JSON)
 */
import { confirmNativeClose, debugAbortHost } from '../platform';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error('[storage-kill]', ...args);

/** The key a way of dying writes (`abort-50`, `kill`). */
const keyOf = (way: string) => `storageKillProbe.${way}`;

interface Armed {
  way: string;
  key: string;
  stamp: string;
  at?: number;
}

function arm(way: string): Armed {
  const key = keyOf(way);
  localStorage.removeItem(key);
  return { way, key, stamp: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}` };
}

async function abortAfter(way: string, delay: number): Promise<void> {
  const armed = arm(way);
  log(`armed: ${JSON.stringify(armed)}`);
  await sleep(1500);
  localStorage.setItem(armed.key, armed.stamp);
  const setAt = Date.now();
  if (delay > 0) await sleep(delay);
  await debugAbortHost(setAt).catch((e: unknown) => log(`FAIL debug_abort: ${String(e)}`));
}

function killed(): void {
  const armed = arm('kill');
  localStorage.setItem(armed.key, armed.stamp);
  armed.at = Date.now();
  log(`set: ${JSON.stringify(armed)}`);
}

async function read(way: string): Promise<void> {
  const raw = import.meta.env.VITE_LF_PROBE_EXPECT as string | undefined;
  const value = localStorage.getItem(keyOf(way));
  localStorage.removeItem(keyOf(way));
  if (!raw) log(`FAIL no VITE_LF_PROBE_EXPECT handed over from phase ${way}`);
  else {
    const expect = JSON.parse(raw) as Armed;
    const survived = expect.way === way && value === expect.stamp;
    log(`read: ${way} survived=${survived ? 'yes' : 'no'} value=${JSON.stringify(value)} stamp=${expect.stamp}`);
  }
  await sleep(500);
  await confirmNativeClose();
}

export async function runStorageKill(): Promise<void> {
  const phase = (import.meta.env.VITE_LF_PROBE_PHASE as string | undefined) ?? '';
  const abort = phase.match(/^abort-(\d+)$/);
  if (abort) return abortAfter(phase, Number(abort[1]));
  if (phase === 'kill') return killed();
  const back = phase.match(/^read-(.+)$/);
  if (back) return read(back[1]);
  log(`FAIL unknown phase "${phase}"`);
}
