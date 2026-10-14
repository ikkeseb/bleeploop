/**
 * Transport keys (Space/Enter/1–5, `src/app/transport-keys.ts`) vs. focus, in engine mode on the web
 * engine fake (`src/platform/host.web.ts`, the `engine-seam` pattern: an init script sets
 * `window.__lfEngineFake`, the probe scripts the feed through `__lf.native` and reads what the UI sends).
 * The transport yields only to controls the user is operating, never to focus the app moved itself
 * (popover panel, trigger focus return after Escape). Drives the rendered Help popover and BPM field
 * through pointer and keyboard events and reads the commands the keys send:
 *
 * - Help opened by pointer and closed by Escape: Space after it sends the engine's REC/DUB action and
 *   does not re-open Help through the returned focus;
 * - Help open: Space still sends REC/DUB with Help left open; 2 selects lane 2 (`SelectTrack`), which the
 *   lane shows once the feed says so;
 * - real controls keep the key: Space in the BPM field sends nothing; Tab to BPM plus and Enter steps
 *   the tempo (`SetBpm`), never a looper action;
 * - Enter commits a typed BPM (`SetBpm`) with an AUTO REC arm on the feed, and drives no looper action
 *   though the field unmounts under the key;
 * - a held Enter on a lane's CLR and on ✕ ALL sends no clear (a key repeat is not the second press), and a
 *   held Tab still moves on past them; a fresh Enter after the release sends it once;
 * - Enter on a Tab-focused lane core sends `SelectTrack` before its `RecDub`, as a pointer press does;
 * - a select the pointer opened (the slot's input, the FX drawer's division) gives the keys back however
 *   its list closed (a new value, the same value, dismissed): Space and ArrowDown send REC/DUB and NEXT
 *   TRACK and leave the value alone, and a letter is no type-ahead in it; one reached by key keeps its
 *   focus.
 *
 * Cannot see the native engine (what it does with the press: lf-engine `tests/actions.rs`), MIDI or
 * hardware-key input: the fake answers no command by itself. The list a select opens is Chromium's: the
 * probe works it by key, as no pointer can reach it from here, and WebView2's is unmeasured.
 * Run: pnpm probe transport-focus [--shots=<dir>]
 */
import assert from 'node:assert/strict';
import { arg, probe } from '../harness/probe.ts';

const shots = arg('shots');
const RATE = 48000;

const lane = (state) => ({
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
});

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    viewport: { width: 1280, height: 820 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await page.evaluate((f) => window.__lf.native.emit(f), {
    seq: 1,
    reset: true,
    settings: [],
    events: [
      ...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty') } })),
      { Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
      { Selected: { frame: 0, lane: 0 } },
    ],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });

  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  const recDubs = async () => (await sent()).filter((c) => c.Action === 'RecDub').length;
  const helpBtn = page.getByRole('button', { name: 'Help', exact: true });
  const helpPanel = page.locator('#lf-help-popover');

  // 1. Pointer-open Help, Escape closes it, Space arms REC (not a popover re-open via the returned focus).
  await clearSent();
  await helpBtn.click();
  await helpPanel.waitFor();
  await page.keyboard.press('Escape');
  await helpPanel.waitFor({ state: 'detached' });
  await page.keyboard.press('Space');
  await page.waitForTimeout(150);
  assert.equal(await helpPanel.count(), 0, 'Space after Escape re-opened Help');
  assert.equal(await recDubs(), 1, 'Space after Escape did not send REC/DUB');

  // 2. Help open (pointer): Space arms lane 1 with Help still open; 2 selects lane 2.
  await clearSent();
  await helpBtn.click();
  await helpPanel.waitFor();
  await page.keyboard.press('Space');
  await page.waitForTimeout(150);
  assert.equal(await helpPanel.count(), 1, 'Help closed on Space');
  assert.equal(await recDubs(), 1, 'Space with Help open did not send REC/DUB');
  await page.keyboard.press('2');
  await page.waitForTimeout(100);
  assert.ok((await sent()).some((c) => c.SelectTrack === 1), '2 with Help open did not select lane 2');
  await page.evaluate(() => window.__lf.native.emit({ seq: 2, reset: false, events: [{ Selected: { frame: 0, lane: 1 } }] }));
  assert.equal(await page.locator('.lp-lane').nth(1).getAttribute('aria-current'), 'true', 'lane 2 shows the selection the feed reports');
  if (shots) await page.screenshot({ path: `${shots}/help-open-armed.png` });
  await page.keyboard.press('Escape');
  await helpPanel.waitFor({ state: 'detached' });

  // 3. Real controls keep the key: Space in the BPM field does not arm; Tab to BPM plus + Enter
  //    activates that button, not PLAY/STOP.
  await clearSent();
  await page.getByRole('button', { name: /^\d+ BPM$/ }).click();
  // The field takes focus one task after it mounts (Transport.tsx startEditFocused): wait for the
  // focus, not the element, or Space lands on <body> and arms lane 1 — a race no hand can win.
  await page.locator('.transport__bpm-input').waitFor();
  await page.waitForFunction(() => document.activeElement?.classList.contains('transport__bpm-input'));
  await page.keyboard.press('Space');
  await page.waitForTimeout(150);
  assert.equal(await recDubs(), 0, 'Space in the BPM input armed the looper');
  await page.keyboard.press('Tab');
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), 'BPM plus');
  const bpmBefore = await page.evaluate(() => window.__lf.clock.bpm());
  await page.keyboard.press('Enter');
  await page.waitForTimeout(150);
  const afterEnter = await sent();
  console.log('sent after BPM field + Tab + Enter', JSON.stringify(afterEnter));
  assert.ok(afterEnter.some((c) => c.SetBpm === bpmBefore + 1), 'Enter on BPM plus did not step BPM');
  assert.ok(!afterEnter.some((c) => c.Action !== undefined || c.ActionOn !== undefined), 'Enter on a Tab-focused button drove the looper');

  // 4. Enter commits a typed BPM and nothing else: the field unmounts under the key, and the window's
  //    transport keys must not read that Enter as PLAY/STOP (with AUTO REC armed it would end the arm).
  await page.evaluate((info) => window.__lf.native.emit({ seq: 3, reset: false, events: [{ Lane: { frame: 0, lane: 0, info } }] }),
    { ...lane('Empty'), armed: true, autoArmed: true });
  await clearSent();
  await page.getByRole('button', { name: /^\d+ BPM$/ }).click();
  await page.waitForFunction(() => document.activeElement?.classList.contains('transport__bpm-input'));
  await page.keyboard.type('97');
  await page.keyboard.press('Enter');
  await page.waitForTimeout(150);
  const afterCommit = await sent();
  console.log('sent after typing 97 + Enter in the BPM field', JSON.stringify(afterCommit));
  assert.ok(afterCommit.some((c) => c.SetBpm === 97), 'Enter in the BPM field did not set the typed BPM');
  assert.ok(!afterCommit.some((c) => c.Action !== undefined || c.ActionOn !== undefined), 'Enter in the BPM field also drove the looper');

  // 5. A held Enter on a CLEAR control confirms nothing: a key repeat is not a second press. A release
  //    and a fresh press is, and sends the clear once.
  const playing = { ...lane('Playing'), length: 96000, canReverse: true };
  const holdsLoop = () => page.evaluate((info) => window.__lf.native.emit({
    seq: 4, reset: false,
    events: [{ Lane: { frame: 0, lane: 0, info } }, { Transport: { frame: 0, master: 96000, bpm: 120, locked: true } }],
  }), playing);
  await holdsLoop();
  for (const [name, isClear] of [
    ['Track 1 clear', (c) => c.Clear !== undefined],
    ['Clear all tracks', (c) => c === 'ClearAll'],
  ]) {
    const control = page.getByRole('button', { name, exact: true });
    await control.focus();
    await clearSent();
    await page.keyboard.down('Enter');
    await page.keyboard.down('Enter'); // repeat: true
    await page.keyboard.down('Enter');
    await page.keyboard.up('Enter');
    await page.waitForTimeout(100);
    const held = await sent();
    console.log(`sent after a held Enter on ${name}`, JSON.stringify(held));
    assert.equal(held.filter(isClear).length, 0, `a held Enter on ${name} confirmed the clear`);
    await page.keyboard.press('Enter');
    await page.waitForTimeout(100);
    assert.equal((await sent()).filter(isClear).length, 1, `a second Enter on ${name} did not clear once`);
    // A held Tab that reaches it moves on past it (the guard is Enter's only): from the control before
    // it, the first Tab lands on it and the repeat must leave.
    await control.focus();
    await page.keyboard.press('Shift+Tab');
    await page.keyboard.down('Tab');
    const landed = await control.evaluate((el) => document.activeElement === el);
    await page.keyboard.down('Tab'); // repeat: true
    await page.keyboard.up('Tab');
    const left = await control.evaluate((el) => document.activeElement !== el);
    assert.ok(landed && left, `a held Tab ${landed ? 'stays on' : 'never reached'} ${name}`);
  }

  // 6. A keyboard-activated lane core selects its lane before its REC/DUB, as a pointer press does, so a
  //    later Space (the selected track's REC/DUB) ends that lane's take.
  await page.evaluate(() => window.__lf.native.emit({ seq: 5, reset: false, events: [{ Selected: { frame: 0, lane: 0 } }] }));
  await page.locator('.lp-lane').nth(1).locator('.lp-core').focus();
  await clearSent();
  await page.keyboard.press('Enter');
  await page.waitForTimeout(100);
  const coreSent = await sent();
  console.log('sent after Enter on the Track 2 core', JSON.stringify(coreSent));
  assert.deepEqual(coreSent, [{ SelectTrack: 1 }, { RecDub: 1 }], 'a keyboard-activated core did not select its lane, then REC/DUB');

  // 7. A select the pointer opened gives the keys back however its list closed: Space and ArrowDown reach
  //    the looper and leave the value alone. The slot's input select (slot 1 Off, on a four-input device),
  //    then the FX drawer's division select (Stutter's rate on track 1, which holds a loop since case 5).
  await page.evaluate(async () => {
    const { platform } = await import('/src/platform/index.ts');
    const devices = await import('/src/ui/state/audio-devices.ts');
    // The browser build lists no inputs (its plugin host is not there): list four, for this read only.
    const host = platform.pluginHost;
    host.listInputDevices = async () => [{ id: 'fake-in', name: 'Fake input', channels: 4 }];
    host.available = true;
    await devices.refreshInputDevices();
    host.available = false;
  });
  await page.getByRole('combobox', { name: 'Source for slot 1', exact: true }).selectOption('off');
  const inputPick = page.getByRole('combobox', { name: 'Input for slot 1', exact: true });
  const divisionPick = page.locator('.lp-drawer .fxp-param__select').first();
  const revealDivision = async () => {
    await page.getByRole('button', { name: 'Track 1 FX', exact: true }).click();
    await page.getByRole('button', { name: 'Stutter, track 1', exact: true }).click();
  };
  // The pointer opens the list (a click); the keys then go to the list, not the page: ArrowDown + Enter
  // picks the next value, Enter alone the value it had, Escape dismisses it.
  for (const [name, select, listKeys, reveal] of [
    ['a pointer pick of a new value in the slot input select', inputPick, ['ArrowDown', 'Enter'], async () => {}],
    ['a pointer pick of the same value in the slot input select', inputPick, ['Enter'], async () => {}],
    ['a dismissed slot input list', inputPick, ['Escape'], async () => {}],
    ['a pointer pick of a new value in the FX division select', divisionPick, ['ArrowDown', 'Enter'], revealDivision],
  ]) {
    await reveal();
    await select.waitFor();
    await page.evaluate(() => document.activeElement?.blur());
    const before = await select.inputValue();
    await select.click();
    await page.waitForTimeout(150);
    for (const key of listKeys) await page.keyboard.press(key);
    await page.waitForTimeout(100);
    const picked = await select.inputValue();
    await clearSent();
    await page.keyboard.press('Space');
    await page.keyboard.press('ArrowDown');
    await page.waitForTimeout(150);
    const afterPick = await sent();
    const focused = await page.evaluate(() => document.activeElement?.tagName);
    console.log(`after ${name}: ${before} -> ${picked}; Space + ArrowDown sent (focus on ${focused})`, JSON.stringify(afterPick));
    if (listKeys.length === 2) assert.notEqual(picked, before, `${name} picked nothing`);
    else assert.equal(picked, before, `${name} changed the value`);
    assert.ok(afterPick.some((c) => c.Action === 'RecDub'), `Space after ${name} did not send REC/DUB`);
    assert.ok(afterPick.some((c) => c.Action === 'NextTrack'), `ArrowDown after ${name} did not send NEXT TRACK`);
    assert.equal(await select.inputValue(), picked, `the keys after ${name} changed the select`);
  }
  // A letter after a dismissed list is no type-ahead in it ('a' would pick Auto over In 2).
  await inputPick.selectOption('1');
  await page.evaluate(() => document.activeElement?.blur());
  await inputPick.click();
  await page.waitForTimeout(150);
  await page.keyboard.press('Escape');
  const beforeLetter = await inputPick.inputValue();
  await page.keyboard.press('a');
  await page.waitForTimeout(100);
  console.log(`a letter after a dismissed list: ${beforeLetter} -> ${await inputPick.inputValue()}`);
  assert.equal(await inputPick.inputValue(), beforeLetter, 'a letter after a dismissed list picked in it');
  // A keyboard user keeps the select: an arrow there picks, and the focus stays for the next.
  await inputPick.focus();
  await page.keyboard.press('ArrowDown');
  await page.waitForTimeout(100);
  const kept = await inputPick.evaluate((el) => [document.activeElement === el, el.value]);
  console.log('focus and value after ArrowDown in the focused slot input select', JSON.stringify(kept));
  assert.ok(kept[0], 'a keyboard pick in the slot input select lost its focus');
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
