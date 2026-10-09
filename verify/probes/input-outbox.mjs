/**
 * The outbox (`src/platform/index.ts`): the UI's engine commands and note-source events on one ordered
 * path, `input_send`, on the web engine fake (`src/platform/host.web.ts`), which keeps every batch it took
 * (`__lf.native.batches`: its epoch and its items, as they came):
 *
 * - nothing leaves before native MIDI's subscribe answered the page's input epoch (an init script holds
 *   it, `__lfMidiSubscribeHold`); a later subscribe that answers first gives the waiting batch its epoch
 *   (the settings the first reset frame sends and a PC key's note leave in one batch, in the order queued);
 *   a subscribe that never answers is given up after its wait: the player is told, input goes on under
 *   epoch 0 (native refuses its input events), and a late answer becomes the epoch;
 * - one batch at a time: while one is on its way (`sendHold`), what the UI queues waits, and leaves whole
 *   and in order as the next batch once it settled;
 * - a lost batch (`failSends`) is sent once more and nothing is told; lost twice, it is logged and toasted,
 *   and as it held a release a `blur` follows; a blur lost too rides at the head of the next batch;
 * - a batch that gets no answer is given up after its wait (never sent twice: it may still run), the next
 *   goes; meanwhile the waiting batch keeps at most 256 items: past them note-ons are dropped with one
 *   release-log line, never a release;
 * - what native MIDI dropped of a batch (`dropped`: a press with no device running) is logged and toasted
 *   once, and told again only after a press went through (a setting going through is no press); input of a
 *   page native MIDI no longer counts as current (`stale`) is logged once.
 *
 * Cannot see Tauri's IPC (that two calls may arrive out of order is why one goes at a time) nor native MIDI
 * (the epoch rule, what it drops and why: the Rust tests of `engine_io::midi` and `engine_io::midi_mode`).
 * Run: pnpm probe input-outbox
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RESET = {
  seq: 1,
  reset: true,
  settings: [],
  events: [
    ...[0, 1, 2, 3, 4].map((i) => ({
      Lane: { frame: 0, lane: i, info: { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 } },
    })),
    { Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
    { Selected: { frame: 0, lane: 0 } },
  ],
  anchor: { frame: 0, atMs: 0, rate: 48000, grid: 0 },
  meter: { peak: 0, clip: false },
};

/** A batch's items as short words: `engine:SetBpm`, `on:60`, `off:60`, `target`, `blur`. */
const words = (items) =>
  items.map((i) => {
    if ('engine' in i) return `engine:${typeof i.engine === 'string' ? i.engine : Object.keys(i.engine)[0]}`;
    if (i.input === 'blur' || i.input === 'allNotesOff') return i.input;
    if (i.input.note) return `${i.input.note.on ? 'on' : 'off'}:${i.input.note.note}`;
    return 'target';
  });
const notes = (items) => words(items).filter((w) => /^(on|off):/.test(w) || w === 'blur');

/** The app on the fake, its subscribe held until the probe lets it answer; helpers on its page. */
async function app(open) {
  const { page, consoleErrors } = await open({
    init: async (p) => {
      await p.addInitScript(() => void (window.__lfEngineFake = true));
      await p.addInitScript(() => void (window.__lfMidiSubscribeHold = new Promise((r) => (window.__lfReleaseSubscribe = r))));
    },
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1);
  await page.evaluate((frame) => window.__lf.native.emit({ ...frame, anchor: { ...frame.anchor, atMs: Date.now() } }), RESET);
  await page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
  return {
    page,
    consoleErrors,
    batches: () => page.evaluate(() => window.__lf.native.batches.map((b) => ({ epoch: b.epoch, items: b.items }))),
    count: () => page.evaluate(() => window.__lf.native.batches.length),
    tap: async (key) => {
      await page.keyboard.down(key);
      await page.keyboard.up(key);
    },
    toasts: () => page.evaluate(() => window.__lf.notify.toasts().map((t) => [t.message, t.detail, t.count])),
    clearToasts: () => page.evaluate(() => window.__lf.notify.toasts().forEach((t) => window.__lf.notify.dismissToast(t.id))),
    seam: (name, value) => page.evaluate(([n, v]) => void (window.__lf.native[n] = v), [name, value]),
    lines: (prefix) => consoleErrors.filter((e) => e.startsWith(prefix)).length,
  };
}

await probe(async ({ open }) => {
  // ── A subscribe that never answers is given up after its wait; a late answer becomes the epoch ──────
  {
    const a = await app(open);
    await a.tap('a');
    await a.page.waitForFunction(() => window.__lf.native.batches.length >= 1, undefined, { timeout: 5000 });
    const given = await a.batches();
    assert.equal(given[0].epoch, 0, 'given up, input goes on under epoch 0 (native refuses its input events)');
    assert.deepEqual((await a.toasts()).map(([m]) => m), ['The app lost contact with MIDI']);
    assert.equal(a.lines('[platform] native MIDI did not answer the subscribe in time'), 1);
    await a.page.evaluate(() => window.__lfReleaseSubscribe());
    await a.page.waitForTimeout(50);
    await a.tap('s');
    await a.page.waitForFunction((n) => window.__lf.native.batches.length > n, given.length);
    assert.equal((await a.batches()).at(-1).epoch, 1, 'the late answer is the epoch from then on');
    await a.page.close();
  }

  const a = await app(open);
  const { page, batches, count, tap, toasts, clearToasts, seam, lines } = a;

  // ── Nothing before the epoch; a later subscribe that answers first gives the batch its epoch ──────────
  await tap('a');
  await page.waitForTimeout(150);
  assert.deepEqual(await batches(), [], 'nothing leaves before the subscribe answered');
  assert.deepEqual(await page.evaluate(() => [window.__lf.native.sent.length, window.__lf.native.inputSent.length]), [0, 0]);
  await page.evaluate(async () => {
    window.__lfMidiSubscribeHold = undefined;
    (await import('/src/ui/state/midi.ts')).startMidi();
  });
  await page.waitForFunction(() => window.__lf.native.batches.length >= 1);
  await page.waitForTimeout(50);
  const first = await batches();
  console.log('first batch', JSON.stringify(words(first[0].items)));
  assert.equal(first.length, 1, 'what waited leaves as one batch');
  assert.equal(first[0].epoch, 2, 'under the later subscribe\'s epoch, which answered first');
  const order = words(first[0].items);
  const on = order.indexOf('on:60');
  assert.ok(order.indexOf('engine:SetMasterVolume') >= 0 && order.indexOf('engine:SetMasterVolume') < on, 'the reset frame\'s settings, then the key');
  assert.ok(order.lastIndexOf('target') < on && on < order.indexOf('off:60'), 'the note target, the press, the release, in that order');
  await page.evaluate(() => window.__lfReleaseSubscribe());
  await page.waitForTimeout(50);
  let mark = await count();
  await tap('e');
  await page.waitForFunction((m) => window.__lf.native.batches.slice(m).some((b) => b.items.some((i) => i.input?.note?.note === 63 && !i.input.note.on)), mark);
  assert.deepEqual([...new Set((await batches()).slice(mark).map((b) => b.epoch))], [2], 'the earlier subscribe\'s late answer changes nothing');

  // ── One batch at a time ───────────────────────────────────────────────────────────────────────────
  mark = await count();
  await page.evaluate(() => void (window.__lf.native.sendHold = new Promise((r) => (window.__lfReleaseSend = r))));
  await page.keyboard.down('s');
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 1, mark);
  await page.keyboard.up('s');
  await tap('d');
  await page.waitForTimeout(150);
  assert.equal(await count(), mark + 1, 'nothing else leaves while a batch is on its way');
  await page.evaluate(() => {
    window.__lf.native.sendHold = null;
    window.__lfReleaseSend();
  });
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 2, mark);
  assert.deepEqual(notes((await batches())[mark + 1].items), ['off:62', 'on:64', 'off:64'], 'what waited leaves whole, in order, as the next batch');

  // ── A lost batch: sent once more; lost twice, told, and a blur lets go of what it held ──────────────
  mark = await count();
  await seam('failSends', 1);
  await page.keyboard.down('f');
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 2, mark);
  const [lost, again] = (await batches()).slice(mark);
  assert.deepEqual(again.items, lost.items, 'a lost batch is sent once more');
  assert.deepEqual(notes(again.items), ['on:65']);
  assert.deepEqual(await toasts(), [], 'lost once, nothing is told');

  mark = await count();
  await seam('failSends', 2);
  await page.keyboard.up('f');
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 3, mark);
  assert.deepEqual((await batches()).slice(mark).map((b) => notes(b.items)), [['off:65'], ['off:65'], ['blur']], 'lost twice with a release: a blur follows');
  assert.deepEqual((await toasts()).map(([m]) => m), ['The audio engine did not take a command'], 'and the player is told');
  assert.equal(lines('[platform] input batch failed'), 1, 'with its release-log line');
  await clearToasts();

  // The blur lost too: it rides at the head of the next batch, until one is taken.
  mark = await count();
  await page.keyboard.down('g');
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 1, mark);
  await seam('failSends', 3);
  await page.keyboard.up('g');
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 4, mark);
  assert.equal(lines('[platform] the blur after a lost release failed'), 1);
  mark = await count();
  await tap('h');
  await page.waitForFunction((m) => window.__lf.native.batches.slice(m).some((b) => b.items.some((i) => i.input?.note?.note === 69 && !i.input.note.on)), mark);
  const owed = (await batches()).slice(mark).flatMap((b) => notes(b.items));
  assert.deepEqual(owed, ['blur', 'on:69', 'off:69'], 'the owed blur leads the next batch, once');
  await clearToasts();

  // ── No answer: given up after its wait, never sent twice; the waiting batch is capped ────────────────
  mark = await count();
  await page.evaluate(() => void (window.__lf.native.sendHold = new Promise(() => {})));
  await page.keyboard.down('w');
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 1, mark);
  await page.evaluate(() => {
    for (let i = 0; i < 300; i++) window.__lf.input.note('pointer:9', 80, 100, true);
    window.__lf.input.note('pointer:9', 80, 0, false);
  });
  assert.equal(lines('[platform] input waits on a native call'), 1, 'the overflow is logged once');
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 2, mark, { timeout: 5000 });
  const capped = (await batches())[mark + 1];
  assert.equal(capped.items.length, 257, 'the waiting batch kept 256 items and the release');
  assert.equal(words(capped.items).at(-1), 'off:80', 'the release was never dropped');
  assert.ok((await toasts()).some(([m]) => m === 'The audio engine did not take a command'), 'the batch with no answer is told');
  // That batch gets no answer either, and held a release: its blur gets none, and is owed.
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 3, mark, { timeout: 5000 });
  assert.deepEqual(notes((await batches())[mark + 2].items), ['blur']);
  await page.waitForTimeout(2300);
  assert.equal((await batches()).slice(mark).filter((b) => notes(b.items).join() === 'on:61').length, 1, 'a batch with no answer is never sent twice');
  await seam('sendHold', null);
  await page.keyboard.up('w');
  await page.waitForFunction((m) => window.__lf.native.batches.length === m + 4, mark, { timeout: 5000 });
  assert.deepEqual(notes((await batches())[mark + 3].items), ['blur', 'off:61'], 'the owed blur leads the next batch');
  await clearToasts();

  // ── What native MIDI dropped: told once, until a press goes through (a setting is none) ─────────────
  const droppedLines = () => lines('[platform] native MIDI dropped input');
  await seam('dropped', 'noDevice');
  await tap('g');
  await tap('h');
  await page.waitForTimeout(100);
  assert.deepEqual(await toasts(), [['The audio engine did not take a command', 'No audio device is running.', 1]], 'told once');
  assert.equal(droppedLines(), 1);
  await seam('dropped', null);
  await page.getByRole('button', { name: 'BPM plus' }).click();
  await page.waitForTimeout(50);
  await seam('dropped', 'noDevice');
  await tap('y');
  await page.waitForTimeout(100);
  assert.equal(droppedLines(), 1, 'a setting that went through is no press: not told again');
  await seam('dropped', null);
  await tap('j');
  await page.waitForTimeout(50);
  await seam('dropped', 'noDevice');
  await tap('k');
  await page.waitForTimeout(100);
  assert.equal(droppedLines(), 2, 'told again once a press went through');

  // ── A page native MIDI no longer counts as current: logged once ───────────────────────────────────
  await seam('dropped', 'stale');
  await tap('a');
  await tap('s');
  await page.waitForTimeout(100);
  assert.equal(lines("[platform] native MIDI refused this page's input: epoch 2"), 1, 'logged once for its epoch');
  assert.equal(droppedLines(), 2, 'and not told as a dropped press');
  await seam('dropped', null);
});
