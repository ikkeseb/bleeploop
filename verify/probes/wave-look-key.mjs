/**
 * The wave's look switch (`src/ui/looper/wave-looks.ts`, bound in `src/app/transport-keys.ts`). Engine
 * mode on the web engine fake, one STOPPED one-bar take on lane 1 (no playhead and no ruler, so a drawn
 * frame is the cached bitmap alone and two draws of one look match to the pixel).
 *
 * Proves: I steps the look every lane draws (stripes, then dense, light bars, plain) and the lane's pixels
 * change with it; each press shows one note toast naming the look, a run of presses still leaves one (the
 * newest), and it goes after about 1.5 s (still there at 1 s, gone by 3 s, where the store's linger is
 * 8 s); the pick survives a reload, drawn the same; and I typed into an editable field (the BPM field,
 * a field in Audio Settings, any text input) stays the field's: the look holds and no toast shows.
 *
 * Cannot see WebView2 or the eye: which look reads best is the owner's call.
 * Run: pnpm probe wave-look-key
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const PEAK_FRAMES = 1024;

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
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
const transport = (master, locked, bpm = 120) => ({ Transport: { frame: 0, master, bpm, locked } });

await probe(async ({ open }) => {
  const { page } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) => p.addInitScript(() => (window.__lfEngineFake = true)),
  });

  /** Boot the fake's feed: a one-bar loop, lane 1 STOPPED with a take that swells and falls. */
  const boot = async () => {
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    const count = Math.ceil(BAR / PEAK_FRAMES);
    const max = Array.from({ length: count }, (_, b) => 0.08 + 0.8 * Math.abs(Math.sin((b / count) * Math.PI * 3)));
    await page.evaluate(
      (f) => window.__lf.native.emit(f),
      {
        seq: 1,
        reset: true,
        settings: [],
        events: [
          ...[1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))),
          laneEvent(0, lane('Stopped', { length: BAR, canReverse: true })),
          transport(BAR, true),
          { Selected: { frame: 0, lane: 0 } },
        ],
        anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
        meter: { peak: [0, 0], clip: [false, false] },
        peaks: [{ lane: 0, start: 0, count, min: max.map((v) => -v), max }],
      },
    );
  };

  /** The look now, and lane 1's canvas as one FNV-1a hash of its pixels, two frames after the ask. */
  const read = () =>
    page.evaluate(async () => {
      const { waveLook } = await import('/src/ui/looper/wave-looks.ts');
      for (let i = 0; i < 2; i++) await new Promise((r) => requestAnimationFrame(r));
      const c = document.querySelector('.lp-lane canvas');
      const px = c.getContext('2d').getImageData(0, 0, c.width, c.height).data;
      let h = 0x811c9dc5;
      let ink = 0;
      for (let i = 0; i < px.length; i++) {
        h = Math.imul(h ^ px[i], 0x01000193);
        if ((i & 3) === 3 && px[i] > 0) ink++;
      }
      return { look: waveLook(), stored: localStorage.getItem('lf.waveLook'), hash: h >>> 0, ink };
    });
  const waveToasts = () =>
    page.evaluate(() => [...document.querySelectorAll('[data-toast-id]')].map((t) => t.textContent ?? '').filter((t) => t.includes('Wave:')));

  await boot();
  const start = await read();
  assert.equal(start.look, 'stripes', 'a returning player with no pick draws the stripes');
  assert.ok(start.ink > 1000, `lane 1 draws its take (${start.ink} px of ink)`);

  // ── I steps the look, says so, and the lane draws it ─────────────────────────────────────────────
  await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
  await page.keyboard.press('i');
  const dense = await read();
  assert.equal(dense.look, 'dense', 'I steps stripes → dense');
  assert.equal(dense.stored, 'dense', 'the pick is kept in localStorage');
  assert.notEqual(dense.hash, start.hash, 'the lane draws the new look');
  const first = await waveToasts();
  assert.equal(first.length, 1, `one wave toast (${first.join(' | ')})`);
  assert.ok(first[0].includes('Wave: Dense'), `the toast names the look (${first[0]})`);

  // A run of presses leaves one toast, naming the newest look.
  await page.keyboard.press('i');
  await page.keyboard.press('i');
  const pressed = Date.now();
  const plain = await read();
  assert.equal(plain.look, 'plain', 'two more presses: dense → light bars → plain');
  const run = await waveToasts();
  assert.equal(run.length, 1, `a run of presses leaves one wave toast (${run.join(' | ')})`);
  assert.ok(run[0].includes('Wave: Plain'), `it names the newest look (${run[0]})`);

  // About 1.5 s: there at 1 s, gone by 3 s (the store's own linger is 8 s).
  await page.waitForTimeout(Math.max(0, 1000 - (Date.now() - pressed)));
  assert.equal((await waveToasts()).length, 1, 'the wave toast is still there 1 s after the last press');
  await page.waitForFunction(() => ![...document.querySelectorAll('[data-toast-id]')].some((t) => t.textContent?.includes('Wave:')), undefined, {
    timeout: Math.max(1, 3000 - (Date.now() - pressed)),
  });
  console.log(`looks: stripes ${start.hash}, dense ${dense.hash}, plain ${plain.hash}; the toast went within 3 s`);

  // ── The pick survives a reload, drawn the same ──────────────────────────────────────────────────
  await page.reload();
  await page.waitForFunction(() => '__lf' in window, undefined, { timeout: 30_000 });
  await boot();
  const reloaded = await read();
  assert.equal(reloaded.look, 'plain', 'the look survives a reload');
  assert.equal(reloaded.hash, plain.hash, 'and the lane draws it as before the reload');

  // ── I typed into an editable field stays the field's ────────────────────────────────────────────
  /** Press I in the field `focus` focused (its name for the log); the look and the toasts hold. */
  const typeInto = async (name, focus) => {
    const found = await page.evaluate(focus);
    assert.ok(found, `${name}: the field is there to type into`);
    await page.keyboard.press('i');
    const after = await read();
    assert.equal(after.look, 'plain', `${name}: I typed there leaves the look`);
    assert.equal((await waveToasts()).length, 0, `${name}: and shows no wave toast`);
    console.log(`${name}: I stayed the field's`);
  };

  // The BPM field: the tempo opens its number box on a click while the tempo is free (a clear loop).
  await page.evaluate((f) => window.__lf.native.emit(f), {
    seq: 2,
    reset: false,
    events: [laneEvent(0, lane('Empty')), transport(0, false)],
  });
  const bpm = page.getByRole('button', { name: /BPM/ }).first();
  if (await bpm.isVisible().catch(() => false)) {
    await bpm.click();
    await typeInto('the BPM field', () => {
      const el = document.querySelector('input[aria-label="BPM"]');
      el?.focus();
      return document.activeElement === el && el !== null;
    });
    await page.keyboard.press('Escape');
  } else console.log('the BPM field: no tempo button in this build of the command bar; skipped');

  // Audio Settings' first field (a text input where the page has one, else a select).
  await page.evaluate(() => window.__lf.ui.openSettings('device'));
  await page.waitForSelector('.audio-settings', { timeout: 5000 });
  await typeInto('an Audio Settings field', () => {
    const el =
      document.querySelector('.audio-settings input:not([type]), .audio-settings input[type="text"], .audio-settings input[type="number"]') ??
      document.querySelector('.audio-settings select');
    el?.focus();
    return el !== null && document.activeElement === el;
  });
  await page.evaluate(() => window.__lf.ui.closeSettings());

  // Any text input on the page (the yield rule is the field's kind, not its place).
  await typeInto('a text input', () => {
    const el = document.createElement('input');
    el.type = 'text';
    el.id = 'probe-text';
    document.body.append(el);
    el.focus();
    return document.activeElement === el;
  });
  assert.equal(await page.evaluate(() => document.getElementById('probe-text')?.value), 'i', 'the text input took the letter');
});
