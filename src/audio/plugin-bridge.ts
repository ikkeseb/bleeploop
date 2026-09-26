/**
 * OWNS: the JS half of the plugin audio transport (native plugin audio → Web Audio) — wiring the hop-1
 * ring into its source worklet, the per-slot gain staging into `recordTap` + the muteable web monitor,
 * and the process-lifetime PCM-loss totals the looper's take integrity check compares.
 *
 * The native host (Rust) writes the plugin's mono PCM into a WebView2 SharedBuffer ring (hop 1: a
 * regular cross-process ArrayBuffer — NO Atomics, plain ordered reads). The buffer is TRANSFERRED to a
 * `plugin-pcm-source` AudioWorkletNode, which reads the ring on the render thread and writes the
 * consumer half of its header (see the worklet's header for the queue policy: one setpoint, a settle
 * after every step). The node feeds a per-plugin gain that splits to `recordTap` at full level and
 * `webMonitorGain → masterGain` for audible monitoring. The audible branch mutes while the native
 * monitor is armed; the record branch does not. After the transfer the main thread holds no view of
 * the ring: it reads the worklet's counters and queue from a small stats SharedArrayBuffer.
 *
 * BOUNDARY: this file lives in `src/audio/` and is WebView2-agnostic. It receives a *plain*
 * ArrayBuffer + parsed meta from the platform layer (`host.tauri.ts` owns the `chrome.webview`
 * `sharedbufferreceived` event and forwards it here via `acceptPluginBuffer`), and releases it via
 * an injected `release` callback. No `@tauri-apps` / `chrome.webview` knowledge here.
 *
 * Hop-1 header layout (32 bytes, 8×u32; MUST match `host/transport.rs::create_shared_ring`):
 *   [0]=write_frames (Rust)     — hop-1 produced total, post-resample C-rate
 *   [1]=read_frames  (worklet)  — the render thread's read cursor
 *   [2]=capacity_frames (Rust once)
 *   [3]=level        (worklet)  — the drift controller's PV: the queue at each quantum, smoothed; held on
 *                                 the setpoint while the queue settles after a step
 *   [4]=consumed     (worklet)  — frames rendered (gate liveness + drift slope)
 *   [5]=underruns    (worklet)  — quanta that ran short, plus settles that inserted silence
 *   [6]=dropped      (worklet)  — frames dropped by the lag cap or a settle
 *   [7]=epoch        (Rust)     — bumped where production jumps rather than drifts (starts a settle)
 *   data: f32×cap, immediately after the 32-byte header (32 is 4-byte aligned → f32-aligned).
 *
 * The worklet writes [1] and [3]..[6] every quantum as plain ordered stores (TSO-visible to Rust's
 * acquire-loads). WebView2 lets the SharedBuffer's ArrayBuffer transfer into the AudioWorklet realm
 * with its mapping live (both directions, measured 2026-09-24). Teardown tells the worklet to drop its
 * views, so the mapping goes when they are collected, and still calls `release` on the detached buffer
 * (best-effort: whether WebView2 honours that after a transfer is unknown).
 */
import { createSignal } from 'solid-js';
import pluginPcmUrl from './worklets/plugin-pcm-source.ts?worker&url';
import { engine } from './engine';
import { notifyError } from '../notify';

/** Parsed from the SharedBuffer's `additionalData` JSON (set Rust-side in `create_shared_ring`). */
export interface PluginBufferMeta {
  kind: string; // "plugin-audio"
  slot: number;
  capacityFrames: number;
  headerBytes: number;
  sampleRate: number;
  inChannels: number; // plugin audio-input channel count; >0 ⇒ FX/amp-sim, 0 ⇒ synth
  loadToken: number; // frontend request identity; rejects late buffers from failed/superseded loads
}

const H_LEVEL = 3; // hop-1 header word the drift controller reads as its level (PV)
// Stats SAB (i32), written by the worklet; mirrored in `worklets/plugin-pcm-source.ts`.
const STAT_CONSUMED = 0;
const STAT_UNDERRUNS = 1;
const STAT_DROPPED = 2;
const STAT_QUEUE = 3;
const STAT_LEVEL_X16 = 4;
const STAT_LIVE = 5;
const STAT_WORDS = 6;
/** The queue's setpoint: the drift controller's `TARGET_FILL_SECONDS` in src-tauri/src/host/transport.rs. */
const TARGET_SECONDS = 0.03;
/** A backlog past this is dropped to the setpoint at once, so jitter shows as bounded latency. */
const MAX_LAG_SECONDS = 0.06;
/** How long the queue's mean is measured after a step before the one correction. */
const SETTLE_SECONDS = 1;
/** Per-slot plugin output-gain defaults, chosen by plugin TYPE at load. An amp-sim/FX (has an
 * audio-input bus, `meta.inChannels > 0`) is already internally gain-staged, so its wet wants ~unity;
 * a synth (no input bus) renders default patches that clip, so it starts conservative. The user trims
 * either from the plugin panel's output slider (→ `setGain`, also `__lf.setPluginGain`). The slider
 * scale runs 0..1.5, so these defaults leave headroom both ways. */
export const FX_DEFAULT_GAIN = 0.9;
export const SYNTH_DEFAULT_GAIN = 0.1;

interface BridgeSlot {
  ab: ArrayBuffer; // the hop-1 SharedBuffer, detached once transferred to the worklet
  node: AudioWorkletNode;
  gain: GainNode; // per-plugin gain staging (worklet → gain → {recordTap, webMonitorGain})
  gainValue: number; // intended target — survives setTargetAtTime ramps so the reactive value stays exact
  webMonitorGain: GainNode; // the audible web path (gain → webMonitorGain → masterGain); muted
  // (0) while the native cpal-out monitor is armed so the wet isn't heard twice (flam). The record tap
  // (gain → recordTap) stays at full level regardless, so the wet is still recorded.
  stats: Int32Array; // the worklet's counters and queue (STAT_*)
  accountedUnderruns: number; // last worklet total folded into the process-wide record-loss counters
  accountedDropped: number;
  loadToken: number; // the frontend load request that owns this wiring
}

let ctx: AudioContext | null = null;
let moduleReady: Promise<void> | null = null;
let release: ((ab: ArrayBuffer) => void) | null = null;
let onPluginConnected: (() => void) | null = null;
const slotMap = new Map<number, BridgeSlot>();
/**
 * Process-lifetime loss totals for the plugin PCM path into recordTap. They deliberately outlive a
 * bridge slot: a plugin can be cleared/swapped after losing PCM but before the looper commits, and the
 * take must still fail closed. A record/overdub snapshots these totals at arm and compares at commit.
 */
let recordDroppedFrames = 0;
let recordUnderruns = 0;
const [gainValues, setGainValues] = createSignal<[number | null, number | null]>([null, null]);

function setGainValue(slot: number, value: number | null): void {
  if (slot !== 0 && slot !== 1) return;
  setGainValues((current) => {
    if (current[slot] === value) return current;
    const next: [number | null, number | null] = [...current];
    next[slot] = value;
    return next;
  });
}
/** Per-slot plugin kind for the output-gain default: true = audio effect (FX), false = instrument
 * (synth). Set by `beginPluginLoad` BEFORE the slot's plugin loads (from the scan descriptor's category),
 * so `acceptPluginBuffer` reads it self-sufficiently. Absent ⇒ fall back to the meta input-bus count. */
const slotKinds = new Map<number, boolean>();
// Request identity starts in the frontend before IPC and returns in SharedBuffer metadata. Slot-only
// generations minted when a buffer arrives cannot distinguish a late buffer A from retry B.
let nextLoadToken = 0;
const pendingLoadTokens = new Map<number, number>();
/** Best-effort release of a hop-1 SharedBuffer's JS view (the injected platform `releaseBuffer`). */
function releaseBuffer(ab: ArrayBuffer): void {
  if (release) {
    try {
      release(ab);
    } catch {
      /* best-effort */
    }
  }
}

/**
 * Prepare the bridge: adopt the shared AudioContext and load the `plugin-pcm-source` worklet module
 * once. Idempotent. `release` is the platform's `chrome.webview.releaseBuffer` (no-op in a browser).
 */
export async function initPluginBridge(
  audioCtx: AudioContext,
  opts?: { release?: (ab: ArrayBuffer) => void; onPluginConnected?: () => void },
): Promise<void> {
  ctx = audioCtx;
  if (opts?.release) release = opts.release;
  if (opts?.onPluginConnected) onPluginConnected = opts.onPluginConnected;
  if (!moduleReady) moduleReady = ctx.audioWorklet.addModule(pluginPcmUrl);
  const ready = moduleReady;
  try {
    await ready;
  } catch (err) {
    if (moduleReady === ready) moduleReady = null;
    throw err;
  }
}

/**
 * Wire a freshly-posted hop-1 SharedBuffer into the audio graph for `meta.slot`: build hop 2, create
 * the source worklet, then route it through gain to the always-full `recordTap` branch and the
 * independently muteable `webMonitorGain → masterGain` branch. Replaces any existing setup for that
 * slot (handles plugin swap / reload).
 */
export async function acceptPluginBuffer(ab: ArrayBuffer, meta: PluginBufferMeta): Promise<void> {
  if (!ctx) {
    console.error('[plugin-bridge] no AudioContext; call initPluginBridge() first');
    notifyError("Plugin sound isn't ready yet — try reloading the plugin");
    releaseBuffer(ab); // don't leak the hop-1 buffer on the (defensive) no-ctx path
    return;
  }
  if (!Number.isInteger(meta.loadToken) || meta.loadToken <= 0 || meta.loadToken > 0xffff_ffff) {
    releaseBuffer(ab);
    return;
  }
  const ownsLoad = () => pendingLoadTokens.get(meta.slot) === meta.loadToken;
  if (!ownsLoad()) {
    releaseBuffer(ab);
    return;
  }
  if (!moduleReady) moduleReady = ctx.audioWorklet.addModule(pluginPcmUrl);
  const ready = moduleReady;
  try {
    await ready;
  } catch (err) {
    if (moduleReady === ready) moduleReady = null;
    releaseBuffer(ab);
    throw err;
  }
  if (!ownsLoad()) {
    // The load failed, was cleared, or was superseded while the worklet module awaited.
    releaseBuffer(ab);
    return;
  }
  teardownSlotWiring(meta.slot); // replace any existing wiring without cancelling this load token

  // Everything below adopts `ab` (the hop-1 cross-process SharedBuffer). If any of it throws — most
  // realistically `new AudioWorkletNode` on a closed/interrupted context during a swap — `ab`
  // is neither stored in slotMap nor released, so the OS shared mapping leaks for the
  // plugin's lifetime. Release it on any failure (the slot stays empty, which is correct for a failed
  // accept), then rethrow so the call-site .catch logs it.
  let node: AudioWorkletNode | null = null;
  try {
    const sr = meta.sampleRate;
    const targetFrames = Math.round(sr * TARGET_SECONDS);
    // Until the worklet's first quantum the controller reads the setpoint (neutral), not a stale 0.
    new Uint32Array(ab, 0, meta.headerBytes / 4)[H_LEVEL] = targetFrames;
    const statsSab = new SharedArrayBuffer(STAT_WORDS * Int32Array.BYTES_PER_ELEMENT);
    const stats = new Int32Array(statsSab);

    node = new AudioWorkletNode(ctx, 'plugin-pcm-source', {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [1],
      processorOptions: {
        statsSab,
        headerBytes: meta.headerBytes,
        capacityFrames: meta.capacityFrames,
        targetFrames,
        maxLagFrames: Math.round(sr * MAX_LAG_SECONDS),
        settleFrames: Math.round(sr * SETTLE_SECONDS),
      },
    });
    node.port.postMessage(ab, [ab]); // the render thread owns the ring from here
    // Per-plugin gain staging: worklet → gain → {recordTap (record), webMonitorGain → masterGain
    // (audible)}. The plugin kind decides the default: the scan-category (slotKinds, set before load) is
    // authoritative; fall back to the input-bus count only when the plugin was unclassifiable.
    // Self-sufficient (no UI-mount timing). Splitting record from audible lets the native
    // cpal-out monitor mute the audible web path (webMonitorGain → 0) without losing the record tap —
    // gain → recordTap stays full. With webMonitorGain at unity the level matches a direct
    // looperInputBus edge (looperInputBus is always unity; master volume lives on masterGain).
    const gain = ctx.createGain();
    const kind = slotKinds.get(meta.slot); // true=effect, false=instrument, undefined=unclassified
    const isEffect = kind ?? meta.inChannels > 0;
    const gainValue = isEffect ? FX_DEFAULT_GAIN : SYNTH_DEFAULT_GAIN;
    gain.gain.value = gainValue;
    const webMonitorGain = ctx.createGain(); // unity = audible; set to 0 when native monitor armed
    node.connect(gain);
    gain.connect(engine.recordTap); // record tap (always full level)
    gain.connect(webMonitorGain);
    webMonitorGain.connect(engine.masterGain); // audible (web), independently muteable

    const slot: BridgeSlot = {
      ab,
      node,
      gain,
      gainValue,
      webMonitorGain,
      stats,
      accountedUnderruns: 0,
      accountedDropped: 0,
      loadToken: meta.loadToken,
    };
    slotMap.set(meta.slot, slot);
    setGainValue(meta.slot, gainValue);
    // The bridge is the earliest common success point for both native instruments and effects.
    // Let the composition root warm the capture tap here, before a note, GO LIVE, or REC gesture,
    // so the record-level meter observes the first plugin signal. The callback keeps this module
    // independent of looper/capture.ts (machine.ts already imports this bridge).
    onPluginConnected?.();
    console.log(
      `[plugin-bridge] slot ${meta.slot} wired: cap=${meta.capacityFrames} sr=${sr} target=${targetFrames}`,
    );
  } catch (err) {
    node?.port.postMessage('close'); // a worklet that already took the ring must stop driving it
    node?.disconnect();
    releaseBuffer(ab);
    throw err;
  }
}

/** Fold the worklet's loss counters since the last read into the process-lifetime totals (u32 wrap-safe). */
function accountLoss(s: BridgeSlot): void {
  const underruns = Atomics.load(s.stats, STAT_UNDERRUNS) >>> 0;
  const dropped = Atomics.load(s.stats, STAT_DROPPED) >>> 0;
  recordUnderruns += (underruns - s.accountedUnderruns) >>> 0;
  recordDroppedFrames += (dropped - s.accountedDropped) >>> 0;
  s.accountedUnderruns = underruns;
  s.accountedDropped = dropped;
}

/** Stop and release the current wiring without changing which load request may still supply a buffer. */
function teardownSlotWiring(slot: number): void {
  setGainValue(slot, null);
  const s = slotMap.get(slot);
  if (!s) return;
  accountLoss(s); // preserve any final worklet loss after this slot disappears
  s.node.port.postMessage('close');
  try {
    s.node.disconnect();
    s.gain.disconnect();
    s.webMonitorGain.disconnect();
  } catch {
    /* already disconnected */
  }
  releaseBuffer(s.ab);
  slotMap.delete(slot);
}

/** Begin one native load and return the identity that must round-trip with its SharedBuffer. */
function beginPluginLoad(slot: number, isEffect: boolean | null): number {
  if (isEffect == null) slotKinds.delete(slot);
  else slotKinds.set(slot, isEffect);
  nextLoadToken = (nextLoadToken + 1) >>> 0;
  if (nextLoadToken === 0) nextLoadToken = 1;
  pendingLoadTokens.set(slot, nextLoadToken);
  return nextLoadToken;
}

/** Cancel only the named failed request; a newer retry may already own the slot. */
function cancelPluginLoad(slot: number, loadToken: number): void {
  if (pendingLoadTokens.get(slot) !== loadToken) return;
  pendingLoadTokens.delete(slot);
  if (slotMap.get(slot)?.loadToken === loadToken) teardownSlotWiring(slot);
}

/** Tear down a slot's bridge and invalidate every in-flight buffer for its current load. */
export function teardownPluginSlot(slot: number): void {
  pendingLoadTokens.delete(slot);
  teardownSlotWiring(slot);
}

export interface PluginRecordLossSnapshot {
  droppedFrames: number;
  underruns: number;
}

/** Monotonic process-lifetime snapshot used by the looper's arm→commit integrity check. */
function recordLossSnapshot(): PluginRecordLossSnapshot {
  for (const s of slotMap.values()) accountLoss(s);
  return { droppedFrames: recordDroppedFrames, underruns: recordUnderruns };
}

/** DEV fault injection imported only by the stripped `src/debug/lf.ts` surface. */
export function injectRecordLossForTest(loss: Partial<PluginRecordLossSnapshot> = {}): void {
  recordDroppedFrames += Math.max(0, Math.trunc(loss.droppedFrames ?? 0));
  recordUnderruns += Math.max(0, Math.trunc(loss.underruns ?? 0));
}

/** DEV health snapshot for a slot (for __lf / diagnostics): the queue after the last render quantum and
 * the worklet's totals. */
function stats(slot: number): { queue: number; consumed: number; underruns: number; dropped: number } | null {
  const s = slotMap.get(slot);
  if (!s) return null;
  return {
    queue: Atomics.load(s.stats, STAT_QUEUE),
    consumed: Atomics.load(s.stats, STAT_CONSUMED) >>> 0,
    underruns: Atomics.load(s.stats, STAT_UNDERRUNS) >>> 0,
    dropped: Atomics.load(s.stats, STAT_DROPPED) >>> 0,
  };
}

/** Set a slot's plugin output gain (0..1.5). Click-free ramp; the target is stored on the slot and
 * published through `gainValues`. No-op if no plugin is wired in the slot yet. */
function setGain(slot: number, value: number): void {
  const s = slotMap.get(slot);
  if (!s || !ctx) return;
  const v = Math.max(0, value);
  s.gainValue = v;
  s.gain.gain.setTargetAtTime(v, ctx.currentTime, 0.01);
  setGainValue(slot, v);
}

/**
 * Mute/unmute the slot's AUDIBLE web monitor path. Called by native-io.ts when the
 * native cpal-out monitor is armed/disarmed: muted ⇒ the wet is heard only via the low-latency native
 * monitor (no WebView2-path doubling/flam); unmuted ⇒ the web path is audible as normal. The record
 * tap (gain → recordTap) is untouched either way, so the wet is recorded at full level regardless.
 * Click-free ramp. No-op if no plugin is wired in the slot yet.
 */
function setWebMonitorMuted(slot: number, muted: boolean): void {
  const s = slotMap.get(slot);
  if (!s || !ctx) return;
  s.webMonitorGain.gain.setTargetAtTime(muted ? 0 : 1, ctx.currentTime, 0.01);
}

/** The slot's queue smoothed over ~0.3 s (frames), the level the drift controller holds: the record
 * path's moving delay term; null until the slot's queue first reached its setpoint. Record
 * compensation reads its shift since the freeze for each take. */
function queueFrames(slot: number): number | null {
  const s = slotMap.get(slot);
  return s && Atomics.load(s.stats, STAT_LIVE) === 1 ? Atomics.load(s.stats, STAT_LEVEL_X16) / 16 : null;
}

export const pluginBridge = {
  init: initPluginBridge,
  acceptPluginBuffer,
  teardownPluginSlot,
  /** Mint the identity and record the kind for a pending native load. */
  beginPluginLoad,
  /** Invalidate a failed load without cancelling a newer retry. */
  cancelPluginLoad,
  /** Set per-plugin output gain (gain staging). */
  setGain,
  /** Reactive per-slot intended output gain (null if no plugin is wired). */
  gains: gainValues,
  /** Mute/unmute the slot's audible web monitor path (record tap unaffected). */
  setWebMonitorMuted,
  /** DEV: live health for a slot (queue, consumed frames, underruns, drops). */
  stats,
  /** Monotonic loss totals for fail-closed looper recording integrity. */
  recordLossSnapshot,
  /** The smoothed queue (frames) record compensation tracks between takes. */
  queueFrames,
} as const;
