/**
 * Note ownership in the real MIDI parser (`src/ui/state/midi.ts`) and input router
 * (`src/ui/state/input-router.ts`), on the web engine fake (`src/platform/host.web.ts`, the engine-seam
 * pattern) with two virtual Web MIDI ports fed raw bytes. The router's notes reach the engine as
 * `NoteOn`/`NoteOff` commands, read from `__lf.native.sent`:
 *
 * - one port's or one channel's note-off never releases another's held note;
 * - a sustain pedal (CC64) is scoped to its port: another port's release is sent at once, another port's
 *   pedal-up releases nothing, the own pedal-up sends the deferred `NoteOff`;
 * - CC123 releases only its own owner's keys and honours its own pedal (the `NoteOff` waits for pedal-up);
 * - unplugging one port keeps the other port's held note, and a computer-keyboard note after a pedal
 *   controller was unplugged still sends its `NoteOff` on release;
 * - two pointers on one on-screen key: the first release keeps the note held and the key lit, the last
 *   releases both; a MIDI note lights and clears the same on-screen key.
 *
 * Cannot see the native engine (what a `NoteOn` sounds like: lf-engine `tests/synth.rs`,
 * `tests/voices.rs`, `tests/slots.rs`), real MIDI hardware or Tauri IPC.
 * Run: pnpm probe midi-note-ownership
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({
    init: (p) => p.addInitScript(() => {
      window.__lfEngineFake = true;
      const inputs = new Map(['a', 'b'].map((id) => [id, { id, name: `Probe ${id}`, state: 'connected', onmidimessage: null }]));
      const access = { inputs, onstatechange: null };
      window.__probeMidi = access;
      Object.defineProperty(navigator, 'requestMIDIAccess', { configurable: true, value: async () => access });
    }),
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1 && !!window.__probeMidi.inputs.get('a').onmidimessage);
  await page.evaluate(() => {
    const lane = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };
    window.__lf.native.emit({
      seq: 1,
      reset: true,
      settings: [],
      events: [...[0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } }, { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: 48000, grid: 0 },
      meter: { peak: 0, clip: false },
    });
  });

  const result = await page.evaluate(async () => {
    const lf = window.__lf;
    lf.selectSynth(0, 'organ'); lf.setActiveSlot(0);
    const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
    await pause(50); lf.ensureActive();
    const access = window.__probeMidi;
    const send = (port, bytes) => access.inputs.get(port).onmidimessage({ data: Uint8Array.from(bytes) });
    /** The notes the engine received since the last call, as 'on:60' / 'off:60'. */
    let seen = 0;
    const notes = async () => {
      await pause(10); // the outbox flushes on a microtask, the fake records on its async send
      const all = lf.native.sent.slice(seen);
      seen = lf.native.sent.length;
      return all.filter((c) => c.NoteOn || c.NoteOff !== undefined).map((c) => (c.NoteOn ? `on:${c.NoteOn[0]}` : `off:${c.NoteOff}`));
    };
    await notes();
    send('a', [0x90, 60, 100]); send('b', [0x90, 60, 100]); send('a', [0x80, 60, 0]);
    const heldAfterFirstPortRelease = [...lf.inputRouter.held];
    const firstPortRelease = await notes();
    lf.inputRouter.allNotesOff(); await notes();
    send('a', [0x90, 60, 100]); send('a', [0x91, 60, 100]); send('a', [0x80, 60, 0]);
    const heldAfterFirstChannelRelease = [...lf.inputRouter.held];
    const firstChannelRelease = await notes();
    lf.inputRouter.allNotesOff(); await notes();

    // Port a's pedal down; port b's note releases at once.
    send('a', [0xb0, 64, 127]);
    send('b', [0x90, 67, 100]); send('b', [0x80, 67, 0]);
    const otherPortRelease = await notes();
    // Port a's own note under its pedal; port b's pedal-up releases nothing, port a's pedal-up does.
    send('a', [0x90, 67, 100]); send('a', [0x80, 67, 0]);
    send('b', [0xb0, 64, 0]);
    const otherPedalUp = await notes();
    send('a', [0xb0, 64, 0]);
    const ownPedalUp = await notes();

    send('a', [0x90, 60, 100]); send('b', [0x90, 60, 100]);
    send('a', [0xb0, 123, 0]);
    const heldAfterOtherCc123 = [...lf.inputRouter.held];
    send('b', [0xb0, 123, 0]);
    const heldAfterBothCc123 = [...lf.inputRouter.held];
    await notes();
    // CC123 under port a's own pedal: the note keeps sounding until pedal-up.
    send('a', [0xb0, 64, 127]); send('a', [0x90, 67, 100]);
    send('a', [0xb0, 123, 0]);
    const cc123UnderPedal = await notes();
    send('a', [0xb0, 64, 0]);
    const cc123PedalUp = await notes();

    // Port a (its pedal down) unplugs while port b holds a note.
    send('a', [0xb0, 64, 127]);
    send('b', [0x90, 60, 100]);
    access.inputs.get('a').state = 'disconnected'; access.onstatechange();
    const heldAfterOtherDisconnect = [...lf.inputRouter.held];
    send('b', [0x80, 60, 0]);
    await notes();
    lf.inputRouter.handle({ type: 'on', note: 67, velocity: 100, source: 'computer' });
    lf.inputRouter.handle({ type: 'off', note: 67, velocity: 0, source: 'computer' });
    const keyboardAfterDisconnect = await notes();
    lf.inputRouter.setSustain(false); lf.inputRouter.allNotesOff(); await notes();

    const key = document.querySelector('.kb__key[data-note="60"]');
    if (!key) throw new Error('pointer ownership test requires the visible keyboard');
    const capture = key.setPointerCapture;
    key.setPointerCapture = () => {}; // synthetic events cannot acquire browser-native pointer capture
    const pointer = (type, pointerId) => key.dispatchEvent(new PointerEvent(type, { bubbles: true, pointerId, pointerType: 'touch' }));
    pointer('pointerdown', 901); pointer('pointerdown', 902); await pause(30);
    pointer('pointerup', 901); await pause(30);
    const firstPointerUp = { held: [...lf.inputRouter.held], highlighted: key.classList.contains('kb__key--down') };
    pointer('pointerup', 902); await pause(30);
    const lastPointerUp = { held: [...lf.inputRouter.held], highlighted: key.classList.contains('kb__key--down') };
    const pointerNotes = await notes();
    key.setPointerCapture = capture;
    // A MIDI controller lights the same on-screen key as the pointer (the keys mirror the router).
    send('b', [0x90, 60, 100]); await pause(30);
    const midiDown = key.classList.contains('kb__key--down');
    send('b', [0x80, 60, 0]); await pause(30);
    const midiUp = key.classList.contains('kb__key--down');
    return {
      heldAfterFirstPortRelease, firstPortRelease, heldAfterFirstChannelRelease, firstChannelRelease, otherPortRelease, otherPedalUp,
      ownPedalUp, heldAfterOtherCc123, heldAfterBothCc123, cc123UnderPedal, cc123PedalUp, heldAfterOtherDisconnect,
      keyboardAfterDisconnect, firstPointerUp, lastPointerUp, pointerNotes, midiDown, midiUp,
    };
  });
  console.log(JSON.stringify(result));
  assert.deepEqual(result.firstPointerUp, { held: [60], highlighted: true }, 'first pointer release preserves the note and key highlight');
  assert.deepEqual(result.lastPointerUp, { held: [], highlighted: false }, 'last pointer releases the note and key highlight');
  assert.deepEqual(result.pointerNotes, ['on:60', 'off:60'], 'two pointers on one key send one NoteOn and one NoteOff');
  assert.deepEqual([result.midiDown, result.midiUp], [true, false], 'a MIDI note highlights and clears its on-screen key');
  assert.deepEqual(result.heldAfterFirstPortRelease, [60], 'one MIDI port must not release another port\'s held note');
  assert.deepEqual(result.firstPortRelease, ['on:60'], 'the note stays on while the other port holds it');
  assert.deepEqual(result.heldAfterFirstChannelRelease, [60], 'one MIDI channel must not release another channel\'s held note');
  assert.deepEqual(result.firstChannelRelease, ['on:60'], 'the note stays on while the other channel holds it');
  assert.deepEqual(result.otherPortRelease, ['on:67', 'off:67'], 'one port\'s pedal must not sustain another port');
  assert.deepEqual(result.otherPedalUp, ['on:67'], 'an unrelated pedal-up must preserve a sustained note');
  assert.deepEqual(result.ownPedalUp, ['off:67'], 'the own pedal-up releases the note');
  assert.deepEqual(result.heldAfterOtherCc123, [60], 'CC123 must leave the other owner held');
  assert.deepEqual(result.heldAfterBothCc123, [], 'CC123 must release its own keys');
  assert.deepEqual(result.cc123UnderPedal, ['on:67'], 'CC123 honours the physical pedal');
  assert.deepEqual(result.cc123PedalUp, ['off:67'], 'pedal-up completes a deferred CC123 release');
  assert.deepEqual(result.heldAfterOtherDisconnect, [60], 'unplugging one port must preserve the other port');
  assert.deepEqual(result.keyboardAfterDisconnect, ['on:67', 'off:67'], 'an unplugged pedal controller leaves no keyboard note sustained');
});
