/**
 * The golden jam's UI half, in engine mode on the web engine fake (`src/platform/host.web.ts`, the
 * `engine-seam` pattern: an init script sets `window.__lfEngineFake`, the probe scripts the feed through
 * `__lf.native` and reads what the UI sends). The jam itself (grid, takes, overdub, RETAKE, AUTO REC,
 * short takes, COPY/CLEAR, the gates and the CLEAR double press as the engine judges them) is lf-engine
 * `tests/golden_jam.rs`; what stays here is what the browser shows and sends:
 *
 * - KEYS, real key presses through the window handler (`src/app/transport-keys.ts` → `actions.ts`): a digit
 *   sends `SelectTrack`, Space `Action RecDub`, Backspace `Action Undo`, the arrows and PageUp/PageDown
 *   `Action PrevTrack`/`NextTrack`, Delete `Action Clear`, each once; the selection the UI shows is the
 *   feed's `Selected` (5,1,5,1,2,1,2,3 as the engine wraps it). An engine refusal shows its reason on its
 *   lane only and leaves by itself (1.6 s); CLEAR's `ConfirmClear` shows "press again to clear" on its lane
 *   for the confirm window (2.5 s); the next looper press (an arrow, a digit, the second Delete) takes the
 *   cue down at once; a Delete past the window finds no cue, and the engine's answer shows it again; the
 *   engine's `Cleared` empties the lane with no cue left.
 * - FEED → DOM: an AUTO REC arm reads LISTENING, "WAITING FOR INPUT", beside its sensitivity slider (AUTO
 *   sends `SetAutoRecord`); a rolling RETAKE first take reads TAKE 3, and an EMPTY lane's core stays
 *   pressable as the approve gesture (while a plain take records, it is refused); `TakeRejected` and
 *   `PassDropped` each reach the player (a toast naming the lane) and the release log (one console.error);
 *   a `Copied` and a `Cleared` reach the store as the lane's `Mix`, which the fake reports as the engine
 *   does (the copy gets the source's volume, mute and FX, sharing no FX object; CLEAR puts volume, mute
 *   and every FX entry back to their defaults, on that lane only).
 * - EXPORT (`src/session/export.ts`): while a lane overdubs, the Export button is disabled and a wet export
 *   refuses ("Finish the active recording"), while a recovery snapshot (`includeMaster: false`) still builds.
 *
 * Cannot see the native engine, Tauri IPC or any timing: the fake answers no command by itself but a
 * lane's mix, so every state, selection, refusal and rejection the DOM shows was scripted; whether the engine wraps, confirms,
 * clears or rejects as scripted is lf-engine's (`tests/golden_jam.rs`, `tests/actions.rs`).
 * Run: pnpm probe jam-keys
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const MASTER = 2 * BAR;

const lane = (state, extra = {}) => ({
  state,
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
  ...extra,
});
const looping = (state, extra = {}) => lane(state, { length: MASTER, canReverse: true, ...extra });
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
const transport = (master, locked) => ({ Transport: { frame: 0, master, bpm: 120, locked } });
const anchorAt = (frame) => ({ frame, atMs: Date.now(), rate: RATE, grid: 0 });

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  let seq = 0;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const sentAtLeast = async (count) => {
    await page.waitForFunction((n) => window.__lf.native.sent.length >= n, count, { timeout: 5000 });
    return sent();
  };
  /** Press `key` on a page with nothing focused and return exactly what it sent. */
  const press = async (key, count = 1) => {
    await clearSent();
    await page.keyboard.press(key);
    const out = await sentAtLeast(count);
    await page.waitForTimeout(50);
    assert.deepEqual(await sent(), out, `${key} sends nothing more`);
    return out;
  };
  const blur = () => page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
  const lanes = page.locator('.lp-lane');
  const cues = () =>
    page.evaluate(() => [...document.querySelectorAll('.lp-lane')].map((l) => l.querySelector('.lp-lane__wellmsg.is-cue')?.textContent?.trim() ?? ''));
  const selected = () => page.evaluate(() => [...document.querySelectorAll('.lp-lane')].findIndex((l) => l.getAttribute('aria-current') === 'true') + 1);
  /** Emit `frame` and return, in page time, how long the lane cue it raises stays up. */
  const cueLife = (frame) =>
    page.evaluate(
      (f) =>
        new Promise((resolve, reject) => {
          window.__lf.native.emit(f);
          const t0 = performance.now();
          if (!document.querySelector('.lp-lane__wellmsg.is-cue')) return void reject(new Error('no cue raised'));
          const poll = () => {
            if (!document.querySelector('.lp-lane__wellmsg.is-cue')) return void resolve(performance.now() - t0);
            if (performance.now() - t0 > 8000) return void reject(new Error('the cue never went away'));
            setTimeout(poll, 10);
          };
          poll();
        }),
      { seq: ++seq, reset: false, events: [], ...frame },
    );

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await emit({
    reset: true,
    settings: [],
    events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false), { Selected: { frame: 0, lane: 0 } }],
    anchor: anchorAt(0),
    meter: { peak: 0, clip: false },
  });

  // ── FEED → DOM: AUTO REC listening, then a rolling RETAKE ────────────────────────────────────────
  await clearSent();
  await page.getByRole('button', { name: 'Auto record', exact: true }).click();
  assert.deepEqual(await sentAtLeast(1), [{ SetAutoRecord: true }], 'AUTO sends SetAutoRecord');
  await emit({ events: [laneEvent(0, lane('Recording', { autoArmed: true }))] });
  const listening = await page.evaluate(() => {
    const l = document.querySelector('.lp-lane');
    return {
      state: l.getAttribute('data-state'),
      well: l.querySelector('.lp-lane__wellmsg')?.textContent?.trim(),
      slider: document.querySelector('[aria-label="Auto record sensitivity"]') !== null,
    };
  });
  console.log('AUTO arm', JSON.stringify(listening));
  assert.deepEqual(listening, { state: 'listening', well: 'WAITING FOR INPUT', slider: true }, 'an AUTO arm reads LISTENING beside its sensitivity');
  await emit({ events: [laneEvent(0, lane('Empty'))] });
  await clearSent();
  await page.getByRole('button', { name: 'Auto record', exact: true }).click();
  assert.deepEqual(await sentAtLeast(1), [{ SetAutoRecord: false }]);

  const core = (i) => lanes.nth(i).locator('.lp-core');
  await emit({ events: [laneEvent(0, lane('Recording'))] });
  assert.equal(await core(1).isDisabled(), true, 'while a plain take records, an EMPTY lane refuses REC');
  await emit({ events: [laneEvent(0, lane('Recording', { retakePass: 3 }))] });
  assert.equal((await lanes.nth(0).locator('.lp-lane__state').textContent()).trim(), 'TAKE 3', 'a rolling first take shows its pass');
  assert.equal(await core(1).isDisabled(), false, 'an EMPTY lane offers REC as the approve gesture while a take rolls');

  // ── FEED → DOM: a rejected take and a dropped pass reach the player and the release log ────────────
  await emit({
    events: [
      { TakeRejected: { frame: BAR, lane: 0, overdub: true } },
      { TakeRejected: { frame: BAR, lane: 2, overdub: false } },
      { PassDropped: { frame: BAR, lane: 3, pass: 1 } },
    ],
  });
  const toasts = await page.evaluate(() => window.__lf.notify.toasts().map((t) => t.message));
  console.log('toasts', JSON.stringify(toasts));
  for (const message of ['Track 1: overdub layer discarded', 'Track 3: take discarded', 'Track 4: take 1 dropped']) {
    assert.ok(toasts.includes(message), `the player hears "${message}"`);
  }
  const logged = consoleErrors.splice(0);
  console.log('release log', JSON.stringify(logged));
  assert.equal(logged.filter((e) => e.includes('rejected: input gap')).length, 2, 'each rejected take reaches the release log once');
  assert.equal(logged.filter((e) => e.includes('dropped: input gap')).length, 1, 'the dropped pass reaches the release log once');
  assert.equal(logged.length, 3, 'and nothing else');
  await page.evaluate(() => {
    for (const t of window.__lf.notify.toasts()) window.__lf.notify.dismissToast(t.id);
  });

  // ── EXPORT: blocked while a lane overdubs, recovery still builds ───────────────────────────────────
  await page.evaluate(async (master) => {
    const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
    const pcm = Float32Array.from({ length: master }, (_, i) => (i % 24000 === 0 ? 0.9 : 0));
    window.__lf.native.snapshotBytes = encodeSessionBytes(
      { rate: 48000, masterLengthFrames: master, bpm: 120, tracks: [{ index: 0, frames: master, reversed: false, state: 'Playing' }] },
      [pcm],
    ).buffer;
  }, MASTER);
  await emit({ events: [transport(MASTER, true), laneEvent(0, looping('Playing', { canUndo: true })), laneEvent(1, looping('Playing'))] });
  const exportBtn = page.getByRole('button', { name: 'Export loops as a zip of WAV files' });
  assert.equal(await exportBtn.isDisabled(), false, 'a committed loop can export');
  await emit({ events: [laneEvent(0, looping('Overdubbing', { canUndo: true }))] });
  const blocked = await page.evaluate(async () => {
    const { session } = await import('/src/ui/state/audio.ts');
    let error = '';
    try {
      await window.__lf.buildExportBundle(session);
    } catch (e) {
      error = String(e);
    }
    const recovery = await window.__lf.buildExportBundle(session, { includeMaster: false });
    return { error, recovery: recovery !== null };
  });
  console.log('export during a dub', JSON.stringify(blocked));
  assert.equal(await exportBtn.isDisabled(), true, 'Export is disabled while a lane overdubs');
  assert.match(blocked.error, /Finish the active recording/, 'a wet export refuses during capture');
  assert.equal(blocked.recovery, true, 'a recovery snapshot still builds during capture');

  // ── FEED → STORE: COPY carries the lane's mix and FX, CLEAR resets them all ───────────────────────
  // The engine copies and clears (lf-engine tests/sound.rs) and reports each lane's mix after (`Mix`);
  // the lane controls and the FX drawer read it. Two effects dirtied, in both flag and value.
  await emit({ events: [laneEvent(0, looping('Playing', { canUndo: true }))] });
  const pristine = await page.evaluate(() => {
    const L = window.__lf.looper;
    const fx = JSON.stringify(L.fxState(0));
    L.setVolume(0, 0.4);
    L.setMute(0, true);
    L.setFxBypass(0, 0, false); // filter
    L.setFxBypass(0, 3, false); // delay
    L.setFxParam(0, 0, 'cutoff', 500);
    L.setFxParam(0, 3, 'feedback', 0.9);
    return fx;
  });
  // The lane's mix is the engine's: it changes when the fake reports it (`Mix`).
  await page.waitForFunction(() => window.__lf.looper.trackMuted(0) && window.__lf.looper.fxState(0)[3].params.feedback !== 0.4, undefined, { timeout: 5000 });
  const mixed = { pristine, dirty: await page.evaluate(() => JSON.stringify(window.__lf.looper.fxState(0))) };
  assert.notEqual(mixed.dirty, mixed.pristine, 'the lane was actually dirtied first');
  await emit({ events: [{ Copied: { frame: BAR, from: 0, to: 3, feedback: 1 } }, laneEvent(3, looping('Playing'))] });
  await page.waitForFunction(() => window.__lf.looper.trackMuted(3), undefined, { timeout: 5000 });
  const copied = await page.evaluate(() => {
    const L = window.__lf.looper;
    return {
      vol: L.trackVolume(3),
      muted: L.trackMuted(3),
      fx: JSON.stringify(L.fxState(3)),
      shared: L.fxState(3).some((s, k) => s === L.fxState(0)[k] || s.params === L.fxState(0)[k].params),
    };
  });
  assert.deepEqual({ vol: copied.vol, muted: copied.muted }, { vol: 0.4, muted: true }, 'volume and mute followed the copy');
  assert.equal(copied.fx, mixed.dirty, 'FX followed the copy');
  assert.equal(copied.shared, false, 'the copy shares no FX state with its source');
  await emit({ events: [{ Cleared: { frame: BAR, lane: 0 } }, laneEvent(0, lane('Empty'))] });
  await page.waitForFunction(() => !window.__lf.looper.trackMuted(0), undefined, { timeout: 5000 });
  const cleared = await page.evaluate(() => {
    const L = window.__lf.looper;
    return { vol: L.trackVolume(0), muted: L.trackMuted(0), fx: JSON.stringify(L.fxState(0)), bypassed: L.fxState(0).every((s) => s.bypassed), copy: JSON.stringify(L.fxState(3)) };
  });
  assert.deepEqual({ vol: cleared.vol, muted: cleared.muted }, { vol: 1, muted: false }, 'CLEAR reset volume and mute');
  assert.ok(cleared.fx === mixed.pristine && cleared.bypassed, 'CLEAR reset ALL FX entries in lockstep');
  assert.equal(cleared.copy, mixed.dirty, "and only that lane's");

  // ── KEYS: a refused Space shows its reason on its lane only, and leaves by itself ──────────────────
  await emit({ events: [laneEvent(0, looping('Stopped', { canUndo: true })), laneEvent(1, looping('Playing'))] });
  await blur();
  assert.deepEqual(await press('2'), [{ SelectTrack: 1 }], 'a digit selects its track');
  assert.equal(await selected(), 1, 'the selection waits for the feed');
  await emit({ events: [{ Selected: { frame: BAR, lane: 1 } }] });
  assert.equal(await selected(), 2, "the feed's selection shows");
  assert.deepEqual(await press('Space'), [{ Action: 'RecDub' }], "Space sends the engine's REC/DUB");
  assert.deepEqual(await press('1'), [{ SelectTrack: 0 }]);
  await emit({ events: [{ Selected: { frame: BAR, lane: 0 } }] });
  assert.deepEqual(await press('Space'), [{ Action: 'RecDub' }], 'Space on a STOPPED lane still goes to the engine, which judges it');
  await emit({ events: [{ Refused: { frame: BAR, lane: 0, reason: 'PlayFirst' } }] });
  const refused = await cues();
  console.log('cues after a refused Space', JSON.stringify(refused));
  assert.deepEqual(refused, ['play first to overdub', '', '', '', ''], 'a refused Space shows its reason on the selected lane, and only there');
  await press('1'); // a press takes the cue down; raise it again to time it
  const playFirstLife = await cueLife({ events: [{ Refused: { frame: BAR, lane: 0, reason: 'PlayFirst' } }] });
  console.log(`the refusal cue lived ${playFirstLife.toFixed(0)} ms`);
  assert.ok(playFirstLife > 1500 && playFirstLife < 4500, `the refusal cue went away by itself after a moment (${playFirstLife.toFixed(0)} ms)`);

  // ── KEYS: UNDO is Backspace, and a second Backspace redoes ────────────────────────────────────────
  assert.deepEqual(await press('Backspace'), [{ Action: 'Undo' }], "Backspace sends the engine's UNDO");
  assert.deepEqual(await press('Backspace'), [{ Action: 'Undo' }], 'a second Backspace is the same action: the engine redoes');

  // ── KEYS: next / prev, and the selection the engine wraps ────────────────────────────────────────
  const navKeys = ['ArrowUp', 'ArrowDown', 'PageUp', 'PageDown', 'PageDown', 'ArrowLeft', 'ArrowRight', 'ArrowRight'];
  const engineSelects = [4, 0, 4, 0, 1, 0, 1, 2]; // what the engine answers (golden_jam.rs: 5,1,5,1,2,1,2,3)
  const navSent = [];
  const navSeen = [];
  for (const [k, key] of navKeys.entries()) {
    navSent.push(...(await press(key)).map((c) => c.Action));
    await emit({ events: [{ Selected: { frame: BAR, lane: engineSelects[k] } }] });
    navSeen.push(await selected());
  }
  console.log('nav sent', navSent.join(','), 'seen', navSeen.join(','));
  assert.deepEqual(navSent, ['PrevTrack', 'NextTrack', 'PrevTrack', 'NextTrack', 'NextTrack', 'PrevTrack', 'NextTrack', 'NextTrack'], 'the arrow and page keys step the selection');
  assert.deepEqual(navSeen, [5, 1, 5, 1, 2, 1, 2, 3], 'the selection shown is the one the engine wrapped to');

  // ── KEYS: CLEAR is Delete, guarded by the engine; the cue is the UI's ────────────────────────────
  await emit({ events: [{ Copied: { frame: BAR, from: 0, to: 2, feedback: 1 } }, laneEvent(2, looping('Stopped'))] });
  const confirm = { events: [{ Refused: { frame: BAR, lane: 2, reason: 'ConfirmClear' } }] };
  const laneNow = async () => ({ state: await lanes.nth(2).getAttribute('data-state'), cue: (await cues())[2] });
  assert.deepEqual(await press('3'), [{ SelectTrack: 2 }]);
  await emit({ events: [{ Selected: { frame: BAR, lane: 2 } }] });
  assert.deepEqual(await press('Delete'), [{ Action: 'Clear' }], "Delete sends the engine's CLEAR");
  await emit(confirm);
  const oneDelete = await laneNow();
  assert.ok(oneDelete.state !== 'empty' && oneDelete.cue === 'press again to clear', `one Delete does not clear; the lane says to press again (${JSON.stringify(oneDelete)})`);
  assert.deepEqual(await press('ArrowDown'), [{ Action: 'NextTrack' }]);
  assert.equal((await laneNow()).cue, '', 'an arrow key between the two Deletes takes the cue down');
  await press('ArrowUp');
  assert.deepEqual(await press('Delete'), [{ Action: 'Clear' }]);
  await emit(confirm);
  assert.equal((await laneNow()).cue, 'press again to clear', 'the engine asks again after the arrow');
  assert.deepEqual(await press('1'), [{ SelectTrack: 0 }]);
  assert.equal((await laneNow()).cue, '', 'a digit key between the two Deletes takes the cue down');
  await press('3');
  await press('Delete');
  const confirmLife = await cueLife(confirm);
  console.log(`the CLEAR cue lived ${confirmLife.toFixed(0)} ms`);
  assert.ok(confirmLife > 2400 && confirmLife < 4500, `the CLEAR cue lasts the 2.5 s confirm window, then goes (${confirmLife.toFixed(0)} ms)`);
  assert.deepEqual(await press('Delete'), [{ Action: 'Clear' }], 'a Delete after the confirm window goes to the engine');
  await emit(confirm);
  const lateDelete = await laneNow();
  assert.ok(lateDelete.state !== 'empty' && lateDelete.cue === 'press again to clear', 'a Delete after the confirm window arms again instead of clearing');
  assert.deepEqual(await press('Delete'), [{ Action: 'Clear' }]);
  assert.equal((await laneNow()).cue, '', 'the second Delete takes the cue down at once');
  await emit({ events: [{ Cleared: { frame: BAR, lane: 2 } }, laneEvent(2, lane('Empty'))] });
  const doubleDelete = await laneNow();
  assert.deepEqual(doubleDelete, { state: 'empty', cue: '' }, 'two Deletes in a row clear the track, no cue left');

  assert.deepEqual(consoleErrors, [], 'no console errors');
});
