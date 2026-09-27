/**
 * DEV probe: tone recall on the engine (`src-tauri/src/host/tone.rs`) — a plugin's settings survive a
 * restart and ride in a session export. `pnpm native:tone-recall` launches the WASAPI dev app in a
 * profile of its own (engine mode on) once per phase, in this order:
 *   save   starts from empty slots (a record an earlier failed run left is unloaded, which forgets it,
 *          and the phase fails: run again); loads the two plugins into slots A and B through
 *          `selectPlugin`, moves the first parameters of each through the host's set_param, unloads
 *          and loads both, and picks on each the first parameter that came back exactly as set and
 *          away from its default (a continuous one); moves it once more, to v, and approves its own
 *          close within a second of that last set, so v reaches the store on the exit path alone
 *   check  rig recall brought both back, each load answering `tone: 'restored'` and each parameter at v.
 *          Then the session path: GO LIVE on slot A, a FIXED 1-bar take on lane 1, an export whose zip
 *          carries a tone file per slot and names both plugins in session.json; CLEAR ALL, both
 *          parameters moved to w, the zip imported: both slots reload and read v again, slot A is live
 *          again. CLEAR ALL, slot B moved to w and unloaded, the zip imported again: one toast names
 *          slot B's plugin and slot B stays empty; loading that plugin then reads v. Last, CLEAR ALL,
 *          both parameters moved to w2, and the probe waits past the save debounce: the runner kills
 *          app.exe, as a crash would
 *   after  the relaunch brings both back at w2 (the debounced save, no exit path); unloads both, which
 *          forgets the record so the next run starts empty, and closes
 * The record lives under this probe's own keys (`src/audio/rig-recall.ts`), never the owner's, and the
 * tones in this profile's own store. It hears nothing, and it cannot reach a plugin's own editor: an
 * edit made there is the Rust fixtures' (`clap_restart_fixture.rs`, `vst3_restart_fixture.rs`).
 * Each launch mutes the master (for that launch only), so the take's count-in and the live slot stay
 * off the speakers. `[tone]` lines go through `console.error`; the runner fails the run on any other.
 *
 * Trigger: `VITE_LF_PROBE=tone-recall` at Vite start (DEV only). Knobs:
 *   `VITE_LF_PROBE_PHASE`    the phase (set by the runner)
 *   `VITE_LF_PROBE_PLUGINS`  `<name>:<format>,<name>:<format>` for slots A and B (default
 *                            `Pro-Q 3:vst3,Surge XT Effects:clap`: two effects, one per format; the
 *                            same plugin twice checks that each slot keeps a tone of its own)
 *   `VITE_LF_PROBE_EXPECT`   what the previous phase set, handed over by the runner (JSON)
 */
import { autosave } from '../audio/autosave';
import { buildExportBundle } from '../audio/export/export';
import { importSession } from '../audio/export/import';
import { parseZip } from '../audio/export/unzip';
import { availablePlugins, clearPlugin, nativeHostReady, selectPlugin, slotPendingCounts, slotPlugins } from '../audio/instrument';
import { goLive, inputArmed } from '../audio/native-io';
import { pluginDescriptorKey } from '../audio/plugin-descriptor';
import { rigRecallDone } from '../audio/rig-recall';
import { restoreSessionTones } from '../audio/slot-tones';
import { toasts } from '../notify';
import { confirmNativeClose, engineMode, platform, type PluginDescriptor, type PluginInfo } from '../platform';
import { clock, looper, master, session } from '../ui/state/audio';
import { engineDevice } from '../ui/state/engine-store';

const TAG = '[tone]';
const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));
const log = (...args: unknown[]) => console.error(TAG, ...args);
/** The native owner saves a tone this long after the last change (`tone::SAVE_QUIET`). */
const SAVE_QUIET_MS = 2000;

async function until(label: string, predicate: () => boolean | Promise<boolean>, seconds: number): Promise<void> {
  for (let i = 0; i < seconds * 10; i++) {
    if (await predicate()) return;
    await sleep(100);
  }
  throw Error(`timed out after ${seconds} s waiting for ${label}`);
}

function check(ok: boolean, what: string): void {
  if (!ok) throw Error(what);
}

/** One slot's probed parameter: its id, minimum, range and default, and the value it holds. */
interface Probed {
  id: number;
  name: string;
  min: number;
  range: number;
  default: number;
  value: number;
}
type Expect = [Probed, Probed];

/** Every load's answer from this module's load on (the rig recall's included). */
const loads: { slot: number; name: string; tone: PluginInfo['tone'] }[] = [];
function spy(): void {
  const host = platform.pluginHost;
  const load = host.loadPlugin.bind(host);
  host.loadPlugin = async (slot, ...rest) => {
    const info = await load(slot, ...rest);
    loads.push({ slot, name: info.descriptor.name, tone: info.tone });
    return info;
  };
}

function pickPlugins(): [PluginDescriptor, PluginDescriptor] | string {
  const spec = (import.meta.env.VITE_LF_PROBE_PLUGINS as string | undefined) ?? 'Pro-Q 3:vst3,Surge XT Effects:clap';
  const picks: PluginDescriptor[] = [];
  for (const entry of spec.split(',')) {
    const [name, format] = entry.split(':').map((s) => s.trim());
    const desc = availablePlugins().find((d) => d.name === name && d.format === format);
    if (!desc) return `no scanned plugin "${name}" (${format})`;
    picks.push(desc);
  }
  return picks.length === 2 ? [picks[0], picks[1]] : `need two plugins, got "${spec}"`;
}

/** The plugin's current value of parameter `id`, through the host's list_params. */
async function read(slot: 0 | 1, id: number): Promise<number> {
  const param = (await platform.pluginHost.listParams(slot)).find((p) => p.id === id);
  if (!param) throw Error(`slot ${slot === 0 ? 'A' : 'B'} lists no parameter ${id}`);
  return param.value;
}

const near = (a: number, b: number, range: number) => Math.abs(a - b) <= 1e-4 * Math.max(range, 1e-9);

/** Set `p` to `fraction` of its range; returns what the plugin reads back. */
async function set(slot: 0 | 1, p: Probed, fraction: number): Promise<number> {
  await platform.pluginHost.setParameter(slot, p.id, p.min + fraction * p.range);
  await sleep(300);
  return read(slot, p.id);
}

/**
 * Move the first eight listed parameters that have a range to 0.37 of it (0.71 where the default sits
 * near 0.37); returns them with the value each was set to.
 */
async function moveCandidates(slot: 0 | 1): Promise<Probed[]> {
  const moved: Probed[] = [];
  for (const param of (await platform.pluginHost.listParams(slot)).slice(0, 8)) {
    const range = param.maxValue - param.minValue;
    if (!(range > 0)) continue;
    const fraction = Math.abs((param.defaultValue - param.minValue) / range - 0.37) > 0.1 ? 0.37 : 0.71;
    const p: Probed = { id: param.id, name: param.name, min: param.minValue, range, default: param.defaultValue, value: param.minValue + fraction * range };
    await platform.pluginHost.setParameter(slot, p.id, p.value);
    moved.push(p);
  }
  await sleep(300);
  return moved;
}

/**
 * The parameter the later phases watch on `slot`: the first of `moved` that held its value exactly
 * across the unload and load just done (the unload saved the tone, the load restored it) and sits
 * clearly away from its default, so a load at the defaults can never pass for a restore. Exactly: a
 * stepped parameter snaps inside the plugin while a VST3 controller echoes the host's own set, so only
 * a continuous one reads back what was set.
 */
async function surviving(slot: 0 | 1, moved: Probed[]): Promise<Probed> {
  const now = await platform.pluginHost.listParams(slot);
  for (const p of moved) {
    const value = now.find((q) => q.id === p.id)?.value;
    if (value !== undefined && near(value, p.value, p.range) && Math.abs(value - p.default) > 1e-2 * p.range) return { ...p, value };
  }
  const seen = moved.map((p) => `${p.name} set ${p.value}, reads ${now.find((q) => q.id === p.id)?.value}`);
  throw Error(`no parameter on slot ${slot === 0 ? 'A' : 'B'} held its value across an unload and a load: ${seen.join('; ')}`);
}

/** Move `p` on `slot` to a value clearly away from each of `avoid`, exactly; returns that value. */
async function moveAway(slot: 0 | 1, p: Probed, avoid: number[]): Promise<number> {
  for (const fraction of [0.12, 0.88, 0.55]) {
    const value = await set(slot, p, fraction);
    if (near(value, p.min + fraction * p.range, p.range) && avoid.every((a) => Math.abs(value - a) > 1e-2 * p.range)) return value;
  }
  throw Error(`parameter ${p.name} on slot ${slot === 0 ? 'A' : 'B'} would not move away from ${avoid.join(', ')}`);
}

/** Both slots' parameters read `want` (within 1e-4 of their range); the problems otherwise. */
async function valueProblems(expect: Expect, want: [number, number]): Promise<string[]> {
  const problems: string[] = [];
  for (const slot of [0, 1] as const) {
    const got = await read(slot, expect[slot].id);
    if (!near(got, want[slot], expect[slot].range)) problems.push(`slot ${slot === 0 ? 'A' : 'B'} ${expect[slot].name} reads ${got}, want ${want[slot]}`);
  }
  return problems;
}

async function loaded(): Promise<(string | null)[]> {
  const host: (string | null)[] = [null, null];
  for (const { slot, descriptor } of await platform.pluginHost.listLoaded()) host[slot] = pluginDescriptorKey(descriptor);
  return host;
}

const lane = (i: number) => looper.track(i)();
const allEmpty = () => Array.from({ length: looper.trackCount }, (_, i) => lane(i).state).every((s) => s === 'EMPTY');

async function clearJam(): Promise<void> {
  looper.clearAll();
  await until('an empty looper', () => allEmpty() && looper.masterLengthFrames() === 0, 5);
}

async function save(): Promise<string> {
  const stale = await loaded();
  if (slotPlugins().some(Boolean) || stale.some(Boolean)) {
    await clearPlugin(0);
    await clearPlugin(1);
    throw Error(`this launch restored a record an earlier run left (${JSON.stringify(stale)}); unloaded, which forgets it: run again`);
  }
  await clearJam();
  const picks = pickPlugins();
  if (typeof picks === 'string') throw Error(picks);
  await selectPlugin(0, picks[0]);
  await selectPlugin(1, picks[1]);
  const now = await loaded();
  check(now.join() === picks.map(pluginDescriptorKey).join(), `the loads did not land: ${JSON.stringify(now)}`);
  const moved = [await moveCandidates(0), await moveCandidates(1)];
  for (const slot of [0, 1] as const) {
    await clearPlugin(slot);
    await selectPlugin(slot, picks[slot]);
  }
  const held = [await surviving(0, moved[0]), await surviving(1, moved[1])];
  // One more move each, then the close well inside the owner's save debounce: these values reach the
  // store on the exit path alone. The same plugin in both slots gets two values, so a store that kept
  // one tone per plugin would hand one slot the other's.
  const a: Probed = { ...held[0], value: await moveAway(0, held[0], [held[0].value, held[0].default]) };
  const same = pluginDescriptorKey(picks[0]) === pluginDescriptorKey(picks[1]);
  const b: Probed = { ...held[1], value: await moveAway(1, held[1], [held[1].value, held[1].default, ...(same ? [a.value] : [])]) };
  const expect: Expect = [a, b];
  return JSON.stringify(expect);
}

/** The recall's loads, each answering a restored tone, with both slots holding the saved plugins. */
async function recalled(what: string): Promise<void> {
  await until('the recall', () => rigRecallDone(), 180);
  const restored = loads.filter((l) => l.tone === 'restored').map((l) => l.slot).sort();
  check(restored.join() === '0,1', `${what}: the loads answered ${JSON.stringify(loads)}, not a restored tone in both slots`);
  check(slotPlugins().every(Boolean), `${what}: the slots hold ${JSON.stringify(slotPlugins().map((d) => d?.name ?? null))}`);
}

async function checkPhase(expect: Expect): Promise<string> {
  // A1: the restart brought both plugins back with their tones.
  await recalled('after the restart');
  const v: [number, number] = [expect[0].value, expect[1].value];
  const a1 = await valueProblems(expect, v);
  check(a1.length === 0, `after the restart: ${a1.join('; ')}`);
  log(`restart: both slots restored their tone (${expect.map((p) => `${p.name} ${p.value.toFixed(4)}`).join(', ')})`);

  // A2: a session carries each slot's tone.
  const [a, b] = slotPlugins() as [PluginDescriptor, PluginDescriptor];
  await goLive(0);
  check(inputArmed()[0], 'slot A did not go live');
  clock.setBpm(120);
  await until('120 BPM', () => clock.bpm() === 120, 5);
  looper.setFixedLengthEnabled(true);
  looper.setFixedLengthBars(1);
  void looper.recDub(0);
  await until('lane 1 PLAYING', () => lane(0).state === 'PLAYING', 15);
  const bundle = await buildExportBundle({ bpm: 120, bars: 1 }, {}, session);
  check(bundle !== null, 'the export had nothing to export');
  const entries = parseZip(bundle!.zipBytes);
  const tones = entries.filter((e) => /-tone-slot-[ab]\.bin$/.test(e.name)).map((e) => e.name.slice(-10));
  const json = JSON.parse(new TextDecoder().decode(entries.find((e) => e.name.endsWith('-session.json'))!.data)) as {
    plugins?: { slot: string; name: string }[];
  };
  const named = (json.plugins ?? []).map((p) => `${p.slot} ${p.name}`).join(', ');
  check(tones.join() === 'slot-a.bin,slot-b.bin' && named === `A ${a.name}, B ${b.name}`, `the export carries tones ${JSON.stringify(tones)}, plugins "${named}"`);
  log(`exported: a tone per slot, session.json names ${named}`);

  await clearJam();
  const w: [number, number] = [await moveAway(0, expect[0], [v[0]]), await moveAway(1, expect[1], [v[1]])];
  loads.length = 0;
  await restoreSessionTones(await importSession(bundle!.zipBytes, session));
  await until('the slots back after the import', () => slotPendingCounts().every((n) => n === 0) && slotPlugins().every(Boolean), 30);
  const reloaded = loads.filter((l) => l.tone === 'restored').map((l) => l.slot).sort();
  check(reloaded.join() === '0,1', `the import reloaded ${JSON.stringify(loads)}, not both slots with a restored tone`);
  const a2 = await valueProblems(expect, v);
  check(a2.length === 0, `after the import (moved to ${w.join(', ')} first): ${a2.join('; ')}`);
  check(inputArmed()[0] && !inputArmed()[1], `GO LIVE after the reload: ${JSON.stringify(inputArmed())}`);
  check(lane(0).state === 'PLAYING', `lane 1 is ${lane(0).state} after the import`);
  log(`imported: both slots reloaded and read their session tone again (from ${w.map((x) => x.toFixed(4)).join(', ')}); slot A live again`);

  // An import whose slot holds another plugin or none swaps nothing: it says what to load.
  await clearJam();
  const wB = await moveAway(1, expect[1], [v[1]]);
  await clearPlugin(1);
  check(slotPlugins()[1] === null, 'slot B did not unload');
  await restoreSessionTones(await importSession(bundle!.zipBytes, session));
  await until('slot A back after the second import', () => slotPendingCounts().every((n) => n === 0) && slotPlugins()[0] !== null, 30);
  const want = `This session used ${b.name} in slot B — load it there to hear the session's tone`;
  const shown = toasts().map((t) => t.message);
  check(shown.includes(want), `toasts ${JSON.stringify(shown)}, want "${want}"`);
  check(slotPlugins()[1] === null && (await loaded())[1] === null, 'the import loaded something into slot B');
  await selectPlugin(1, b);
  const fromStore = await read(1, expect[1].id);
  check(near(fromStore, v[1], expect[1].range), `slot B loaded ${b.name} at ${fromStore} (moved to ${wB} before its unload), want the session's ${v[1]}`);
  log(`not held: a toast named ${b.name}, slot B stayed empty; its next load read the session's tone`);

  // The debounce: the tones of what is set now must survive a kill that skips the exit path.
  await clearJam();
  await until('the recovery deleted', async () => !(await autosave.hasSaved()), 20);
  const w2: Expect = [
    { ...expect[0], value: await moveAway(0, expect[0], [v[0], expect[0].default]) },
    { ...expect[1], value: await moveAway(1, expect[1], [v[1], expect[1].default]) },
  ];
  await sleep(SAVE_QUIET_MS + 1500);
  return JSON.stringify(w2);
}

async function after(expect: Expect): Promise<string> {
  await recalled('after the kill');
  const problems = await valueProblems(expect, [expect[0].value, expect[1].value]);
  check(problems.length === 0, `after the kill: ${problems.join('; ')}`);
  // Unloading forgets the record, so the next run's save phase starts from empty slots.
  await clearPlugin(0);
  await clearPlugin(1);
  return `both slots restored the tone saved ${SAVE_QUIET_MS / 1000} s after the last change, with no exit path (${expect.map((p) => `${p.name} ${p.value.toFixed(4)}`).join(', ')})`;
}

export async function runToneRecall(): Promise<void> {
  const phase = import.meta.env.VITE_LF_PROBE_PHASE as string | undefined;
  spy();
  // Every phase but check (the runner kills it) closes itself, on any verdict.
  const closes = phase !== 'check';
  try {
    check(engineMode(), 'engine mode is off in this profile (the runner writes its toggle file)');
    await until('the engine device', () => engineDevice() !== null, 90);
    // Nothing needs to be heard: the master mute (not stored) keeps the take's count-in and a live
    // slot off the speakers.
    master.setMuted(true);
    await until('the plugin scan', () => nativeHostReady() && availablePlugins().length > 0, 600);
    const raw = import.meta.env.VITE_LF_PROBE_EXPECT as string | undefined;
    if (phase === 'save') {
      await until('the recall', () => rigRecallDone(), 180);
      log(`saved: ${await save()}`);
    } else if (phase === 'check' || phase === 'after') {
      check(raw !== undefined, 'no VITE_LF_PROBE_EXPECT handed over from the phase before');
      const expect = JSON.parse(raw!) as Expect;
      if (phase === 'check') log(`set: ${await checkPhase(expect)}`);
      else log(`restored: ${await after(expect)}`);
    } else {
      throw Error(`unknown phase "${phase}"`);
    }
  } catch (e) {
    // Empty the looper and the slots (unloading forgets the record): the next run starts clean.
    looper.clearAll();
    await clearPlugin(0);
    await clearPlugin(1);
    await sleep(500);
    log(`FAIL ${String(e instanceof Error ? e.message : e)}`);
  }
  if (closes) {
    await sleep(300);
    await confirmNativeClose();
  }
}
