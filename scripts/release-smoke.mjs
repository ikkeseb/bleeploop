// scripts/release-smoke.mjs — drive an unmodified RELEASE build of BleepLoop from outside and prove it
// boots on the native engine and records a take.
//
//   pnpm exec tauri build --no-bundle --features asio --config scripts/release-smoke.tauri.json
//   pnpm release:smoke [--exe=<path>] [--fresh]
//
// What it proves, one PASS/FAIL line per check (exit 0 only when every check passes):
//   boots        the window loads the app UI (five lanes, the command bar), with no uncaught error and
//                no CSP violation: not in the page (replayed over CDP from before the attach) and not in
//                the release log (`[csp]`, `[window.error]`, `[unhandledrejection]` lines,
//                src/platform/logging.ts)
//   engine       Audio Settings' engine switch reads `native` and its engine row names a running device;
//                the log says the engine owns the device. The device a launch opens by itself is printed,
//                not judged (a `--fresh` run shows a new user's first launch)
//   device       ASIO on, input channel `--channel=` (0-based, default 1 = input 2) and buffer
//                `--buffer=` (default 128) picked in Audio Settings as a user would; the engine row shows
//                ASIO at that buffer and the log shows the reopen
//   take         with CLICK on, FIXED 1 bar and MIC live, a press on lane 1's record core takes the lane
//                armed → rec → play, and its waveform canvas is not flat (lane 3, empty, is the control)
//   feed         something on screen moves with the engine: the beat LEDs, the record-level meter, the
//                lane-1 playhead. The meter reads the device input, and before the first take the
//                click is silent (it sounds on a count-in or while the transport runs: lf-engine
//                `clock.rs` `fire_due`), so it stays still until the count-in even with CLICK on
//   exit         CLEAR ALL empties the lanes, the OS close (as the close button does, through the app's
//                close guard) ends the process within 60 s, and this run's release log holds no ERROR line
// A screenshot after the take and this run's slice of the release log land in logs/release-smoke/.
//
// What it cannot see: sound. A take that is not flat proves the input reached the loop, not that it
// sounds right or sits on the grid (the ear, and `pnpm native:engine-loopback`, judge those). The take hears
// the click only through a cable from an output into the picked input; without one it records the
// input's noise floor and may read flat. Nothing before the CDP attach is watched live: early page
// errors come from the CDP replay and the release log. Plugins are not loaded.
//
// How: the exe is launched with WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=<port>
// (`--port=`, default 9333). WebView2 appends it to the arguments wry sets itself; the browser
// process's command line is printed as evidence. Playwright attaches with `connectOverCDP` and clicks the
// UI; nothing in the app changes. If CDP does not answer within `--cdp-wait=` s (default 60), the run
// fails and keeps a PrintWindow capture of the app window (by its handle, never a screen grab).
//
// Profiles: the exe's identifier (compiled in) picks its %LOCALAPPDATA% folder and WebView2 data. The
// smoke build carries scripts/release-smoke.tauri.json's identifier; `--fresh` deletes that folder first.
// A CI or `pnpm build:app` exe carries the owner's identifier (com.bleeploop.app): the run refuses it
// unless `--owner-profile` is passed, and `--fresh` never deletes the owner's folder. Refuses to start
// while an `app` process runs. Stops only the app.exe it launched. Windows node only.

import { execFileSync, spawn } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';
import { appRunning, assertWindows } from './native-kill.mjs';

const OWNER_ID = 'com.bleeploop.app';
const EXIT_WAIT_MS = 60_000;
const root = join(dirname(fileURLToPath(import.meta.url)), '..');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// ── Arguments ─────────────────────────────────────────────────────────────────────────────────────
const opts = { exe: join(root, 'src-tauri', 'target', 'release', 'app.exe'), port: 9333, buffer: 128, channel: 1, cdpWait: 60 };
const flags = new Set();
for (const arg of process.argv.slice(2)) {
  const m = arg.match(/^--([\w-]+)(?:=(.*))?$/);
  if (!m) usage(`unknown argument ${arg}`);
  const [, key, value] = m;
  if (key === 'exe' && value) opts.exe = resolve(value);
  else if (key === 'port' && value) opts.port = Number(value);
  else if (key === 'buffer' && value) opts.buffer = Number(value);
  else if (key === 'channel' && value !== undefined) opts.channel = Number(value);
  else if (key === 'cdp-wait' && value !== undefined) opts.cdpWait = Number(value);
  else if ((key === 'fresh' || key === 'owner-profile') && value === undefined) flags.add(key);
  else usage(`unknown argument ${arg}`);
}
function usage(why) {
  console.error(`${why}\nusage: node scripts/release-smoke.mjs [--exe=<path>] [--fresh] [--port=9333] [--buffer=128] [--channel=1] [--cdp-wait=60] [--owner-profile]`);
  process.exit(1);
}
assertWindows('release smoke');

// ── Which profile the exe opens ───────────────────────────────────────────────────────────────────
if (!existsSync(opts.exe)) usage(`no exe at ${opts.exe}`);
const smokeId = JSON.parse(readFileSync(join(root, 'scripts', 'release-smoke.tauri.json'), 'utf8')).identifier;
const bytes = readFileSync(opts.exe);
const identifier = [smokeId, OWNER_ID].find((id) => bytes.includes(id));
if (!identifier) {
  console.error(`release smoke: cannot tell which profile ${opts.exe} opens (neither ${smokeId} nor ${OWNER_ID} is in it)`);
  process.exit(1);
}
if (identifier === OWNER_ID && !flags.has('owner-profile')) {
  console.error(`release smoke: ${opts.exe} runs in the owner's profile (${OWNER_ID}); refusing. Build with --config scripts/release-smoke.tauri.json, or pass --owner-profile when the owner asks for it.`);
  process.exit(1);
}
if (identifier === OWNER_ID && flags.has('fresh')) {
  console.error(`release smoke: --fresh never deletes the owner's profile (${OWNER_ID})`);
  process.exit(1);
}
if (appRunning()) {
  console.error('release smoke: an app process runs (the dev app, a probe or another release). Close it first.');
  process.exit(1);
}
const cdpUrl = `http://127.0.0.1:${opts.port}`;
if (await cdpVersion()) {
  console.error(`release smoke: something already answers CDP on port ${opts.port}; pick another --port`);
  process.exit(1);
}

const profile = join(process.env.LOCALAPPDATA ?? '', identifier);
const logFile = join(profile, 'logs', 'bleeploop.log');
const out = join(root, 'logs', 'release-smoke');
mkdirSync(out, { recursive: true });
if (flags.has('fresh')) rmSync(profile, { recursive: true, force: true });
const toggle = join(profile, 'engine-mode');
const logStart = existsSync(logFile) ? statSync(logFile).size : 0;
const linesBefore = logStart ? readFileSync(logFile).subarray(0, logStart).toString('utf8').split('\n').length - 1 : 0;
console.log(`release smoke: ${opts.exe}`);
console.log(`  profile ${profile}${flags.has('fresh') ? ' (deleted first: a first launch)' : ''}; engine toggle file: ${existsSync(toggle) ? JSON.stringify(readFileSync(toggle, 'utf8')) : 'none (engine by default)'}`);

// ── Helpers ───────────────────────────────────────────────────────────────────────────────────────
function ps(script) {
  // An encoded command reports progress as CLIXML on stderr: silence it, keep stdout.
  const encoded = Buffer.from(`$ProgressPreference = 'SilentlyContinue'\n${script}`, 'utf16le').toString('base64');
  return execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-EncodedCommand', encoded], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
}
function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}
async function cdpVersion() {
  try {
    const res = await fetch(`${cdpUrl}/json/version`, { signal: AbortSignal.timeout(1000) });
    return res.ok ? await res.json() : null;
  } catch {
    return null;
  }
}
/** This run's release log lines, each with its line number in the file. */
function runLog() {
  if (!existsSync(logFile)) return [];
  const buf = readFileSync(logFile);
  const rotated = buf.length < logStart; // KeepAll rotation started a new file mid-run
  const text = buf.subarray(rotated ? 0 : logStart).toString('utf8');
  const first = rotated ? 0 : linesBefore;
  return text.split(/\r?\n/).filter((l, i, all) => l || i < all.length - 1).map((line, i) => ({ n: first + i + 1, line }));
}
const cite = (e) => `bleeploop.log:${e.n} ${e.line.replace(/^\[[^\]]*\]\[[^\]]*\]/, '')}`;
async function waitLog(re, seconds) {
  for (let i = 0; i < seconds * 4; i++) {
    const hit = runLog().find((e) => re.test(e.line));
    if (hit) return hit;
    await sleep(250);
  }
  return null;
}
/** The OS close, as the close button does: WM_CLOSE to this process's main window only. */
function osClose(pid) {
  return ps(`$p = Get-Process -Id ${pid} -ErrorAction SilentlyContinue; if ($p -and $p.MainWindowHandle -ne 0) { $p.CloseMainWindow() } else { 'no window' }`);
}
async function waitExit(pid, ms) {
  const t0 = Date.now();
  while (alive(pid)) {
    if (Date.now() - t0 > ms) return null;
    await sleep(250);
  }
  return Date.now() - t0;
}
/** A capture of the app window by its handle (client area, full content), for when CDP cannot see it. */
function printWindow(pid, file) {
  return ps(`
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class LfPw {
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
}
"@
[LfPw]::SetProcessDPIAware() | Out-Null
$h = (Get-Process -Id ${pid}).MainWindowHandle
if ($h -eq 0) { 'no window'; exit }
$r = New-Object LfPw+RECT
[LfPw]::GetClientRect($h, [ref]$r) | Out-Null
$bmp = New-Object System.Drawing.Bitmap ([Math]::Max(1, $r.R - $r.L)), ([Math]::Max(1, $r.B - $r.T))
$g = [System.Drawing.Graphics]::FromImage($bmp)
$hdc = $g.GetHdc()
$ok = [LfPw]::PrintWindow($h, $hdc, 3)
$g.ReleaseHdc($hdc)
$bmp.Save('${file.replace(/'/g, "''")}', [System.Drawing.Imaging.ImageFormat]::Png)
"PrintWindow $ok $($bmp.Width)x$($bmp.Height)"`);
}

const results = [];
function report(name, ok, lines) {
  results.push({ name, ok });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}: ${lines[0]}`);
  for (const l of lines.slice(1)) console.log(`       ${l}`);
}
/** Run one check: `fn` returns its evidence lines, or throws with the reason. */
async function check(name, fn) {
  try {
    report(name, true, await fn());
    return true;
  } catch (e) {
    report(name, false, String(e instanceof Error ? e.message : e).split('\n'));
    return false;
  }
}
function must(ok, why) {
  if (!ok) throw Error(why);
}

// ── Launch ────────────────────────────────────────────────────────────────────────────────────────
const t0 = Date.now();
const child = spawn(opts.exe, [], {
  env: { ...process.env, WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${opts.port}` },
  detached: true,
  stdio: 'ignore',
});
child.unref();
const pid = child.pid;
console.log(`  launched pid ${pid}; CDP on ${cdpUrl}`);
let browser = null;
let exited = false;
process.once('SIGINT', () => {
  if (alive(pid)) execFileSync('taskkill', ['/T', '/F', '/PID', String(pid)], { stdio: 'ignore' });
  process.exit(130);
});

try {
  let version = null;
  for (let i = 0; i < opts.cdpWait * 2 && !version && alive(pid); i++) {
    version = await cdpVersion();
    if (!version) await sleep(500);
  }
  const args = ps(`Get-CimInstance Win32_Process -Filter "ParentProcessId=${pid} AND Name='msedgewebview2.exe'" | Where-Object { $_.CommandLine -notmatch '--type=' } | Select-Object -First 1 -ExpandProperty CommandLine`);
  console.log(`  webview2 browser: ${args.replace(/^"[^"]*"\s*/, '') || 'none found'}`);
  must(existsSync(logFile), `the exe did not write ${logFile}: it may not run in the profile this script expected`);
  if (!version) {
    await sleep(3000);
    const shot = join(out, 'cdp-failed.png');
    const pw = alive(pid) ? printWindow(pid, shot) : 'the app is gone';
    throw Error(`CDP did not answer on ${cdpUrl} within ${opts.cdpWait} s (app ${alive(pid) ? 'running' : 'exited'}; ${pw}${alive(pid) ? `, ${shot}` : ''})`);
  }
  console.log(`  CDP: ${version.Browser} after ${Date.now() - t0} ms`);

  browser = await chromium.connectOverCDP(cdpUrl);
  let page = null;
  for (let i = 0; i < 40 && !page; i++) {
    page = browser.contexts().flatMap((c) => c.pages()).find((p) => p.url().startsWith('http')) ?? null;
    if (!page) await sleep(250);
  }
  must(page !== null, 'CDP attached but shows no page');

  // Everything the page said before and after the attach: Log.enable and Runtime.enable replay what
  // the page logged before this session existed.
  const pageErrors = [];
  const cdp = await page.context().newCDPSession(page);
  cdp.on('Log.entryAdded', ({ entry }) => {
    if (entry.level === 'error' || /Content Security Policy/i.test(entry.text)) pageErrors.push(`log/${entry.source}: ${entry.text}`);
  });
  cdp.on('Runtime.exceptionThrown', ({ exceptionDetails: d }) => pageErrors.push(`uncaught: ${d.exception?.description ?? d.text}`));
  cdp.on('Runtime.consoleAPICalled', ({ type, args: a }) => {
    if (type === 'error') pageErrors.push(`console.error: ${a.map((x) => x.value ?? x.description ?? '').join(' ')}`);
  });
  await cdp.send('Log.enable');
  await cdp.send('Runtime.enable');

  // A sampler in the page: what the lanes, the beat LEDs, the meter and lane 1's count-in numeral show,
  // every 20 ms. Reads the DOM only.
  const startSampler = () =>
    page.evaluate(() => {
      const w = /** @type {any} */ (window);
      clearInterval(w.__releaseSmoke?.timer);
      const s = { states: [[], [], [], [], []], leds: [], meter: new Set(), meterMax: 0, counts: new Set() };
      s.timer = setInterval(() => {
        document.querySelectorAll('.lp-lane').forEach((el, i) => {
          const st = /** @type {HTMLElement} */ (el).dataset.state;
          const seq = s.states[i];
          if (seq && st && seq[seq.length - 1] !== st) seq.push(st);
        });
        const on = [...document.querySelectorAll('.transport__beat')].findIndex((l) => l.classList.contains('on'));
        if (s.leds[s.leds.length - 1] !== on) s.leds.push(on);
        const lvl = Number(/** @type {HTMLElement | null} */ (document.querySelector('.transport__inmeter'))?.style.getPropertyValue('--lvl') || 0);
        s.meter.add(lvl.toFixed(2));
        s.meterMax = Math.max(s.meterMax, lvl);
        const count = document.querySelectorAll('.lp-lane')[0]?.querySelector('.lp-lane__count')?.textContent;
        if (count) s.counts.add(count);
      }, 20);
      w.__releaseSmoke = s;
    });
  const readSampler = () =>
    page.evaluate(() => {
      const s = /** @type {any} */ (window).__releaseSmoke;
      return { states: s.states, leds: s.leds, meter: s.meter.size, meterMax: s.meterMax, counts: [...s.counts] };
    });
  /** Per-column ink of a lane's waveform canvas (alpha ≥ 100; full-height columns — grid, playhead — dropped). */
  const laneInk = (index) =>
    page.evaluate((i) => {
      const canvas = document.querySelectorAll('.lp-lane')[i]?.querySelector('canvas');
      if (!canvas || !canvas.width || !canvas.height) return null;
      const { width: w, height: h } = canvas;
      const d = /** @type {CanvasRenderingContext2D} */ (canvas.getContext('2d')).getImageData(0, 0, w, h).data;
      const cols = [];
      let playhead = -1;
      for (let x = 0; x < w; x++) {
        let n = 0;
        for (let y = 0; y < h; y++) if (d[(y * w + x) * 4 + 3] >= 100) n++;
        if (n >= 0.9 * h) {
          if (playhead < 0) playhead = x;
        } else cols.push(n);
      }
      const max = Math.max(0, ...cols);
      const tall = cols.filter((n) => n >= 0.1 * h).length;
      return { w, h, max, tall, distinct: new Set(cols).size, playhead };
    }, index);
  const diag = (label) =>
    page.evaluate(
      (l) =>
        [...document.querySelectorAll('.audio-settings__diag-row')].find((r) => r.querySelector('span')?.textContent === l)?.querySelector('b')?.textContent ?? null,
      label,
    );
  const engineRow = () => diag('engine');
  const openSettings = async () => {
    if (!(await page.locator('#lf-audio-popover').isVisible())) await page.locator('button.tool--gear').click();
    await page.locator('#lf-audio-popover').waitFor({ state: 'visible', timeout: 5000 });
  };
  const closeSettings = async () => {
    await page.keyboard.press('Escape');
    await page.locator('#lf-audio-popover').waitFor({ state: 'hidden', timeout: 5000 });
  };
  const laneState = (i) => page.locator('.lp-lane').nth(i).getAttribute('data-state');
  const pressed = (label) => page.locator(`button[aria-label="${label}"]`).getAttribute('aria-pressed');

  // ── boots ─────────────────────────────────────────────────────────────────────────────────────
  const booted = await check('boots', async () => {
    await page.waitForFunction(() => document.querySelectorAll('.lp-lane').length === 5 && document.querySelector('.transport__beats'), undefined, { timeout: 30_000 });
    await sleep(2000); // let the boot chain (device open, ASIO probe, scan start) log what it logs
    const dom = await page.evaluate(() => ({
      url: location.href,
      title: document.title,
      lanes: [...document.querySelectorAll('.lp-lane')].map((l) => /** @type {HTMLElement} */ (l).dataset.state).join(','),
      looperHeight: Math.round(document.querySelector('.lp')?.getBoundingClientRect().height ?? 0),
      textLength: document.body.innerText.length,
      isolated: self.crossOriginIsolated,
    }));
    await page.screenshot({ path: join(out, 'boot.png') });
    const logBad = runLog().filter((e) => /\[(csp|window\.error|unhandledrejection)\]/.test(e.line));
    must(dom.looperHeight > 100 && dom.textLength > 100, `the window looks blank: ${JSON.stringify(dom)}`);
    must(pageErrors.length === 0, `page errors: ${pageErrors.join(' | ')}`);
    must(logBad.length === 0, `release log: ${logBad.map(cite).join(' | ')}`);
    return [
      `${dom.url} "${dom.title}", lanes ${dom.lanes}, looper ${dom.looperHeight} px, crossOriginIsolated ${dom.isolated}`,
      `no page error or CSP report (CDP replay + live); no [csp]/[window.error]/[unhandledrejection] in the release log`,
      `screenshot ${join(out, 'boot.png')}`,
    ];
  });

  // ── engine ────────────────────────────────────────────────────────────────────────────────────
  const engined = booted && (await check('engine', async () => {
    await openSettings();
    const sw = await page.evaluate(() => {
      const box = /** @type {HTMLInputElement | null} */ (document.querySelector('input[aria-label="Use the native audio engine from the next launch"]'));
      return box ? { checked: box.checked, text: box.closest('label')?.querySelector('.audio-settings__toggle-text')?.textContent ?? null } : null;
    });
    must(sw !== null, 'Audio Settings shows no engine switch (platform.engine unavailable?)');
    must(sw.checked && sw.text === 'native', `the engine switch reads ${JSON.stringify(sw)}`);
    await page.waitForFunction(() => {
      const row = [...document.querySelectorAll('.audio-settings__diag-row')].find((r) => r.querySelector('span')?.textContent === 'engine');
      const text = row?.querySelector('b')?.textContent;
      return text && text !== 'no device open';
    }, undefined, { timeout: 30_000 });
    const readout = await engineRow();
    const host = await diag('host');
    const owns = await waitLog(/\[engine_io\] engine mode: the native engine owns the audio device/, 5);
    const opened = await waitLog(/\[engine_io\] \w+ running: /, 5);
    must(owns !== null, 'the release log has no "engine mode: the native engine owns the audio device" line');
    const asio = await page.evaluate(() => /** @type {HTMLInputElement | null} */ (document.querySelector('input[aria-label="Use ASIO low-latency audio"]'))?.checked ?? null);
    const buffer = await page.locator('select[aria-label="Buffer size in frames"]').inputValue();
    return [
      `switch "${sw.text}" (checked), host ${host}, engine row "${readout}"`,
      cite(owns),
      `opened by itself (not judged): ${opened ? cite(opened) : 'no "running" log line'}; ASIO toggle ${asio}, buffer select ${buffer}`,
    ];
  }));

  // ── device ────────────────────────────────────────────────────────────────────────────────────
  const deviced = engined && (await check('device', async () => {
    await openSettings();
    const asioBox = page.locator('input[aria-label="Use ASIO low-latency audio"]');
    must((await asioBox.count()) === 1, `no ASIO toggle: the row reads "${await page.locator('.audio-settings__soon').first().textContent().catch(() => '?')}"`);
    const asioWasOn = await asioBox.isChecked();
    if (!asioWasOn) await asioBox.check();
    await page.waitForFunction(() => document.querySelector('input[aria-label="Use ASIO low-latency audio"]')?.closest('label')?.textContent?.includes('low-latency'), undefined, { timeout: 20_000 });
    const driver = await page.locator('select[aria-label="Audio input device"] option').first().textContent();
    must(/focusrite/i.test(driver ?? ''), `the ASIO driver is "${driver}", not the Focusrite (Audio Settings offers no driver pick)`);
    const channel = page.locator('select[aria-label="Input channel"]');
    let channelNote = 'the UI offers no channel pick';
    if ((await channel.count()) === 1) {
      await channel.selectOption(String(opts.channel));
      channelNote = `channel "${await channel.locator('option:checked').textContent()}" picked`;
    }
    const bufferSel = page.locator('select[aria-label="Buffer size in frames"]');
    const bufferWas = await bufferSel.inputValue();
    await bufferSel.selectOption(String(opts.buffer));
    const want = `${opts.buffer} frames`;
    await page.waitForFunction(
      (w) => {
        const row = [...document.querySelectorAll('.audio-settings__diag-row')].find((r) => r.querySelector('span')?.textContent === 'engine');
        const text = row?.querySelector('b')?.textContent ?? '';
        return text.startsWith('ASIO') && text.includes(` ${w} `);
      },
      want,
      { timeout: 20_000 },
    );
    const readout = await engineRow();
    const reopen = await waitLog(new RegExp(`\\[engine_io\\] Asio running: .*${opts.buffer}-frame blocks`), 10);
    const request = runLog().filter((e) => /\[engine_io\] open requested: /.test(e.line)).pop();
    must(/focusrite/i.test(readout ?? ''), `the engine row names no Focusrite device: "${readout}"`);
    must(reopen !== null, `the release log shows no ASIO reopen at ${opts.buffer} frames`);
    await closeSettings();
    return [
      `ASIO ${asioWasOn ? 'already on' : 'turned on'}, driver "${driver}", ${channelNote}, buffer ${bufferWas} → ${opts.buffer}`,
      `engine row "${readout}"`,
      ...(request ? [cite(request)] : []),
      cite(reopen),
    ];
  }));

  // ── take (and the feed, sampled across it) ────────────────────────────────────────────────────
  let feedEvidence = null;
  const taken = deviced && (await check('take', async () => {
    const control = await laneInk(2);
    await startSampler();
    if ((await pressed('Metronome click')) !== 'true') await page.locator('button[aria-label="Metronome click"]').click();
    await page.waitForFunction(() => document.querySelector('button[aria-label="Metronome click"]')?.getAttribute('aria-pressed') === 'true', undefined, { timeout: 5000 });
    if ((await pressed('Fixed take length')) !== 'true') await page.locator('button[aria-label="Fixed take length"]').click();
    await page.waitForFunction(() => document.querySelector('button[aria-label="Fixed take length"]')?.getAttribute('aria-pressed') === 'true', undefined, { timeout: 5000 });
    for (let i = 0; i < 16; i++) {
      const bars = Number.parseInt((await page.locator('.transport__bars-val').textContent()) ?? '', 10);
      if (bars === 1) break;
      await page.locator(`button[aria-label="${bars > 1 ? 'Fewer bars' : 'More bars'}"]`).click();
    }
    const fixed = (await page.locator('button[aria-label="Fixed take length"]').textContent())?.trim();
    must(fixed === 'FIXED 1', `the take length reads "${fixed}"`);
    if ((await pressed('Mic / line input')) !== 'true') await page.locator('button[aria-label="Mic / line input"]').click();
    await page.waitForFunction(() => document.querySelector('button[aria-label="Mic / line input"]')?.getAttribute('aria-pressed') === 'true', undefined, { timeout: 5000 });
    await sleep(3000); // the idle window: LEDs and the meter before any take
    const idle = await readSampler();

    const tRec = Date.now();
    await page.locator('.lp-lane').nth(0).locator('.lp-core').click();
    await page.waitForFunction(() => document.querySelectorAll('.lp-lane')[0]?.getAttribute('data-state') === 'play', undefined, { timeout: 20_000 });
    const recMs = Date.now() - tRec;
    await page.locator('button[aria-label="Mic / line input"]').click(); // MIC off: the loop plays on
    await sleep(600);
    const inkA = await laneInk(0);
    await sleep(400);
    const inkB = await laneInk(0);
    await page.screenshot({ path: join(out, 'after-take.png') });
    const seen = await readSampler();
    const lane1 = seen.states[0];
    const order = ['armed', 'rec', 'play'].map((s) => lane1.indexOf(s));
    feedEvidence = { idle, seen, inkA, inkB };
    must(order.every((k, i) => k >= 0 && (i === 0 || k > order[i - 1])), `lane 1 went ${lane1.join(' → ')}`);
    must(inkA !== null && control !== null, 'a lane canvas could not be read');
    must(inkA.max >= 0.15 * inkA.h && inkA.tall >= 4, `lane 1's waveform is flat: tallest column ${inkA.max}/${inkA.h} px, ${inkA.tall} columns ≥ 10 %`);
    must(inkA.max > control.max, `lane 1 draws no more than the empty lane 3 (${inkA.max} vs ${control.max} px)`);
    return [
      `lane 1 ${lane1.join(' → ')} in ${recMs} ms (count-in numerals ${seen.counts.join(',') || 'none'}), ${fixed}, CLICK on, MIC live for the take`,
      `waveform: tallest column ${inkA.max}/${inkA.h} px, ${inkA.tall} columns ≥ 10 % of the height, ${inkA.distinct} distinct heights; empty lane 3: tallest ${control.max} px`,
      `screenshot ${join(out, 'after-take.png')}`,
    ];
  }));

  // ── feed ──────────────────────────────────────────────────────────────────────────────────────
  if (taken) {
    await check('feed', async () => {
      const { idle, seen, inkA, inkB } = feedEvidence;
      const ledMoves = (leds) => leds.filter((k) => k >= 0).length;
      const moved = {
        leds: ledMoves(seen.leds) >= 4,
        meter: seen.meter > 3 && seen.meterMax > 0,
        playhead: inkA.playhead >= 0 && inkB.playhead >= 0 && inkA.playhead !== inkB.playhead,
      };
      must(Object.values(moved).some(Boolean), `nothing moved: ${JSON.stringify({ leds: seen.leds, meter: seen.meter, playhead: [inkA.playhead, inkB.playhead] })}`);
      return [
        `moved: ${Object.entries(moved).filter(([, v]) => v).map(([k]) => k).join(', ')}${Object.values(moved).every(Boolean) ? '' : `; still: ${Object.entries(moved).filter(([, v]) => !v).map(([k]) => k).join(', ')}`}`,
        `beat LEDs: ${ledMoves(idle.leds)} lit steps in the 3 s before the take (${idle.leds.slice(0, 12).join(',')}), ${ledMoves(seen.leds)} over the run`,
        `record meter: ${idle.meter} distinct levels before the take (max ${idle.meterMax.toFixed(2)}), ${seen.meter} over the run (max ${seen.meterMax.toFixed(2)})`,
        `lane 1 playhead: x=${inkA.playhead} then x=${inkB.playhead} 400 ms later`,
      ];
    });
  }

  // ── exit ──────────────────────────────────────────────────────────────────────────────────────
  await check('exit', async () => {
    const clearAll = page.locator('button.transport__tgl', { hasText: '✕' });
    await clearAll.click();
    await page.locator('button[aria-label="Clear all tracks, press again to confirm"]').click({ timeout: 3000 }).catch(() => {});
    await page.waitForFunction(() => [...document.querySelectorAll('.lp-lane')].every((l) => l.getAttribute('data-state') === 'empty'), undefined, { timeout: 10_000 });
    const lanes = (await Promise.all([0, 1, 2, 3, 4].map(laneState))).join(',');
    const sent = osClose(pid);
    must(sent === 'True', `the OS close was not sent: ${sent}`);
    const ms = await waitExit(pid, EXIT_WAIT_MS);
    must(ms !== null, `app.exe was still running ${EXIT_WAIT_MS / 1000} s after the OS close`);
    exited = true;
    await sleep(500);
    const lines = runLog();
    const errors = lines.filter((e) => /\]\[ERROR\]/.test(e.line));
    const warns = lines.filter((e) => /\]\[WARN\]/.test(e.line));
    const down = lines.find((e) => /engine mode shut down/.test(e.line));
    must(errors.length === 0, `release log ERROR lines: ${errors.map(cite).join(' | ')}`);
    return [
      `lanes ${lanes} after CLEAR ALL; the process exited ${ms} ms after the OS close`,
      `release log: ${lines.length} lines this run, 0 ERROR, ${warns.length} WARN${warns.length ? ` (${warns.map(cite).join(' | ')})` : ''}`,
      down ? cite(down) : 'no "engine mode shut down" line',
    ];
  });
} catch (e) {
  report('run', false, [String(e instanceof Error ? e.message : e)]);
} finally {
  if (!exited && alive(pid)) {
    // A failed run: the close guard may hold a take; give the OS close a moment, then stop this pid's tree.
    try {
      osClose(pid);
    } catch {
      // the window may already be gone
    }
    if ((await waitExit(pid, 15_000)) === null) {
      execFileSync('taskkill', ['/T', '/F', '/PID', String(pid)], { stdio: 'ignore' });
      console.log(`  stopped app.exe pid ${pid}`);
    }
  }
  await browser?.close().catch(() => {});
  const slice = runLog().map((e) => e.line).join('\n');
  writeFileSync(join(out, 'release.log'), `${slice}\n`);
  console.log(`  this run's release log: ${join(out, 'release.log')} (${logFile}, from line ${linesBefore + 1})`);
}

const ok = results.length > 0 && results.every((r) => r.ok) && ['boots', 'engine', 'device', 'take', 'feed', 'exit'].every((n) => results.some((r) => r.name === n));
console.log(`\n=== release-smoke: ${ok ? 'PASS' : 'FAIL'}: ${results.filter((r) => r.ok).length}/${results.length} checks passed (${Math.round((Date.now() - t0) / 1000)} s) ===`);
process.exit(ok ? 0 : 1);
