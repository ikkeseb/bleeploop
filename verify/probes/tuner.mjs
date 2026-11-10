/**
 * The tuner's overlay (`src/ui/tuner/`), on the web engine fake (`src/platform/host.web.ts`), booted as
 * `engine-seam.mjs` boots it. The fake answers the tuner's press (`{ Action: 'Tuner' }`) and its setter
 * (`SetTuner`) as the engine does, with `Tuner` a microtask later or `Refused` (`TunerCapturing`) while a
 * scripted lane records; the probe scripts readings (`tuner: { hz, level }`) through `__lf.native.emit`,
 * the real decoder, at the host's pace (every 40 ms, `pump`).
 *
 * Proves, by group (`--case=<name>[,<name>]` runs some):
 * - open: N (either case, the keyboard shown or hidden) sends the press and nothing opens until the
 *   engine's `Tuner` (held with `holdApply`); then the dialog shows, named Tuner and modal, takes focus,
 *   and the normal UI is inert; transport keys stay live, note keys are paused, a note held as it opened
 *   still releases, and the digits select;
 * - readings: the letter, sharp, octave, cents and in-tune state for E2 18 cents flat, A2 4 sharp, E4 in
 *   tune and A#4; `hz: null` shows the dash; a reading 300 ms old is dropped; a second under -60 dBFS
 *   says no signal arrives on the input, and a signal takes it down; the indicator eases (and steps with
 *   reduced motion);
 * - close: Escape sends `SetTuner(false)` once per press and the overlay stays until the engine's `Tuner`
 *   (two quick ones never reopen it); the on-screen close does the same; an engine-side close closes it and
 *   focus returns where it was; a reset frame closes it unless it carries `Tuner` on (then it stays,
 *   unremounted, its reading dropped at once and a fresh one shown); a refused press shows its reason
 *   on the lane and opens nothing;
 * - key: the command bar's TUNER key sends the press and is lit while the tuner is on;
 * - stage: over the stage view the tuner is on top, the view inert under it, and Escape closes the tuner
 *   first, focus back on the view;
 * - gap: a loud reading then no more drops the pitch and never says no signal (only fresh readings under
 *   -60 dBFS count, and a stale stretch ends the second);
 * - text: a second of one steady reading writes the cents and frequency text nodes not once;
 * - held: a held Escape (its key repeats) closes the tuner and leaves the stage view under it open;
 * - return: the focus target resolved at close: a popover the tuner closed falls back to the TUNER key, an
 *   inert one (a stage view opened under the tuner) to the view's root, a live one gets it back;
 * - confirm: a pending ask (`src/ui/confirm/`) under a pedal-opened tuner is not shown, takes no focus and
 *   no key (Escape closes the tuner, the ask stays pending), and is back with focus on its button when the
 *   tuner closes; an ask that arrives under the tuner takes focus when it closes;
 * - shots: the four states at 2560x1440, 1920x1080, 1280x820, 1000x700 and 960x600 into `logs/tuner/`,
 *   each measured: the letter's cap height at least a fifth of the window, the note, the scale and the
 *   lines inside it and apart;
 * - cmdbar: the command bar at 1000x700 and 960x600, keyboard shown and hidden, with and without the TUNER
 *   key: the key inside the bar, and the bar's fold rung with it (`logs/tuner/cmd-*.png`).
 *
 * Cannot see the native engine (the mute, the detector, what closes it natively), WebView2, a 27" screen
 * at 1.5 m or the eye: the screenshots are for a person to judge.
 * Run: pnpm probe tuner
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { arg, probe } from '../harness/probe.ts';

const RATE = 48000;
const OUT = 'logs/tuner';
const only = arg('case')?.split(',');

const lane = (state = 'Empty') => ({
  state,
  length: state === 'Empty' ? 0 : 2 * RATE,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: false,
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
  clearArmed: false,
});
const empties = () => [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane() } }));
const transport = { Transport: { frame: 0, master: 0, bpm: 120, meter: { num: 4, den: 4, beats: 4 }, locked: false } };
const tunerEvent = (on) => ({ Tuner: { frame: 0, on } });

/** `hz` moved by `c` cents, to the wire's 3 decimals. */
const off = (hz, c) => Math.round(hz * 2 ** (c / 1200) * 1000) / 1000;
/** The four states the brief names, and a sharp note: the reading and what the overlay must show. */
const STATES = {
  'e2-flat-18': { hz: off(82.407, -18), level: -14, letter: 'E', sharp: false, octave: '2', cents: '−18', state: 'off' },
  'a2-sharp-4': { hz: off(110, 4), level: -16, letter: 'A', sharp: false, octave: '2', cents: '+4', state: 'off' },
  'e4-in-tune': { hz: 329.628, level: -20, letter: 'E', sharp: false, octave: '4', cents: '0', state: 'in' },
  'a#4': { hz: 466.164, level: -22, letter: 'A', sharp: true, octave: '4', cents: '0', state: 'in' },
};
const NO_SIGNAL = "no signal on the input, check the cable and the slot's input";
const REFUSED = 'a take is recording, the tuner opens after it';
const SIZES = [
  [2560, 1440],
  [1920, 1080],
  [1280, 820],
  [1000, 700],
  [960, 600],
];

await probe(async ({ open }) => {
  const app = await open({ viewport: { width: 1280, height: 820 }, init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
  const { page } = app;
  await mkdir(OUT, { recursive: true });
  let seq = 0;
  const emit = (frame) =>
    page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], device: [], anchor: null, meter: null, peaks: [], ...frame });
  const boot = (events = []) =>
    emit({
      reset: true,
      settings: [],
      events: [...empties(), transport, { Selected: { frame: 0, lane: 0 } }, ...events],
      anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      meter: { peak: [0, 0], clip: [false, false] },
    });
  const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
  const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = window.__lf.native.inputSent.length = 0));
  const notesOn = () => page.evaluate(() => window.__lf.native.inputSent.filter((e) => e.note?.on).length);
  const notesOff = () => page.evaluate(() => window.__lf.native.inputSent.filter((e) => e.note && !e.note.on).length);
  const settle = (ms = 120) => page.waitForTimeout(ms);
  const hold = (on) => page.evaluate((v) => void (window.__lf.native.holdApply = v), on);

  /** Readings at the host's pace (every 40 ms) until `stopPump`, each through the real decoder. */
  const pump = (hz, level) =>
    page.evaluate(
      ([h, l]) => {
        clearInterval(window.__tunerPump);
        let s = 1_000_000;
        const send = () => {
          window.__lf.native.emit({ seq: s++, reset: false, events: [], device: [], anchor: null, meter: null, peaks: [], tuner: { hz: h, level: l } });
          window.__tunerLastAt = performance.now();
        };
        send();
        window.__tunerPump = setInterval(send, 40);
        window.__tunerPumpAt = performance.now();
      },
      [hz, level],
    );
  const stopPump = () => page.evaluate(() => clearInterval(window.__tunerPump));

  /** The overlay as a player and a screen reader meet it, and what is inert under it. */
  const read = () =>
    page.evaluate(() => {
      const t = document.querySelector('.tn');
      const base = {
        open: !!t,
        cmdInert: document.querySelector('.cmd').inert,
        stageInert: document.querySelector('main.stage').inert,
        keyOn: document.querySelector('.tool--tuner')?.classList.contains('tool--on') ?? null,
        keyPressed: document.querySelector('.tool--tuner')?.getAttribute('aria-pressed') ?? null,
      };
      if (!t) return base;
      return {
        ...base,
        role: t.getAttribute('role'),
        modal: t.getAttribute('aria-modal'),
        name: t.getAttribute('aria-label'),
        state: t.dataset.state,
        signal: t.dataset.signal ?? null,
        letter: t.querySelector('.tn-letter').textContent,
        sharp: !!t.querySelector('.tn-note .tn-sharp'),
        octave: t.querySelector('.tn-octave').textContent,
        cents: t.querySelector('.tn-cents > span').textContent,
        hz: t.querySelector('.tn-hz > span').textContent,
        readShown: getComputedStyle(t.querySelector('.tn-read')).visibility === 'visible',
        quiet: t.querySelector('.tn-quiet').textContent,
        muted: t.querySelector('.tn-muted').textContent,
        needle: t.querySelector('.tn-needle').style.transform,
        focused: document.activeElement === t,
        pumpMs: window.__tunerPumpAt === undefined ? null : performance.now() - window.__tunerPumpAt,
      };
    });
  const needleAt = (r) => Number(/translateX\((-?[\d.]+)%\)/.exec(r.needle)?.[1] ?? Number.NaN);
  const waitOpen = (want) => page.waitForFunction((w) => !!document.querySelector('.tn') === w, want, { timeout: 3000 });
  /** Open it the engine's way: a scripted `Tuner` (the fake's value follows the scripted frame). */
  const openByEngine = async () => {
    await emit({ events: [tunerEvent(true)] });
    await waitOpen(true);
    await page.waitForFunction(() => document.activeElement === document.querySelector('.tn'));
  };

  const failures = [];
  const groups = new Set();
  let ran = 0;
  const group = async (name, body) => {
    groups.add(name);
    if (only && !only.includes(name)) return;
    ran++;
    console.log(`── ${name}`);
    try {
      await page.emulateMedia({ reducedMotion: 'no-preference' });
      await page.setViewportSize({ width: 1280, height: 820 });
      await stopPump();
      await hold(false);
      await boot();
      await waitOpen(false);
      await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
      await clearSent();
      await body();
      console.log(`ok ${name}`);
    } catch (error) {
      failures.push(name);
      console.log(`FAIL ${name}: ${error.message}`);
      // A failed group leaves no ask pending and no stage view open for the next one.
      await page.evaluate(async () => {
        const { pendingConfirm } = await import('/src/ui/confirm/confirm-store.ts');
        pendingConfirm()?.answer(false);
      });
      if (await page.evaluate(() => !!document.querySelector('.sv'))) await page.click('.sv-btn--exit').catch(() => {});
    } finally {
      await stopPump();
    }
  };

  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });

  await group('open', async () => {
    // The press goes out; nothing shows until the engine answers.
    await hold(true);
    await page.keyboard.press('n');
    assert.deepEqual(await sent(), [{ Action: 'Tuner' }], 'N sends the tuner press, nothing else');
    await settle();
    assert.equal((await read()).open, false, 'the press alone opens nothing');
    await hold(false);
    await waitOpen(true);
    await page.waitForFunction(() => document.activeElement === document.querySelector('.tn'));
    const r = await read();
    console.log(`open: ${JSON.stringify({ role: r.role, modal: r.modal, name: r.name, state: r.state, letter: r.letter, focused: r.focused })}`);
    assert.deepEqual([r.role, r.modal, r.name], ['dialog', 'true', 'Tuner'], 'a modal dialog named Tuner');
    assert.equal(r.focused, true, 'focus moves to the overlay');
    assert.deepEqual([r.cmdInert, r.stageInert], [true, true], 'the normal UI is inert');
    assert.deepEqual([r.state, r.letter, r.readShown], ['none', '–', false], 'no reading yet: the dash, no read-out');
    assert.ok(r.muted.includes('muted') && r.muted.includes('N, Esc or the pedal'), `the standing line says muted and how to leave: "${r.muted}"`);
    const z = await page.evaluate(() => Number(getComputedStyle(document.querySelector('.tn')).zIndex));
    assert.ok(z > 100 && z < 200, `over the stage view (100), under the toasts (200): ${z}`);

    // Transport keys stay live; the note keys are paused; the digits select.
    await clearSent();
    await page.keyboard.press('Space');
    await page.keyboard.press('3');
    await page.keyboard.down('a');
    await page.keyboard.up('a');
    await settle(60);
    const live = await sent();
    assert.deepEqual(live, [{ Action: 'RecDub' }, { SelectTrack: 2 }], `Space and 3 still drive the looper: ${JSON.stringify(live)}`);
    assert.equal(await notesOn(), 0, 'A plays no note while the tuner is open');

    // Shift+N closes it as N does (the engine flips it off).
    await clearSent();
    await page.keyboard.press('Shift+N');
    assert.deepEqual(await sent(), [{ Action: 'Tuner' }], 'Shift+N sends the press too');
    await waitOpen(false);
    assert.deepEqual([(await read()).cmdInert, (await read()).stageInert], [false, false], 'closed: the UI is live again');

    // A note held as the tuner opens still ends on its key-up.
    await clearSent();
    await page.keyboard.down('a');
    await settle(60);
    assert.equal(await notesOn(), 1, 'control: with the tuner closed, A plays its note');
    await openByEngine();
    await page.keyboard.up('a');
    await settle(60);
    assert.equal(await notesOff(), 1, 'the note held as it opened is released');
    await emit({ events: [tunerEvent(false)] });
    await waitOpen(false);

    // The keyboard hidden: N still opens it (it is no note or pad key either way).
    await page.click('.tool--kbd');
    await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
    const hidden = await page.evaluate(() => !document.querySelector('.zone--keyboard'));
    await clearSent();
    await page.keyboard.press('n');
    await waitOpen(true);
    assert.deepEqual(await sent(), [{ Action: 'Tuner' }], `the keyboard hidden (${hidden}), N opens it`);
    await page.keyboard.press('n');
    await waitOpen(false);
    await page.click('.tool--kbd');
  });

  await group('readings', async () => {
    await openByEngine();
    for (const [name, s] of Object.entries(STATES)) {
      await pump(s.hz, s.level);
      await settle(200);
      const r = await read();
      console.log(`${name}: ${s.hz} Hz -> ${r.letter}${r.sharp ? '#' : ''}${r.octave} ${r.cents} (${r.state}), needle ${r.needle}, hz "${r.hz}"`);
      assert.deepEqual(
        [r.letter, r.sharp, r.octave, r.cents, r.state, r.readShown],
        [s.letter, s.sharp, s.octave, s.cents, s.state, true],
        `${name}: letter, sharp, octave, cents, in tune`,
      );
      assert.equal(r.hz, s.hz.toFixed(2), `${name}: the frequency`);
      assert.equal(r.signal, null, `${name}: a signal arrives`);
      const want = Number(s.cents.replace('−', '-').replace('+', ''));
      assert.ok(Math.abs(needleAt(r) - want) < 0.6, `${name}: the indicator at ${want} cents: ${r.needle}`);
    }

    // No pitch: the dash, the indicator back to the centre, no read-out.
    await pump(null, -30);
    await settle(250);
    let r = await read();
    assert.deepEqual([r.letter, r.state, r.readShown, r.sharp, r.octave], ['–', 'none', false, false, ''], 'hz null: the dash');
    assert.ok(Math.abs(needleAt(r)) < 0.6, `no pitch parks the indicator at the centre: ${r.needle}`);
    assert.equal(r.signal, null, 'a level of -30 dBFS is a signal');

    // A reading goes stale after 300 ms.
    await pump(STATES['e2-flat-18'].hz, -14);
    await settle(150);
    await stopPump();
    assert.equal((await read()).letter, 'E', 'a fresh reading shows');
    await settle(380);
    r = await read();
    assert.deepEqual([r.letter, r.state], ['–', 'none'], 'a reading older than 300 ms is dropped');

    // A second of fresh readings under -60 dBFS: no signal arrives on the input (a stale reading counts
    // for nothing, `gap`).
    await pump(110, -20);
    await settle(150);
    await pump(null, -75);
    await settle(500);
    r = await read();
    if (r.pumpMs < 950) assert.equal(r.quiet, '', `${Math.round(r.pumpMs)} ms of quiet: no line yet`);
    await page.waitForFunction(() => document.querySelector('.tn')?.dataset.signal === 'none', undefined, { timeout: 3000 });
    r = await read();
    console.log(`no signal after ${Math.round(r.pumpMs)} ms: "${r.quiet}"`);
    assert.ok(r.pumpMs >= 1000, `not before a second: ${r.pumpMs} ms`);
    assert.equal(r.quiet, NO_SIGNAL, 'the line says so');
    await pump(110, -25);
    await settle(150);
    r = await read();
    assert.deepEqual([r.quiet, r.signal, r.letter], ['', null, 'A'], 'a signal takes the line down at once');
    // Pitched but under -60 dBFS: still no signal worth tuning.
    await pump(110, -64);
    await page.waitForFunction(() => document.querySelector('.tn')?.dataset.signal === 'none', undefined, { timeout: 3000 });

    // The indicator eases; with reduced motion it steps.
    await pump(null, -30);
    await settle(250);
    await pump(STATES['e2-flat-18'].hz, -14);
    const eased = await page.evaluate(
      () =>
        new Promise((done) => {
          const out = [];
          const tick = () => {
            out.push(document.querySelector('.tn-needle').style.transform);
            if (out.length < 4) requestAnimationFrame(tick);
            else done(out);
          };
          requestAnimationFrame(tick);
        }),
    );
    const steps = eased.map((t) => needleAt({ needle: t }));
    console.log(`eased: ${steps.join(', ')}`);
    assert.ok(steps.some((x) => x < -0.5 && x > -17.5), `the indicator passes between 0 and -18: ${steps.join(', ')}`);
    await page.emulateMedia({ reducedMotion: 'reduce' });
    await pump(null, -30);
    await settle(250);
    await pump(STATES['a2-sharp-4'].hz, -14);
    await page.waitForFunction(() => document.querySelector('.tn')?.dataset.state === 'off');
    await page.evaluate(() => new Promise((done) => requestAnimationFrame(() => requestAnimationFrame(done))));
    r = await read();
    assert.ok(Math.abs(needleAt(r) - 4) < 0.2, `reduced motion: the indicator steps to +4 at once: ${r.needle}`);
  });

  await group('close', async () => {
    // Escape: SetTuner(false) once per press; the overlay waits for the engine.
    await openByEngine();
    await hold(true);
    await clearSent();
    await page.keyboard.press('Escape');
    assert.deepEqual(await sent(), [{ SetTuner: false }], 'Escape sends SetTuner(false), nothing else');
    await settle();
    assert.equal((await read()).open, true, 'Escape alone hides nothing');
    await page.keyboard.press('Escape');
    assert.deepEqual(await sent(), [{ SetTuner: false }, { SetTuner: false }], 'a second Escape: one more, never a press');
    await hold(false);
    await waitOpen(false);
    await settle(150);
    assert.equal((await read()).open, false, 'two quick Escapes closed it and never reopened it');
    const answers = await page.evaluate(() => window.__lf.native.sent.length);
    assert.equal(answers, 2);

    // The on-screen close: the same command, the same wait.
    await openByEngine();
    await hold(true);
    await clearSent();
    await page.click('.tn-close');
    assert.deepEqual(await sent(), [{ SetTuner: false }], 'the close sends SetTuner(false)');
    await settle();
    assert.equal((await read()).open, true, 'the close alone hides nothing');
    await hold(false);
    await waitOpen(false);

    // An engine-side close: focus returns where it was.
    await page.evaluate(() => document.querySelector('.tool--help').focus());
    await openByEngine();
    await emit({ events: [tunerEvent(false)] });
    await waitOpen(false);
    await page.waitForFunction(() => document.activeElement === document.querySelector('.tool--help'), undefined, { timeout: 2000 });
    await page.evaluate(() => document.activeElement.blur());

    // A reset frame closes it, unless it carries the engine's Tuner on.
    await openByEngine();
    await boot();
    await waitOpen(false);
    await openByEngine();
    await page.evaluate(() => (document.querySelector('.tn').__probeMark = 1));
    await pump(110, -20);
    await settle(120);
    // The pump stops before the reset, so nothing after it can stand in for the reading it drops.
    await stopPump();
    assert.equal((await read()).letter, 'A', 'control: the reading shows before the reset');
    await boot([tunerEvent(true)]);
    const after = await page.evaluate(
      () =>
        new Promise((done) =>
          requestAnimationFrame(() =>
            requestAnimationFrame(() => {
              const t = document.querySelector('.tn');
              done({ mark: t?.__probeMark ?? null, letter: t?.querySelector('.tn-letter').textContent, sinceMs: performance.now() - window.__tunerLastAt });
            }),
          ),
        ),
    );
    console.log(`reset with the tuner on: ${JSON.stringify(after)}`);
    assert.equal(after.mark, 1, 'a reset frame carrying Tuner on keeps it open, the same element');
    assert.ok(after.sinceMs < 250, `read well inside the 300 ms stale window: ${after.sinceMs.toFixed(0)} ms`);
    assert.equal(after.letter, '–', 'the reset dropped the reading before it went stale');
    await emit({ tuner: { hz: 110, level: -20 } });
    await page.waitForFunction(() => document.querySelector('.tn-letter')?.textContent === 'A', undefined, { timeout: 1000 });
    assert.equal(await page.evaluate(() => document.querySelector('.tn')?.__probeMark ?? null), 1, 'a fresh reading shows on the same overlay');

    // A refused press: the reason on the lane, nothing opens.
    await emit({ events: [tunerEvent(false)] });
    await waitOpen(false);
    await emit({ events: [{ Lane: { frame: 0, lane: 0, info: { ...lane('Recording'), length: 0 } } }] });
    await clearSent();
    await page.keyboard.press('n');
    assert.deepEqual(await sent(), [{ Action: 'Tuner' }]);
    await page.waitForFunction(
      (t) => [...document.querySelectorAll('.lp-lane__wellmsg.is-cue')].some((el) => el.textContent.includes(t)),
      REFUSED,
      { timeout: 3000 },
    );
    await settle();
    assert.equal((await read()).open, false, 'a refused press opens nothing');
  });

  await group('key', async () => {
    let r = await read();
    assert.deepEqual([r.keyOn, r.keyPressed], [false, 'false'], 'the TUNER key rests unlit');
    await page.click('.tool--tuner');
    assert.deepEqual(await sent(), [{ Action: 'Tuner' }], 'the TUNER key sends the press');
    await waitOpen(true);
    r = await read();
    assert.deepEqual([r.keyOn, r.keyPressed], [true, 'true'], 'lit while the tuner is on');
    await emit({ events: [tunerEvent(false)] });
    await waitOpen(false);
    r = await read();
    assert.deepEqual([r.keyOn, r.keyPressed], [false, 'false'], 'unlit once it is off');
  });

  await group('stage', async () => {
    await page.keyboard.press('b');
    await page.waitForSelector('.sv');
    await page.waitForFunction(() => document.activeElement === document.querySelector('.sv'));
    await clearSent();
    await page.keyboard.press('n');
    assert.deepEqual(await sent(), [{ Action: 'Tuner' }], 'N inside the stage view sends the press');
    await waitOpen(true);
    const top = await page.evaluate(() => {
      const hit = document.elementFromPoint(innerWidth / 2, innerHeight / 2);
      return { onTuner: !!hit?.closest('.tn'), stageInert: document.querySelector('.sv-host').inert, svThere: !!document.querySelector('.sv') };
    });
    assert.deepEqual(top, { onTuner: true, stageInert: true, svThere: true }, 'the tuner over the stage view, the view inert under it');
    await page.waitForFunction(() => document.activeElement === document.querySelector('.tn'));
    await clearSent();
    await page.keyboard.press('Escape');
    assert.deepEqual(await sent(), [{ SetTuner: false }], 'Escape goes to the tuner first');
    await waitOpen(false);
    await page.waitForFunction(() => document.activeElement === document.querySelector('.sv'), undefined, { timeout: 2000 });
    assert.equal(await page.evaluate(() => !!document.querySelector('.sv') && !document.querySelector('.sv-host').inert), true, 'the stage view stays, live again');
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => !document.querySelector('.sv'));
  });

  await group('gap', async () => {
    // A gap in the feed after a loud reading is no measurement of the input: the pitch drops when the
    // reading goes stale, and the no-signal line never comes.
    await openByEngine();
    await pump(110, -20);
    await settle(150);
    await stopPump();
    await settle(1700);
    const r = await read();
    console.log(`1.7 s with no reading after a loud one: letter ${r.letter}, signal ${r.signal}, quiet "${r.quiet}"`);
    assert.deepEqual([r.letter, r.state], ['–', 'none'], 'the stale reading is dropped');
    assert.deepEqual([r.signal, r.quiet], [null, ''], 'a feed gap never says no signal');
    // Quiet readings that go stale part way: the second restarts with the next fresh quiet one.
    await pump(null, -75);
    await settle(600);
    await stopPump();
    await settle(700);
    assert.equal((await read()).signal, null, 'a stale stretch never continues the quiet second');
  });

  await group('text', async () => {
    // A steady reading writes the cents and the frequency once, not on every arrival (invariant 6).
    await openByEngine();
    await pump(off(110, 4), -20);
    await settle(200);
    const writes = await page.evaluate(
      () =>
        new Promise((done) => {
          let n = 0;
          const obs = new MutationObserver((records) => (n += records.length));
          for (const el of [document.querySelector('.tn-cents > span'), document.querySelector('.tn-hz > span')]) {
            obs.observe(el, { childList: true, characterData: true, subtree: true });
          }
          setTimeout(() => {
            obs.disconnect();
            done(n);
          }, 1000);
        }),
    );
    const r = await read();
    console.log(`a second of one reading: ${writes} text writes, cents "${r.cents}", hz "${r.hz}"`);
    assert.deepEqual([r.cents, r.hz], ['+4', off(110, 4).toFixed(2)], 'control: the reading shows');
    assert.equal(writes, 0, 'no text write while the shown strings stand still');
    // A change still writes.
    await pump(off(110, -7), -20);
    await page.waitForFunction(() => document.querySelector('.tn-cents > span').textContent === '−7', undefined, { timeout: 1000 });
  });

  await group('held', async () => {
    // One physical Escape, one dismissal: a held Escape closes the tuner and leaves the stage view under it.
    await page.keyboard.press('b');
    await page.waitForFunction(() => document.activeElement === document.querySelector('.sv'));
    await page.keyboard.press('n');
    await waitOpen(true);
    await clearSent();
    await page.keyboard.down('Escape');
    await waitOpen(false);
    for (let i = 0; i < 4; i++) await page.keyboard.down('Escape'); // the key's repeats
    await settle();
    await page.keyboard.up('Escape');
    const left = await page.evaluate(() => ({ sv: !!document.querySelector('.sv'), sent: window.__lf.native.sent.slice() }));
    assert.deepEqual(left.sent, [{ SetTuner: false }], `one SetTuner(false), nothing more: ${JSON.stringify(left.sent)}`);
    assert.equal(left.sv, true, 'the held Escape left the stage view open');
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => !document.querySelector('.sv'));
  });

  await group('return', async () => {
    const focusIs = (sel) =>
      page
        .waitForFunction((s) => document.activeElement === document.querySelector(s), sel, { timeout: 1500 })
        .then(() => true)
        .catch(() => false);
    const active = () => page.evaluate(() => document.activeElement?.className || document.activeElement?.tagName);
    // Help open with focus in its panel: the tuner closes Help; on close the panel is gone, so the TUNER key.
    await page.click('.tool--help');
    await page.waitForFunction(() => document.activeElement?.closest('[role="dialog"]') && !document.querySelector('.tn'));
    await openByEngine();
    await emit({ events: [tunerEvent(false)] });
    await waitOpen(false);
    assert.ok(await focusIs('.tool--tuner'), `a closed popover's focus falls back to the TUNER key: on ${await active()}`);
    // A stage view opened under the tuner leaves the saved target inert: Escape returns to the view.
    await page.evaluate(() => document.querySelector('.tool--help').focus());
    await openByEngine();
    await page.keyboard.press('b');
    await page.waitForSelector('.sv');
    await page.keyboard.press('Escape');
    await waitOpen(false);
    assert.ok(await focusIs('.sv'), `an inert target falls back to the stage view: on ${await active()}`);
    // The on-screen close: a target still live gets focus back.
    await page.evaluate(() => document.querySelector('.sv-btn--exit').focus());
    await openByEngine();
    await page.click('.tn-close');
    await waitOpen(false);
    assert.ok(await focusIs('.sv-btn--exit'), `a live target gets focus back: on ${await active()}`);
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => !document.querySelector('.sv'));
  });

  await group('confirm', async () => {
    // The app's own ask, pending, when a pedal opens the tuner: the ask steps aside (not shown, no focus,
    // no key) and comes back as it was when the tuner closes.
    await page.evaluate(async () => {
      const { confirm } = await import('/src/ui/confirm/confirm-store.ts');
      window.__answer = undefined;
      void confirm({ title: 'Probe ask', body: 'Pending under the tuner.', yes: 'Yes', no: 'No' }).then((a) => (window.__answer = a));
    });
    const noBtn = () => page.evaluate(() => document.activeElement?.matches('.confirm__btn:last-child') ?? false);
    await page.waitForFunction(() => document.activeElement?.matches('.confirm__btn:last-child'), undefined, { timeout: 2000 });
    await emit({ events: [tunerEvent(true)] }); // the pedal's press, as the engine answers it
    await waitOpen(true);
    await settle(150);
    const under = await page.evaluate(() => {
      const c = document.querySelector('.confirm');
      return { shown: !!c && c.checkVisibility(), tunerFocused: document.activeElement === document.querySelector('.tn') };
    });
    console.log(`ask under the tuner: ${JSON.stringify(under)}`);
    assert.equal(under.shown, false, 'the ask is not shown while the tuner is on');
    assert.equal(under.tunerFocused, true, 'the tuner has the focus, not the ask');
    await clearSent();
    await page.keyboard.press('Space');
    assert.deepEqual(await sent(), [{ Action: 'RecDub' }], 'the transport keys reach the looper, not the ask');
    await clearSent();
    await hold(true);
    await page.keyboard.press('Escape');
    assert.deepEqual(await sent(), [{ SetTuner: false }], 'Escape closes the tuner first');
    assert.equal(await page.evaluate(() => window.__answer), undefined, 'and leaves the ask unanswered');
    await hold(false);
    await waitOpen(false);
    await page.waitForFunction(() => document.querySelector('.confirm')?.checkVisibility(), undefined, { timeout: 1500 });
    assert.ok(await page.waitForFunction(() => document.activeElement?.matches('.confirm__btn:last-child'), undefined, { timeout: 1500 }).then(() => true, () => false), 'the ask is back with focus on its no button');
    await settle(100);
    assert.equal(await noBtn(), true, 'and keeps it');
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => window.__answer === false, undefined, { timeout: 1000 });
    // An ask that arrives while the tuner is on waits under it, then takes the focus when it closes.
    await openByEngine();
    await page.evaluate(async () => {
      const { confirm } = await import('/src/ui/confirm/confirm-store.ts');
      window.__answer = undefined;
      void confirm({ title: 'Probe ask', body: 'Arrives under the tuner.', yes: 'Yes', no: 'No' }).then((a) => (window.__answer = a));
    });
    await settle(150);
    assert.equal(await page.evaluate(() => document.activeElement === document.querySelector('.tn') && !document.querySelector('.confirm')?.checkVisibility()), true, 'a new ask stays out of the tuner');
    await emit({ events: [tunerEvent(false)] });
    await waitOpen(false);
    assert.ok(await page.waitForFunction(() => document.activeElement?.matches('.confirm__btn:last-child'), undefined, { timeout: 1500 }).then(() => true, () => false), 'it takes the focus once the tuner closes');
    await page.keyboard.press('Escape');
    await page.waitForFunction(() => window.__answer === false, undefined, { timeout: 1000 });
    // Nothing had focus when the tuner opened, so the tuner's return was the TUNER key: the answered ask sends it there.
    assert.ok(
      await page.waitForFunction(() => document.activeElement === document.querySelector('.tool--tuner'), undefined, { timeout: 1000 }).then(() => true, () => false),
      'answered, focus goes where the tuner would have returned it',
    );
  });

  await group('shots', async () => {
    await openByEngine();
    // The brief's four states, and a sharp note (the sign beside the letter) in tune.
    const scenes = [
      ['no-signal', null, -78],
      ['e2-flat-18', STATES['e2-flat-18'].hz, -14],
      ['a2-sharp-4', STATES['a2-sharp-4'].hz, -16],
      ['e4-in-tune', STATES['e4-in-tune'].hz, -20],
      ['a#4-in-tune', STATES['a#4'].hz, -22],
    ];
    for (const [w, h] of SIZES) {
      await page.setViewportSize({ width: w, height: h });
      for (const [name, hz, level] of scenes) {
        await pump(hz, level);
        if (name === 'no-signal') await page.waitForFunction(() => document.querySelector('.tn')?.dataset.signal === 'none', undefined, { timeout: 3000 });
        else await settle(220);
        const m = await page.evaluate(() => {
          const t = document.querySelector('.tn');
          const box = (sel) => {
            const r = t.querySelector(sel).getBoundingClientRect();
            return { l: r.left, r: r.right, t: r.top, b: r.bottom };
          };
          const letter = t.querySelector('.tn-letter');
          const ctx = document.createElement('canvas').getContext('2d');
          ctx.font = getComputedStyle(letter).font;
          const cap = ctx.measureText('E').actualBoundingBoxAscent;
          const note = [box('.tn-letter'), box('.tn-octave'), ...(t.querySelector('.tn-note .tn-sharp') ? [box('.tn-note .tn-sharp')] : [])];
          return {
            cap,
            note: { l: Math.min(...note.map((b) => b.l)), r: Math.max(...note.map((b) => b.r)), t: Math.min(...note.map((b) => b.t)), b: Math.max(...note.map((b) => b.b)) },
            scale: { l: box('.tn-end--flat').l, r: box('.tn-end--sharp').r, t: box('.tn-track').t, b: box('.tn-track').b },
            read: box('.tn-read'),
            quiet: box('.tn-quiet'),
            muted: box('.tn-muted'),
            close: box('.tn-close'),
          };
        });
        await page.screenshot({ path: `${OUT}/${w}x${h}-${name}.png` });
        const r = await read();
        console.log(`${w}x${h} ${name}: ${r.letter}${r.sharp ? '#' : ''}${r.octave} ${r.cents} ${r.state}${r.signal ? ' no-signal' : ''}; cap ${m.cap.toFixed(0)} px = ${((m.cap / h) * 100).toFixed(1)} % of the height`);
        assert.ok(m.cap >= h / 5, `${w}x${h}: the letter's cap height ${m.cap.toFixed(1)} px is at least a fifth of ${h}`);
        const inside = (b, what) => assert.ok(b.l >= 0 && b.r <= w && b.t >= 0 && b.b <= h, `${w}x${h} ${name}: ${what} inside the window ${JSON.stringify(b)}`);
        inside(m.note, 'the note');
        inside(m.scale, 'the scale and its flat and sharp marks');
        inside(m.muted, 'the standing line');
        assert.ok(m.note.b < m.scale.t, `${w}x${h}: the note clears the scale`);
        assert.ok(m.scale.b < m.read.t && m.read.b <= m.quiet.t, `${w}x${h}: the scale, the read-out and the quiet line in order`);
        assert.ok(m.quiet.b <= m.muted.t, `${w}x${h}: the standing line clears the body (${m.quiet.b.toFixed(0)} <= ${m.muted.t.toFixed(0)})`);
        assert.ok(m.close.b < m.note.t || m.close.l > m.note.r, `${w}x${h}: the close clears the note`);
      }
    }
    console.log(`${SIZES.length * scenes.length} screenshots in ${OUT}`);
  });

  await group('cmdbar', async () => {
    const bar = async () =>
      page.evaluate(() => {
        const cmd = document.querySelector('.cmd');
        const c = cmd.getBoundingClientRect();
        const k = document.querySelector('.tool--tuner').getBoundingClientRect();
        return {
          rungs: [...cmd.classList].filter((x) => x.startsWith('cmd--stack')).join(' ') || 'one row',
          height: Math.round(c.height),
          keyInside: k.width > 0 && k.left >= c.left && k.right <= c.right && k.top >= c.top && k.bottom <= c.bottom,
          overflow: cmd.scrollWidth > cmd.clientWidth + 1,
        };
      });
    const without = () => page.evaluate(() => void (document.querySelector('.tool--tuner').style.display = 'none'));
    const restore = () => page.evaluate(() => void (document.querySelector('.tool--tuner').style.display = ''));
    for (const [w, h] of [
      [1000, 700],
      [960, 600],
    ]) {
      await page.setViewportSize({ width: w, height: h });
      for (const kbd of ['shown', 'hidden']) {
        const visible = await page.evaluate(() => !!document.querySelector('.zone--keyboard'));
        if ((kbd === 'shown') !== visible) await page.click('.tool--kbd');
        await page.mouse.move(w / 2, h - 2);
        await page.evaluate(() => document.activeElement instanceof HTMLElement && document.activeElement.blur());
        await settle(150);
        const withKey = await bar();
        await page.locator('.cmd').screenshot({ path: `${OUT}/cmd-${w}x${h}-kbd-${kbd}.png` });
        await without();
        await settle(150);
        const noKey = await bar();
        await restore();
        await settle(150);
        console.log(`${w}x${h} keyboard ${kbd}: with the key ${withKey.rungs} (${withKey.height} px), without ${noKey.rungs} (${noKey.height} px)`);
        assert.ok(withKey.keyInside, `${w}x${h}: the TUNER key sits inside the bar`);
        assert.equal(withKey.overflow, false, `${w}x${h}: nothing clips`);
      }
    }
    const visible = await page.evaluate(() => !!document.querySelector('.zone--keyboard'));
    if (!visible) await page.click('.tool--kbd');
    // Wider windows, measured only: where the bar is one row, whether the key moves its fold. The native
    // build carries two more tools (rescan, the gear) that the browser build leaves out.
    for (const [w, h] of [
      [1280, 820],
      [1440, 900],
      [1600, 900],
      [1920, 1080],
    ]) {
      await page.setViewportSize({ width: w, height: h });
      await settle(150);
      const withKey = await bar();
      await without();
      await settle(150);
      const noKey = await bar();
      await restore();
      console.log(`${w}x${h} (measured only): with the key ${withKey.rungs} (${withKey.height} px), without ${noKey.rungs} (${noKey.height} px)`);
    }
  });

  if (only) for (const name of only) assert.ok(groups.has(name), `no group is named ${name}`);
  console.log(`${ran} groups ran, ${failures.length} failed${failures.length ? `: ${failures.join(', ')}` : ''}`);
  assert.deepEqual(failures, [], 'every group passes');
});
