/**
 * Session tones on the engine fake (`src/audio/slot-tones.ts`): engine mode's export carries each loaded
 * slot's tone, taken fresh through the host, and names its plugin in session.json; an import hands each
 * tone to the host (`importTone`), reloads a slot that holds that plugin through the normal unload and
 * load, keeping its level and GO LIVE, and leaves a slot that holds another plugin or none alone, with
 * one toast naming the plugin to load. A load whose stored tone the host could not restore toasts. The
 * host is handed the plugin session.json names with each tone (it refuses a tone of another plugin). A
 * reload is skipped when the player picked another plugin for the slot while the import was in flight, or
 * the same plugin again (another instance), and its parked tone is handed back; the reload's load passes
 * the reload token the import answered. GO LIVE comes back only if the player chose no other live slot
 * after the reload began, its unload included. An archive whose tone
 * entry is past the tone limit is refused before anything is handed to the host. The plugin host is a
 * stand-in that records its calls and treats a tone as opaque bytes, as the frontend does. Cannot see
 * the native store, a real plugin or the Rust side: `pnpm native:tone-recall` does.
 * Run: pnpm probe session-tones
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const MASTER = 2 * RATE; // one bar at 120 BPM

await probe(async ({ open }) => {
  const { page, consoleErrors } = await open({
    init: async (p) => {
      // The stand-in native host must be available at boot, as the Tauri one is.
      await p.route('**/src/platform/host.web.ts', async (route) => {
        const response = await route.fetch();
        const body = (await response.text()).replace('available: false', 'available: true');
        await route.fulfill({ response, body });
      });
      await p.addInitScript(() => {
        window.__lfEngineFake = true;
      });
    },
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });

  const out = await page.evaluate(
    async ({ RATE, MASTER }) => {
      const { platform, encodeSessionBytes } = await import('/src/platform/index.ts');
      const instrument = await import('/src/audio/instrument.ts');
      const slots = await import('/src/audio/instrument-slots.ts');
      const nativeIo = await import('/src/audio/native-io.ts');
      const { buildExportBundle } = await import('/src/audio/export/export.ts');
      const { importSession } = await import('/src/audio/export/import.ts');
      const { parseZip } = await import('/src/audio/export/unzip.ts');
      const { makeZip } = await import('/src/audio/export/zip.ts');
      const { restoreSessionTones } = await import('/src/audio/slot-tones.ts');
      const { toasts } = await import('/src/notify.ts');
      const { session } = await import('/src/ui/state/audio.ts');
      const native = window.__lf.native;
      const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
      const until = async (what, predicate) => {
        for (let i = 0; i < 100; i++) {
          if (predicate()) return;
          await pause(20);
        }
        throw new Error(`timed out waiting for ${what}`);
      };
      await until('the native host ready', () => slots.nativeHostReady());

      // ── The stand-in host: plugins by id, a tone store by plugin id, every call recorded ────────────
      const amp = { id: 'amp', name: 'Probe Amp', path: 'C:\\probe\\amp.vst3', format: 'vst3', isEffect: true };
      const synth = { id: 'syn', name: 'Probe Synth', path: 'C:\\probe\\syn.clap', format: 'clap', isEffect: false };
      const byId = (id) => [amp, synth].find((d) => d.id === id);
      const calls = [];
      const held = [null, null];
      const store = new Map();
      let refuse = false;
      // A gate holds the answer of one slot's import or load until the test opens it.
      const gate = () => {
        let open;
        const closed = new Promise((resolve) => (open = resolve));
        return { closed, open };
      };
      let importGate = null;
      let loadGate = null;
      let unloadGate = null;
      const expected = [];
      // The reload tokens the stand-in answered, and the one each load passed.
      let lastToken = 0;
      const answered = [];
      const loadTokens = [];
      const host = platform.pluginHost;
      host.loadPlugin = async (slot, _path, id, _loadToken, toneToken) => {
        calls.push(`load ${slot} ${id}`);
        loadTokens.push(toneToken ?? null);
        if (loadGate?.slot === slot) await loadGate.closed;
        held[slot] = id;
        return { slot, descriptor: byId(id), tone: refuse ? 'failed' : store.has(id) ? 'restored' : undefined };
      };
      host.unloadPlugin = async (slot) => {
        calls.push(`unload ${slot}`);
        if (unloadGate?.slot === slot) await unloadGate.closed;
        held[slot] = null;
      };
      host.forgetTone = async (slot, token) => {
        calls.push(`forget ${slot} ${token}`);
      };
      host.takeTone = async (slot) => {
        calls.push(`take ${slot}`);
        const bytes = new TextEncoder().encode(`${held[slot]} state ${slot}`);
        store.set(held[slot], bytes);
        return bytes;
      };
      host.importTone = async (slot, bytes, plugin) => {
        const text = new TextDecoder().decode(bytes.subarray(0, 32));
        const [id] = text.split(' ');
        calls.push(`import ${slot} ${text}`);
        expected.push(plugin ? { format: plugin.format, path: plugin.path, id: plugin.id } : null);
        store.set(id, bytes);
        const { name, format, path } = byId(id);
        // Answered as the native host answers: for the slot as it was when the tone was stored.
        const reloadToken = held[slot] === id ? ++lastToken : null;
        if (reloadToken !== null) answered.push(reloadToken);
        const answer = { reloadToken, name, format, path, id };
        if (importGate?.slot === slot) await importGate.closed;
        return answer;
      };

      // ── A rig: the amp in slot A, live, at 0.6; the synth in slot B; one committed lane ───────────
      await instrument.selectPlugin(0, amp);
      await instrument.selectPlugin(1, synth);
      instrument.setPluginGain(0, 0.6);
      await nativeIo.goLive(0);
      let seq = 0;
      const lanes = (state) =>
        [0, 1, 2, 3, 4].map((i) => ({
          Lane: {
            frame: 0,
            lane: i,
            info: {
              state: i === 0 ? state : 'Empty',
              length: i === 0 && state !== 'Empty' ? MASTER : 0,
              armed: false,
              autoArmed: false,
              canUndo: false,
              canReverse: false,
              reversed: false,
              stopAt: null,
              fading: false,
              retakePass: 0,
            },
          },
        }));
      const feed = (state) =>
        native.emit({
          seq: ++seq,
          reset: true,
          events: [{ Transport: { frame: 0, master: state === 'Empty' ? 0 : MASTER, bpm: 120, locked: state !== 'Empty' } }, ...lanes(state)],
          anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
          meter: { peak: 0, clip: false },
        });
      feed('Playing');
      const pcm = Float32Array.from({ length: MASTER }, (_, i) => 0.25 * Math.sin((2 * Math.PI * 220 * i) / RATE));
      native.snapshotBytes = encodeSessionBytes(
        { rate: RATE, masterLengthFrames: MASTER, bpm: 120, tracks: [{ index: 0, frames: MASTER, reversed: false, state: 'Playing' }] },
        [pcm],
      ).buffer;
      await pause(50);

      // ── Export ──────────────────────────────────────────────────────────────────────────────────────
      calls.length = 0;
      const bundle = await buildExportBundle({ bpm: 120, bars: 1 }, {}, session);
      const entries = parseZip(bundle.zipBytes);
      const text = (name) => new TextDecoder().decode(entries.find((e) => e.name.endsWith(name)).data);
      const exported = {
        calls: calls.slice(),
        tones: [text('-tone-slot-a.bin'), text('-tone-slot-b.bin')],
        plugins: JSON.parse(text('-session.json')).plugins,
      };

      // ── Import into empty lanes: both slots hold their plugin, so both reload ──────────────────────
      const reimport = async () => {
        feed('Empty');
        await pause(50);
        calls.length = 0;
        const sentBefore = native.sent.length;
        await restoreSessionTones(await importSession(bundle.zipBytes, session));
        return { calls: calls.slice(), sent: native.sent.slice(sentBefore).map((c) => JSON.stringify(c)) };
      };
      loadTokens.length = 0;
      const first = await reimport();
      const afterFirst = {
        ...first,
        tokens: { answered: answered.splice(0), passed: loadTokens.splice(0) },
        expected: expected.splice(0),
        slots: instrument.slotPlugins().map((d) => d?.id ?? null),
        live: nativeIo.inputArmed().slice(),
        gain: instrument.pluginGain().slice(),
        toasts: toasts().map((t) => t.message),
      };

      // ── Slot B emptied: its tone is stored, nothing loads there, one toast says what to load ────────
      await instrument.clearPlugin(1);
      const second = await reimport();
      const afterSecond = {
        ...second,
        slots: instrument.slotPlugins().map((d) => d?.id ?? null),
        toasts: toasts().map((t) => t.message),
      };

      // ── A stored tone the host could not restore: the load goes on, and says so ────────────────────
      refuse = true;
      await instrument.selectPlugin(1, synth);
      const refused = { slot: instrument.slotPlugins()[1]?.id ?? null, toasts: toasts().map((t) => t.message) };
      refuse = false;

      // ── The player picks another plugin for slot A while the import is in flight ────────────────────
      // The host answered for slot A as it was (the amp, held); by the time the answer lands the slot
      // holds the synth, which must not be reloaded.
      const toastsBefore = new Set(toasts().map((t) => t.message));
      feed('Empty');
      await pause(50);
      importGate = { slot: 0, ...gate() };
      calls.length = 0;
      const inFlight = importSession(bundle.zipBytes, session).then(restoreSessionTones);
      await until('the import of slot A', () => calls.includes('import 0 amp state 0'));
      await instrument.selectPlugin(0, synth);
      const picked = calls.length;
      importGate.open();
      await inFlight;
      importGate = null;
      const moved = {
        after: calls.slice(picked),
        slots: instrument.slotPlugins().map((d) => d?.id ?? null),
        newToasts: toasts().map((t) => t.message).filter((m) => !toastsBefore.has(m)),
      };

      // ── The player makes slot B live while slot A reloads: the newer choice stands ──────────────────
      await instrument.selectPlugin(0, amp);
      await nativeIo.goLive(0);
      feed('Empty');
      await pause(50);
      loadGate = { slot: 0, ...gate() };
      calls.length = 0;
      const reloading = importSession(bundle.zipBytes, session).then(restoreSessionTones);
      await until('slot A reloading', () => calls.includes('load 0 amp'));
      await nativeIo.goLive(1);
      const liveMidReload = nativeIo.inputArmed().slice();
      loadGate.open();
      await reloading;
      loadGate = null;
      const liveChoice = { calls: calls.slice(), liveMidReload, live: nativeIo.inputArmed().slice() };

      // ── The player makes slot B live while slot A's reload unloads: the newer choice stands ─────────
      await nativeIo.goLive(0);
      feed('Empty');
      await pause(50);
      unloadGate = { slot: 0, ...gate() };
      calls.length = 0;
      const unloading = importSession(bundle.zipBytes, session).then(restoreSessionTones);
      await until('slot A unloading', () => calls.includes('unload 0'));
      await nativeIo.goLive(1);
      const liveMidUnload = nativeIo.inputArmed().slice();
      unloadGate.open();
      await unloading;
      unloadGate = null;
      const liveDuringUnload = { calls: calls.slice(), liveMidUnload, live: nativeIo.inputArmed().slice() };

      // ── The player switches slot A to the synth and back to the amp while the import is in flight ───
      // The host answered for the amp slot A held before; the amp there now is another instance, which
      // must not be reloaded, and the tone parked for that reload is handed back.
      feed('Empty');
      await pause(50);
      importGate = { slot: 0, ...gate() };
      calls.length = 0;
      answered.length = 0;
      const backAgain = importSession(bundle.zipBytes, session).then(restoreSessionTones);
      await until('the import of slot A', () => calls.includes('import 0 amp state 0'));
      await instrument.selectPlugin(0, synth);
      await instrument.selectPlugin(0, amp);
      const switched = calls.length;
      importGate.open();
      await backAgain;
      importGate = null;
      const sameAgain = {
        after: calls.slice(switched),
        token: answered[0] ?? null,
        slots: instrument.slotPlugins().map((d) => d?.id ?? null),
      };

      // ── A tone entry past the tone limit never reaches the host ─────────────────────────────────────
      feed('Empty');
      await pause(50);
      const oversized = makeZip(
        parseZip(bundle.zipBytes).map((e) =>
          e.name.endsWith('-tone-slot-a.bin') ? { name: e.name, data: new Uint8Array((16 << 20) + (1 << 16) + 1) } : e,
        ),
      );
      calls.length = 0;
      let bigTone = null;
      try {
        await restoreSessionTones(await importSession(oversized, session));
      } catch (e) {
        bigTone = e instanceof Error ? e.message : String(e);
      }
      const tooBig = { error: bigTone, calls: calls.slice() };
      return {
        exported,
        afterFirst,
        afterSecond,
        refused,
        moved,
        liveChoice,
        liveDuringUnload,
        sameAgain,
        tooBig,
        loadedSessions: native.loadedSessions.length,
      };
    },
    { RATE, MASTER },
  );
  console.log(JSON.stringify(out, null, 1));

  assert.deepEqual(out.exported.calls, ['take 0', 'take 1'], 'the export takes each loaded slot’s tone fresh');
  assert.deepEqual(out.exported.tones, ['amp state 0', 'syn state 1'], 'a tone file per slot, as the host handed it');
  assert.deepEqual(
    out.exported.plugins.map((p) => [p.slot, p.format, p.path, p.id, p.name, p.file.slice(-16)]),
    [
      ['A', 'vst3', 'C:\\probe\\amp.vst3', 'amp', 'Probe Amp', '-tone-slot-a.bin'],
      ['B', 'clap', 'C:\\probe\\syn.clap', 'syn', 'Probe Synth', '-tone-slot-b.bin'],
    ],
    'session.json names each slot’s plugin and its tone file',
  );

  assert.deepEqual(
    out.afterFirst.calls,
    ['import 0 amp state 0', 'unload 0', 'load 0 amp', 'import 1 syn state 1', 'unload 1', 'load 1 syn'],
    'each tone goes to the host, and a slot holding its plugin reloads through the normal path',
  );
  assert.deepEqual(out.afterFirst.slots, ['amp', 'syn']);
  assert.deepEqual(out.afterFirst.live, [true, false], 'slot A is live again after its reload');
  const sent = out.afterFirst.sent;
  const offAt = sent.indexOf(JSON.stringify({ SetSlotLive: [0, false] }));
  const onAt = sent.lastIndexOf(JSON.stringify({ SetSlotLive: [0, true] }));
  assert.ok(offAt >= 0 && onAt > offAt, `GO LIVE ends at the unload and comes back after the load: ${sent.join(' ')}`);
  assert.equal(out.afterFirst.gain[0], 0.6, 'slot A keeps its level');
  assert.equal(
    sent.filter((c) => c.startsWith('{"SetSlotGain":[0,')).at(-1),
    JSON.stringify({ SetSlotGain: [0, 0.6] }),
    'and the engine gets it back',
  );
  assert.equal(out.afterFirst.toasts.length, 0, `no toast: ${JSON.stringify(out.afterFirst.toasts)}`);
  assert.deepEqual(
    out.afterFirst.expected,
    [
      { format: 'vst3', path: 'C:\\probe\\amp.vst3', id: 'amp' },
      { format: 'clap', path: 'C:\\probe\\syn.clap', id: 'syn' },
    ],
    'the host is handed the plugin session.json names with each tone, to check it against the tone file',
  );

  assert.deepEqual(
    out.afterSecond.calls,
    ['import 0 amp state 0', 'unload 0', 'load 0 amp', 'import 1 syn state 1'],
    'an empty slot B is left alone: nothing loads there',
  );
  assert.deepEqual(out.afterSecond.slots, ['amp', null]);
  assert.deepEqual(out.afterSecond.toasts, ["This session used Probe Synth in slot B — load it to hear the session's tone"]);

  assert.equal(out.refused.slot, 'syn', 'a tone the host could not restore never fails the load');
  assert.ok(
    out.refused.toasts.includes('Probe Synth: saved settings could not be restored; it loaded with its defaults'),
    `the player is told: ${JSON.stringify(out.refused.toasts)}`,
  );
  assert.ok(
    !out.moved.after.some((c) => c.startsWith('unload 0') || c.startsWith('load 0')),
    `a slot the player moved to another plugin during the import is not reloaded: ${out.moved.after.join(', ')}`,
  );
  assert.deepEqual(out.moved.slots, ['syn', 'syn'], 'the player’s pick stands');
  assert.deepEqual(out.moved.newToasts, [], 'and nothing extra is toasted');

  assert.deepEqual(out.liveChoice.liveMidReload, [false, true], 'slot B went live while slot A reloaded');
  assert.ok(out.liveChoice.calls.includes('load 0 amp'), `slot A reloaded: ${out.liveChoice.calls.join(', ')}`);
  assert.deepEqual(out.liveChoice.live, [false, true], 'the reload does not take GO LIVE back from the newer choice');
  assert.deepEqual(out.afterFirst.tokens.passed, out.afterFirst.tokens.answered, 'each reload passes the token its import answered');
  assert.equal(out.afterFirst.tokens.answered.length, 2);

  assert.deepEqual(out.liveDuringUnload.liveMidUnload, [false, true], 'slot B went live while slot A unloaded');
  assert.ok(out.liveDuringUnload.calls.includes('load 0 amp'), `slot A reloaded: ${out.liveDuringUnload.calls.join(', ')}`);
  assert.deepEqual(out.liveDuringUnload.live, [false, true], 'a choice made during the reload’s unload stands too');

  assert.ok(
    !out.sameAgain.after.some((c) => c.startsWith('unload 0') || c.startsWith('load 0')),
    `the same plugin loaded again during the import is another instance, not reloaded: ${out.sameAgain.after.join(', ')}`,
  );
  assert.deepEqual(out.sameAgain.slots, ['amp', 'syn']);
  assert.ok(out.sameAgain.token !== null && out.sameAgain.after.includes(`forget 0 ${out.sameAgain.token}`), `its parked tone is handed back: ${out.sameAgain.after.join(', ')}`);

  assert.match(out.tooBig.error ?? '', /tone/, `an oversized tone entry is refused: ${out.tooBig.error}`);
  assert.deepEqual(out.tooBig.calls, [], 'before anything reaches the host');
  assert.equal(out.loadedSessions, 6, 'the six imports that passed reached the engine');
  assert.deepEqual(consoleErrors, [], 'no console errors');
});
