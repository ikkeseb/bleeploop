/**
 * DEV probe: does the rig come back after a restart, and does a launch that dies while restoring
 * stop the recall at the next launch? `pnpm native:recall` launches the app once per phase
 * (`VITE_LF_PROBE_PHASE`), in this order:
 *   save   loads the two plugins into slots 0 and 1 through `selectPlugin`, sets the input channel
 *          through the Audio Settings store, and closes the app the way the close button does
 *   check  the recall restored both slots (frontend state and the host's own list), the channel
 *          survived, nothing is armed and no arm or editor call was made; then the same again after
 *          a WebView reload; puts the channel back (the owner's Audio Settings share this dev
 *          origin), closes
 *   crash  prints `in flight` once the recall's marker is stored; the runner kills app.exe on that
 *          line, as a plugin that crashes the host would
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
import { availablePlugins, selectPlugin, slotPlugins } from '../audio/instrument';
import { inputArmed, monitorArmed } from '../audio/native-io';
import { readAudioDeviceSettings, writeAudioDeviceSettings } from '../audio/audio-settings';
import { pluginDescriptorKey } from '../audio/plugin-descriptor';
import { recallInFlight, rigRecallDone } from '../audio/rig-recall';
import { toasts } from '../notify';
import { confirmNativeClose, platform, type PluginDescriptor } from '../platform';

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error('[recall]', ...args);

/** Set by the check phase's first document, so the reloaded one knows it is the second pass. */
const RELOADED = 'recallProbe.afterRestart';

interface Expect {
  slot0: string;
  slot1: string;
  channel: string;
  ownerChannel: string;
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
  writeAudioDeviceSettings({ inputChannel: channel });
  const expect: Expect = { slot0: want[0], slot1: want[1], channel, ownerChannel };
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
 * After the restart, then once more after a WebView reload in the same launch (STATUS Stop 5's path:
 * `resyncNativeSlots` unloads the stranded plugins, the recall loads them again). `early` = the probe
 * was running before this document's recall finished, so its spy saw every call.
 */
async function check(expect: Expect, early: boolean): Promise<void> {
  const finished = await recallFinished();
  const firstPass = sessionStorage.getItem(RELOADED);
  const started = !finished
    ? ['the recall did not finish within 3 min']
    : !early
      ? ['the probe started after the recall had finished: its spy saw nothing']
      : [];
  if (firstPass === null) {
    const channel = readAudioDeviceSettings().inputChannel;
    const problems = started.length ? started : await restoreProblems(expect);
    if (channel !== expect.channel) problems.push(`channel "${channel}", saved "${expect.channel}"`);
    if (problems.length) {
      writeAudioDeviceSettings({ inputChannel: expect.ownerChannel });
      return log(`FAIL after the restart: ${problems.join('; ')}`);
    }
    sessionStorage.setItem(RELOADED, `channel ${channel}`);
    location.reload();
    return new Promise(() => {}); // this document is going away; the reloaded one finishes the phase
  }
  sessionStorage.removeItem(RELOADED);
  writeAudioDeviceSettings({ inputChannel: expect.ownerChannel });
  const problems = started.length ? started : await restoreProblems(expect);
  if (problems.length) return log(`FAIL after the WebView reload: ${problems.join('; ')}`);
  const names = slotPlugins().map((d) => `${d?.name} (${d?.format})`);
  log(`restored: slot 1 ${names[0]}, slot 2 ${names[1]} after the restart (${firstPass}) and after a WebView reload; nothing armed, no arm or editor call`);
}

async function crash(): Promise<void> {
  const t0 = performance.now();
  while (!recallInFlight()) {
    if (rigRecallDone()) return log('FAIL the recall finished without storing its marker');
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
  // Every other phase ends the way the close button does (an empty jam closes without asking), so
  // what it stored reaches the next launch as it would for the owner; a FAIL closes too.
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
