/**
 * DEV probe: does the rig come back after a restart, and does a launch that dies while restoring
 * stop the recall at the next launch? `pnpm native:recall` launches the app once per phase
 * (`VITE_LF_PROBE_PHASE`), in this order:
 *   save   puts back the owner's input channel if an earlier run failed before doing so; starts
 *          from nothing restored (a record an earlier failed run left is unloaded, which forgets it,
 *          and the phase fails: run again), loads the two plugins into slots 0 and 1 through
 *          `selectPlugin`, sets the input channel through the Audio Settings store, and approves its
 *          own close (`confirmNativeClose`)
 *   check  the recall restored both slots (frontend state and the host's own list), the channel
 *          survived, nothing is armed and no arm or editor call was made; then the same again after
 *          a WebView reload, judged once the recalled loads are back while the marker is still
 *          stored; puts the channel back (the owner's Audio Settings share this dev origin). The
 *          runner then closes the app the way its close button does (STATUS Stop 5's close inside
 *          the settle window), and this page notes whether the marker was still stored when that
 *          close arrived. The close guard's jam question is answered OK and its recovery save
 *          skipped (on any verdict): this dev origin may hold the owner's recovery
 *   crash  that close came inside the settle window and the close guard cleared the marker, so the
 *          recall runs again; prints `in flight` once it has stored its marker and slot 0's load is
 *          under way, and the runner kills app.exe on that line, as a plugin that crashes the host
 *          would
 *   skip   nothing restored, in either slot or the host, and one "Plugins not restored" toast; closes
 *   after  a clean launch: nothing restored, no toast; closes
 * The record lives under this probe's own keys (`src/audio/rig-recall.ts`), never the owner's.
 * `[recall]` lines go through `console.error` (→ the same log as the Rust host); the runner counts
 * each phase's `[rig-recall]` lines itself.
 *
 * Trigger: `VITE_LF_PROBE=recall-restart` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_PHASE`    the phase (set by the runner)
 *   `VITE_LF_PROBE_PLUGINS`  `<name>:<format>,<name>:<format>` for slots 0 and 1 (default
 *                            `Surge XT Effects:clap,Surge XT:vst3`: an effect and an instrument)
 *   `VITE_LF_PROBE_EXPECT`   what `save` picked, handed over by the runner (JSON)
 */
import { autosave } from '../audio/autosave';
import { availablePlugins, clearPlugin, selectPlugin, slotPendingCounts, slotPlugins } from '../audio/instrument';
import { inputArmed, monitorArmed } from '../audio/native-io';
import { readAudioDeviceSettings, writeAudioDeviceSettings } from '../audio/audio-settings';
import { pluginDescriptorKey } from '../audio/plugin-descriptor';
import { recallInFlight, rigRecallDone } from '../audio/rig-recall';
import { toasts } from '../notify';
import { confirmNativeClose, onNativeCloseRequested, platform, type PluginDescriptor } from '../platform';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error('[recall]', ...args);

/** Set by the check phase's first document, so the reloaded one knows it is the second pass. */
const RELOADED = 'recallProbe.afterRestart';
/** Where the check phase's close found the marker, handed to the crash phase (localStorage). */
const CLOSED = 'recallProbe.checkClose';
/** The owner's input channel while the probe's own is set (localStorage), so a run that fails between
 * save and check has it put back by the next run's save. */
const OWNER_CHANNEL = 'recallProbe.ownerChannel';

function putBackOwnerChannel(): void {
  const owner = localStorage.getItem(OWNER_CHANNEL);
  if (owner === null) return;
  writeAudioDeviceSettings({ inputChannel: owner });
  localStorage.removeItem(OWNER_CHANNEL);
}

interface Expect {
  slot0: string;
  slot1: string;
  channel: string;
}

/** Arm and editor calls made from this module's load on, so a recall that arms shows up. */
const calls: string[] = [];
function spy(): void {
  const host = platform.pluginHost;
  const armInput = host.armInput.bind(host);
  const armMonitor = host.armMonitor.bind(host);
  const openEditor = host.openEditor.bind(host);
  host.armInput = (slot, ...rest) => (calls.push(`armInput ${slot}`), armInput(slot, ...rest));
  host.armMonitor = (slot, ...rest) => (calls.push(`armMonitor ${slot}`), armMonitor(slot, ...rest));
  host.openEditor = (slot, mode) => (calls.push(`openEditor ${slot}`), openEditor(slot, mode));
}

async function recallFinished(): Promise<boolean> {
  for (let i = 0; i < 1800 && !rigRecallDone(); i++) await sleep(100);
  return rigRecallDone();
}

/** The loaded plugin keys as the frontend and the native host each see them. */
async function loaded(): Promise<{ frontend: (string | null)[]; host: (string | null)[] }> {
  const host: (string | null)[] = [null, null];
  for (const { slot, descriptor } of await platform.pluginHost.listLoaded()) host[slot] = pluginDescriptorKey(descriptor);
  return { frontend: slotPlugins().map((d) => (d ? pluginDescriptorKey(d) : null)), host };
}

function pickPlugins(): PluginDescriptor[] | string {
  const spec = (import.meta.env.VITE_LF_PROBE_PLUGINS as string | undefined) ?? 'Surge XT Effects:clap,Surge XT:vst3';
  const picks: PluginDescriptor[] = [];
  for (const entry of spec.split(',')) {
    const [name, format] = entry.split(':').map((s) => s.trim());
    const desc = availablePlugins().find((d) => d.name === name && d.format === format);
    if (!desc) return `no scanned plugin "${name}" (${format})`;
    picks.push(desc);
  }
  return picks.length === 2 ? picks : `need two plugins, got "${spec}"`;
}

async function save(): Promise<void> {
  localStorage.removeItem(CLOSED);
  putBackOwnerChannel();
  // Only loads made from nothing prove that a load is remembered: `selectPlugin` returns early for a
  // plugin already in its slot, so a restored record would pass the check phase on its own.
  const stale = await loaded();
  if (stale.frontend.some(Boolean) || stale.host.some(Boolean)) {
    await clearPlugin(0);
    await clearPlugin(1);
    return log(`FAIL this launch restored a record an earlier run left (${JSON.stringify(stale)}); unloaded, which forgets it: run again`);
  }
  const picks = pickPlugins();
  if (typeof picks === 'string') return log(`FAIL ${picks}`);
  await selectPlugin(0, picks[0]);
  await selectPlugin(1, picks[1]);
  const now = await loaded();
  const want = picks.map(pluginDescriptorKey);
  if (now.frontend.join() !== want.join() || now.host.join() !== want.join()) {
    return log(`FAIL the loads did not land: ${JSON.stringify(now)}`);
  }
  const ownerChannel = readAudioDeviceSettings().inputChannel;
  const channel = ownerChannel === '1' ? '0' : '1';
  localStorage.setItem(OWNER_CHANNEL, ownerChannel);
  writeAudioDeviceSettings({ inputChannel: channel });
  const expect: Expect = { slot0: want[0], slot1: want[1], channel };
  log(`saved: ${JSON.stringify(expect)}`);
}

/** What is wrong with a restored rig: both slots, in the frontend and the host, unarmed, no toast. */
async function restoreProblems(expect: Expect): Promise<string[]> {
  const now = await loaded();
  const armed = [...inputArmed(), ...monitorArmed()];
  const monitorLatency = [await platform.pluginHost.monitorLatencySeconds(0), await platform.pluginHost.monitorLatencySeconds(1)];
  const shown = toasts().map((t) => t.message);
  const want = [expect.slot0, expect.slot1].join();
  return [
    now.frontend.join() !== want && `frontend slots ${JSON.stringify(now.frontend)}`,
    now.host.join() !== want && `host slots ${JSON.stringify(now.host)}`,
    armed.some(Boolean) && `armed ${JSON.stringify(armed)}`,
    monitorLatency.some((s) => s !== 0) && `monitor latency ${JSON.stringify(monitorLatency)}`,
    calls.length > 0 && `calls ${calls.join(', ')}`,
    shown.length > 0 && `toasts ${JSON.stringify(shown)}`,
  ].filter((p): p is string => typeof p === 'string');
}

/**
 * Wait until the recall's loads are back while its marker is still stored, i.e. the settle window
 * (true), or until the recall finished without one (false).
 */
async function settleWindow(): Promise<boolean> {
  for (let i = 0; i < 9000; i++) {
    if (rigRecallDone()) return false;
    if (recallInFlight() && slotPlugins().every(Boolean) && slotPendingCounts().every((n) => n === 0)) return true;
    await sleep(20);
  }
  return false;
}

/**
 * After the restart, then once more after a WebView reload in the same launch (STATUS Stop 5's path:
 * `resyncNativeSlots` unloads the stranded plugins, the recall loads them again), judged inside the
 * settle window so the runner's close lands there. `early` = the probe was running before this
 * document's recall finished, so its spy saw every call.
 */
async function check(expect: Expect, early: boolean): Promise<void> {
  // The runner closes this phase the way the close button does, on any verdict. A jam this dev origin
  // restored from its recovery makes the close guard ask first, in a dialog that blocks the page:
  // answer it as the owner would, and leave that recovery (maybe the owner's) unwritten.
  window.confirm = () => true;
  Object.assign(autosave, { flush: async () => {} });
  const firstPass = sessionStorage.getItem(RELOADED);
  const notEarly = early ? [] : ['the probe started after the recall had finished: its spy saw nothing'];
  if (firstPass === null) {
    const started = !(await recallFinished()) ? ['the recall did not finish within 3 min'] : notEarly;
    const channel = readAudioDeviceSettings().inputChannel;
    const problems = started.length ? started : await restoreProblems(expect);
    if (channel !== expect.channel) problems.push(`channel "${channel}", saved "${expect.channel}"`);
    if (problems.length) {
      putBackOwnerChannel();
      return log(`FAIL after the restart: ${problems.join('; ')}`);
    }
    sessionStorage.setItem(RELOADED, `channel ${channel}`);
    location.reload();
    return new Promise(() => {}); // this document is going away; the reloaded one finishes the phase
  }
  sessionStorage.removeItem(RELOADED);
  putBackOwnerChannel();
  // Registered before the verdict: the runner's close follows that line within a second or so.
  onNativeCloseRequested(() => {
    const where = recallInFlight() ? 'inside the settle window' : 'after the settle window';
    localStorage.setItem(CLOSED, where);
    log(`close request reached the page ${where}`);
  });
  const inWindow = await settleWindow();
  const problems = notEarly.length ? notEarly : await restoreProblems(expect);
  if (!problems.length && !inWindow) problems.push('the recall finished without a settle window to close in');
  if (problems.length) return log(`FAIL after the WebView reload: ${problems.join('; ')}`);
  const names = slotPlugins().map((d) => `${d?.name} (${d?.format})`);
  log(`restored: slot 1 ${names[0]}, slot 2 ${names[1]} after the restart (${firstPass}) and after a WebView reload; nothing armed, no arm or editor call; closing inside the settle window`);
}

async function crash(): Promise<void> {
  const t0 = performance.now();
  const closed = localStorage.getItem(CLOSED);
  localStorage.removeItem(CLOSED);
  if (closed !== 'inside the settle window') return log(`FAIL the check phase's close: ${closed ?? 'no close request reached its page'}`);
  // This launch's marker, with slot 0's load under way: a marker left over from the check phase is
  // stored from the start too, but then the recall skips and never loads.
  while (!(recallInFlight() && slotPendingCounts()[0] > 0)) {
    if (rigRecallDone()) {
      const shown = toasts().map((t) => t.message);
      return log(`FAIL the recall finished without loading under a marker of its own; toasts ${JSON.stringify(shown)}`);
    }
    await sleep(5);
  }
  const slots = slotPlugins().map((d) => d?.name ?? null);
  log(`in flight: marker seen ${Math.round(performance.now() - t0)} ms into the probe, slots ${JSON.stringify(slots)}`);
}

/** The skip and after phases: nothing loaded anywhere, the marker gone, `verdict` judges the toasts. */
async function nothingRestored(verdict: (shown: string[]) => string | null): Promise<void> {
  const now = await loaded();
  const shown = toasts().map((t) => `${t.message} x${t.count}`);
  const problems = [
    now.frontend.some(Boolean) && `frontend slots ${JSON.stringify(now.frontend)}`,
    now.host.some(Boolean) && `host slots ${JSON.stringify(now.host)}`,
    recallInFlight() && 'the marker is still stored',
    calls.length > 0 && `calls ${calls.join(', ')}`,
  ].filter(Boolean);
  const line = verdict(shown);
  if (problems.length || !line) return log(`FAIL ${[...problems, `toasts ${JSON.stringify(shown)}`].join('; ')}`);
  log(line);
}

export async function runRecallRestart(): Promise<void> {
  const phase = import.meta.env.VITE_LF_PROBE_PHASE as string | undefined;
  if (phase === 'crash') return crash();
  spy();
  await runPhase(phase);
  // The runner closes the check phase through the OS close. Every other phase approves its own close
  // (an empty jam closes without asking), so what it stored reaches the next launch as it would for
  // the owner; a FAIL closes too.
  if (phase === 'check') return;
  await sleep(500);
  await confirmNativeClose();
}

async function runPhase(phase: string | undefined): Promise<void> {
  const early = !rigRecallDone();
  if (phase === 'check') {
    const raw = import.meta.env.VITE_LF_PROBE_EXPECT as string | undefined;
    if (!raw) return log('FAIL no VITE_LF_PROBE_EXPECT handed over from the save phase');
    return check(JSON.parse(raw) as Expect, early);
  }
  if (!(await recallFinished())) return log('FAIL the recall did not finish within 3 min');
  if (phase === 'save') {
    sessionStorage.removeItem(RELOADED);
    return save();
  }
  if (phase === 'skip') {
    return nothingRestored((shown) =>
      shown.length === 1 && shown[0] === 'Plugins not restored x1' ? `skipped: nothing restored, one toast "${shown[0]}"` : null);
  }
  if (phase === 'after') return nothingRestored((shown) => (shown.length === 0 ? 'clean: nothing restored, no toast' : null));
  log(`FAIL unknown phase "${phase}"`);
}
