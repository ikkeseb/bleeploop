/**
 * MIDI learn's UI over native MIDI: the real Audio Settings learn row and bindings list, `src/ui/state/midi.ts`
 * and the bridge `src/app/midi-actions.ts`, on the web engine fake (`src/platform/host.web.ts`, the
 * engine-seam pattern). What the UI asks native MIDI is read from `__lf.native.midiCalls`; what native MIDI
 * tells it is scripted with `__lf.native.midiEmit` (its serde JSON); what the UI sends the engine from
 * `__lf.native.sent`. The fake answers `learn` and `cancelLearn` with a `learning` event and nothing else.
 *
 * - boot: the UI subscribes, and hands the web build's `lf.midiLearn` to `importLegacy` verbatim (the key
 *   stays);
 * - LEARN waits for an open port; a pick (a lane action on a named track, a global one without a track)
 *   and LEARN call `learn(action, target)` and show LISTENING at once; Esc ends LISTENING in the same task
 *   (before native MIDI answers), calls `cancelLearn` and leaves the panel open; a second LEARN click and
 *   closing the panel cancel too; a `learning: null` event ends LISTENING, an `awaitingRelease` event
 *   shows the learned message's hint until its null;
 * - the list renders native MIDI's `bindings`: action, message, pedal kind, HOLD on REC/DUB; each line's
 *   kind switch, HOLD and ✕ call `setMomentary`, `setHold` and `forget` by its index; a line that does not
 *   run says why (blocked with native MIDI's reason, not connected, several ports or another missing
 *   port of its name) and assigns it to a port picked here (`assign(index, portId)`);
 * - a binding's `run` runs the UI's action with no second `Press` (native MIDI sent it): the stage view
 *   opens, steps its look and closes; two TAPs send the tapped `SetBpm`; GO LIVE sends no `Press`;
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
import { probe } from '../harness/probe.ts';

const LEARN = '[aria-label="Learn a MIDI control for this action"]';
const PICK = '[aria-label="Action to learn"]';
const TRACK = '[aria-label="Track the action acts on"]';
const LEGACY = JSON.stringify([{ port: 'input-1', portName: 'FS-6 Pedal', channel: 0, kind: 'cc', number: 64, action: 'recDub', pressHigh: true, momentary: true }]);

const binding = (extra) => ({ portId: 'pedal', portName: 'FS-6 Pedal', channel: 0, kind: 'cc', number: 64, action: 'recDub', target: null, pressHigh: true, momentary: true, hold: false, ...extra });
const listed = (b, extra = {}) => ({ binding: b, origin: 'native', ordinal: false, blocked: null, displayName: b.portName, state: 'live', ...extra });

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
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
    events: [...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info } })),
      { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
    meter: { peak: 0, clip: false },
  }), lane);

  const calls = () => page.evaluate(() => window.__lf.native.midiCalls.slice());
  const lastCall = async () => (await calls()).at(-1);
  /** Hand native MIDI's events to the UI and let them land. */
  const midi = async (...events) => {
    for (const e of events) await page.evaluate((ev) => window.__lf.native.midiEmit(ev), e);
    await page.waitForTimeout(30);
  };
  const listening = () => page.evaluate((s) => document.querySelector(s)?.getAttribute('aria-pressed') ?? null, LEARN);
  const openSettings = async () => {
    await page.evaluate(() => window.__lf.ui.openSettings());
    await page.waitForSelector(LEARN);
  };
  const sentSince = async (from) => {
    await page.waitForTimeout(30);
    return page.evaluate((m) => window.__lf.native.sent.slice(m), from);
  };
  const mark = () => page.evaluate(() => window.__lf.native.sent.length);

  // ── Boot: the subscription and the legacy hand-over ─────────────────────────────────────────────────
  await page.waitForFunction(() => window.__lf.native.midiCalls.some((c) => c[0] === 'importLegacy'));
  const boot = await calls();
  console.log('boot calls', JSON.stringify(boot));
  assert.ok(boot.some((c) => c[0] === 'subscribe'), 'the UI subscribes to native MIDI');
  assert.deepEqual(boot.filter((c) => c[0] === 'importLegacy'), [['importLegacy', LEGACY]], 'the web build\'s bindings go to native MIDI verbatim, once');
  assert.equal(await page.evaluate(() => localStorage.getItem('lf.midiLearn')), LEGACY, 'the legacy key stays');

  // ── The learn row ───────────────────────────────────────────────────────────────────────────────────
  await openSettings();
  assert.equal(await page.isDisabled(LEARN), true, 'LEARN waits for an open port');
  await midi({ ports: { ports: [{ id: 'pedal', name: 'FS-6 Pedal', state: 'open' }, { id: 'keys', name: 'Keystation 49', state: 'busy' }] } });
  assert.equal(await page.isDisabled(LEARN), false, 'an open port can learn');

  await page.selectOption(PICK, 'clear');
  await page.selectOption(TRACK, '1');
  await page.click(LEARN);
  assert.equal(await listening(), 'true', 'LEARN listens at once');
  assert.deepEqual(await lastCall(), ['learn', 'clear', 1], 'a lane action learns onto its named track');
  assert.ok(await page.getByText('Tap a pedal or key on a MIDI device. Esc cancels.').isVisible());
  await page.waitForTimeout(30); // the fake's `learning` answer
  // Esc: LISTENING ends in the same task, before native MIDI answers.
  const escaped = await page.evaluate((s) => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
    return document.querySelector(s)?.getAttribute('aria-pressed');
  }, LEARN);
  assert.equal(escaped, 'false', 'Esc ends LISTENING at once');
  assert.deepEqual(await lastCall(), ['cancelLearn'], 'Esc cancels natively');
  await page.waitForTimeout(30);
  assert.equal(await listening(), 'false', 'Esc spends itself on the learn: the panel stays open');

  await page.click(LEARN);
  await page.click(LEARN);
  assert.deepEqual((await calls()).slice(-2), [['learn', 'clear', 1], ['cancelLearn']], 'a second LEARN click cancels');
  assert.equal(await listening(), 'false');

  // A global action takes no track, whatever the track select held.
  await page.selectOption(PICK, 'tapTempo');
  await page.click(LEARN);
  assert.deepEqual(await lastCall(), ['learn', 'tapTempo', null], 'a global action learns with no track');
  await page.evaluate(() => window.__lf.ui.closeSettings());
  await page.waitForSelector(LEARN, { state: 'detached' });
  assert.deepEqual(await lastCall(), ['cancelLearn'], 'closing the panel cancels a learn');

  // Native MIDI captured: LISTENING ends on its event, the hint waits for the release.
  await openSettings();
  await page.selectOption(PICK, 'recDub');
  await page.click(LEARN);
  await page.waitForTimeout(30);
  const learned = binding({ number: 80 });
  await midi({ learning: { learning: null } }, { learned: { binding: learned } }, { awaitingRelease: { binding: learned } });
  assert.equal(await listening(), 'false', 'a capture ends LISTENING');
  const hint = page.getByText('Learned CC 80 · ch 1. Let go of the pedal, and try it once this line is gone.');
  assert.ok(await hint.isVisible(), 'the learned message waits for its release');
  await midi({ awaitingRelease: { binding: null } });
  assert.equal(await hint.count(), 0, 'the hint goes when native MIDI ends the wait');

  // ── The bindings list ───────────────────────────────────────────────────────────────────────────────
  await midi({ bindings: { bindings: [
    listed(binding({ hold: true })),
    listed(binding({ portId: 'input-3', portName: 'Keystation 49', channel: 9, kind: 'note', number: 36, action: 'playStop', target: 2, momentary: false }),
      { origin: 'legacy', ordinal: true, state: 'blocked', blocked: 'learned again in another run' }),
    listed(binding({ portId: 'gone', portName: 'Launchkey', number: 21, action: 'clickToggle', momentary: false }), { state: 'noPort' }),
    listed(binding({ portId: 'input-0', portName: 'Twin', number: 22, action: 'undo' }), { origin: 'legacy', ordinal: true, state: 'severalPorts' }),
    listed(binding({ portId: 'input-1', portName: 'Twin', number: 23, action: 'mute', target: 4, momentary: false }), { origin: 'legacy', ordinal: true, state: 'severalAbsent' }),
  ] } });
  const lines = await page.evaluate(() =>
    [...document.querySelectorAll('.audio-settings__binding')].map((li) => ({
      text: [...li.children].slice(0, 3).map((el) => el.textContent.trim()).join(' | '),
      why: li.querySelector('.audio-settings__binding-idle')?.textContent ?? null,
      assign: li.querySelector('[aria-label^="Assign"]') !== null,
    })),
  );
  console.log('bindings', JSON.stringify(lines));
  assert.deepEqual(lines, [
    { text: 'Record / overdub | CC 64 · ch 1 | momentary', why: null, assign: false },
    { text: 'Play / stop · Track 3 | note 36 · ch 10 | latching', why: 'learned again in another run', assign: true },
    { text: 'Click on / off | CC 21 · ch 1 | latching', why: 'Launchkey is not connected', assign: true },
    { text: 'Undo / redo | CC 22 · ch 1 | momentary', why: 'several connected ports are named Twin', assign: true },
    { text: 'Mute / unmute · Track 5 | CC 23 · ch 1 | latching', why: 'another missing port is named Twin', assign: true },
  ], 'each line shows its binding, and a line that does not run says why and offers an assignment');
  const row = (i) => page.locator('.audio-settings__binding').nth(i);
  await row(0).getByRole('button', { name: /momentary pedal, switch to latching/ }).click();
  assert.deepEqual(await lastCall(), ['setMomentary', 0, false], 'the kind switch reads the pedal the other way');
  await row(0).getByRole('button', { name: 'Hold to record on CC 64 · ch 1' }).click();
  assert.deepEqual(await lastCall(), ['setHold', 0, false], 'HOLD switches off');
  assert.equal(await row(1).locator('[aria-label^="Hold to record"]').count(), 0, 'only REC/DUB can HOLD');
  await row(2).locator('select').selectOption('keys');
  await row(2).getByRole('button', { name: /^Assign/ }).click();
  assert.deepEqual(await lastCall(), ['assign', 2, 'keys'], 'ASSIGN moves the binding to the picked port');
  await row(3).getByRole('button', { name: /^Assign/ }).click();
  assert.deepEqual(await lastCall(), ['assign', 3, 'pedal'], 'the first port is the default pick');
  await mkdir('logs/midi-learn', { recursive: true });
  await page.locator('.audio-settings').screenshot({ path: 'logs/midi-learn/bindings.png' });
  await row(4).getByRole('button', { name: /^Forget/ }).click();
  assert.deepEqual(await lastCall(), ['forget', 4], '✕ forgets by index');
  await page.evaluate(() => window.__lf.ui.closeSettings());

  // ── What a binding fires that the UI runs ───────────────────────────────────────────────────────────
  const stage = () => page.evaluate(() => import('/src/ui/stage/stage-store.ts').then((m) => ({ open: m.stageOpen(), look: m.currentStageView().id })));
  let from = await mark();
  await midi({ run: { action: 'stageView' } });
  const opened = await stage();
  assert.equal(opened.open, true, 'run stageView opens the stage view');
  await midi({ run: { action: 'stageNextView' } });
  assert.notEqual((await stage()).look, opened.look, 'run stageNextView steps the look');
  await midi({ run: { action: 'stageView' } });
  assert.equal((await stage()).open, false, 'and closes it');
  await midi('pressed', { run: { action: 'tapTempo' } });
  await page.waitForTimeout(500);
  await midi('pressed', { run: { action: 'tapTempo' } });
  await midi('pressed', { run: { action: 'goLive' } });
  const ran = await sentSince(from);
  console.log('runs sent', JSON.stringify(ran));
  assert.ok(ran.some((c) => c.SetBpm !== undefined), 'two TAPs send the tapped tempo');
  assert.ok(!ran.includes('Press'), 'no run sends a second Press (native MIDI sent it)');

  // `pressed` takes the lane cue down; `refused` puts its own up.
  const cue = () => page.evaluate(() => import('/src/ui/looper/gates.ts').then((m) => m.laneCue()));
  await page.evaluate(() => import('/src/ui/looper/gates.ts').then((m) => m.refuseOnLane(2, 'probe cue')));
  assert.equal((await cue())?.text, 'probe cue');
  await midi('pressed');
  assert.equal(await cue(), null, 'a native press takes the lane cue down');
  await midi({ refused: { reason: 'holdControlsTaken' } });
  assert.deepEqual(await cue(), { track: 0, text: 'every HOLD control is down, let a HOLD pedal go first' }, 'a refused HOLD press says why on the selected lane');

  // ── Toasts ──────────────────────────────────────────────────────────────────────────────────────────
  await midi({ gone: { names: ['FS-6 Pedal'] } }, { store: { problem: { failed: { why: 'Access is denied. (os error 5)' } } } });
  const toasts = await page.evaluate(() => window.__lf.notify.toasts().map((t) => [t.message, t.detail]));
  console.log('toasts', JSON.stringify(toasts));
  assert.ok(toasts.some(([m, d]) => m === 'MIDI device disconnected — FS-6 Pedal' && d === 'Held notes were released.'), 'an unplugged port toasts');
  assert.ok(toasts.some(([m, d]) => m === 'MIDI bindings could not be saved; the next change tries again' && d === 'Access is denied. (os error 5)'), 'a store problem toasts');
  assert.deepEqual(consoleErrors, [
    '[midi] input disconnected: FS-6 Pedal',
    '[midi] bindings store: MIDI bindings could not be saved; the next change tries again: Access is denied. (os error 5)',
  ], 'each reaches the release log, and nothing else does');
});
