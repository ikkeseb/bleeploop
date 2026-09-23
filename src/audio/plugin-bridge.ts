/**
 * OWNS: the JS half of the two-ring plugin audio transport (native plugin audio → Web Audio) — the hop-1
 * → hop-2 drain, the per-slot gain staging into `recordTap` + the muteable web monitor, the feedback
 * fields the Rust drift controller reads, and the process-lifetime PCM-loss totals the looper's take
 * integrity check compares.
 *
 * The native host (Rust) writes the plugin's mono PCM into a WebView2 SharedBuffer ring (hop 1: a
 * regular cross-process ArrayBuffer — NO Atomics, plain ordered reads). A main-thread drain copies
 * available frames from hop 1 into a real `ringbuf.js` SharedArrayBuffer ring (hop 2: Atomics OK),
 * which a `plugin-pcm-source` AudioWorkletNode pops into a per-plugin gain. That gain splits to
 * `recordTap` at full level and `webMonitorGain → masterGain` for audible monitoring. The audible
 * branch mutes while the native monitor is armed; the record branch does not.
 *
 * BOUNDARY: this file lives in `src/audio/` and is WebView2-agnostic. It receives a *plain*
 * ArrayBuffer + parsed meta from the platform layer (`host.tauri.ts` owns the `chrome.webview`
 * `sharedbufferreceived` event and forwards it here via `acceptPluginBuffer`), and releases it via
 * an injected `release` callback. No `@tauri-apps` / `chrome.webview` knowledge here.
 *
 * Hop-1 header layout (28 bytes, 7×u32; MUST match `host/transport.rs::create_shared_ring`):
 *   [0]=write_frames (Rust)   — hop-1 produced total, post-resample C-rate
 *   [1]=read_frames  (JS)     — drain copy cursor (lag-capped; NOT a control signal)
 *   [2]=capacity_frames (Rust once)
 *   [3]=hop2_fill    (JS)     — the PI controller's level signal (PV): hop2.available_read(),
 *                               discard-neutral (no flush/lag-cap term)
 *   [4]=consumed     (JS)     — worklet STAT_CONSUMED total (gate liveness + drift slope)
 *   [5]=underruns    (JS)     — worklet STAT_UNDERRUNS total
 *   [6]=js_dropped   (JS)     — cumulative lag-cap + flush discards
 *   data: f32×cap, immediately after the 28-byte header (28 is 4-byte aligned → f32-aligned).
 *
 * The four JS-written feedback fields ([3]..[6]) feed the Rust producer's drift controller + gate.
 * They are plain ordered writes (TSO-visible to Rust's acquire-loads), mirrored on EVERY drain tick
 * — not just ticks that moved audio — so the controller's PV and the gate's counters stay fresh even
 * when hop-1 has nothing to copy (the worklet keeps consuming from hop-2 regardless).
 *
 * DRAIN = main thread `setInterval`. A dedicated Worker (lower jitter) would need the WebView2
 * ArrayBuffer to survive transfer into a Worker realm — unproven on this engine, while the measured
 * main-thread drain keeps up (fill ~2 %, 0 drops), so the Worker stays deferred. The drain caps real
 * lag so jitter shows up as bounded latency, not an overrun.
 */
import { RingBuffer } from 'ringbuf.js';
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

// hop-1 header indices (u32 view) — must match the 7-field layout in host/transport.rs.
const H_WRITE = 0;
const H_READ = 1;
// [2] = capacity_frames (Rust-written once; JS reads it via meta.capacityFrames, not the header).
const H_HOP2_FILL = 3; // PI level signal (PV) — discard-neutral
const H_CONSUMED = 4; // worklet STAT_CONSUMED total
const H_UNDERRUNS = 5; // worklet STAT_UNDERRUNS total
const H_JS_DROPPED = 6; // cumulative lag-cap + flush discards
const DRAIN_INTERVAL_MS = 5;
/** hop-2 capacity (~340ms @48k). Power-of-two-ish; ringbuf.js sizes the SAB from this. */
const HOP2_CAPACITY_FRAMES = 16384;
/** Real-lag cap: keep hop-1 backlog under this so jitter/drift stays bounded (~60ms latency). */
const MAX_LAG_SECONDS = 0.06;
/** Per-slot plugin output-gain defaults, chosen by plugin TYPE at load. An amp-sim/FX (has an
 * audio-input bus, `meta.inChannels > 0`) is already internally gain-staged, so its wet wants ~unity;
 * a synth (no input bus) renders default patches that clip, so it starts conservative. The user trims
 * either from the plugin panel's output slider (→ `setGain`, also `__lf.setPluginGain`). The slider
 * scale runs 0..1.5, so these defaults leave headroom both ways. */
const FX_DEFAULT_GAIN = 0.9;
const SYNTH_DEFAULT_GAIN = 0.1;

interface BridgeSlot {
  ab: ArrayBuffer;
  header: Uint32Array; // view over hop-1 header (first headerBytes)
  data: Float32Array; // view over hop-1 data region (capacityFrames f32)
  cap: number; // hop-1 capacity (power of two)
  mask: number; // cap - 1
  maxLagFrames: number;
  node: AudioWorkletNode;
  gain: GainNode; // per-plugin gain staging (worklet → gain → {recordTap, webMonitorGain})
  gainValue: number; // intended target — survives setTargetAtTime ramps so the reactive value stays exact
  webMonitorGain: GainNode; // the audible web path (gain → webMonitorGain → masterGain); muted
  // (0) while the native cpal-out monitor is armed so the wet isn't heard twice (flam). The record tap
  // (gain → recordTap) stays at full level regardless, so the wet is still recorded.
  hop2: RingBuffer;
  stats: Int32Array;
  scratch: Float32Array;
  timer: ReturnType<typeof setInterval>;
  flushed: boolean; // discarded the suspended-context backlog on the first running drain
  jsDropped: number; // cumulative lag-cap + flush discards — mirrored into header[6]
  accountedUnderruns: number; // last worklet total folded into the process-wide record-loss counter
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

  // Everything below adopts `ab` (the hop-1 cross-process SharedBuffer). If any of it throws AFTER the
  // await — most realistically `new AudioWorkletNode` on a closed/interrupted context during a swap —
  // `ab` is neither stored in slotMap nor released, so the OS shared mapping leaks for the plugin's
  // lifetime. Release it on any failure (the slot stays empty, which is correct for a failed accept),
  // then rethrow so the call-site .catch logs it.
  try {
    const cap = meta.capacityFrames;
    const header = new Uint32Array(ab, 0, meta.headerBytes / 4);
    const data = new Float32Array(ab, meta.headerBytes, cap);

    // hop-2: a real SharedArrayBuffer ring (Atomics OK) feeding the worklet, plus a stats SAB.
    const ringSab = RingBuffer.getStorageForCapacity(HOP2_CAPACITY_FRAMES, Float32Array);
    const hop2 = new RingBuffer(ringSab, Float32Array);
    const statsSab = new SharedArrayBuffer(2 * Int32Array.BYTES_PER_ELEMENT);
    const stats = new Int32Array(statsSab);

    const node = new AudioWorkletNode(ctx, 'plugin-pcm-source', {
      numberOfInputs: 0,
      numberOfOutputs: 1,
      outputChannelCount: [1],
      processorOptions: { ringSab, statsSab },
    });
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
      header,
      data,
      cap,
      mask: cap - 1,
      maxLagFrames: Math.max(128, Math.round(meta.sampleRate * MAX_LAG_SECONDS)),
      node,
      gain,
      gainValue,
      webMonitorGain,
      hop2,
      stats,
      scratch: new Float32Array(cap),
      timer: undefined as unknown as ReturnType<typeof setInterval>,
      flushed: false,
      jsDropped: 0,
      accountedUnderruns: 0,
      loadToken: meta.loadToken,
    };
    slot.timer = setInterval(() => drain(slot), DRAIN_INTERVAL_MS);
    slotMap.set(meta.slot, slot);
    setGainValue(meta.slot, gainValue);
    // The bridge is the earliest common success point for both native instruments and effects.
    // Let the composition root warm the capture tap here, before a note, GO LIVE, or REC gesture,
    // so the record-level meter observes the first plugin signal. The callback keeps this module
    // independent of looper/capture.ts (machine.ts already imports this bridge).
    onPluginConnected?.();
    console.log(
      `[plugin-bridge] slot ${meta.slot} wired: cap=${cap} sr=${meta.sampleRate} maxLag=${slot.maxLagFrames}`,
    );
  } catch (err) {
    releaseBuffer(ab);
    throw err;
  }
}

/** Fold worklet underruns since the last read into the process-lifetime total (u32 wrap-safe). */
function accountUnderruns(s: BridgeSlot): void {
  const current = Atomics.load(s.stats, 1) >>> 0;
  const delta = (current - s.accountedUnderruns) >>> 0;
  recordUnderruns += delta;
  s.accountedUnderruns = current;
}

/** Count a hop-1 discard both on the slot's native diagnostic surface and for take integrity. */
function dropFrames(s: BridgeSlot, frames: number): void {
  if (frames <= 0) return;
  s.jsDropped = (s.jsDropped + frames) >>> 0;
  recordDroppedFrames += frames;
}

/**
 * Mirror the four JS-owned feedback fields into the hop-1 header (plain ordered writes; TSO-visible
 * to Rust's acquire-loads). The PI controller's level signal is hop-2 fill — discard-neutral
 * (`available_read = pushed_total − consumed_total`, no flush/lag-cap term), so the loop never
 * chases a phantom backlog. Called on every running drain tick, even ones that moved no audio.
 */
function mirror(s: BridgeSlot): void {
  accountUnderruns(s);
  s.header[H_HOP2_FILL] = s.hop2.available_read() >>> 0; // PV
  s.header[H_CONSUMED] = Atomics.load(s.stats, 0) >>> 0; // STAT_CONSUMED
  s.header[H_UNDERRUNS] = Atomics.load(s.stats, 1) >>> 0; // STAT_UNDERRUNS
  s.header[H_JS_DROPPED] = s.jsDropped >>> 0; // lag-cap + flush discards
}

/**
 * Move available frames from hop 1 (plain reads) into hop 2. Only runs while the context is running
 * (so a suspended context doesn't buffer stale audio = latency); flushes the suspended backlog once
 * on resume, then caps real lag so jitter/drift never grows the monitoring latency unbounded. Counts
 * every discarded frame into `jsDropped` and mirrors the feedback fields at the end of the tick.
 */
function drain(s: BridgeSlot): void {
  if (!ctx || ctx.state !== 'running') return;
  // Plain ordered reads of the cross-process header (no Atomics on the WebView2 buffer).
  const write = s.header[H_WRITE] >>> 0;
  if (!s.flushed) {
    // Discard whatever accumulated while suspended so playback starts live (minimal latency). Count
    // the discard and seed the feedback fields so Rust's first acquire-loads aren't garbage.
    const read0 = s.header[H_READ] >>> 0;
    dropFrames(s, (write - read0) >>> 0);
    s.header[H_READ] = write;
    s.flushed = true;
    mirror(s);
    return;
  }
  let read = s.header[H_READ] >>> 0;
  let avail = (write - read) >>> 0;
  // Real-lag cap: if we've fallen too far behind (GC stall / clock drift), drop the oldest so the
  // monitoring latency stays bounded. A brief discontinuity beats an ever-growing delay. The dropped
  // frames are an audible glitch — count them so the gate can reject a stream that crackles.
  if (avail > s.maxLagFrames) {
    dropFrames(s, avail - s.maxLagFrames);
    read = (write - s.maxLagFrames) >>> 0;
    s.header[H_READ] = read;
    avail = s.maxLagFrames;
  }
  if (avail > 0) {
    const space = s.hop2.available_write();
    const toMove = Math.min(avail, space, s.scratch.length);
    if (toMove > 0) {
      // Copy from the hop-1 ring into scratch (wraparound), then push to hop-2.
      const start = read & s.mask;
      const first = Math.min(toMove, s.cap - start);
      s.scratch.set(s.data.subarray(start, start + first), 0);
      if (toMove > first) s.scratch.set(s.data.subarray(0, toMove - first), first);
      const pushed = s.hop2.push(s.scratch, toMove);
      s.header[H_READ] = (read + pushed) >>> 0; // advance by what actually landed in hop-2
    }
    // toMove === 0 (hop-2 full, worklet not consuming yet) leaves the frames in hop-1 — not dropped.
  }
  mirror(s); // refresh PV + counters every tick, regardless of whether audio moved
}

/** Stop and release the current wiring without changing which load request may still supply a buffer. */
function teardownSlotWiring(slot: number): void {
  setGainValue(slot, null);
  const s = slotMap.get(slot);
  if (!s) return;
  accountUnderruns(s); // preserve any final worklet loss after this slot disappears
  clearInterval(s.timer);
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
  for (const s of slotMap.values()) accountUnderruns(s);
  return { droppedFrames: recordDroppedFrames, underruns: recordUnderruns };
}

/** DEV fault injection imported only by the stripped `src/debug/lf.ts` surface. */
export function injectRecordLossForTest(loss: Partial<PluginRecordLossSnapshot> = {}): void {
  recordDroppedFrames += Math.max(0, Math.trunc(loss.droppedFrames ?? 0));
  recordUnderruns += Math.max(0, Math.trunc(loss.underruns ?? 0));
}

/** DEV health snapshot for a slot (for __lf / diagnostics). */
function stats(slot: number): Record<string, number> | null {
  const s = slotMap.get(slot);
  if (!s) return null;
  const write = s.header[H_WRITE] >>> 0;
  const read = s.header[H_READ] >>> 0;
  return {
    hop1Lag: (write - read) >>> 0,
    hop2Fill: s.hop2.available_read(),
    consumed: Atomics.load(s.stats, 0),
    underruns: Atomics.load(s.stats, 1),
    jsDropped: s.jsDropped,
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
  /** DEV: live health for a slot (hop-1 lag, hop-2 fill, consumed frames, underruns). */
  stats,
  /** Monotonic loss totals for fail-closed looper recording integrity. */
  recordLossSnapshot,
} as const;
