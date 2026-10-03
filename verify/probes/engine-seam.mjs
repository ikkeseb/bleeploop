/**
 * Engine mode's UI seam, on the web engine fake (`src/platform/host.web.ts`). An init script sets
 * `window.__lfEngineFake` before the app loads, so it boots in engine mode against the fake host; the
 * probe then drives the real UI and scripts the feed through `__lf.native`:
 *
 * - boot: the saved device opens once (WASAPI in the browser, the saved buffer); the first reset frame
 *   gets what the UI persists and owns (master and click level, the note target), not defaults the
 *   engine has; no AudioContext is ever built (the app has no Web Audio path);
 * - gesture → command: BPM +, CLICK, the lane core (its pointerdown selects), Space (the engine's
 *   hands-free `Action`), a lane volume, no MIC (a slot set to Off goes live instead: `slot-sources.mjs`),
 *   a PC key (NoteOn, NoteOff), the
 *   FIXED stepper over a committed loop (a bar at a time up to the loop, whole loops past it: F14), IN FX
 *   (the input sends: ECHO on, its level and division, kept in localStorage; the pill reads engaged);
 * - frame → DOM: a count-in (ARMED, the numeral, the beat LED, the BPM lock), a beat LED shown when the
 *   beat is heard (its frame and the output latency against the clock anchor), a live take, a committed
 *   loop (PLAYING, the loop readout, a moving ring dial from the clock anchor), the record meter, a
 *   refusal on its lane, the selection, a COPY carrying the lane's volume, a lane's mix kept when it only
 *   goes EMPTY and reset by `Cleared`, a multiply take's record head sweeping its window (the canvas),
 *   a later take's wait as the engine's beats tell it (a count-in from stopped loops: COUNT-IN, the
 *   numeral, no head on the canvas; an arm beside a playing loop: WAITING FOR DOWNBEAT, no numeral, the
 *   amber head; a cancelled count leaving no numeral behind, a beat still to be shown included; a
 *   cancel and a re-arm in one frame; COUNT-IN and no head from the frame the count's first beat
 *   arrives, the numeral when it is heard; a cancelled count's late beat leaving a newer numeral up; a
 *   reset frame's replayed count leaving the next arm uncounted),
 *   and a reload's reset frame whose remembered settings are adopted (the input sends it lacks are sent
 *   from what the UI kept).
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
  fading: false,
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
  assert.deepEqual(opened, [{ backend: 'Wasapi', input: null, output: null, inputChannels: [null, null], buffer: 256, sampleRate: null }]);
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
  await emit({ events: [{ Copied: { frame: BAR, from: 0, to: 1, feedback: 1 } }, laneEvent(1, committed)] });
  assert.equal(await page.getByRole('slider', { name: 'Track 2 volume' }).inputValue(), '80', 'COPY carries the volume');
  // A lane going EMPTY keeps its mix (an aborted take); only the engine's CLEAR resets it.
  await clearSent();
  await emit({ events: [laneEvent(1, lane('Empty'))] });
  assert.equal(await page.getByRole('slider', { name: 'Track 2 volume' }).inputValue(), '80', 'EMPTY alone keeps the mix');
  await emit({ events: [{ Cleared: { frame: BAR, lane: 1 } }] });
  assert.equal(await page.getByRole('slider', { name: 'Track 2 volume' }).inputValue(), '100', 'Cleared resets the mix');
  assert.deepEqual(await sent(), [], 'the engine reset its own mix: nothing is sent');

  // ── No MIC, and the play path ─────────────────────────────────────────────────────────────────
  assert.equal(await page.getByRole('button', { name: 'Mic / line input' }).count(), 0, 'engine mode has no MIC button');

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

  // ── IN FX: the input sends, a rig setting the UI keeps ────────────────────────────────────────────
  const infx = page.getByRole('button', { name: 'Input effects' });
  const engaged = () => infx.evaluate((el) => el.classList.contains('is-on'));
  const dialog = page.getByRole('dialog', { name: 'Input effects' });
  assert.equal(await engaged(), false, 'IN FX starts off');
  await infx.click();
  await dialog.waitFor();
  await clearSent();
  await page.getByRole('button', { name: 'Input echo' }).click();
  assert.deepEqual(await sentAtLeast(1), [{ SetInputSend: ['echo', true] }], 'ECHO sends SetInputSend');
  assert.equal(await engaged(), true, 'IN FX reads engaged while a send is on');
  await clearSent();
  await page.getByRole('slider', { name: 'Echo level' }).fill('0.8');
  assert.deepEqual(await sentAtLeast(1), [{ SetInputSendParam: ['echoLevel', 0.8] }], 'the level slider sends its value');
  await clearSent();
  await page.getByRole('combobox', { name: 'Echo time' }).selectOption({ label: '1/16' });
  assert.deepEqual(await sentAtLeast(1), [{ SetInputSendParam: ['echoTime', 3] }], 'the division sends its index');
  const kept = await page.evaluate(() => [localStorage.getItem('lf.inputSend.echo'), localStorage.getItem('lf.inputSend.echoLevel')]);
  assert.deepEqual(kept, ['1', '0.8'], 'kept for the next launch');
  // RING MOD: its toggle and its Freq slider, in whole Hz; kept as the echo is, then switched off again.
  const ringToggle = page.getByRole('button', { name: 'Input ring mod' });
  await clearSent();
  await ringToggle.click();
  assert.deepEqual(await sentAtLeast(1), [{ SetInputSend: ['ring', true] }], 'RING MOD sends SetInputSend');
  await clearSent();
  const ringFreq = page.getByRole('slider', { name: 'Ring mod freq' });
  await ringFreq.fill('900');
  assert.deepEqual(await sentAtLeast(1), [{ SetInputSendParam: ['ringFreq', 900] }], 'the Freq slider sends its Hz');
  const ringRead = await page.locator('.fxp-param', { has: ringFreq }).locator('.fxp-param__val').textContent();
  assert.equal(ringRead, '900 Hz', 'the Freq reads whole Hz with its unit');
  const keptRing = await page.evaluate(() => [localStorage.getItem('lf.inputSend.ring'), localStorage.getItem('lf.inputSend.ringFreq')]);
  assert.deepEqual(keptRing, ['1', '900'], 'the ring is kept for the next launch');
  await clearSent();
  await ringToggle.click();
  assert.deepEqual(await sentAtLeast(1), [{ SetInputSend: ['ring', false] }], 'RING MOD switches off');
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), 'Input effects', 'Escape returns focus to IN FX');
  // The stage view (a pedal's press here: B with nothing focused) closes an open IN FX, whose portal it
  // would otherwise leave focusable behind the stage.
  await infx.click();
  await dialog.waitFor();
  await page.evaluate(() => /** @type {HTMLElement | null} */ (document.activeElement)?.blur());
  await page.keyboard.press('b');
  await page.locator('.sv').waitFor();
  await dialog.waitFor({ state: 'detached' });
  await page.keyboard.press('b');
  await page.locator('.sv').waitFor({ state: 'detached' });

  // ── A WebView reload: the reset frame carries what the engine remembers, and the UI adopts it ─────
  await clearSent();
  await emit({
    reset: true,
    settings: [
      { SetMasterVolume: 0.6 },
      { SetInputSend: ['echo', false] },
      { SetMetronome: true },
      { SetVolume: [0, 0.5] },
      { SetFxBypass: [0, 'filter', false] },
    ],
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
  assert.equal(await engaged(), false, "the engine's echo (off) is adopted");
  assert.ok(!adopted.some((c) => c.SetInputSend?.[0] === 'echo'), 'the adopted send is not pushed back');
  assert.ok(adopted.some((c) => JSON.stringify(c) === JSON.stringify({ SetInputSendParam: ['echoLevel', 0.8] })), 'the kept echo level the engine lacks is sent');
  assert.equal(await page.evaluate(() => localStorage.getItem('lf.inputSend.echo')), '0', "the engine's echo is kept");

  // ── FIXED past the loop (F14 multiply): a bar at a time up to the loop, whole loops above it ─────
  await emit({ events: [laneEvent(0, lane('Playing', { length: 2 * BAR, canReverse: true })), transport(2 * BAR, true, 120)] });
  await clearSent();
  const fixedToggle = page.getByRole('button', { name: 'Fixed take length', exact: true });
  await fixedToggle.click();
  assert.deepEqual(await sentAtLeast(1), [{ SetFixedLength: true }]);
  assert.equal((await fixedToggle.textContent()).trim(), 'FIXED 4', 'FIXED 4 over a 2-bar loop: two loops');
  assert.match(await fixedToggle.getAttribute('title'), /Longer: the loop grows to it in whole loops/);
  const more = page.getByRole('button', { name: 'More bars', exact: true });
  const fewer = page.getByRole('button', { name: 'Fewer bars', exact: true });
  await clearSent();
  for (const step of [more, fewer, fewer, fewer]) await step.click();
  assert.deepEqual(await sentAtLeast(4), [{ SetFixedBars: 6 }, { SetFixedBars: 4 }, { SetFixedBars: 2 }, { SetFixedBars: 1 }]);
  await emit({ events: [laneEvent(0, lane('Playing', { length: 3 * BAR, canReverse: true })), transport(3 * BAR, true, 120)] });
  await clearSent();
  for (const step of [more, more, more, fewer]) await step.click();
  assert.deepEqual(await sentAtLeast(4), [{ SetFixedBars: 2 }, { SetFixedBars: 3 }, { SetFixedBars: 6 }, { SetFixedBars: 3 }], 'a 3-bar loop steps 1, 2, 3, 6');
  await clearSent();
  await more.click();
  await emit({ events: [laneEvent(0, committed), transport(BAR, true, 120)] });
  assert.equal((await fixedToggle.textContent()).trim(), 'FIXED 6', 'over a 1-bar loop every bar is a whole loop');
  await fixedToggle.click();
  assert.deepEqual(await sentAtLeast(2), [{ SetFixedBars: 6 }, { SetFixedLength: false }]);

  // A multiply take's record head sweeps its window, not the old loop: FIXED 6 over the 1-bar loop,
  // three bars into the take, the head (the rec-red column on the canvas's top row) is halfway across.
  await clearSent();
  await fixedToggle.click();
  assert.deepEqual(await sentAtLeast(1), [{ SetFixedLength: true }]);
  await emit({ events: [laneEvent(1, lane('Recording'), 0)], anchor: anchorAt(3 * BAR) });
  const recHeadAt = () =>
    lanes.nth(1).locator('canvas').evaluate((c) => {
      const row = c.getContext('2d').getImageData(0, 0, c.width, 1).data;
      const hits = [];
      for (let x = 0; x < c.width; x++) {
        const [r, g, b, a] = row.slice(4 * x, 4 * x + 4);
        if (a > 200 && r > 200 && g < 120 && b < 140) hits.push(x);
      }
      return hits.length ? hits[hits.length >> 1] / c.width : -1;
    });
  let headAt = -1;
  for (let tries = 0; tries < 40 && headAt < 0; tries++) {
    await page.waitForTimeout(50);
    headAt = await recHeadAt();
  }
  console.log('multiply take record head at', headAt.toFixed(3), 'of the lane');
  assert.ok(Math.abs(headAt - 0.5) < 0.05, `three bars into a 6-bar multiply the head is halfway (${headAt})`);
  await emit({ events: [laneEvent(1, lane('Empty'))] });
  await clearSent();
  await fixedToggle.click();
  assert.deepEqual(await sentAtLeast(1), [{ SetFixedLength: false }]);

  // ── A later take's wait: counted in or not is the engine's to say (its beats' `countLeft`) ─────────
  const count = (i) => lanes.nth(i).locator('.lp-lane__count');
  const armed = lane('Recording', { armed: true });
  const stopped = lane('Stopped', { length: BAR, canReverse: true });
  const beat = (frame, countLeft) => ({ Beat: { frame, beatInBar: (4 - countLeft) % 4, countLeft, clicked: countLeft > 0 } });
  /** The armed lane's amber head (--dub) on the canvas's top row, 0..1 across it; -1: none drawn. */
  const amberHeadAt = (i) =>
    lanes.nth(i).locator('canvas').evaluate((c) => {
      const row = c.getContext('2d').getImageData(0, 0, c.width, 1).data;
      const hits = [];
      for (let x = 0; x < c.width; x++) {
        const [r, g, b, a] = row.slice(4 * x, 4 * x + 4);
        if (a > 200 && r > 200 && g > 120 && g < 220 && b < 110) hits.push(x);
      }
      return hits.length ? hits[hits.length >> 1] / c.width : -1;
    });
  /** Whether lane `i` draws an amber head in any of a few animation frames. */
  const amberHeadSeen = async (i) => {
    let seen = false;
    for (let tries = 0; tries < 8; tries++) {
      await page.waitForTimeout(40);
      seen ||= (await amberHeadAt(i)) >= 0;
    }
    return seen;
  };

  // (a) Every loop stopped, REC on an empty lane: the engine counts in (the count's first beat precedes
  // the lane's own event, as the engine emits them), and the lane reads as a first take's count-in does.
  let at = 20 * BAR + 1000;
  await emit({ events: [laneEvent(0, stopped, at)], anchor: anchorAt(at) });
  await emit({ events: [beat(at, 4), laneEvent(1, armed, at)], anchor: anchorAt(at) });
  console.log('counted in from stopped loops:', JSON.stringify(await well(1).textContent()));
  assert.equal(await lanes.nth(1).getAttribute('data-state'), 'armed');
  assert.match(await well(1).textContent(), /COUNT-IN/, 'a later take the engine counts in reads COUNT-IN');
  assert.equal(await count(1).textContent(), '4');
  assert.equal(await amberHeadSeen(1), false, 'no head rides the stopped loop during the count');
  for (const left of [3, 2, 1]) {
    at += BAR / 4;
    await emit({ events: [beat(at, left)], anchor: anchorAt(at) });
    assert.equal(await count(1).textContent(), String(left), `the count shows ${left}`);
    assert.match(await well(1).textContent(), /COUNT-IN/);
  }
  // The downbeat: the loops restart and the count is over; the lane, still armed for its alignment,
  // keeps the word it had and shows no numeral.
  at += BAR / 4;
  await emit({ events: [laneEvent(0, committed, at), beat(at, 0)], anchor: anchorAt(at) });
  assert.match(await well(1).textContent(), /COUNT-IN/, 'counted in until the lane leaves ARMED');
  assert.equal(await count(1).count(), 0, 'no numeral past the count');
  await emit({ events: [laneEvent(1, lane('Empty'), at)] });

  // (b) A loop plays, REC on an empty lane: the engine arms it for the loop boundary and counts nothing.
  await emit({ events: [laneEvent(1, armed, at), beat(at + BAR / 4, 0)], anchor: anchorAt(at + BAR / 4) });
  console.log('armed beside a playing loop:', JSON.stringify(await well(1).textContent()));
  assert.equal(await lanes.nth(1).getAttribute('data-state'), 'armed');
  assert.match(await well(1).textContent(), /WAITING FOR DOWNBEAT/, 'an arm beside a playing loop waits for the boundary');
  assert.equal(await count(1).count(), 0, 'no numeral without a count');
  assert.equal(await amberHeadSeen(1), true, 'the amber head rides the loop phase while it waits');
  await emit({ events: [laneEvent(1, lane('Empty'), at)] });

  // (c) A count cancelled mid-way, then, before any new beat, an ordinary arm beside a playing loop:
  // the cancelled count's numeral is gone, so nothing reads as counted in.
  at += 4 * BAR;
  await emit({ events: [laneEvent(0, stopped, at)], anchor: anchorAt(at) });
  await emit({ events: [beat(at, 4), laneEvent(1, armed, at)], anchor: anchorAt(at) });
  await emit({ events: [beat(at + BAR / 4, 3)], anchor: anchorAt(at + BAR / 4) });
  assert.equal(await count(1).textContent(), '3');
  await emit({ events: [laneEvent(1, lane('Empty'), at + BAR / 4 + 100)] });
  await emit({ events: [laneEvent(0, committed, at + BAR / 4 + 200), laneEvent(1, armed, at + BAR / 4 + 300)] });
  console.log('armed after a cancelled count:', JSON.stringify(await well(1).textContent()));
  assert.match(await well(1).textContent(), /WAITING FOR DOWNBEAT/, 'a cancelled count does not make the next arm read counted in');
  assert.equal(await count(1).count(), 0, 'a cancelled count leaves no numeral behind');
  assert.equal(await amberHeadSeen(1), true, 'the head rides the phase again once the count is cancelled');
  await emit({ events: [laneEvent(1, lane('Empty'), at)] });

  // (d) A counted arm cancelled and an ordinary arm beside a playing loop, all in ONE feed frame: the
  // lane is ARMED before and after the frame, and still nothing of the cancelled count is left on it.
  at += 4 * BAR;
  await emit({ events: [laneEvent(0, stopped, at)], anchor: anchorAt(at) });
  await emit({ events: [beat(at, 4), laneEvent(1, armed, at)], anchor: anchorAt(at) });
  assert.equal(await count(1).textContent(), '4');
  await emit({ events: [laneEvent(1, lane('Empty'), at + 100), laneEvent(0, committed, at + 200), laneEvent(1, armed, at + 300)] });
  console.log('cancelled and re-armed in one frame:', JSON.stringify(await well(1).textContent()));
  assert.equal(await lanes.nth(1).getAttribute('data-state'), 'armed');
  assert.match(await well(1).textContent(), /WAITING FOR DOWNBEAT/, 'a cancel and a re-arm in one frame leave no count on the lane');
  assert.equal(await count(1).count(), 0, 'a cancel and a re-arm in one frame leave no numeral');
  await emit({ events: [laneEvent(1, lane('Empty'), at)] });

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

  // A count beat still waiting to be heard when its count is cancelled shows no numeral afterwards: the
  // arm that follows, beside a playing loop, never reads as counted in (0.4 s until the beat is shown).
  at = 40 * BAR;
  await emit({ events: [laneEvent(0, stopped, at), beat(at + 14400, 3), laneEvent(1, armed, at)], anchor: anchorAt(at) });
  await emit({ events: [laneEvent(1, lane('Empty'), at + 100)] });
  await emit({ events: [laneEvent(0, committed, at + 200), laneEvent(1, armed, at + 300)] });
  await page.waitForTimeout(700);
  console.log('armed after a cancelled count whose beat was still to be shown:', JSON.stringify(await well(1).textContent()));
  assert.match(await well(1).textContent(), /WAITING FOR DOWNBEAT/, 'a cancelled count beat shown late starts no count');
  assert.equal(await count(1).count(), 0, 'a cancelled count beat shown late shows no numeral');
  await emit({ events: [laneEvent(1, lane('Empty'), at)] });

  // (e) The count is known from the frame its first beat ARRIVES, 0.6 s before that beat is heard: the
  // lane reads COUNT-IN and draws no head at once, and the numeral follows when the beat is heard.
  at = 44 * BAR;
  await emit({ events: [laneEvent(0, stopped, at), beat(at + 24000, 4), laneEvent(1, armed, at)], anchor: anchorAt(at) });
  const e0 = Date.now();
  const wellAtOnce = await well(1).textContent();
  const numeralsAtOnce = await count(1).count();
  const headBeforeNumeral = await amberHeadSeen(1);
  console.log(`count beat received, not yet heard: ${JSON.stringify(wellAtOnce)}, ${numeralsAtOnce} numeral(s), amber head ${headBeforeNumeral} (${Date.now() - e0} ms in)`);
  assert.match(wellAtOnce, /COUNT-IN/, 'COUNT-IN from the frame the count beat arrives');
  assert.equal(numeralsAtOnce, 0, 'no numeral before the count beat is heard');
  assert.equal(headBeforeNumeral, false, 'no head from the frame the count beat arrives');
  await count(1).waitFor({ timeout: 2000 });
  console.log(`numeral shown ${Date.now() - e0} ms after its beat arrived (heard 600 ms later)`);
  assert.equal(await count(1).textContent(), '4', 'the numeral shows when the beat is heard');
  await emit({ events: [laneEvent(1, lane('Empty'), at)] });

  // (f) A cancelled count's beat, shown late, leaves a newer count's numeral alone: count A's beat is
  // heard 0.6 s on, the arm is cancelled, and count B's first beat (already heard: its frame is one
  // output latency behind the new anchor) puts up its numeral before A's beat is shown.
  at = 48 * BAR;
  await emit({ events: [beat(at + 24000, 3), laneEvent(1, armed, at)], anchor: anchorAt(at) });
  await emit({ events: [laneEvent(1, lane('Empty'), at + 100)] });
  await emit({ events: [beat(at, 4), laneEvent(1, armed, at + 4800)], anchor: anchorAt(at + 4800) });
  assert.equal(await count(1).textContent(), '4', "the newer count's numeral is up before the cancelled beat is shown");
  await page.waitForTimeout(900);
  const numeralAfter = (await count(1).count()) ? await count(1).textContent() : '';
  console.log("the newer count's numeral after the cancelled count's beat was shown:", JSON.stringify(numeralAfter));
  assert.equal(numeralAfter, '4', "a cancelled count's late beat does not clear a newer count's numeral");
  assert.match(await well(1).textContent(), /COUNT-IN/);
  await emit({ events: [laneEvent(1, lane('Empty'), at)] });

  // (g) A reload's reset frame replays the lanes as they are and the drained beats after them: a count's
  // last beats arrive with its lane already recording, so no lane ends that count, and the ordinary arm
  // that follows beside the playing loop must not read as counted in.
  at = 52 * BAR;
  await emit({
    reset: true,
    settings: [],
    events: [laneEvent(1, lane('Recording'), at), laneEvent(0, committed, at), transport(BAR, true, 120), beat(at - BAR / 4, 1), beat(at, 0)],
    anchor: anchorAt(at),
    meter: { peak: 0, clip: false },
  });
  assert.equal(await lanes.nth(1).getAttribute('data-state'), 'rec');
  await emit({ events: [laneEvent(2, armed, at + 4800)], anchor: anchorAt(at + 4800) });
  console.log("armed after a reset frame carrying a count's last beats:", JSON.stringify(await well(2).textContent()));
  assert.equal(await lanes.nth(2).getAttribute('data-state'), 'armed');
  assert.match(await well(2).textContent(), /WAITING FOR DOWNBEAT/, "a reset frame's replayed count does not make the next arm read counted in");
  assert.equal(await count(2).count(), 0, "no numeral after a reset frame's replayed count");
  assert.equal(await amberHeadSeen(2), true, "the amber head rides the loop phase after a reset frame's replayed count");
  await emit({ events: [laneEvent(1, lane('Empty'), at), laneEvent(2, lane('Empty'), at)] });

  // The app never builds an AudioContext: the engine plays everything.
  const contexts = await page.evaluate(() => window.__audioContexts);
  for (const stack of contexts) console.log(stack);
  assert.equal(contexts.length, 0, 'no AudioContext was constructed');
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
