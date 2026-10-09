/**
 * The outbox (`src/platform/index.ts`): the UI's engine commands and note-source events on one ordered
 * path, `input_send`, on the web engine fake (`src/platform/host.web.ts`), which keeps every batch it took
 * (`__lf.native.batches`: its epoch and its items, as they came):
 *
 * - nothing leaves before native MIDI's subscribe answered the document's input epoch (an init script
 *   holds it, `__lfMidiSubscribeHold`): the settings the first reset frame sends and a PC key's note wait,
 *   then leave in one batch under that epoch, in the order they were queued;
 * - one batch at a time: while one is on its way (`sendHold`), what the UI queues waits, and leaves whole
 *   and in order as the next batch once it settled;
 * - a lost batch (`failSends`) is sent once more and nothing is told; lost twice, it is logged and toasted,
 *   and as it held a release a `blur` follows (native MIDI lets go of this document's holds);
 * - what native MIDI dropped of a batch (`dropped`: a press with no device running) is logged and toasted
 *   once, and told again only after a press went through.
 *
 * Cannot see Tauri's IPC (that two calls may arrive out of order is why one goes at a time) nor native MIDI
 * (the epoch rule, what it drops and why: the Rust tests of `engine_io::midi` and `engine_io::midi_mode`).
 * Run: pnpm probe input-outbox
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    init: async (p) => {
      await p.addInitScript(() => void (window.__lfEngineFake = true));
      await p.addInitScript(() => void (window.__lfMidiSubscribeHold = new Promise((r) => (window.__lfReleaseSubscribe = r))));
    },
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1);
  await page.evaluate(() => {
    const lane = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };
    window.__lf.native.emit({
      seq: 1,
      reset: true,
      settings: [],
      events: [...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
      meter: { peak: 0, clip: false },
    });
  });
  await page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));

  const batches = () => page.evaluate(() => window.__lf.native.batches.map((b) => ({ epoch: b.epoch, items: b.items })));
  /** A batch's items as short words: `engine:SetBpm`, `on:60`, `off:60`, `target`, `blur`. */
  const words = (items) =>
    items.map((i) => {
      if ('engine' in i) return `engine:${typeof i.engine === 'string' ? i.engine : Object.keys(i.engine)[0]}`;
      if (i.input === 'blur' || i.input === 'allNotesOff') return i.input;
      if (i.input.note) return `${i.input.note.on ? 'on' : 'off'}:${i.input.note.note}`;
      return 'target';
    });
  const notes = (items) => words(items).filter((w) => /^(on|off):/.test(w) || w === 'blur');
  const tap = async (key) => {
    await page.keyboard.down(key);
    await page.keyboard.up(key);
  };
  const toasts = () => page.evaluate(() => window.__lf.notify.toasts().map((t) => [t.message, t.detail, t.count]));
  const clearToasts = () => page.evaluate(() => window.__lf.notify.toasts().forEach((t) => window.__lf.notify.dismissToast(t.id)));

  // ── Nothing before the epoch; then everything queued, in one batch, in order ─────────────────────
  await tap('a');
  await page.waitForTimeout(150);
  assert.deepEqual(await batches(), [], 'nothing leaves before the subscribe answered');
  assert.deepEqual(await page.evaluate(() => [window.__lf.native.sent.length, window.__lf.native.inputSent.length]), [0, 0]);
  await page.evaluate(() => window.__lfReleaseSubscribe());
  await page.waitForFunction(() => window.__lf.native.batches.length >= 1);
  await page.waitForTimeout(50);
  const first = await batches();
  console.log('first batch', JSON.stringify(words(first[0].items)));
  assert.equal(first.length, 1, 'what waited leaves as one batch');
  assert.equal(first[0].epoch, 1, 'under the epoch the subscribe answered');
  const order = words(first[0].items);
  const on = order.indexOf('on:60');
  assert.ok(order.indexOf('engine:SetMasterVolume') >= 0 && order.indexOf('engine:SetMasterVolume') < on, 'the reset frame\'s settings, then the key');
  assert.ok(order.lastIndexOf('target') < on && on < order.indexOf('off:60'), 'the note target, the press, the release, in that order');

  // ── One batch at a time ───────────────────────────────────────────────────────────────────────────
  await page.evaluate(() => void (window.__lf.native.sendHold = new Promise((r) => (window.__lfReleaseSend = r))));
  await page.keyboard.down('s');
  await page.waitForFunction(() => window.__lf.native.batches.length === 2);
  await page.keyboard.up('s');
  await tap('d');
  await page.waitForTimeout(150);
  assert.equal((await batches()).length, 2, 'nothing else leaves while a batch is on its way');
  await page.evaluate(() => {
    window.__lf.native.sendHold = null;
    window.__lfReleaseSend();
  });
  await page.waitForFunction(() => window.__lf.native.batches.length === 3);
  const third = (await batches())[2];
  assert.deepEqual(notes(third.items), ['off:62', 'on:64', 'off:64'], 'what waited leaves whole, in order, as the next batch');

  // ── A lost batch: sent once more; lost twice, told, and a blur lets go of what it held ──────────────
  await page.evaluate(() => void (window.__lf.native.failSends = 1));
  await page.keyboard.down('f');
  await page.waitForFunction(() => window.__lf.native.batches.length === 5);
  const [lost, again] = (await batches()).slice(3);
  assert.deepEqual(again.items, lost.items, 'a lost batch is sent once more');
  assert.deepEqual(notes(again.items), ['on:65']);
  assert.deepEqual(await toasts(), [], 'lost once, nothing is told');

  await page.evaluate(() => void (window.__lf.native.failSends = 2));
  await page.keyboard.up('f');
  await page.waitForFunction(() => window.__lf.native.batches.length === 8);
  const lostTwice = (await batches()).slice(5);
  assert.deepEqual(lostTwice.map((b) => notes(b.items)), [['off:65'], ['off:65'], ['blur']], 'lost twice with a release: a blur follows');
  assert.deepEqual((await toasts()).map(([m]) => m), ['The audio engine did not take a command'], 'and the player is told');
  assert.ok(consoleErrors.some((e) => e.startsWith('[platform] input batch failed')), 'with its release-log line');
  await clearToasts();

  // ── What native MIDI dropped: told once, until a press goes through ─────────────────────────────────
  const droppedLines = () => consoleErrors.filter((e) => e.startsWith('[platform] native MIDI dropped input')).length;
  await page.evaluate(() => void (window.__lf.native.dropped = 'noDevice'));
  await tap('g');
  await tap('h');
  await page.waitForTimeout(100);
  assert.deepEqual(await toasts(), [['The audio engine did not take a command', 'No audio device is running.', 1]], 'told once');
  assert.equal(droppedLines(), 1);
  await page.evaluate(() => void (window.__lf.native.dropped = null));
  await tap('j');
  await page.waitForTimeout(50);
  await page.evaluate(() => void (window.__lf.native.dropped = 'noDevice'));
  await tap('k');
  await page.waitForTimeout(100);
  assert.equal(droppedLines(), 2, 'told again once a press went through');
  await page.evaluate(() => void (window.__lf.native.dropped = null));
});
