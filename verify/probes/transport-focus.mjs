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
 *   the tempo (`SetBpm`), never a looper action.
 *
 * Cannot see the native engine (what it does with the press: lf-engine `tests/actions.rs`), MIDI or
 * hardware-key input: the fake answers no command by itself.
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
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
