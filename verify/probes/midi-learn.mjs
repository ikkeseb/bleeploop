/**
 * MIDI learn's UI over native MIDI: the real Audio Settings learn row and bindings list, `src/ui/state/midi.ts`
 * and the bridge `src/app/midi-actions.ts`, on the web engine fake (`src/platform/host.web.ts`, the
 * engine-seam pattern). What the UI asks native MIDI is read from `__lf.native.midiCalls`; what native MIDI
 * tells it is scripted with `__lf.native.midiEmit` (its serde JSON); what the UI sends the engine from
 * `__lf.native.sent`. The fake answers `learn` and `cancelLearn` with a `learning` event and nothing else.
 *
 * - boot: the UI subscribes first, then hands the web build's `lf.midiLearn` to `importLegacy` verbatim
 *   (the key stays); the import's answer is told once: a toast for the bindings that wait for a port
 *   picked here, one for those it could not read (with its release-log line); a launch that cannot read
 *   the key logs it and hands nothing over (an empty list would mark the import done for good); each toast's
 *   action (a button the keyboard reaches) and Help's MIDI LEARN link open Audio Settings on MIDI PEDALS
 *   with focus in the panel, the toast gone and Help closed;
 * - LEARN waits for an open port and says why in its title (no input open, or the input held by another
 *   program); a pick (a lane action on a named track, a global one without a track) and LEARN call
 *   `learn(action, target)` and show LISTENING at once; Esc ends LISTENING in the same task (before native
 *   MIDI answers), calls `cancelLearn` and leaves the panel open; a second LEARN click and closing the
 *   panel cancel too; every native MIDI call waits for the one before it, so a cancel made while the learn
 *   is still on its way reaches native MIDI after it; a `learning: null` event ends LISTENING, an
 *   `awaitingRelease` event shows the learned message's hint until its null;
 * - the list renders native MIDI's `bindings` as a table every line shares (action and message, pedal
 *   kind, HOLD on REC/DUB, ✕; one height a line), and a line from the previous version says so; each line's kind switch, HOLD and ✕ call `setMomentary`, `setHold`
 *   and `forget` by its index and the store revision its list came with, and a refused edit (the list
 *   changed since) toasts nothing; a line that does not run says why (blocked with native MIDI's reason,
 *   not connected, several ports or another missing port of its name, its port held by another program);
 *   one no port can be found for alone (blocked, no port of its name, several ports, another missing
 *   port) offers a port select that starts on no port, with ASSIGN off until a port is picked
 *   (`assign(revision, index, portId)`); a live one offers none;
 * - a binding's `run` runs the UI's action with no second `Press` (native MIDI sent it): the stage view
 *   opens, steps its look and closes; two TAPs about 500 ms apart send their tapped `SetBpm` (the
 *   transport starts at a tempo they cannot round to); GO LIVE sends no `Press`;
 *   `pressed` takes the lane cue down; `refused` (every HOLD control down) puts its cue on the selected
 *   lane;
 * - `gone` toasts "MIDI device disconnected — <name>" ("Held notes were released.") with its release-log
 *   line; a store problem toasts and logs.
 *
 * logs/midi-learn/bindings.png shows the row and a list with every binding state, for the eye (the
 * assignment control is a taste item). What moved to Rust (`src-tauri/src/engine_io/midi/` tests): the
 * learn capture, consume-first, channel-mode CCs never learned, the momentary/latching read and its 10 s
 * release wait, HOLD's controls and their release, a port's bindings by identity, the legacy import and
 * the store, and which engine commands a binding fires. Cannot see a real controller, WebView2 or Tauri
 * IPC. Run: pnpm probe midi-learn
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { isDeepStrictEqual } from 'node:util';
import { probe } from '../harness/probe.ts';

const LEARN = '[aria-label="Learn a MIDI control for this action"]';
const PICK = '[aria-label="Action to learn"]';
const TRACK = '[aria-label="Track the action acts on"]';
const LEGACY = JSON.stringify([{ port: 'input-1', portName: 'FS-6 Pedal', channel: 0, kind: 'cc', number: 64, action: 'recDub', pressHigh: true, momentary: true }]);

const binding = (extra) => ({ portId: 'pedal', portName: 'FS-6 Pedal', channel: 0, kind: 'cc', number: 64, action: 'recDub', target: null, pressHigh: true, momentary: true, hold: false, ...extra });
const listed = (b, extra = {}) => ({ binding: b, origin: 'native', ordinal: false, blocked: null, displayName: b.portName, state: 'live', ...extra });
const toastsOf = (page) => page.evaluate(() => window.__lf.notify.toasts().map((t) => [t.message, t.detail]));

await probe(async ({ open }) => {
  // ── The import's answer, told once; a key that cannot be read hands nothing over ─────────────────────
  {
    const answer = { already: false, unreadable: null, imported: [0], blocked: [{ index: 1, why: 'x' }, { index: 2, why: 'x' }], rejected: [{ index: 3, reason: 'missing field `action`' }], skipped: [] };
    const told = await open({
      init: async (p) => {
        await p.addInitScript(() => void (window.__lfEngineFake = true));
        await p.addInitScript((a) => void (window.__lfMidiImportAnswer = a), answer);
      },
    });
    await told.page.waitForFunction(() => window.__lf.notify.toasts().length >= 2, undefined, { timeout: 5000 });
    const toasts = await toastsOf(told.page);
    console.log('import toasts', JSON.stringify(toasts));
    assert.deepEqual(toasts, [
      ['Could not run 2 MIDI bindings from the previous version. Pick their port in Audio Settings.', undefined],
      ['Could not read 1 MIDI binding from the previous version. Learn them again in Audio Settings.', undefined],
    ], 'the import tells what waits for a port and what it could not read');
    assert.deepEqual(told.consumeConsoleErrors(/^\[midi\]/), [
      '[midi] stored web binding 3 not imported: missing field `action`',
      '[midi] 2 MIDI bindings from the previous version need a port picked in Audio Settings',
    ]);
    // Each toast offers the page it names as a real button; the keyboard reaches it, and pressing it
    // opens Audio Settings on MIDI PEDALS, takes the toast away and puts focus in the panel.
    const shown = () => told.page.evaluate(() => ({
      open: !!document.getElementById('lf-audio-popover'),
      page: document.querySelector('.audio-settings [role="tab"][aria-selected="true"]')?.id ?? null,
      inPanel: !!document.getElementById('lf-audio-popover')?.contains(document.activeElement),
      help: !!document.getElementById('lf-help-popover'),
      toasts: window.__lf.notify.toasts().length,
    }));
    const actions = await told.page.locator('.toast__action').allTextContents();
    assert.deepEqual(actions, ['Audio Settings → MIDI PEDALS', 'Audio Settings → MIDI PEDALS'], 'each import toast offers the MIDI page');
    await told.page.locator('.toast__action').first().focus();
    await told.page.keyboard.press('Enter');
    await told.page.waitForFunction(() => !!document.getElementById('lf-audio-popover'));
    const fromToast = await shown();
    console.log('toast action', JSON.stringify(fromToast));
    assert.deepEqual(fromToast, { open: true, page: 'lf-settings-tab-midi', inPanel: true, help: false, toasts: 1 }, "a toast's action opens the MIDI page, and the toast goes");
    // Help's MIDI LEARN line does the same, and closes Help.
    await told.page.evaluate(() => { window.__lf.ui.closeSettings(); window.__lf.ui.openHelp(); });
    await told.page.locator('.help__link:visible', { hasText: 'MIDI LEARN' }).click();
    await told.page.waitForFunction(() => !!document.getElementById('lf-audio-popover'));
    const fromHelp = await shown();
    console.log('help link', JSON.stringify(fromHelp));
    assert.deepEqual({ ...fromHelp, toasts: undefined }, { open: true, page: 'lf-settings-tab-midi', inPanel: true, help: false, toasts: undefined }, "Help's link opens the MIDI page and closes Help");
    await told.page.close();

    const unreadable = await open({
      init: async (p) => {
        await p.addInitScript(() => void (window.__lfEngineFake = true));
        await p.addInitScript(() => {
          const getItem = Storage.prototype.getItem;
          Storage.prototype.getItem = function (key) {
            if (key === 'lf.midiLearn') throw new Error('storage blocked');
            return getItem.call(this, key);
          };
        });
      },
    });
    await unreadable.page.waitForFunction(() => window.__lf.native.midiCalls.some((c) => c[0] === 'subscribe'));
    await unreadable.page.waitForTimeout(200);
    const calls = await unreadable.page.evaluate(() => window.__lf.native.midiCalls.map((c) => c[0]));
    assert.ok(!calls.includes('importLegacy'), `a key that cannot be read hands nothing over: ${calls}`);
    assert.equal(
      unreadable.consumeConsoleErrors('[midi] the stored web bindings could not be read; none handed over this launch').length, 1,
      `and the release log says so: ${JSON.stringify(unreadable.consoleErrors)}`,
    );
    assert.deepEqual((await toastsOf(unreadable.page)).map(([m]) => m), ['Could not read the MIDI bindings from the previous version. Learn them again in Audio Settings.'], 'and the player hears it');
    await unreadable.page.close();

    const refused = await open({
      init: async (p) => {
        await p.addInitScript(() => void (window.__lfEngineFake = true));
        await p.addInitScript(() => void (window.__lfMidiImportAnswer = 'fail'));
      },
    });
    await refused.page.waitForFunction(() => window.__lf.notify.toasts().length >= 1, undefined, { timeout: 5000 });
    assert.deepEqual((await toastsOf(refused.page)).map(([m]) => m), ['Could not read the MIDI bindings from the previous version. Learn them again in Audio Settings.'], 'a refused import is told');
    assert.equal(refused.consumeConsoleErrors('[midi] importing the stored web bindings failed').length, 1, 'with its release-log line');
    await refused.page.close();
  }

  const { page, consumeConsoleErrors } = await open({
    init: async (p) => {
      await p.addInitScript(() => void (window.__lfEngineFake = true));
      await p.addInitScript((legacy) => localStorage.setItem('lf.midiLearn', legacy), LEGACY);
    },
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1);
  const lane = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };
  await page.evaluate((info) => window.__lf.native.emit({
    seq: 1,
    reset: true,
    settings: [],
    // 200 BPM: a tempo two TAPs about 500 ms apart (120 BPM or slower) cannot round to. At 120, a gap
    // under ~502 ms rounded to the tempo already set and `setBpm` rightly sent nothing.
    events: [...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info } })),
      { Transport: { frame: 0, master: 0, bpm: 200, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
    meter: { peak: [0, 0], clip: [false, false] },
  }), lane);

  /** The native MIDI calls, each without the page's epoch it carries (checked once, at the end: 1). */
  const calls = () => page.evaluate(() => window.__lf.native.midiCalls.map((c) => (c[0] === 'subscribe' ? c : [c[0], ...c.slice(2)])));
  const lastCall = async () => (await calls()).at(-1);
  /** Hand native MIDI's events to the UI (each lands a microtask later: what it changes is waited on). */
  const midi = async (...events) => {
    for (const e of events) await page.evaluate((ev) => window.__lf.native.midiEmit(ev), e);
  };
  /** Reads `read` until `want` holds (a predicate, or a value it deep-equals) or 2 s pass; the last read. */
  const until = async (read, want) => {
    const holds = typeof want === 'function' ? want : (value) => isDeepStrictEqual(value, want);
    const deadline = Date.now() + 2000;
    for (;;) {
      const value = await read();
      if (holds(value) || Date.now() >= deadline) return value;
      await new Promise((r) => setTimeout(r, 10));
    }
  };
  /** Asserts that `read` comes to deep-equal `want` within `until`'s deadline. */
  const settles = async (read, want, message) => assert.deepEqual(await until(read, want), want, message);
  const listening = () => page.evaluate((s) => document.querySelector(s)?.getAttribute('aria-pressed') ?? null, LEARN);
  const openSettings = async () => {
    await page.evaluate(() => window.__lf.ui.openSettings('midi'));
    await page.waitForSelector(LEARN);
  };
  const sentSince = (from) => page.evaluate((m) => window.__lf.native.sent.slice(m), from);
  const mark = () => page.evaluate(() => window.__lf.native.sent.length);

  // ── Boot: the subscription and the legacy hand-over ─────────────────────────────────────────────────
  await page.waitForFunction(() => window.__lf.native.midiCalls.some((c) => c[0] === 'importLegacy'));
  const boot = await calls();
  console.log('boot calls', JSON.stringify(boot));
  assert.deepEqual(boot[0], ['subscribe'], 'the UI subscribes to native MIDI first');
  assert.deepEqual(boot.filter((c) => c[0] === 'importLegacy'), [['importLegacy', LEGACY]], 'the web build\'s bindings go to native MIDI verbatim, once');
  assert.equal(await page.evaluate(() => localStorage.getItem('lf.midiLearn')), LEGACY, 'the legacy key stays');
  assert.deepEqual(await toastsOf(page), [], 'an import with nothing waiting tells nothing');

  // ── The learn row ───────────────────────────────────────────────────────────────────────────────────
  await openSettings();
  const learnTitle = () => page.evaluate((s) => document.querySelector(s)?.getAttribute('title') ?? null, LEARN);
  assert.equal(await page.isDisabled(LEARN), true, 'LEARN waits for an open port');
  assert.equal(await learnTitle(), 'No MIDI input is open', 'and says why');
  await midi({ ports: { ports: [{ id: 'keys', name: 'Keystation 49', state: 'busy' }] } });
  await settles(learnTitle, 'The MIDI input is held by another program', 'a port another program holds is no open port');
  assert.equal(await page.isDisabled(LEARN), true);
  await midi({ ports: { ports: [{ id: 'pedal', name: 'FS-6 Pedal', state: 'open' }, { id: 'keys', name: 'Keystation 49', state: 'busy' }] } });
  await settles(() => page.isDisabled(LEARN), false, 'an open port can learn');
  assert.equal(await learnTitle(), null);

  await page.selectOption(PICK, 'clear');
  await page.selectOption(TRACK, '1');
  await page.click(LEARN);
  assert.equal(await listening(), 'true', 'LEARN listens at once');
  await settles(lastCall, ['learn', 'clear', 1], 'a lane action learns onto its named track');
  assert.ok(await page.getByText('Tap a pedal or key on a MIDI device. Esc cancels.').isVisible());
  await page.waitForTimeout(30); // the fake's `learning` answer
  // Esc: LISTENING ends in the same task, before native MIDI answers.
  const escaped = await page.evaluate((s) => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    return document.querySelector(s)?.getAttribute('aria-pressed');
  }, LEARN);
  assert.equal(escaped, 'false', 'Esc ends LISTENING at once');
  await settles(lastCall, ['cancelLearn'], 'Esc cancels natively');
  await page.waitForTimeout(30); // a settle: the fake's `learning` answer must leave LISTENING off
  assert.equal(await listening(), 'false', 'Esc spends itself on the learn: the panel stays open');

  await page.click(LEARN);
  await page.click(LEARN);
  await settles(async () => (await calls()).slice(-2), [['learn', 'clear', 1], ['cancelLearn']], 'a second LEARN click cancels');
  assert.equal(await listening(), 'false');

  // A learn still on its way: the cancel waits for it, so native MIDI never listens unseen.
  await page.evaluate(() => void (window.__lf.native.midiHold = new Promise((r) => (window.__releaseMidi = r))));
  const before = (await calls()).length;
  await page.click(LEARN);
  assert.equal(await listening(), 'true');
  const held = await page.evaluate((s) => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    return document.querySelector(s)?.getAttribute('aria-pressed');
  }, LEARN);
  assert.equal(held, 'false', 'Esc ends LISTENING at once, the learn still on its way');
  await page.waitForTimeout(50);
  assert.deepEqual((await calls()).slice(before), [['learn', 'clear', 1]], 'the cancel waits for the learn to settle');
  await page.evaluate(() => {
    window.__lf.native.midiHold = null;
    window.__releaseMidi();
  });
  await page.waitForFunction((n) => window.__lf.native.midiCalls.length >= n + 2, before);
  assert.deepEqual((await calls()).slice(before), [['learn', 'clear', 1], ['cancelLearn']], 'and then reaches native MIDI after it');
  await page.waitForTimeout(30); // a settle: nothing marks a change that must not come
  assert.equal(await listening(), 'false', 'the late learning event changes nothing the cancel ended');

  // A learn call that never settles: the UI gives it up in time, says so, and the next call goes.
  await page.evaluate(() => void (window.__lf.native.midiHold = new Promise(() => {})));
  const stuck = (await calls()).length;
  await page.click(LEARN);
  await page.keyboard.press('Escape');
  await page.waitForTimeout(500);
  assert.deepEqual((await calls()).slice(stuck), [['learn', 'clear', 1]], 'the cancel waits for the learn');
  await page.waitForFunction(() => window.__lf.notify.toasts().some((t) => t.message === 'Could not start the MIDI learn. Press LEARN again.'), undefined, { timeout: 5000 });
  await page.waitForFunction((n) => window.__lf.native.midiCalls.length >= n + 2, stuck, { timeout: 5000 });
  assert.deepEqual((await calls()).slice(stuck), [['learn', 'clear', 1], ['cancelLearn']], 'given up after its wait, the learn lets the cancel go');
  assert.equal(consumeConsoleErrors('[midi] learn failed').length, 1, 'with its release-log line');
  // The cancel waits on the same hold, and is given up in its turn.
  await page.waitForFunction(() => window.__lf.notify.toasts().some((t) => t.message === 'Could not stop the MIDI learn. Press LEARN, then stop it again.'), undefined, { timeout: 5000 });
  await page.evaluate(() => {
    window.__lf.native.midiHold = null;
    window.__lf.notify.toasts().forEach((t) => window.__lf.notify.dismissToast(t.id));
  });
  await page.waitForFunction(() => window.__lf.notify.toasts().length === 0);
  assert.equal(consumeConsoleErrors('[midi] cancel learn failed').length, 1, 'with its release-log line');

  // A global action takes no track, whatever the track select held.
  await page.selectOption(PICK, 'tapTempo');
  await page.click(LEARN);
  await settles(lastCall, ['learn', 'tapTempo', null], 'a global action learns with no track');
  await page.evaluate(() => window.__lf.ui.closeSettings());
  await page.waitForSelector(LEARN, { state: 'detached' });
  await settles(lastCall, ['cancelLearn'], 'closing the panel cancels a learn');

  // Native MIDI captured: LISTENING ends on its event, the hint waits for the release.
  await openSettings();
  await page.selectOption(PICK, 'recDub');
  await page.click(LEARN);
  await page.waitForTimeout(30); // the fake's `learning` answer, before native MIDI's capture
  const learned = binding({ number: 80 });
  await midi({ learning: { learning: null } }, { learned: { binding: learned } }, { awaitingRelease: { binding: learned } });
  await settles(listening, 'false', 'a capture ends LISTENING');
  const hint = page.getByText('Learned CC 80 · ch 1. Let go of the pedal, and try it once this line is gone.');
  await settles(() => hint.isVisible(), true, 'the learned message waits for its release');
  await midi({ awaitingRelease: { binding: null } });
  await settles(() => hint.count(), 0, 'the hint goes when native MIDI ends the wait');

  // ── The bindings list ───────────────────────────────────────────────────────────────────────────────
  await midi({ bindings: { revision: 7, bindings: [
    listed(binding({ hold: true })),
    listed(binding({ portId: 'input-3', portName: 'Keystation 49', channel: 9, kind: 'note', number: 36, action: 'playStop', target: 2, momentary: false }),
      { origin: 'legacy', ordinal: true, state: 'blocked', blocked: 'learned again in another run' }),
    listed(binding({ portId: 'gone', portName: 'Launchkey', number: 21, action: 'clickToggle', momentary: false }), { state: 'noPort' }),
    listed(binding({ portId: 'input-0', portName: 'Twin', number: 22, action: 'undo' }), { origin: 'legacy', ordinal: true, state: 'severalPorts' }),
    listed(binding({ portId: 'input-1', portName: 'Twin', number: 23, action: 'mute', target: 4, momentary: false }), { origin: 'legacy', ordinal: true, state: 'severalAbsent' }),
    listed(binding({ portId: 'keys', portName: 'Keystation 49', number: 24, action: 'stopAll', momentary: false }), { origin: 'legacy' }),
  ] } });
  const readLines = () => page.evaluate(() =>
    [...document.querySelectorAll('.audio-settings__binding')].map((li) => ({
      text: ['action', 'src'].map((part) => li.querySelector(`.audio-settings__binding-${part}`).textContent)
        .concat(li.querySelector('.audio-settings__chip').textContent.trim()).join(' | '),
      origin: li.querySelector('.audio-settings__binding-origin')?.textContent ?? null,
      why: li.querySelector('.audio-settings__binding-idle')?.textContent ?? null,
      idle: li.classList.contains('is-idle'),
      assign: li.querySelector('[aria-label^="Assign"]') !== null,
    })),
  );
  const PREVIOUS = 'from the previous version';
  const want = [
    { text: 'Record / overdub | CC 64 · ch 1 | MOMENTARY', origin: null, why: null, idle: false, assign: false },
    { text: 'Play / stop · Track 3 | note 36 · ch 10 | LATCHING', origin: PREVIOUS, why: 'learned again in another run', idle: true, assign: true },
    { text: 'Click on / off | CC 21 · ch 1 | LATCHING', origin: null, why: 'Launchkey is not connected', idle: true, assign: true },
    { text: 'Undo / redo | CC 22 · ch 1 | MOMENTARY', origin: PREVIOUS, why: 'several connected ports are named Twin', idle: true, assign: true },
    { text: 'Mute / unmute · Track 5 | CC 23 · ch 1 | LATCHING', origin: PREVIOUS, why: 'another missing port is named Twin', idle: true, assign: true },
    { text: 'Stop all | CC 24 · ch 1 | LATCHING', origin: PREVIOUS, why: 'Keystation 49 is held by another program', idle: true, assign: false },
  ];
  const lines = await until(readLines, want);
  console.log('bindings', JSON.stringify(lines));
  assert.deepEqual(lines, want, 'each line shows its binding and where it came from; one that does not run says why; only one no port is found for alone offers an assignment, one no present port answers to included');
  // The list is a table: every kind switch on one column (both edges), HOLD beside it, each line one
  // height, and a second line's port select and ASSIGN ending on the switches' right edge.
  const table = await page.evaluate(() => {
    const edge = (el, side) => Math.round(el.getBoundingClientRect()[side] * 2) / 2;
    const lines = [...document.querySelectorAll('.audio-settings__binding')];
    const all = (sel, side) => [...new Set(lines.flatMap((li) => [...li.querySelectorAll(sel)].map((el) => edge(el, side))))];
    return {
      kindLeft: [...new Set(lines.map((li) => edge(li.querySelector('.audio-settings__chip'), 'left')))],
      kindRight: [...new Set(lines.map((li) => edge(li.querySelector('.audio-settings__chip'), 'right')))],
      holdRight: all('[aria-label^="Hold to record"]', 'right'),
      assignRight: all('[aria-label^="Assign"]', 'right'),
      clearRight: all('[aria-label^="Forget"]', 'right'),
      rows: [...new Set(lines.flatMap((li) => getComputedStyle(li).gridTemplateRows.split(' ')))],
      heights: [...new Set(lines.map((li) => edge(li, 'height')))],
    };
  });
  console.log('bindings table', JSON.stringify(table));
  assert.equal(table.kindLeft.length, 1, 'every kind switch starts on one edge');
  assert.equal(table.kindRight.length, 1, 'and ends on one edge');
  assert.equal(table.clearRight.length, 1, 'every ✕ ends on one edge');
  assert.deepEqual(table.assignRight, table.holdRight, 'ASSIGN ends where HOLD does');
  assert.deepEqual(table.rows, ['24px'], 'every line one height');
  assert.equal(table.heights.length, 2, 'a binding is one line or two');
  const row = (i) => page.locator('.audio-settings__binding').nth(i);
  await row(0).getByRole('button', { name: /momentary pedal, switch to latching/ }).click();
  await settles(lastCall, ['setMomentary', 7, 0, false], 'the kind switch reads the pedal the other way, on the list it was made against');
  await row(0).getByRole('button', { name: 'Hold to record on CC 64 · ch 1' }).click();
  await settles(lastCall, ['setHold', 7, 0, false], 'HOLD switches off');
  assert.equal(await row(1).locator('[aria-label^="Hold to record"]').count(), 0, 'only REC/DUB can HOLD');

  // ASSIGN: no port until the player picks one.
  const assignButton = (i) => row(i).getByRole('button', { name: /^Assign/ });
  assert.equal(await row(1).locator('select').inputValue(), '', 'the port select starts on no port');
  assert.equal((await row(1).locator('select option').first().textContent()).trim(), 'Pick a port');
  assert.equal(await assignButton(1).isDisabled(), true, 'ASSIGN waits for a pick');
  assert.equal(await assignButton(3).isDisabled(), true);
  await row(1).locator('select').selectOption('keys');
  assert.equal(await assignButton(1).isDisabled(), false);
  await assignButton(1).click();
  await settles(lastCall, ['assign', 7, 1, 'keys'], 'ASSIGN moves the binding to the picked port');
  await row(2).locator('select').selectOption('keys');
  await assignButton(2).click();
  await settles(lastCall, ['assign', 7, 2, 'keys'], 'a binding no present port answers to moves the same way');
  await mkdir('logs/midi-learn', { recursive: true });
  await page.locator('.audio-settings').screenshot({ path: 'logs/midi-learn/bindings.png' });
  await row(4).getByRole('button', { name: /^Forget/ }).click();
  await settles(lastCall, ['forget', 7, 4], '✕ forgets by index');

  // An edit native MIDI refuses (the list changed since it was made) says nothing: the new list shows.
  await page.evaluate(() => void (window.__lf.native.editAnswer = false));
  await row(2).getByRole('button', { name: /^Forget/ }).click();
  await settles(lastCall, ['forget', 7, 2]);
  await page.waitForTimeout(30); // a settle: the refusal's answer must toast and log nothing
  assert.deepEqual(await toastsOf(page), [], 'a refused edit toasts nothing (and logs nothing: the harness fails on a log)');
  await page.evaluate(() => void (window.__lf.native.editAnswer = true));
  await midi({ bindings: { revision: 8, bindings: [listed(binding({ hold: true }))] } });
  await settles(() => page.locator('.audio-settings__binding').count(), 1, 'the new list shows');
  await row(0).getByRole('button', { name: /^Forget/ }).click();
  await settles(lastCall, ['forget', 8, 0], 'the next edit names the new list');
  await page.evaluate(() => window.__lf.ui.closeSettings());

  // ── What a binding fires that the UI runs ───────────────────────────────────────────────────────────
  const stage = () => page.evaluate(() => import('/src/ui/stage/stage-store.ts').then((m) => ({ open: m.stageOpen(), look: m.currentStageView().id })));
  let from = await mark();
  await midi({ run: { action: 'stageView' } });
  const opened = await until(stage, (s) => s.open);
  assert.equal(opened.open, true, 'run stageView opens the stage view');
  await midi({ run: { action: 'stageNextView' } });
  assert.notEqual((await until(stage, (s) => s.look !== opened.look)).look, opened.look, 'run stageNextView steps the look');
  await midi({ run: { action: 'stageView' } });
  assert.equal((await until(stage, (s) => !s.open)).open, false, 'and closes it');
  await midi('pressed', { run: { action: 'tapTempo' } });
  await page.waitForTimeout(500);
  await midi('pressed', { run: { action: 'tapTempo' } });
  await midi('pressed', { run: { action: 'goLive' } });
  const tapped = await until(() => sentSince(from), (sent) => sent.some((c) => c.SetBpm !== undefined));
  const sentBpm = tapped.find((c) => c.SetBpm !== undefined)?.SetBpm;
  // 500 ms apart is 120 BPM; the slack covers the page's and the runner's clocks.
  assert.ok(sentBpm >= 40 && sentBpm <= 125, `two TAPs send the tapped tempo (about 120 BPM or slower), sent ${sentBpm}`);
  await page.waitForTimeout(30); // a settle: no condition marks a Press that was never sent
  const ran = await sentSince(from);
  console.log('runs sent', JSON.stringify(ran));
  assert.ok(!ran.includes('Press'), 'no run sends a second Press (native MIDI sent it)');

  // `pressed` takes the lane cue down; `refused` puts its own up.
  const cue = () => page.evaluate(() => import('/src/ui/looper/gates.ts').then((m) => m.laneCue()));
  await page.evaluate(() => import('/src/ui/looper/gates.ts').then((m) => m.refuseOnLane(2, 'probe cue')));
  assert.equal((await cue())?.text, 'probe cue');
  await midi('pressed');
  await settles(cue, null, 'a native press takes the lane cue down');
  await midi({ refused: { reason: 'holdControlsTaken' } });
  await settles(cue, { track: 0, text: 'every HOLD control is down, let a HOLD pedal go first' }, 'a refused HOLD press says why on the selected lane');

  // ── Toasts ──────────────────────────────────────────────────────────────────────────────────────────
  await midi({ gone: { names: ['FS-6 Pedal'] } }, { store: { problem: { failed: { why: 'Access is denied. (os error 5)' } } } });
  const toasts = await until(() => toastsOf(page), (t) => t.length >= 2);
  console.log('toasts', JSON.stringify(toasts));
  assert.ok(toasts.some(([m, d]) => m === 'Could not reach FS-6 Pedal. Reconnect it to play on.' && d === 'The MIDI device disconnected, and its held notes were released.'), 'an unplugged port toasts');
  assert.ok(toasts.some(([m, d]) => m === 'Could not save the MIDI bindings. BleepLoop keeps trying.' && d === 'Access is denied. (os error 5)'), 'a store problem toasts');
  const epochs = await page.evaluate(() => [...new Set(window.__lf.native.midiCalls.filter((c) => c[0] !== 'subscribe').map((c) => c[1]))]);
  assert.deepEqual(epochs, [1], 'every native MIDI call carries the page\'s epoch');
  assert.deepEqual(consumeConsoleErrors(/^\[midi\]/), [
    '[midi] input disconnected: FS-6 Pedal',
    '[midi] bindings store: Could not save the MIDI bindings. BleepLoop keeps trying. Access is denied. (os error 5)',
  ], 'each reaches the release log (and nothing else does: the harness fails on any other)');
});
