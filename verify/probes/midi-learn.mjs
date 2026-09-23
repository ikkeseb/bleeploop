/**
 * MIDI learn (`src/app/midi-actions.ts`) through the real Audio Settings learn row and the real MIDI
 * parser, with two virtual Web MIDI ports fed raw bytes (the midi-note-ownership pattern): a CC learned
 * onto REC/DUB survives a reload and records the selected track; the learning press and its release run
 * nothing; a momentary, a latching and a reversed-polarity footswitch each fire once per press; Esc and a
 * second LEARN click cancel a learn; unlearned CC64/1/123 still reach the input router, while CC64 learned
 * on another port with its pedal down lets that pedal go and never sustains, and a learned CC1 returns its
 * vibrato to rest; a learned note does not sound and neither its note-on nor its
 * note-off reaches the router; forgetting a binding hands its CC back to the play path. It cannot see a
 * real controller, whether WebView2 keeps a port's id across a restart or replug, or a real foot against
 * the learn window (`STATUS.md` § Play first). Run: pnpm probe midi-learn
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const LEARN = '[aria-label="Learn a MIDI control for this action"]';
const PICK = '[aria-label="Action to learn"]';

await probe(async ({ open }) => {
  const { page } = await open({
    init: (p) => p.addInitScript(() => {
      const inputs = new Map(['a', 'b'].map((id) => [id, { id, name: `Probe ${id}`, state: 'connected', onmidimessage: null }]));
      const access = { inputs, onstatechange: null };
      window.__probeMidi = access;
      window.__send = (port, messages) => {
        for (const bytes of messages) access.inputs.get(port).onmidimessage({ data: Uint8Array.from(bytes) });
      };
      Object.defineProperty(navigator, 'requestMIDIAccess', { configurable: true, value: async () => access });
    }),
  });
  const midiReady = () => page.waitForFunction(() => !!window.__probeMidi.inputs.get('a').onmidimessage);
  await midiReady();

  /** Feed raw messages to a port's handler, all in one synchronous burst (a tap is one burst). */
  const send = (port, ...messages) => page.evaluate(([p, m]) => window.__send(p, m), [port, messages]);
  const pause = (ms) => page.waitForTimeout(ms);
  const selected = () => page.evaluate(() => window.__lf.looper.selectedTrack());
  const selectTrack = (i) => page.evaluate((t) => window.__lf.looper.selectTrack(t), i);
  const learning = () => page.evaluate((s) => document.querySelector(s)?.getAttribute('aria-pressed') ?? null, LEARN);
  const openSettings = async () => {
    await page.evaluate(() => window.__lf.ui.openSettings());
    await page.waitForSelector(LEARN);
  };
  const bindingLines = () =>
    page.evaluate(() =>
      [...document.querySelectorAll('.audio-settings__binding')].map((li) =>
        [...li.children].slice(0, 2).map((el) => el.textContent.trim()).join(' | '),
      ),
    );
  /** Learn through the row: pick `action`, click LEARN, then `messages` arrive from `port` in one burst. */
  const learnVia = async (action, port, ...messages) => {
    await page.selectOption(PICK, action);
    await page.click(LEARN);
    await send(port, ...messages);
  };
  /** Poll track 1's state for up to `ms` until it is `want`; returns the last state seen. */
  const track1State = (want, ms) =>
    page.evaluate(
      ([w, limit]) =>
        new Promise((resolve) => {
          const until = performance.now() + limit;
          const tick = () => {
            const s = window.__lf.looper.stateOf(0);
            if (s === w || performance.now() > until) resolve(s);
            else setTimeout(tick, 20);
          };
          tick();
        }),
      [want, ms],
    );
  const out = {};

  // ---- learn a CC onto REC/DUB through the row, then reload ------------------------------------------
  await openSettings();
  await learnVia('recDub', 'a', [0xb0, 20, 127], [0xb0, 20, 0]);
  out.learned = { lines: await bindingLines(), learning: await learning(), track1: await track1State('RECORDING', 300) };

  await page.reload();
  await page.waitForFunction(() => '__lf' in window, undefined, { timeout: 30_000 });
  await midiReady();
  await send('a', [0xb0, 20, 127]);
  const pressed = await track1State('RECORDING', 5000);
  const armed = await page.evaluate(() => window.__lf.looper.waitingOf(0));
  await send('a', [0xb0, 20, 0]);
  await pause(300);
  out.afterReload = { pressed, armed, afterRelease: await page.evaluate(() => window.__lf.looper.stateOf(0)) };
  // Housekeeping, not a claim: nothing below reads the looper's track state.
  await page.evaluate(() => window.__lf.looper.clearAll());
  out.cleared = await track1State('EMPTY', 5000);

  await openSettings();
  out.reloadedLines = await bindingLines();

  // ---- cancel: Esc, and a second click on LEARN -------------------------------------------------------
  const beforeCancel = await bindingLines();
  await page.click(LEARN);
  const escArmed = await learning();
  await page.keyboard.press('Escape');
  out.esc = { armed: escArmed, after: await learning(), panelOpen: await page.isVisible('#lf-audio-popover') };
  await openSettings(); // a no-op unless Esc closed the panel
  await page.click(LEARN);
  const clickArmed = await learning();
  await page.click(LEARN);
  out.secondClick = { armed: clickArmed, after: await learning() };
  await send('a', [0xb0, 30, 127], [0xb0, 30, 0]);
  out.afterCancel = { before: beforeCancel, after: await bindingLines() };

  // ---- footswitches: every press fires once, counted on NEXT TRACK from track 1 ----------------------
  // Momentary: 127 on press, 0 on release. The learning tap sends both.
  await selectTrack(0);
  await learnVia('nextTrack', 'a', [0xb0, 21, 127], [0xb0, 21, 0]);
  const momentaryLearn = await selected();
  await send('a', [0xb0, 21, 127], [0xb0, 21, 0], [0xb0, 21, 127], [0xb0, 21, 0], [0xb0, 21, 127], [0xb0, 21, 0]);
  out.momentary = { learn: momentaryLearn, after3: await selected() };
  // Latching: 127 on one press, 0 on the next. The learning press sends 127 and nothing follows it.
  await selectTrack(0);
  await learnVia('nextTrack', 'a', [0xb0, 22, 127]);
  await pause(1300); // past the learn window, so no release was seen
  const latchingLearn = await selected();
  await send('a', [0xb0, 22, 0], [0xb0, 22, 127], [0xb0, 22, 0], [0xb0, 22, 127]);
  out.latching = { learn: latchingLearn, after4: await selected() };
  // Reversed polarity (0 on press, 127 on release), on port b, channel 3.
  await selectTrack(0);
  await learnVia('nextTrack', 'b', [0xb2, 23, 0], [0xb2, 23, 127]);
  await send('b', [0xb2, 23, 0], [0xb2, 23, 127], [0xb2, 23, 0], [0xb2, 23, 127]);
  out.reversed = { after2: await selected() };

  // ---- the play path: router calls and sound -------------------------------------------------------
  await page.evaluate(async () => {
    const lf = window.__lf;
    await lf.engine.start();
    lf.selectSynth(0, 'organ'); lf.setActiveSlot(0);
    await new Promise((resolve) => setTimeout(resolve, 150));
    lf.ensureActive();
    const analyser = lf.engine.ctx.createAnalyser(); analyser.fftSize = 4096;
    lf.engine.instrumentBus.connect(analyser);
    const data = new Float32Array(analyser.fftSize);
    window.__rms = () => {
      analyser.getFloatTimeDomainData(data);
      return Math.sqrt(data.reduce((sum, sample) => sum + sample * sample, 0) / data.length);
    };
    // Record every router entry the MIDI parser uses, then call through.
    const router = lf.inputRouter;
    window.__calls = [];
    for (const name of ['setSustain', 'setModulation', 'releaseSource', 'handle']) {
      const real = router[name].bind(router);
      router[name] = (...args) => {
        window.__calls.push([name, ...args.map((a) => (typeof a === 'object' && a !== null ? { ...a } : a))]);
        return real(...args);
      };
    }
  });
  const rms = () => page.evaluate(() => window.__rms());
  const takeCalls = () => page.evaluate(() => window.__calls.splice(0));
  const A0 = JSON.stringify(['a', 0]);
  const B0 = JSON.stringify(['b', 0]);
  const A5 = JSON.stringify(['a', 5]);

  // Unlearned CC64/1/123 on port a reach the router, scoped to their port and channel.
  await takeCalls();
  await send('a', [0xb0, 64, 127], [0xb0, 1, 64], [0xb0, 123, 0], [0xb0, 64, 0]);
  out.unmapped = await takeCalls();

  // CC64 learned on port b while its pedal is down: the learn lets that held pedal go, and from then on the
  // pedal runs NEXT TRACK and never sustains port b's note. The learning tap is the pedal coming up and
  // going down again, so it reads as reversed and fires as it comes up, once per press.
  await send('b', [0xb0, 64, 127]); // down before LEARN: the router holds port b's sustain
  await selectTrack(0);
  await takeCalls();
  await learnVia('nextTrack', 'b', [0xb0, 64, 0], [0xb0, 64, 127]);
  out.learnLetsGo = await takeCalls();
  await send('b', [0x90, 67, 100]); await pause(120);
  await send('b', [0x80, 67, 0]); await pause(300);
  const mappedRms = await rms();
  await send('b', [0xb0, 64, 0], [0xb0, 64, 127]);
  out.mapped64 = {
    rms: mappedRms,
    sustainCalls: (await takeCalls()).filter(([name]) => name === 'setSustain'),
    selected: await selected(),
  };
  // Control: the same note under port a's unlearned CC64 is held by the pedal.
  await send('a', [0xb0, 64, 127], [0x90, 67, 100]); await pause(120);
  await send('a', [0x80, 67, 0]); await pause(300);
  out.unmapped64Rms = await rms();
  await send('a', [0xb0, 64, 0]); await pause(300);
  // A mod wheel learned on port a, channel 6, lets its vibrato go the same way.
  await send('a', [0xb5, 1, 100]);
  await takeCalls();
  await learnVia('stopAll', 'a', [0xb5, 1, 90]);
  out.wheelLetsGo = await takeCalls();

  // A learned note (port b, channel 2) runs PREVIOUS TRACK: silent, never held, no router event either way
  // (a note-off as 0x80 and as a velocity-0 note-on).
  await selectTrack(4);
  await learnVia('prevTrack', 'b', [0x91, 60, 100], [0x81, 60, 0]);
  await takeCalls();
  await send('b', [0x91, 60, 100]); await pause(150);
  const noteRms = await rms();
  const heldDuring = await page.evaluate(() => [...window.__lf.inputRouter.held]);
  await send('b', [0x81, 60, 0], [0x91, 60, 100], [0x91, 60, 0]);
  out.learnedNote = {
    rms: noteRms,
    held: heldDuring,
    routerEvents: (await takeCalls()).filter(([name, ev]) => name === 'handle' && ev.note === 60),
    selected: await selected(),
  };
  // Control: an unlearned note on the same port and channel sounds.
  await send('b', [0x91, 62, 100]); await pause(150);
  out.unlearnedNoteRms = await rms();
  await send('b', [0x81, 62, 0]); await pause(300);

  // ---- the list, and forgetting a binding --------------------------------------------------------------
  out.lines = await bindingLines();
  const forgetCc64 = page.locator('.audio-settings__binding', { hasText: 'CC 64 · ch 1' }).locator('button');
  out.forgetLabel = (await forgetCc64.count()) === 1 ? await forgetCc64.getAttribute('aria-label') : null;
  if (out.forgetLabel !== null) await forgetCc64.click();
  await takeCalls();
  await send('b', [0xb0, 64, 127], [0xb0, 64, 0]);
  out.forgotten = { sustainCalls: (await takeCalls()).filter(([name]) => name === 'setSustain'), lines: await bindingLines() };

  console.log(JSON.stringify(out));

  // Every check runs and reports, so a red run names each claim it broke.
  const failures = [];
  let checks = 0;
  const check = (claim) => {
    checks++;
    try {
      claim();
    } catch (error) {
      failures.push(error.message.split('\n')[0]);
    }
  };
  check(() => assert.deepEqual(out.learned, {
    lines: ['Record / overdub | CC 20 · ch 1 · momentary'],
    learning: 'false',
    track1: 'EMPTY',
  }, 'a learning tap binds the CC as momentary, ends the learn and runs nothing'));
  check(() => assert.deepEqual(out.afterReload, { pressed: 'RECORDING', armed: true, afterRelease: 'RECORDING' },
    'after a reload the learned CC records the selected track, and its release runs nothing'));
  check(() => assert.deepEqual(out.reloadedLines, ['Record / overdub | CC 20 · ch 1 · momentary'], 'the list survives a reload'));
  check(() => assert.deepEqual(out.esc, { armed: 'true', after: 'false', panelOpen: true }, 'Esc cancels a learn and leaves the panel open'));
  check(() => assert.deepEqual(out.secondClick, { armed: 'true', after: 'false' }, 'a second click on LEARN cancels it'));
  check(() => assert.deepEqual(out.afterCancel.after, out.afterCancel.before, 'a CC after a cancelled learn binds nothing'));
  check(() => assert.deepEqual(out.momentary, { learn: 0, after3: 3 }, 'three momentary presses fire three times, the learning tap none'));
  check(() => assert.deepEqual(out.latching, { learn: 0, after4: 4 }, 'four latching presses fire four times, the learning press none'));
  check(() => assert.deepEqual(out.reversed, { after2: 2 }, 'two reversed-polarity presses fire twice'));
  check(() => assert.deepEqual(out.unmapped, [
    ['setSustain', true, A0],
    ['setModulation', 64 / 127, A0],
    ['releaseSource', A0],
    ['setSustain', false, A0],
  ], 'unlearned CC64/1/123 reach the router as before'));
  check(() => assert.deepEqual(out.learnLetsGo, [['setSustain', false, B0]], 'learning CC64 lets go of the pedal held down on its port'));
  check(() => assert.ok(out.mapped64.rms < 1e-5, `a CC learned onto 64 must not sustain (rms ${out.mapped64.rms})`));
  check(() => assert.deepEqual(out.mapped64.sustainCalls, [], 'a CC learned onto 64 never reaches setSustain'));
  check(() => assert.equal(out.mapped64.selected, 1, 'the learned CC64 press ran its action once'));
  check(() => assert.ok(out.unmapped64Rms > 0.01, `the other port's unlearned CC64 still sustains (rms ${out.unmapped64Rms})`));
  check(() => assert.deepEqual(out.wheelLetsGo, [['setModulation', 0, A5]], 'learning CC1 returns its vibrato to rest'));
  check(() => assert.ok(out.learnedNote.rms < 1e-5, `a learned note must not sound (rms ${out.learnedNote.rms})`));
  check(() => assert.deepEqual(out.learnedNote.held, [], 'a learned note is never held'));
  check(() => assert.deepEqual(out.learnedNote.routerEvents, [], 'neither the learned note-on nor its note-off reaches the router'));
  check(() => assert.equal(out.learnedNote.selected, 2, 'two presses of the learned note stepped back twice'));
  check(() => assert.ok(out.unlearnedNoteRms > 0.01, `an unlearned note on the same channel sounds (rms ${out.unlearnedNoteRms})`));
  check(() => assert.deepEqual(out.lines, [
    'Record / overdub | CC 20 · ch 1 · momentary',
    'Next track | CC 21 · ch 1 · momentary',
    'Next track | CC 22 · ch 1 · latching',
    'Next track | CC 23 · ch 3 · momentary',
    'Next track | CC 64 · ch 1 · momentary',
    'Stop all | CC 1 · ch 6 · latching',
    'Previous track | note 60 · ch 2 · momentary',
  ], 'the list names each binding and the pedal kind it was read as'));
  check(() => assert.equal(out.forgetLabel, 'Forget Next track on CC 64 · ch 1, Probe b', 'the ✕ names the binding and its port'));
  check(() => assert.deepEqual(out.forgotten, {
    sustainCalls: [['setSustain', true, B0], ['setSustain', false, B0]],
    lines: out.lines.filter((line) => !line.includes('CC 64 · ch 1')),
  }, 'a forgotten CC64 leaves the list and sustains again'));
  for (const failure of failures) console.log(`FAIL ${failure}`);
  assert.equal(failures.length, 0, `${failures.length} of ${checks} checks failed`);
});
