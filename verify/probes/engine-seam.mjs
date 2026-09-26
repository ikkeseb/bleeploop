/**
 * Engine mode's UI seam, on the web engine fake (`src/platform/host.web.ts`). An init script sets
 * `window.__lfEngineFake` before the app loads, so it boots in engine mode against the fake host; the
 * probe then drives the real UI and scripts the feed through `__lf.native`:
 *
 * - boot: the saved device opens once (WASAPI in the browser, the saved buffer); the first reset frame
 *   gets what the UI persists and owns (master and click level, the note target), not defaults the
 *   engine has; no AudioContext is ever built (Tone's import-time default context included:
 *   `src/main.tsx` loads the app with the constructors hidden);
 * - gesture → command: BPM +, CLICK, the lane core (its pointerdown selects), Space (the engine's
 *   hands-free `Action`), a lane volume, MIC (an empty slot goes live), a PC key (NoteOn, NoteOff);
 * - frame → DOM: a count-in (ARMED, the numeral, the beat LED, the BPM lock), a beat LED shown when the
 *   beat is heard (its frame and the output latency against the clock anchor), a live take, a committed
 *   loop (PLAYING, the loop readout, a moving ring dial from the clock anchor), the record meter, a
 *   refusal on its lane, the selection, a COPY carrying the lane's volume, a lane's mix kept when it only
 *   goes EMPTY and reset by `Cleared`, and a reload's reset frame whose remembered settings are adopted.
 *
 * Cannot see the native engine, the Rust mirror of the wire, Tauri IPC or any timing: the fake answers
 * no command by itself, so every state the DOM shows here was scripted.
 * Run: pnpm probe engine-seam
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM

const lane = (state, extra = {}) => ({
  state,
  length: 0,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info, frame = 0) => ({ Lane: { frame, lane: i, info } });
/** The clock anchor with `frame` rendering now: a beat at that frame is heard at once (heard lag 0). */
const anchorAt = (frame) => ({ frame, atMs: Date.now(), rate: RATE, grid: 0 });
const transport = (master, locked, bpm = 120) => ({ Transport: { frame: 0, master, bpm, locked } });

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    viewport: { width: 1600, height: 900 },
    init: (p) =>
      p.addInitScript(() => {
        window.__lfEngineFake = true;
        window.__audioContexts = [];
        const Native = window.AudioContext;
        window.AudioContext = class extends Native {
          constructor(...args) {
            super(...args);
            window.__audioContexts.push(new Error('AudioContext built').stack);
          }
        };
      }),
  });

  let seq = 0;
  const emit = (frame) =>
    page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
  /** Wait until the sent log holds `count` commands, then return them. */
  const sentAtLeast = async (count) => {
    await page.waitForFunction((n) => window.__lf.native.sent.length >= n, count, { timeout: 5000 });
    return sent();
  };
  // Hand the keys back to the window handlers (a focused field or slider keeps them).
  const blur = () => page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
  const lanes = page.locator('.lp-lane');
  const well = (i) => lanes.nth(i).locator('.lp-lane__wellmsg');

  // ── Boot ──────────────────────────────────────────────────────────────────────────────────────
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  const opened = await page.evaluate(() => window.__lf.native.opened);
  console.log('opened', JSON.stringify(opened));
  assert.deepEqual(opened, [{ backend: 'Wasapi', input: null, output: null, inputChannel: null, buffer: 256 }]);
  assert.deepEqual(await sent(), [], 'nothing is sent before the feed says what the engine has');

  // The reset frame a subscribe starts with, from a fresh engine: every lane EMPTY, no master, the device
  // and its clock, and no remembered settings. The UI sends what it persists and what it owns only.
  await emit({
    reset: true,
    settings: [],
    events: [...[0, 1, 2, 3, 4].map((i) => laneEvent(i, lane('Empty'))), transport(0, false), { Selected: { frame: 0, lane: 0 } }],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });
  const boot = await sentAtLeast(1);
  console.log('first reset sent', JSON.stringify(boot));
  for (const expected of [{ SetMasterVolume: 1 }, { SetClickVolume: 0.7 }, { SelectInstrument: { Builtin: 'lead' } }]) {
    assert.ok(boot.some((c) => JSON.stringify(c) === JSON.stringify(expected)), `the first reset sends ${JSON.stringify(expected)}`);
  }
  assert.ok(!boot.some((c) => c.SetVolume || c.SetMetronome !== undefined), 'defaults the engine already has are not pushed');

  // ── Gestures → commands ───────────────────────────────────────────────────────────────────────
  await clearSent();
  await page.getByRole('button', { name: 'BPM plus' }).click();
  assert.deepEqual(await sentAtLeast(1), [{ SetBpm: 121 }], 'BPM + sends SetBpm');
  // The engine echoes the tempo on the feed; nothing changes until it does.
  assert.equal(await page.locator('.transport__bpm-num').textContent(), '120');
  await emit({ events: [transport(0, false, 121)] });
  assert.equal(await page.locator('.transport__bpm-num').textContent(), '121');

  await clearSent();
  await page.getByRole('button', { name: 'Metronome click' }).click();
  assert.deepEqual(await sentAtLeast(1), [{ SetMetronome: true }], 'CLICK sends SetMetronome');

  await clearSent();
  await lanes.nth(0).locator('.lp-core').click();
  assert.deepEqual(await sentAtLeast(2), [{ SelectTrack: 0 }, { RecDub: 0 }], 'the lane core selects, then REC/DUB');

  // ── A count-in, then the take, on the feed ────────────────────────────────────────────────────
  await emit({
    events: [
      laneEvent(0, lane('Recording', { armed: true })),
      transport(0, true, 121),
      { Beat: { frame: 0, beatInBar: 0, countLeft: 4, clicked: true } },
    ],
    anchor: anchorAt(0),
  });
  assert.equal(await lanes.nth(0).getAttribute('data-state'), 'armed');
  assert.equal(await lanes.nth(0).locator('.lp-lane__count').textContent(), '4');
  assert.match(await well(0).textContent(), /COUNT-IN/);
  assert.ok(await page.locator('.transport__bpm-group--locked').isVisible(), 'the BPM group reads locked');
  assert.ok(await page.getByRole('button', { name: 'BPM plus' }).isDisabled(), 'BPM + is disabled while locked');
  assert.ok(await page.locator('.transport__beat').nth(0).evaluate((el) => el.classList.contains('on')), 'beat LED 1 lit');

  await emit({ events: [{ Beat: { frame: 12000, beatInBar: 1, countLeft: 3, clicked: true } }], anchor: anchorAt(12000) });
  assert.equal(await lanes.nth(0).locator('.lp-lane__count').textContent(), '3');
  assert.ok(await page.locator('.transport__beat').nth(1).evaluate((el) => el.classList.contains('on')), 'beat LED 2 lit');

  await emit({ events: [laneEvent(0, lane('Recording')), { Beat: { frame: BAR, beatInBar: 0, countLeft: 0, clicked: true } }], anchor: anchorAt(BAR) });
  assert.equal(await lanes.nth(0).getAttribute('data-state'), 'rec');
  assert.equal(await lanes.nth(0).locator('.lp-lane__state').textContent(), '● REC');

  // ── The committed loop: state, readout, a playhead from the clock anchor ──────────────────────
  const committed = lane('Playing', { length: BAR, canReverse: true });
  await emit({
    events: [laneEvent(0, committed), transport(BAR, true, 120)],
    anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
    peaks: [{ lane: 0, start: 0, count: 3, min: [-0.5, -0.2, -0.1], max: [0.5, 0.2, 0.1] }],
    meter: { peak: 0.5, clip: false },
  });
  assert.equal(await lanes.nth(0).getAttribute('data-state'), 'play');
  assert.equal(await lanes.nth(0).locator('.lp-lane__state').textContent(), 'PLAYING');
  const readout = (await page.locator('.transport__loop-v').textContent()).replace(/\s+/g, ' ').trim();
  console.log('loop readout', readout);
  assert.match(readout, /^1 BAR · 2\.0 s$/);
  const dial = page.locator('.transport__dial circle[stroke-dashoffset]');
  const offsetAt = async () => Number(await dial.getAttribute('stroke-dashoffset'));
  const first = await offsetAt();
  await page.waitForTimeout(300);
  const second = await offsetAt();
  console.log('ring dial', first, '→', second);
  assert.notEqual(first, second, 'the ring dial moves with the extrapolated clock');
  const lvl = await page.locator('.transport__inmeter').evaluate((el) => Number(el.style.getPropertyValue('--lvl')));
  console.log('meter --lvl', lvl);
  assert.ok(Math.abs(lvl - 0.9) < 0.02, 'a 0.5 peak reads −6 dBFS on the meter');

  // ── A refusal names its reason on the lane; the selection follows the feed ───────────────────
  await clearSent();
  await blur();
  await page.keyboard.press('Space');
  assert.deepEqual(await sentAtLeast(1), [{ Action: 'RecDub' }], 'Space sends the engine action');
  await emit({ events: [{ Refused: { frame: BAR, lane: 0, reason: 'PlayFirst' } }] });
  assert.match(await well(0).textContent(), /play first to overdub/);
  assert.ok(await lanes.nth(0).evaluate((el) => el.classList.contains('is-cued')), 'lane 1 carries the cue');
  await emit({ events: [{ Selected: { frame: BAR, lane: 2 } }] });
  assert.equal(await lanes.nth(2).getAttribute('aria-current'), 'true');

  // ── The mix this store keeps: a volume, and COPY carrying it ──────────────────────────────────
  await clearSent();
  await page.getByRole('slider', { name: 'Track 1 volume' }).fill('80');
  assert.deepEqual(await sentAtLeast(1), [{ SetVolume: [0, 0.8] }], 'the fader sends SetVolume');
  await emit({ events: [{ Copied: { frame: BAR, from: 0, to: 1 } }, laneEvent(1, committed)] });
  assert.equal(await page.getByRole('slider', { name: 'Track 2 volume' }).inputValue(), '80', 'COPY carries the volume');
  // A lane going EMPTY keeps its mix (an aborted take); only the engine's CLEAR resets it.
  await clearSent();
  await emit({ events: [laneEvent(1, lane('Empty'))] });
  assert.equal(await page.getByRole('slider', { name: 'Track 2 volume' }).inputValue(), '80', 'EMPTY alone keeps the mix');
  await emit({ events: [{ Cleared: { frame: BAR, lane: 1 } }] });
  assert.equal(await page.getByRole('slider', { name: 'Track 2 volume' }).inputValue(), '100', 'Cleared resets the mix');
  assert.deepEqual(await sent(), [], 'the engine reset its own mix: nothing is sent');

  // ── MIC and the play path ─────────────────────────────────────────────────────────────────────
  await clearSent();
  await page.getByRole('button', { name: 'Mic / line input' }).click();
  assert.deepEqual(await sentAtLeast(1), [{ SetSlotLive: [0, true] }], 'MIC takes the empty active slot live');
  assert.match(await page.getByRole('button', { name: 'Mic / line input' }).textContent(), /MIC LIVE/);

  await blur();
  await clearSent();
  await page.keyboard.down('a');
  const down = await sentAtLeast(1);
  await page.keyboard.up('a');
  const played = await sentAtLeast(down.length + 1);
  console.log('play path', JSON.stringify(played));
  const on = played.find((c) => c.NoteOn);
  assert.ok(on, 'a PC key sends NoteOn');
  assert.ok(on.NoteOn[1] > 0 && on.NoteOn[1] <= 1, 'velocity is 0..1');
  assert.deepEqual(played.at(-1), { NoteOff: on.NoteOn[0] }, 'its release sends NoteOff');

  // ── A WebView reload: the reset frame carries what the engine remembers, and the UI adopts it ─────
  await clearSent();
  await emit({
    reset: true,
    settings: [{ SetMasterVolume: 0.6 }, { SetMetronome: true }, { SetVolume: [0, 0.5] }, { SetFxBypass: [0, 'filter', false] }],
    events: [laneEvent(0, committed), transport(BAR, true, 120), { Selected: { frame: BAR, lane: 0 } }],
    anchor: { frame: BAR, atMs: Date.now(), rate: RATE, grid: 0 },
    meter: { peak: 0, clip: false },
  });
  const adopted = await sentAtLeast(1);
  console.log('second reset sent', JSON.stringify(adopted));
  assert.equal(await page.getByRole('slider', { name: 'Track 1 volume' }).inputValue(), '50', "the engine's lane volume is adopted");
  assert.equal(await page.getByRole('button', { name: 'Metronome click' }).getAttribute('aria-pressed'), 'true', "the engine's click is adopted");
  assert.equal(await page.getByRole('slider', { name: 'Master volume' }).inputValue(), '60', "the engine's master volume is adopted");
  assert.ok(!adopted.some((c) => c.SetVolume || c.SetMasterVolume !== undefined || c.SetMetronome !== undefined), 'adopted settings are not pushed back');
  assert.ok(adopted.some((c) => c.SetClickVolume === 0.7), 'the persisted click volume the engine lacks is sent');

  // ── A beat is shown when it is heard: 0.3 s of frames ahead of the render clock, plus 0.1 s of output
  // latency, keeps the beat LED waiting ~0.4 s ───────────────────────────────────────────────────────
  const led = (k) => page.locator('.transport__beat').nth(k).evaluate((el) => el.classList.contains('on'));
  await emit({
    status: { backend: 'Wasapi', sampleRate: RATE, block: 256, inputName: 'Fake input', outputName: 'Fake output', alignFrames: 4800, inputFrames: 0, inputOpen: true },
    anchor: anchorAt(10 * BAR),
    events: [{ Beat: { frame: 10 * BAR + 14400, beatInBar: 2, countLeft: 0, clicked: true } }],
  });
  const t0 = Date.now();
  assert.equal(await led(2), false, 'the beat is not shown before it is heard');
  await page.waitForFunction(() => document.querySelectorAll('.transport__beat')[2].classList.contains('on'), undefined, { timeout: 2000, polling: 10 });
  const shownAfter = Date.now() - t0;
  console.log(`beat shown ${shownAfter} ms after its frame arrived (heard 400 ms later)`);
  assert.ok(shownAfter > 250 && shownAfter < 700, `the beat LED waited for the heard time (${shownAfter} ms)`);

  // Engine mode never builds the web audio path, nor Tone's default context (`src/main.tsx`).
  const contexts = await page.evaluate(() => window.__audioContexts);
  for (const stack of contexts) console.log(stack);
  assert.equal(await page.evaluate(() => window.__lf.engine._ctx), undefined, "the web engine's AudioContext was never built");
  assert.equal(contexts.length, 0, 'no AudioContext was constructed');
  assert.equal(await page.evaluate(() => typeof window.AudioContext), 'function', 'the constructor is back after the app loaded');
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
