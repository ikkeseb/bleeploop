import { createSignal } from 'solid-js';
import { pluginBridge } from './plugin-bridge';
import { recordLatency } from './record-latency';
import { platform, type PluginSlot } from '../platform';
import { notifyError } from '../notify';
import { serializeSlot, slotPlugins, withAt } from './instrument-slots';
import { warm as warmCapture } from './looper/capture';

/**
 * OWNS: native audio I/O per slot — feed a hardware input into the slot's loaded plugin, hear its wet
 * through the native low-latency cpal monitor, and fall back when a stream dies. Everything here is
 * a no-op / all-false in the browser build. The arm paths are serialised on the same per-slot chain
 * as load/unload (`instrument-slots.ts`) so they can never interleave with a plugin swap; the
 * `*Internal` disarms are exported for `instrument.ts`'s teardown paths, which are already inside a
 * serialised op and must not re-enter the chain.
 */

// Whether each slot's plugin is currently being fed a hardware input signal.
const [inputArmed, setInputArmed] = createSignal<[boolean, boolean]>([false, false]);
// Whether each slot's wet is being monitored natively (vs the WebView2 web path).
const [monitorArmed, setMonitorArmed] = createSignal<[boolean, boolean]>([false, false]);

/** Settle delay (ms) for the second cpal_out fetch — comfortably past one diag gate-emit so the
 * owner-mirrored `monitor_out_block` term is populated. A fast first take intentionally wins the freeze. */
export const MONITOR_LATENCY_SETTLE_MS = 3000;
const latencyGeneration = [0, 0];
const latencyRequest = [0, 0];
// Last accepted cpal_out for each slot's CURRENT armed monitor. A non-registered slot can remain
// live while the other owns compensation; keep its value so promotion does not open at zero during
// the replacement IPC query. Cleared when that slot's monitor actually goes away.
const lastMonitorLatencySeconds = [0, 0];
const latencySettleTimers: [ReturnType<typeof setTimeout> | null, ReturnType<typeof setTimeout> | null] = [null, null];

function invalidateLatency(slot: 0 | 1): number {
  const timer = latencySettleTimers[slot];
  if (timer !== null) clearTimeout(timer);
  latencySettleTimers[slot] = null;
  return ++latencyGeneration[slot];
}

// ---------------------------------------------------------------------------
// Native audio input — feed a hardware signal into a slot's loaded plugin
// ---------------------------------------------------------------------------

/**
 * Arm native audio input on `slot`: feed `deviceId`'s capture stream (or the default input when
 * omitted/null) INTO the slot's loaded plugin, isolating one input `channel` (0-based; omitted/null =
 * auto-pick), so an FX plugin (amp-sim) processes a live guitar signal. Reached only through goLive
 * (input + monitor armed as ONE serialized unit) and the unload/swap teardown paths — nothing arms
 * input without the monitor. Rejects (slot left disarmed) if the plugin exposes no audio-input bus.
 */
async function doArmInput(
  slot: 0 | 1,
  deviceId?: string | null,
  channel?: number | null,
): Promise<void> {
  if (!slotPlugins()[slot]) return; // nothing to feed without a loaded plugin
  // `armInput` rejects for a plugin with no input bus (a pure synth); the flag only flips true on
  // success, so the armed state can never claim a plugin is receiving input when it isn't. The
  // rejection propagates out through serializeSlot to the caller (PluginControls shows the error).
  await platform.pluginHost.armInput(slot, deviceId ?? null, channel ?? null);
  setInputArmed((prev) => withAt(prev, slot, true));
}

/**
 * Go LIVE on `slot` (direction A): arm the native capture AND the native low-latency monitor as ONE
 * unit, and mute the web monitor — a guitar → amp-sim plugin is then heard live at low latency, with no
 * delayed WebView2-path doubling. ONE serialized op (input THEN monitor — the cpal full-duplex-friendly
 * order, and what the ASIO duplex lifecycle expects) so the two can't interleave with each other or with
 * load/unload/swap. Rejects (slot left FULLY disarmed) if the plugin has no audio-input bus (a synth):
 * the input arm fails, the monitor is never armed, and a partial arm is rolled back. Reads the persisted
 * capture device/channel + monitor output device from Audio Settings (passed by the caller).
 */
export function goLive(
  slot: 0 | 1,
  inputDeviceId?: string | null,
  channel?: number | null,
  outputDeviceId?: string | null,
): Promise<void> {
  return serializeSlot(slot, () => doGoLive(slot, inputDeviceId, channel, outputDeviceId));
}

async function doGoLive(
  slot: 0 | 1,
  inputDeviceId?: string | null,
  channel?: number | null,
  outputDeviceId?: string | null,
): Promise<void> {
  if (!slotPlugins()[slot]) return; // nothing to go live without a loaded plugin
  // Arm input first — this is the one that rejects a synth (no input bus). If it throws, nothing is
  // armed and the error propagates out through serializeSlot to the caller.
  await doArmInput(slot, inputDeviceId, channel);
  // Push the current output level to the native monitor BEFORE arming (so the cpal stream opens at the
  // right level, not a stale unity), then arm the monitor. If the monitor fails to open, roll the input
  // back so we never leave a half-live state (input armed, monitor not), then surface the error.
  try {
    const g = pluginBridge.gains()[slot];
    if (g != null) await setMonitorGain(slot, g);
    await doArmMonitor(slot, outputDeviceId);
    // The plugin-buffer hook normally warmed capture at load. Keep GO LIVE independently correct if
    // the host connected before that hook was registered (for example during frontend recovery).
    warmCapture();
  } catch (e) {
    await disarmInputInternal(slot);
    throw e;
  }
}

/**
 * Stop LIVE on `slot` (direction A): disarm the native monitor THEN the native input, and unmute the web
 * monitor (done inside disarmMonitorInternal). ONE serialized op so the pair can't interleave. Idempotent
 * (each internal disarm early-returns when its flag is already false).
 */
export function stopLive(slot: 0 | 1): Promise<void> {
  return serializeSlot(slot, () => doStopLive(slot));
}

async function doStopLive(slot: 0 | 1): Promise<void> {
  await disarmMonitorInternal(slot); // monitor first → unmutes the web path (no dropout), then input
  await disarmInputInternal(slot);
}

/**
 * Reconcile THIS module's state with "slot's native capture is gone". Shared by the user-initiated
 * disarm and the terminal-stream-fault fallback so the two can never drift apart. Synchronous and
 * before any await, so it doubles as the double-handling guard: whichever path runs first clears the
 * flag, and the other one's `if (!inputArmed()[slot]) return` makes it a no-op.
 */
function reconcileInputGone(slot: 0 | 1): void {
  setInputArmed((prev) => withAt(prev, slot, false));
}

/**
 * The actual disarm — also called directly from an already-serialized slot op (load-swap / clear
 * teardown in `instrument.ts`), so it must NOT re-enter serializeSlot. Optimistically clears the armed
 * flag, then best-effort tells the host to stop feeding input.
 */
export async function disarmInputInternal(slot: 0 | 1): Promise<void> {
  if (!inputArmed()[slot]) return;
  reconcileInputGone(slot);
  try {
    await platform.pluginHost.disarmInput(slot);
  } catch (e) {
    console.error('[instrument] disarm input failed', e);
    notifyError('Input disarm failed', e);
  }
}

// ---------------------------------------------------------------------------
// Native low-latency monitor — hear a slot's wet via a cpal-out stream on the same device
// ---------------------------------------------------------------------------

/**
 * Arm the native low-latency monitor on `slot`: open a cpal OUTPUT stream on `deviceId` (or the
 * default output when omitted/null) playing the slot's wet, and MUTE the web monitor path so the wet
 * isn't heard twice (the looper record tap is untouched — the wet still records). Serialized on the
 * SAME per-slot chain as load/unload/input so it can't interleave with them. Rejects (slot left
 * disarmed, web still audible) if the stream can't open — the caller surfaces that.
 * Production arms through `goLive`; this monitor-only entry is the browser probe's
 * (`verify/probes/monitor-generation.mjs`), hence `@public` for knip.
 * @public
 */
export function armMonitor(slot: 0 | 1, deviceId?: string | null): Promise<void> {
  return serializeSlot(slot, () => doArmMonitor(slot, deviceId));
}

async function doArmMonitor(slot: 0 | 1, deviceId?: string | null): Promise<void> {
  if (!slotPlugins()[slot]) return; // nothing to monitor without a loaded plugin
  // Open the native stream FIRST (the wet keeps sounding via the web path until it's live, so no
  // dropout), then mute the web path. If arming throws, the web path stays audible and armed stays
  // false — the rejection propagates out through serializeSlot to the caller.
  await platform.pluginHost.armMonitor(slot, deviceId ?? null);
  pluginBridge.setWebMonitorMuted(slot, true);
  setMonitorArmed((prev) => withAt(prev, slot, true));
  // Record-latency compensation: now that the player monitors this slot natively, tell the looper which
  // slot's wet it records + that slot's cpal_out latency, so the auto-compensation lands recorded loops
  // on the click. Fetch immediately (gets the RT-fresh monitor_fill/rate), then once more after the
  // owner has mirrored the device's frames-per-callback (monitor_out_block, ~one gate emit later). That
  // settle updates only if the first take has not already frozen this generation.
  await refreshMonitorLatency(slot, 'generation');
}

/** Fetch native-monitor cpal_out for either a new configuration generation or its delayed settle update.
 * Replies and settle timers belong to the configuration that issued them. Best-effort. Also driven by
 * `audio-devices.ts` `setBufferSize` (a block change starts a new generation for every armed monitor). */
export async function refreshMonitorLatency(slot: 0 | 1, kind: 'generation' | 'settle'): Promise<void> {
  if (!monitorArmed()[slot]) return;
  const generation = kind === 'generation' ? invalidateLatency(slot) : latencyGeneration[slot];
  const request = ++latencyRequest[slot];
  if (kind === 'generation') {
    // Open the generation BEFORE awaiting IPC. A take begun while the query is pending must still
    // freeze this generation once. Until the reply arrives, retain this slot's last accepted estimate
    // (including when it is being promoted after the other of two monitors disappeared).
    recordLatency.beginMonitorGeneration(slot, lastMonitorLatencySeconds[slot]);
  }
  const current = () => monitorArmed()[slot] && latencyGeneration[slot] === generation;
  try {
    const sec = await platform.pluginHost.monitorLatencySeconds(slot);
    if (!current() || latencyRequest[slot] !== request) return;
    const accepted = Number.isFinite(sec) ? Math.max(0, sec) : 0;
    lastMonitorLatencySeconds[slot] = accepted;
    recordLatency.updateMonitorLatency(slot, accepted);
  } catch (e) {
    if (!current() || latencyRequest[slot] !== request) return;
    console.error('[instrument] monitor latency fetch failed', e);
    notifyError("Couldn't read the monitor latency", e);
  } finally {
    if (kind === 'generation' && current()) {
      latencySettleTimers[slot] = setTimeout(() => {
        if (!current()) return; // clearTimeout cannot retract a callback already queued for dispatch.
        latencySettleTimers[slot] = null;
        void refreshMonitorLatency(slot, 'settle');
      }, MONITOR_LATENCY_SETTLE_MS);
    }
  }
}

/** Disarm the slot's native monitor (idempotent). Serialized on the slot's op chain. Probe entry
 * (`verify/probes/monitor-generation.mjs`); production goes through `stopLive`/`disarmMonitorInternal`.
 * @public */
export function disarmMonitor(slot: 0 | 1): Promise<void> {
  return serializeSlot(slot, () => disarmMonitorInternal(slot));
}

/**
 * Reconcile THIS module's state with "slot's native monitor is gone": unmute the web path (the wet
 * keeps sounding through it, at the WebView2 path's higher latency), clear the armed flag, and drop
 * the record-latency monitor registration — a compensation frozen for a monitor that no longer exists
 * would keep pulling takes early. Shared by the user-initiated disarm and the terminal-stream-fault
 * fallback so the two can never drift apart; synchronous and before any await, so it doubles as the
 * double-handling guard (see `reconcileInputGone`).
 */
function reconcileMonitorGone(slot: 0 | 1): void {
  invalidateLatency(slot);
  lastMonitorLatencySeconds[slot] = 0;
  pluginBridge.setWebMonitorMuted(slot, false);
  const remaining = withAt(monitorArmed(), slot, false);
  setMonitorArmed(remaining);
  const survivor: 0 | 1 | null = remaining[0] ? 0 : remaining[1] ? 1 : null;
  const registered = recordLatency.armedSlot();
  if (survivor !== null && (registered === slot || registered === null)) {
    // Compensation owns one native source at a time. If that source disappears while the other
    // slot is still monitored, make the survivor a fresh generation: clear its old settle timer,
    // resample its own queue/cpal terms, and let the next take freeze them normally.
    void refreshMonitorLatency(survivor, 'generation');
  } else {
    recordLatency.clearMonitor(slot); // no survivor/current change ⇒ clear only this registration
  }
}

/**
 * The actual monitor disarm — also called directly from an already-serialized slot op (load-swap /
 * clear teardown in `instrument.ts`), so it must NOT re-enter serializeSlot. Unmutes the web path
 * FIRST (so the wet keeps sounding through it with no dropout), then best-effort drops the native stream.
 */
export async function disarmMonitorInternal(slot: 0 | 1): Promise<void> {
  if (!monitorArmed()[slot]) return;
  reconcileMonitorGone(slot);
  try {
    await platform.pluginHost.disarmMonitor(slot);
  } catch (e) {
    console.error('[instrument] disarm monitor failed', e);
    notifyError('Monitor disarm failed', e);
  }
}

/**
 * Set slot `slot`'s native-monitor output gain (0..1.5). Best-effort fire-and-forget — the value is
 * stored Rust-side regardless of armed state (so the level is right the instant you arm) and the cpal
 * callback reads it without IPC. The web output gain (`setPluginGain`) is driven from the same slider
 * so the heard level matches whether you're monitoring via the web path or the native one. Web build
 * no-ops.
 */
export function setMonitorGain(slot: 0 | 1, value: number): Promise<void> {
  // Returns the (error-swallowed) promise so the caller can AWAIT it where ordering matters — arming
  // the monitor awaits this first so the cpal stream opens with the right gain, not a stale unity (the
  // slider path ignores the return and fires-and-forgets, which is fine: the atomic is idempotent).
  return platform.pluginHost.setMonitorGain(slot, value).catch((e) => {
    console.error('[instrument] set monitor gain failed', e);
    notifyError('Monitor gain change failed', e);
  });
}

// ---------------------------------------------------------------------------
// Terminal native stream faults — a device died mid-jam; fall back, don't go silent
// ---------------------------------------------------------------------------

/**
 * A native cpal stream DIED (interface unplugged, ASIO driver reset). cpal's error callback is
 * terminal — the stream never resumes — so the native side has already dropped it and cleared the RT
 * state that fed it before this event reaches us; all that's left is to stop believing a dead stream
 * is alive. Without this the app kept the web monitor muted against a monitor that no longer exists:
 * the guitar went fully silent with no toast, and the record compensation stayed frozen for it.
 *
 * An `output` fault restores the web monitor path, so the wet keeps sounding (at the WebView2 path's
 * higher latency) instead of going silent. An `input` fault has nothing to fall back TO — the hardware
 * signal is gone — so it only clears the armed state and says so.
 *
 * The armed-flag checks inside the reconcile helpers make a fault racing a user disarm a no-op the
 * second time, in either order (both paths clear the flag synchronously, before any await).
 */
function handleStreamFault(e: { slot: PluginSlot; kind: 'input' | 'output' }): void {
  const slot = e.slot;
  if (e.kind === 'output') {
    if (!monitorArmed()[slot]) return;
    reconcileMonitorGone(slot);
    console.error(
      `[instrument] native monitor stream faulted on slot ${slot} — fell back to the web monitor path`,
    );
    notifyError(
      'Monitor device lost — switched to the backup path',
      'Sound continues at higher latency. Reconnect the interface, then go live again.',
    );
    // Best-effort tidy-up of any straggler owner state. NOT the trigger (the native teardown already
    // ran) and never awaited, so a rejection here can't delay or block the fallback above.
    void platform.pluginHost
      .disarmMonitor(slot)
      .catch((err) => console.error('[instrument] post-fault monitor disarm failed', err));
    return;
  }
  if (!inputArmed()[slot]) return;
  reconcileInputGone(slot);
  console.error(
    `[instrument] native capture stream faulted on slot ${slot} — input is no longer armed`,
  );
  notifyError(
    'Guitar/line input device lost',
    'The capture stream stopped. Reconnect the interface, then go live again.',
  );
  void platform.pluginHost
    .disarmInput(slot)
    .catch((err) => console.error('[instrument] post-fault input disarm failed', err));
}

// Subscribed for the app's lifetime: a fault can land whenever a slot is live, and this module owns
// the armed/monitor state the handler reconciles. Nothing unsubscribes (the module lives as long as
// the app does); the web build hands back a no-op subscription.
platform.pluginHost.onStreamFault(handleStreamFault);

/** Read-only reactive accessor: per-slot input-armed flags. */
export { inputArmed };

/** Read-only reactive accessor: per-slot monitor-armed flags. */
export { monitorArmed };
