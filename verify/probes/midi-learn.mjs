/**
 * MIDI learn (`src/app/midi-actions.ts`) through the real Audio Settings learn row and the real MIDI
 * parser, with two virtual Web MIDI ports fed raw bytes (the midi-note-ownership pattern): a CC learned
 * onto REC/DUB survives a reload and records the selected track; the learning press and its release run
 * nothing; a momentary, a latching and a reversed-polarity footswitch each fire once per press, on the
 * press (the selected track is read after every message); Esc, a second LEARN click and closing the panel
 * cancel a learn; a channel-mode CC (120–127) or a note-off never becomes a learn; unlearned CC64/1/123
 * still reach the input router, while CC64 learned on another port with its pedal down lets that pedal go
 * and never sustains, and a learned CC1 hands the vibrato back to the wheel moved before it; a learned note
 * does not sound and neither its note-on nor its note-off reaches the router; forgetting a binding hands
 * its CC back to the play path. A latching learn's wait for a release ends on its 10 s timer (fired by the
 * probe, not slept) and clears the hint. The foot vocabulary (`src/app/actions.ts`): a learning press held
 * 1.2 s reads as momentary and fires once per tap after it, and so does one released only after the panel
 * closed or after LEARN was pressed again (that release is never the new learn's press); the line's kind
 * switch changes how it fires; tap
 * tempo, CLICK, END STOP and FIXED work their controls and IN FX is refused with a cue on the web path;
 * track actions aimed at a named track (REC/DUB, PLAY/STOP, MUTE, REV, COPY) act there, only REC/DUB
 * moving the selection; HOLD starts an overdub on 127 and ends it on 0, one REC/DUB each; a latching
 * pedal cannot HOLD; CLEAR confirms per target; a binding saved before targets loads as the selected
 * track's; in engine mode (the web engine fake) each press sends its control's command for its lane.
 * logs/midi-learn/bindings.png shows the row and a long list for the eye. It cannot see a real
 * controller, whether WebView2 keeps a port's id across a restart or replug, a real foot against the
 * learn read, or the native engine answering (`STATUS.md` § Play first).
 * Run: pnpm probe midi-learn
 */
import assert from 'node:assert/strict';
import { mkdir } from 'node:fs/promises';
import { probe } from '../harness/probe.ts';

const LEARN = '[aria-label="Learn a MIDI control for this action"]';
const PICK = '[aria-label="Action to learn"]';

/** Two virtual Web MIDI ports, `a` and `b`, and `window.__send(port, messages)` to feed them raw bytes. */
const midiFake = () => {
  const inputs = new Map(['a', 'b'].map((id) => [id, { id, name: `Probe ${id}`, state: 'connected', onmidimessage: null }]));
  const access = { inputs, onstatechange: null };
  window.__probeMidi = access;
  window.__send = (port, messages) => {
    for (const bytes of messages) access.inputs.get(port).onmidimessage({ data: Uint8Array.from(bytes) });
  };
  Object.defineProperty(navigator, 'requestMIDIAccess', { configurable: true, value: async () => access });
};

await probe(async ({ open }) => {
  const { page } = await open({ init: (p) => p.addInitScript(midiFake) });
  const midiReady = () => page.waitForFunction(() => !!window.__probeMidi.inputs.get('a').onmidimessage);
  await midiReady();

  /** Feed raw messages to a port's handler, all in one synchronous burst (a tap is one burst). */
  const send = (port, ...messages) => page.evaluate(([p, m]) => window.__send(p, m), [port, messages]);
  const pause = (ms) => page.waitForTimeout(ms);
  const selected = () => page.evaluate(() => window.__lf.looper.selectedTrack());
  /** Send each message on its own and read the selected track after each, so a press and a release that
   * fire the same number of times still tell apart. */
  const stepThrough = async (port, ...messages) => {
    const seen = [];
    for (const message of messages) {
      await send(port, message);
      seen.push(await selected());
    }
    return seen;
  };
  const selectTrack = (i) => page.evaluate((t) => window.__lf.looper.selectTrack(t), i);
  const learning = () => page.evaluate((s) => document.querySelector(s)?.getAttribute('aria-pressed') ?? null, LEARN);
  const openSettings = async () => {
    await page.evaluate(() => window.__lf.ui.openSettings());
    await page.waitForSelector(LEARN);
  };
  /** Each binding line as "action | message | pedal kind" (the kind is its switch's legend). */
  const bindingLines = () =>
    page.evaluate(() =>
      [...document.querySelectorAll('.audio-settings__binding')].map((li) =>
        [...li.children].slice(0, 3).map((el) => el.textContent.trim()).join(' | '),
      ),
    );
  /** Learn through the row: pick `action`, click LEARN, then `messages` arrive from `port` in one burst. */
  const learnVia = async (action, port, ...messages) => {
    await page.selectOption(PICK, action);
    await page.click(LEARN);
    await send(port, ...messages);
  };
  /** Run `body` (a learn) catching the release-wait timers it starts (RELEASE_WAIT_MS, 10 s, in
   * `midi-actions.ts`), the monitor-generation pattern; returns the function that fires them now and
   * resolves to how many it fired, so the 10 s expiry is checked without sleeping. */
  const catchWait = async (body) => {
    await page.evaluate(() => {
      const real = window.setTimeout;
      window.__realSetTimeout = real;
      window.__waitTimers = [];
      window.setTimeout = (fn, delay, ...args) => {
        if (delay === 10_000) window.__waitTimers.push(fn);
        return real(fn, delay, ...args);
      };
    });
    try {
      await body();
    } finally {
      await page.evaluate(() => void (window.setTimeout = window.__realSetTimeout));
    }
    return () =>
      page.evaluate(() => {
        const due = window.__waitTimers.splice(0);
        for (const fn of due) fn();
        return due.length;
      });
  };
  const HINT = '.audio-settings__hint[role="status"]';
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
  // Closing the panel while LEARN listens cancels it, so a later press cannot bind out of sight.
  await page.click(LEARN);
  const closeArmed = await learning();
  await page.evaluate(() => window.__lf.ui.closeSettings());
  await page.waitForSelector(LEARN, { state: 'detached' });
  await send('a', [0xb0, 31, 127], [0xb0, 31, 0]);
  await openSettings();
  out.closeCancels = { armed: closeArmed, after: await learning(), lines: await bindingLines() };

  // ---- footswitches: every press fires once, on the press, counted on NEXT TRACK from track 1 ----------
  // Momentary: 127 on press, 0 on release. The learning tap sends both.
  await selectTrack(0);
  await learnVia('nextTrack', 'a', [0xb0, 21, 127], [0xb0, 21, 0]);
  const momentaryLearn = await selected();
  const momentarySteps = await stepThrough('a', [0xb0, 21, 127], [0xb0, 21, 0], [0xb0, 21, 127], [0xb0, 21, 0], [0xb0, 21, 127], [0xb0, 21, 0]);
  out.momentary = { learn: momentaryLearn, steps: momentarySteps };
  // Latching: 127 on one press, 0 on the next. The learning press sends 127 and nothing follows it; the
  // panel closing leaves the wait for a release running (the hint is still up), and its 10 s timer ends
  // it and the hint.
  await selectTrack(0);
  const expireLatching = await catchWait(() => learnVia('nextTrack', 'a', [0xb0, 22, 127]));
  const latchingHint = await page.locator(HINT).textContent({ timeout: 3000 }).catch(() => null);
  await page.evaluate(() => window.__lf.ui.closeSettings());
  await openSettings();
  const hintAfterClose = await page.locator(HINT).count();
  const waitTimers = await expireLatching();
  const hintAfterWait = await page.locator(HINT).count();
  const latchingLearn = await selected();
  const latchingSteps = await stepThrough('a', [0xb0, 22, 0], [0xb0, 22, 127], [0xb0, 22, 0], [0xb0, 22, 127]);
  out.latching = { learn: latchingLearn, steps: latchingSteps, hint: latchingHint, hintAfterClose, waitTimers, hintAfterWait };
  // Reversed polarity (0 on press, 127 on release), on port b, channel 3.
  await selectTrack(0);
  await learnVia('nextTrack', 'b', [0xb2, 23, 0], [0xb2, 23, 127]);
  out.reversed = { steps: await stepThrough('b', [0xb2, 23, 0], [0xb2, 23, 127], [0xb2, 23, 0], [0xb2, 23, 127]) };

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

  // Unlearned CC64/1/123 on port a reach the router, scoped to their port and channel.
  await takeCalls();
  await send('a', [0xb0, 64, 127], [0xb0, 1, 64], [0xb0, 123, 0], [0xb0, 64, 0]);
  out.unmapped = await takeCalls();

  // While LEARN listens, a channel-mode CC (120 all sound off, 123 all notes off) and a note-off (as 0x80
  // and as a velocity-0 note-on) are not learned: each reaches the router as unlearned traffic, and the
  // learn keeps listening. The second click on LEARN cancels it.
  const linesBeforeRefusals = await bindingLines();
  await page.selectOption(PICK, 'stopAll');
  await page.click(LEARN);
  await takeCalls();
  await send('a', [0xb0, 123, 0], [0xb0, 120, 0]);
  out.modeCcWhileLearning = { calls: await takeCalls(), learning: await learning(), lines: await bindingLines() };
  await page.click(LEARN);
  await page.click(LEARN);
  await takeCalls();
  await send('a', [0x80, 61, 0], [0x90, 61, 0]);
  out.noteOffWhileLearning = { calls: await takeCalls(), learning: await learning(), lines: await bindingLines() };
  await page.click(LEARN);

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
  const mappedSteps = await stepThrough('b', [0xb0, 64, 0], [0xb0, 64, 127]);
  out.mapped64 = {
    rms: mappedRms,
    sustainCalls: (await takeCalls()).filter(([name]) => name === 'setSustain'),
    steps: mappedSteps,
  };
  // Control: the same note under port a's unlearned CC64 is held by the pedal.
  await send('a', [0xb0, 64, 127], [0x90, 67, 100]); await pause(120);
  await send('a', [0x80, 67, 0]); await pause(300);
  out.unmapped64Rms = await rms();
  await send('a', [0xb0, 64, 0]); await pause(300);
  // A mod wheel learned on port a, channel 6, lets its vibrato go the way an unplug does: the depth falls
  // back to the wheel moved before it (port b, channel 1), not to 0 over it. The depth read is the router's
  // last-moved-wheel result, the value it hands the active synth (a TS-private field, read for the probe).
  const modDepth = () => page.evaluate(() => window.__lf.inputRouter.modDepth);
  await send('b', [0xb0, 1, 50]);
  await send('a', [0xb5, 1, 100]);
  const wheelBefore = await modDepth();
  await learnVia('stopAll', 'a', [0xb5, 1, 90]);
  out.wheelLetsGo = { before: wheelBefore, after: await modDepth() };
  await send('b', [0xb0, 1, 0]); // housekeeping: the vibrato at rest for the notes below

  // A learned note (port b, channel 2) runs PREVIOUS TRACK on its note-on: silent, never held, no router
  // event either way (a note-off as 0x80 and as a velocity-0 note-on).
  await selectTrack(4);
  await learnVia('prevTrack', 'b', [0x91, 60, 100], [0x81, 60, 0]);
  await takeCalls();
  await send('b', [0x91, 60, 100]); await pause(150);
  const noteRms = await rms();
  const heldDuring = await page.evaluate(() => [...window.__lf.inputRouter.held]);
  const selectedOnPress = await selected();
  await send('b', [0x81, 60, 0], [0x91, 60, 100], [0x91, 60, 0]);
  out.learnedNote = {
    rms: noteRms,
    held: heldDuring,
    routerEvents: (await takeCalls()).filter(([name, ev]) => name === 'handle' && ev.note === 60),
    selected: [selectedOnPress, await selected()],
  };
  // Control: an unlearned note on the same port and channel sounds.
  await send('b', [0x91, 62, 100]); await pause(150);
  out.unlearnedNoteRms = await rms();
  await send('b', [0x81, 62, 0]); await pause(300);

  // ---- the list, and forgetting a binding --------------------------------------------------------------
  out.lines = await bindingLines();
  const forgetCc64 = page.locator('.audio-settings__binding', { hasText: 'CC 64 · ch 1' }).locator('.audio-settings__binding-clear');
  out.forgetLabel = (await forgetCc64.count()) === 1 ? await forgetCc64.getAttribute('aria-label') : null;
  if (out.forgetLabel !== null) await forgetCc64.click();
  await takeCalls();
  await send('b', [0xb0, 64, 127], [0xb0, 64, 0]);
  out.forgotten = { sustainCalls: (await takeCalls()).filter(([name]) => name === 'setSustain'), lines: await bindingLines() };

  // ════ The foot vocabulary: lane targets, the global toggles, HOLD, the learn read, the kind switch ════
  const TARGET = '[aria-label="Track the action acts on"]';
  /** Run `body` into out[name]. A step that throws (a control this build lacks) records its error, so the
   * checks below name it instead of the run stopping there. */
  const scenario = async (name, body) => {
    try {
      out[name] = await body();
    } catch (error) {
      out[name] = { error: String(error?.message ?? error).split('\n')[0] };
    }
  };
  /** Learn through the row onto `action`, a track action on `target` ('' the selected track, '0'–'4' a
   * track; null leaves the target row alone), then `messages` arrive from `port` in one burst. */
  const learnOn = async (action, target, port, ...messages) => {
    await page.selectOption(PICK, action, { timeout: 3000 });
    if (target !== null) await page.selectOption(TARGET, target, { timeout: 3000 });
    await page.click(LEARN);
    await send(port, ...messages);
  };
  /** A tap: press and release in one burst. */
  const tap = (cc) => send('a', [0xb0, cc, 127], [0xb0, cc, 0]);
  /** Poll lane `i`'s state for up to `ms` until it is `want`; returns the last state seen. */
  const waitState = (i, want, ms = 5000) =>
    page.evaluate(
      ([t, w, limit]) =>
        new Promise((resolve) => {
          const until = performance.now() + limit;
          const tick = () => {
            const s = window.__lf.looper.stateOf(t);
            if (s === w || performance.now() > until) resolve(s);
            else setTimeout(tick, 20);
          };
          tick();
        }),
      [i, want, ms],
    );
  const laneState = (i) => page.evaluate((t) => window.__lf.looper.stateOf(t), i);
  const pressedOf = (name) =>
    page.evaluate((n) => document.querySelector(`[aria-label="${n}"]`)?.getAttribute('aria-pressed') ?? null, name);
  /** The cue on lane `i`'s well, or '' when it shows none. */
  const cueOn = (i) =>
    page.evaluate((t) => document.querySelectorAll('.lp-lane')[t]?.querySelector('.lp-lane__wellmsg.is-cue')?.textContent.trim() ?? '', i);
  const lineOf = async (source) => (await bindingLines()).find((line) => line.includes(`${source} ·`)) ?? null;
  const bindingRow = (source) => page.locator('.audio-settings__binding', { hasText: `${source} · ch 1` });

  // A learning press held 1.2 s before its release: read as momentary, and neither message runs anything.
  // Then each tap fires once, on the press.
  await scenario('heldLearn', async () => {
    await selectTrack(0);
    await learnOn('nextTrack', null, 'a', [0xb0, 60, 127]);
    await pause(1200);
    await send('a', [0xb0, 60, 0]);
    const afterLearn = await selected();
    const steps = await stepThrough('a', [0xb0, 60, 127], [0xb0, 60, 0], [0xb0, 60, 127], [0xb0, 60, 0]);
    return { afterLearn, line: await lineOf('CC 60'), steps };
  });

  // A momentary pedal still down when the panel closes: its release, out of sight, is still read as its
  // release (momentary) and runs nothing; each later tap fires once.
  await scenario('closeWhileHeld', async () => {
    await selectTrack(0);
    await learnOn('nextTrack', null, 'a', [0xb0, 61, 127]);
    await page.evaluate(() => window.__lf.ui.closeSettings());
    await page.waitForSelector(LEARN, { state: 'detached' });
    await send('a', [0xb0, 61, 0]);
    const afterRelease = await selected();
    const steps = await stepThrough('a', [0xb0, 61, 127], [0xb0, 61, 0], [0xb0, 61, 127], [0xb0, 61, 0]);
    await openSettings();
    return { afterRelease, line: await lineOf('CC 61'), steps };
  });

  // LEARN pressed for another action while the learned pedal is still down: its release is its release
  // (momentary, runs nothing), not the new learn's press; the new learn keeps listening for the next tap.
  await scenario('learnWhileHeld', async () => {
    await selectTrack(0);
    await learnOn('nextTrack', null, 'a', [0xb0, 62, 127]);
    await page.selectOption(PICK, 'prevTrack', { timeout: 3000 });
    await page.click(LEARN);
    await send('a', [0xb0, 62, 0]);
    const stillLearning = await learning();
    await send('a', [0xb0, 63, 127], [0xb0, 63, 0]);
    return { stillLearning, selected: await selected(), lines: [await lineOf('CC 62'), await lineOf('CC 63')] };
  });

  // The kind switch on the held learn's line (CC 60): latching fires on the release too, momentary again only on the press.
  await scenario('kindSwitch', async () => {
    const kind = bindingRow('CC 60').locator('.audio-settings__chip').first();
    await selectTrack(0);
    await kind.click({ timeout: 3000 });
    const latching = { kind: await kind.textContent(), steps: await stepThrough('a', [0xb0, 60, 127], [0xb0, 60, 0]) };
    await selectTrack(0);
    await kind.click();
    const momentary = { kind: await kind.textContent(), steps: await stepThrough('a', [0xb0, 60, 127], [0xb0, 60, 0]) };
    return { latching, momentary };
  });

  // The new global actions on an empty looper, each against its command-bar control. IN FX has no input
  // sends on the web path: refused, with the reason on the selected lane.
  await scenario('globals', async () => {
    await page.evaluate(() => window.__lf.looper.clearAll());
    await selectTrack(1);
    for (const [cc, action] of [[80, 'tapTempo'], [81, 'clickToggle'], [82, 'endStopToggle'], [83, 'fixedToggle'], [84, 'inFxEcho'], [85, 'inFxReverb']]) {
      await learnOn(action, null, 'a', [0xb0, cc, 127], [0xb0, cc, 0]);
    }
    const bpmNum = () => page.evaluate(() => Number(document.querySelector('.transport__bpm-num').textContent));
    const bpmBefore = await bpmNum();
    for (let k = 0; k < 4; k++) {
      await tap(80);
      await pause(400);
    }
    const bpm = await bpmNum();
    const toggles = {};
    for (const [cc, name] of [[81, 'Metronome click'], [82, 'Stop playing loops at loop end'], [83, 'Fixed take length']]) {
      const seen = [await pressedOf(name)];
      await tap(cc);
      seen.push(await pressedOf(name));
      await tap(cc);
      seen.push(await pressedOf(name));
      toggles[name] = seen;
    }
    await tap(84);
    const echoCue = await cueOn(1);
    await tap(85);
    const reverbCue = await cueOn(1);
    return { bpmBefore, bpmMoved: bpm !== bpmBefore && bpm >= 130 && bpm <= 165, toggles, echoCue, reverbCue };
  });

  // Two playing loops (2 bars at 240 BPM: a 2 s loop) on tracks 1 and 2, then presses aimed at a track.
  await scenario('targets', async () => {
    await page.evaluate(async () => {
      const lf = window.__lf;
      const { defaultFxStates } = await import('/src/audio/fx/fx.ts');
      lf.looper.clearAll();
      const bpm = 240, bars = 2;
      const frames = Math.round(lf.engine.ctx.sampleRate * (60 / bpm) * 4 * bars);
      const tracks = [0, 1].map((index) => {
        const pcm = new Float32Array(frames);
        for (let f = 0; f < frames; f++) pcm[f] = 0.2 * Math.sin(f / (40 + index * 9));
        return { index, pcm, volume: 1, muted: false, reversed: false, state: 'PLAYING', fx: defaultFxStates() };
      });
      await lf.looper.loadSession({ bpm, bars, masterLengthFrames: frames, tracks });
    });
    await waitState(1, 'PLAYING');
    for (const [cc, action, target] of [[70, 'recDub', '2'], [71, 'playStop', '1'], [72, 'mute', '0'], [73, 'reverse', '1'], [74, 'copy', '0']]) {
      await learnOn(action, target, 'a', [0xb0, cc, 127], [0xb0, cc, 0]);
    }
    const r = { lines: [await lineOf('CC 70'), await lineOf('CC 71'), await lineOf('CC 72'), await lineOf('CC 73'), await lineOf('CC 74')] };
    await selectTrack(0);
    await tap(80);
    r.tapLocked = await cueOn(0);

    await tap(71);
    r.playStop = { state: await waitState(1, 'STOPPED'), selected: await selected() };
    await tap(71);
    r.playStop.again = await waitState(1, 'PLAYING');

    await tap(72);
    r.mute = { muted: await page.evaluate(() => window.__lf.looper.trackMuted(0)), selected: await selected() };
    await tap(72);
    r.mute.again = await page.evaluate(() => window.__lf.looper.trackMuted(0));

    await tap(73);
    r.reverse = {
      reversed: await page
        .waitForFunction(() => window.__lf.looper.trackInfo(1).reversed, undefined, { timeout: 5000 })
        .then(() => true, () => false),
      selected: await selected(),
    };

    await send('a', [0xb0, 70, 127]);
    r.recDub = { state: await waitState(2, 'RECORDING'), selected: await selected() };
    await send('a', [0xb0, 70, 0]);
    await pause(200);
    r.recDub.afterRelease = await laneState(2);
    await page.evaluate(() => window.__lf.looper.stop(2)); // housekeeping: lane 3 armed, back to EMPTY
    await waitState(2, 'EMPTY');
    await selectTrack(0);

    await tap(74);
    r.copy = {
      filled: await page
        .waitForFunction(() => window.__lf.looper.stateOf(2) !== 'EMPTY', undefined, { timeout: 5000 })
        .then(() => true, () => false),
      selected: await selected(),
    };
    return r;
  });

  // HOLD on a momentary REC/DUB pedal aimed at track 1: 127 starts the overdub, 0 ends it. Every REC/DUB
  // call is logged with the lane's state when it came.
  await scenario('hold', async () => {
    await learnOn('recDub', '0', 'a', [0xb0, 75, 127], [0xb0, 75, 0]);
    const hold = bindingRow('CC 75').getByRole('button', { name: /^Hold to record/ });
    await hold.click({ timeout: 3000 });
    const on = await hold.getAttribute('aria-pressed');
    await page.evaluate(() => {
      const L = window.__lf.looper;
      const real = L.recDub;
      window.__recDubs = [];
      L.recDub = (i) => {
        window.__recDubs.push([i, L.stateOf(i)]);
        return real(i);
      };
      window.__restoreRecDub = () => {
        L.recDub = real;
        return window.__recDubs;
      };
    });
    await send('a', [0xb0, 75, 127]);
    const down = await waitState(0, 'OVERDUBBING');
    await pause(300);
    await send('a', [0xb0, 75, 0]);
    const up = await waitState(0, 'PLAYING');
    await pause(300);
    return { on, down, up, calls: await page.evaluate(() => window.__restoreRecDub()) };
  });

  // A latching REC/DUB pedal has no HOLD: its switch is disabled and the store refuses it. The HOLD pedal
  // switched to latching loses its HOLD.
  await scenario('latchingHold', async () => {
    const expire = await catchWait(() => learnOn('recDub', '', 'a', [0xb0, 76, 127]));
    await expire();
    const holdOf = (source) => bindingRow(source).getByRole('button', { name: /^Hold to record/ });
    const read = async (source) => ({
      kind: await bindingRow(source).locator('.audio-settings__chip').first().textContent({ timeout: 3000 }),
      disabled: await holdOf(source).isDisabled({ timeout: 3000 }),
      pressed: await holdOf(source).getAttribute('aria-pressed'),
    });
    const latching = await read('CC 76');
    await page.evaluate(async () => {
      const m = await import('/src/app/midi-actions.ts');
      m.setHold(m.bindings().find((b) => b.number === 76), true);
    });
    latching.afterSetHold = await holdOf('CC 76').getAttribute('aria-pressed');
    await bindingRow('CC 75').locator('.audio-settings__chip').first().click();
    return { latching, switched: await read('CC 75') };
  });

  // CLEAR confirms per target: track 2 then track 3 arms each, the second track-3 press clears track 3.
  await scenario('clearPerTarget', async () => {
    await learnOn('clear', '1', 'a', [0xb0, 77, 127], [0xb0, 77, 0]);
    await learnOn('clear', '2', 'a', [0xb0, 78, 127], [0xb0, 78, 0]);
    await tap(77);
    await tap(78);
    const afterOneEach = [await laneState(1), await laneState(2)];
    await tap(78);
    return { afterOneEach, track3: await waitState(2, 'EMPTY'), track2: await laneState(1) };
  });

  // For the eye: the learn row and a long bindings list (targets, kinds, HOLD).
  await mkdir('logs/midi-learn', { recursive: true });
  await page.selectOption(PICK, 'recDub');
  await page.locator('#lf-audio-popover').screenshot({ path: 'logs/midi-learn/bindings.png' });

  // A binding saved before targets (no target, no hold) loads as the selected track's.
  await scenario('oldFormat', async () => {
    await page.evaluate(() => {
      const list = JSON.parse(localStorage.getItem('lf.midiLearn') ?? '[]');
      list.push({ port: 'a', portName: 'Probe a', channel: 0, kind: 'cc', number: 90, action: 'playStop', pressHigh: true, momentary: true });
      localStorage.setItem('lf.midiLearn', JSON.stringify(list));
    });
    await page.reload();
    await page.waitForFunction(() => '__lf' in window, undefined, { timeout: 30_000 });
    await midiReady();
    await openSettings();
    const stored = await page.evaluate(async () => {
      const b = (await import('/src/app/midi-actions.ts')).bindings().find((x) => x.number === 90);
      return b ? { target: b.target, hold: b.hold } : null;
    });
    await selectTrack(3);
    await tap(90);
    return { line: await lineOf('CC 90'), stored, cue: await cueOn(3) };
  });

  // Engine mode, on the web engine fake: each press sends what its lane's control sends for that lane.
  await scenario('engine', async () => {
    const { page: ep } = await open({
      init: async (p) => {
        await p.addInitScript(() => {
          window.__lfEngineFake = true;
        });
        await p.addInitScript(midiFake);
      },
    });
    await ep.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await ep.waitForFunction(() => !!window.__probeMidi.inputs.get('a').onmidimessage);
    const info = (state, extra = {}) => ({
      state, length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, retakePass: 0, ...extra,
    });
    const playing = info('Playing', { length: 96000, canReverse: true });
    const lanes = [playing, playing, info('Empty'), info('Empty'), info('Empty')];
    await ep.evaluate((ls) => window.__lf.native.emit({
      seq: 1,
      reset: true,
      settings: [],
      events: [
        ...ls.map((i, lane) => ({ Lane: { frame: 0, lane, info: i } })),
        { Transport: { frame: 0, master: 96000, bpm: 120, locked: true } },
        { Selected: { frame: 0, lane: 0 } },
      ],
      anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
      meter: { peak: 0, clip: false },
    }), lanes);
    await ep.evaluate(() => window.__lf.ui.openSettings());
    await ep.waitForSelector(LEARN);
    const bindings = [
      [10, 'recDub', '2'], [11, 'playStop', '1'], [12, 'mute', '0'], [13, 'reverse', '1'], [14, 'copy', '0'],
      [15, 'playStop', ''], [16, 'clickToggle', null], [17, 'endStopToggle', null], [18, 'fixedToggle', null],
      [19, 'inFxEcho', null], [20, 'inFxReverb', null], [21, 'mute', '3'], [22, 'recDub', '0'],
    ];
    for (const [cc, action, target] of bindings) {
      await ep.selectOption(PICK, action, { timeout: 3000 });
      if (target !== null) await ep.selectOption(TARGET, target, { timeout: 3000 });
      await ep.click(LEARN);
      await ep.evaluate((c) => window.__send('a', [[0xb0, c, 127], [0xb0, c, 0]]), cc);
    }
    await ep.locator('.audio-settings__binding', { hasText: 'CC 22 · ch 1' }).getByRole('button', { name: /^Hold to record/ }).click({ timeout: 3000 });
    await ep.evaluate(() => window.__lf.ui.closeSettings());
    const sentBy = {};
    for (const [cc] of bindings.filter(([c]) => c !== 22)) {
      await ep.evaluate(() => void (window.__lf.native.sent.length = 0));
      await ep.evaluate((c) => window.__send('a', [[0xb0, c, 127], [0xb0, c, 0]]), cc);
      await ep.waitForTimeout(60);
      sentBy[cc] = await ep.evaluate(() => window.__lf.native.sent.slice());
    }
    const cue4 = await ep.evaluate(() => document.querySelectorAll('.lp-lane')[3]?.querySelector('.lp-lane__wellmsg.is-cue')?.textContent.trim() ?? '');
    // HOLD on track 1: the press, then the release while the feed says the lane overdubs; then a press
    // whose capture never shows (a take that closed itself) and a release that sends nothing.
    let seq = 1;
    const holdStep = async (value, feedState) => {
      await ep.evaluate(() => void (window.__lf.native.sent.length = 0));
      if (feedState) {
        const frame = { seq: ++seq, reset: false, events: [{ Lane: { frame: 0, lane: 0, info: info(feedState, { length: 96000, canReverse: true }) } }] };
        await ep.evaluate((f) => window.__lf.native.emit(f), frame);
      }
      await ep.evaluate((v) => window.__send('a', [[0xb0, 22, v]]), value);
      await ep.waitForTimeout(60);
      return ep.evaluate(() => window.__lf.native.sent.slice());
    };
    const hold = [await holdStep(127), await holdStep(0, 'Overdubbing'), await holdStep(127, 'Playing'), await holdStep(0)];
    const pressed = (name) => ep.evaluate((n) => document.querySelector(`[aria-label="${n}"]`)?.getAttribute('aria-pressed') ?? null, name);
    return {
      hold,
      sent: sentBy,
      controls: {
        click: await pressed('Metronome click'),
        endStop: await pressed('Stop playing loops at loop end'),
        fixed: await pressed('Fixed take length'),
        inFx: await ep.evaluate(() => document.querySelector('.infx__pill')?.classList.contains('is-on') ?? null),
      },
      cue4,
    };
  });

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
    lines: ['Record / overdub | CC 20 · ch 1 | momentary'],
    learning: 'false',
    track1: 'EMPTY',
  }, 'a learning tap binds the CC as momentary, ends the learn and runs nothing'));
  check(() => assert.deepEqual(out.afterReload, { pressed: 'RECORDING', armed: true, afterRelease: 'RECORDING' },
    'after a reload the learned CC records the selected track, and its release runs nothing'));
  check(() => assert.deepEqual(out.reloadedLines, ['Record / overdub | CC 20 · ch 1 | momentary'], 'the list survives a reload'));
  check(() => assert.deepEqual(out.esc, { armed: 'true', after: 'false', panelOpen: true }, 'Esc cancels a learn and leaves the panel open'));
  check(() => assert.deepEqual(out.secondClick, { armed: 'true', after: 'false' }, 'a second click on LEARN cancels it'));
  check(() => assert.deepEqual(out.afterCancel.after, out.afterCancel.before, 'a CC after a cancelled learn binds nothing'));
  check(() => assert.deepEqual(out.closeCancels, { armed: 'true', after: 'false', lines: out.afterCancel.before },
    'closing the panel cancels a learn, and a CC after it binds nothing'));
  // One entry per message: the selected track after it. A press steps once; its release steps nothing.
  check(() => assert.deepEqual(out.momentary, { learn: 0, steps: [1, 1, 2, 2, 3, 3] },
    'each momentary press fires once, on the press; the learning tap none'));
  check(() => assert.deepEqual(out.latching, {
    learn: 0,
    steps: [1, 2, 3, 4],
    hint: 'Learned CC 22 · ch 1. Let go of the pedal, and try it once this line is gone.',
    hintAfterClose: 1,
    waitTimers: 1,
    hintAfterWait: 0,
  }, "each latching press fires once, the learning press none; closing the panel leaves the wait for a release running, its 10 s timer ends it and the hint"));
  check(() => assert.deepEqual(out.reversed, { steps: [1, 1, 2, 2] }, 'a reversed-polarity pedal fires once per press, on the press (0)'));
  check(() => assert.deepEqual(out.unmapped, [
    ['setSustain', true, A0],
    ['setModulation', 64 / 127, A0],
    ['releaseSource', A0],
    ['setSustain', false, A0],
  ], 'unlearned CC64/1/123 reach the router as before'));
  check(() => assert.deepEqual(out.modeCcWhileLearning, { calls: [['releaseSource', A0]], learning: 'true', lines: linesBeforeRefusals },
    'CC 120/123 are never learned: they reach the router and the learn keeps listening'));
  const noteOff61 = ['handle', { type: 'off', note: 61, velocity: 0, source: 'midi', owner: A0 }];
  check(() => assert.deepEqual(out.noteOffWhileLearning, { calls: [noteOff61, noteOff61], learning: 'true', lines: linesBeforeRefusals },
    'a note-off (0x80 or velocity 0) is never learned: it reaches the router and the learn keeps listening'));
  check(() => assert.deepEqual(out.learnLetsGo, [['setSustain', false, B0]], 'learning CC64 lets go of the pedal held down on its port'));
  check(() => assert.ok(out.mapped64.rms < 1e-5, `a CC learned onto 64 must not sustain (rms ${out.mapped64.rms})`));
  check(() => assert.deepEqual(out.mapped64.sustainCalls, [], 'a CC learned onto 64 never reaches setSustain'));
  check(() => assert.deepEqual(out.mapped64.steps, [1, 1], 'the learned CC64 press ran its action once, on the press (0)'));
  check(() => assert.ok(out.unmapped64Rms > 0.01, `the other port's unlearned CC64 still sustains (rms ${out.unmapped64Rms})`));
  check(() => assert.deepEqual(out.wheelLetsGo, { before: 100 / 127, after: 50 / 127 },
    'learning CC1 hands the vibrato back to the wheel moved before it'));
  check(() => assert.ok(out.learnedNote.rms < 1e-5, `a learned note must not sound (rms ${out.learnedNote.rms})`));
  check(() => assert.deepEqual(out.learnedNote.held, [], 'a learned note is never held'));
  check(() => assert.deepEqual(out.learnedNote.routerEvents, [], 'neither the learned note-on nor its note-off reaches the router'));
  check(() => assert.deepEqual(out.learnedNote.selected, [3, 2], 'two presses of the learned note stepped back twice, each on its note-on'));
  check(() => assert.ok(out.unlearnedNoteRms > 0.01, `an unlearned note on the same channel sounds (rms ${out.unlearnedNoteRms})`));
  check(() => assert.deepEqual(out.lines, [
    'Record / overdub | CC 20 · ch 1 | momentary',
    'Next track | CC 21 · ch 1 | momentary',
    'Next track | CC 22 · ch 1 | latching',
    'Next track | CC 23 · ch 3 | momentary',
    'Next track | CC 64 · ch 1 | momentary',
    'Stop all | CC 1 · ch 6 | latching',
    'Previous track | note 60 · ch 2 | momentary',
  ], 'the list names each binding and the pedal kind it was read as'));
  check(() => assert.equal(out.forgetLabel, 'Forget Next track on CC 64 · ch 1, Probe b', 'the ✕ names the binding and its port'));
  check(() => assert.deepEqual(out.forgotten, {
    sustainCalls: [['setSustain', true, B0], ['setSustain', false, B0]],
    lines: out.lines.filter((line) => !line.includes('CC 64 · ch 1')),
  }, 'a forgotten CC64 leaves the list and sustains again'));

  // ---- the foot vocabulary ----
  check(() => assert.deepEqual(out.heldLearn, {
    afterLearn: 0,
    line: 'Next track | CC 60 · ch 1 | momentary',
    steps: [1, 1, 2, 2],
  }, 'a learning press held 1.2 s reads as momentary, its release runs nothing, and each later tap fires once'));
  check(() => assert.deepEqual(out.closeWhileHeld, {
    afterRelease: 0,
    line: 'Next track | CC 61 · ch 1 | momentary',
    steps: [1, 1, 2, 2],
  }, 'a learning press released after the panel closed reads as momentary, its release runs nothing, and each later tap fires once'));
  check(() => assert.deepEqual(out.learnWhileHeld, {
    stillLearning: 'true',
    selected: 0,
    lines: ['Next track | CC 62 · ch 1 | momentary', 'Previous track | CC 63 · ch 1 | momentary'],
  }, "a learned pedal released after LEARN was pressed again reads as momentary and is not the new learn's press"));
  check(() => assert.deepEqual(out.kindSwitch, {
    latching: { kind: 'latching', steps: [1, 2] },
    momentary: { kind: 'momentary', steps: [1, 1] },
  }, "the kind switch changes how the pedal fires: latching on both messages, momentary on the press only"));
  check(() => assert.deepEqual(out.globals, {
    bpmBefore: out.globals.bpmBefore,
    bpmMoved: true,
    toggles: {
      'Metronome click': ['false', 'true', 'false'],
      'Stop playing loops at loop end': ['false', 'true', 'false'],
      'Fixed take length': ['false', 'true', 'false'],
    },
    echoCue: 'input effects need the native engine',
    reverbCue: 'input effects need the native engine',
  }, 'tap tempo sets the BPM, CLICK / END STOP / FIXED toggle their controls, IN FX is refused on the web path with a cue'));
  check(() => assert.deepEqual(out.targets, {
    lines: [
      'Record / overdub · Track 3 | CC 70 · ch 1 | momentary',
      'Play / stop · Track 2 | CC 71 · ch 1 | momentary',
      'Mute / unmute · Track 1 | CC 72 · ch 1 | momentary',
      'Reverse / forward · Track 2 | CC 73 · ch 1 | momentary',
      'Copy to an empty track · Track 1 | CC 74 · ch 1 | momentary',
    ],
    tapLocked: 'tempo locked to the loop, clear all to retap',
    playStop: { state: 'STOPPED', selected: 0, again: 'PLAYING' },
    mute: { muted: true, selected: 0, again: false },
    reverse: { reversed: true, selected: 0 },
    recDub: { state: 'RECORDING', selected: 2, afterRelease: 'RECORDING' },
    copy: { filled: true, selected: 0 },
  }, 'a press on a named track acts there from a track-1 selection; only REC/DUB moves the selection'));
  check(() => assert.deepEqual(out.hold, {
    on: 'true',
    down: 'OVERDUBBING',
    up: 'PLAYING',
    calls: [[0, 'PLAYING'], [0, 'OVERDUBBING']],
  }, 'HOLD: 127 starts the overdub and 0 ends it, one REC/DUB each'));
  check(() => assert.deepEqual(out.latchingHold, {
    latching: { kind: 'latching', disabled: true, pressed: 'false', afterSetHold: 'false' },
    switched: { kind: 'latching', disabled: true, pressed: 'false' },
  }, 'a latching pedal cannot HOLD, and a HOLD pedal switched to latching loses it'));
  check(() => assert.deepEqual(out.clearPerTarget, {
    afterOneEach: ['PLAYING', 'PLAYING'],
    track3: 'EMPTY',
    track2: 'PLAYING',
  }, "CLEAR confirms per target: one press on track 2 and one on track 3 clear nothing, track 3's second press clears it"));
  check(() => assert.deepEqual(out.oldFormat, {
    line: 'Play / stop | CC 90 · ch 1 | momentary',
    stored: { target: null, hold: false },
    cue: 'nothing to play, record first',
  }, 'a binding saved without a target loads as the selected track'));
  check(() => assert.deepEqual(out.engine, {
    hold: [
      [{ SelectTrack: 0 }, { ActionOn: [0, 'RecDub'] }],
      [{ ActionOn: [0, 'RecDub'] }],
      [{ SelectTrack: 0 }, { ActionOn: [0, 'RecDub'] }],
      [],
    ],
    sent: {
      10: [{ SelectTrack: 2 }, { ActionOn: [2, 'RecDub'] }],
      11: [{ ActionOn: [1, 'PlayStop'] }],
      12: [{ SetMute: [0, true] }],
      13: [{ Reverse: 1 }],
      14: [{ Copy: 0 }],
      15: [{ Action: 'PlayStop' }],
      16: [{ SetMetronome: true }],
      17: [{ SetLoopEndStop: true }],
      18: [{ SetFixedLength: true }],
      19: [{ SetInputSend: ['echo', true] }],
      20: [{ SetInputSend: ['reverb', true] }],
      21: [],
    },
    controls: { click: 'true', endStop: 'true', fixed: 'true', inFx: true },
    cue4: 'nothing to mute, record first',
  }, "engine mode: a press sends its lane control's command for the target lane, a global its control's; a refused one says why on its lane"));
  for (const failure of failures) console.log(`FAIL ${failure}`);
  assert.equal(failures.length, 0, `${failures.length} of ${checks} checks failed`);
});
