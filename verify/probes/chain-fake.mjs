/**
 * The browser fake's chain per slot (`docs/unit-contract.md` §10; `src/platform/chain-fake.ts`), driven
 * through the page as the UI will drive it: `__lf.platform.chain` (the `ChainHost`), the batch through the
 * platform's `sendEngine`, the feed through `__lf.platform.engine.subscribe` and the seams on
 * `__lf.native.chain`. It proves the fake answers as the contract says the engine will:
 *
 * - structure: the canned plugins in the picker's scan; an add answered through `loading`, then `seated`;
 *   a reserved id refused at once in either slot; a source at any index landing at the head, replacing the
 *   seated source (its id freed); an effect ahead of a source refused (add and move); a ninth unit refused;
 *   a removal through `leaving`; a hold in `loading` (`holdLoading`) and in `waiting` (a scripted recording
 *   lane, and the `capture` seam: a latent edit waits, `held`; a zero-latency effect in the other slot
 *   does not); a scripted failure (`failures`, the id freed) and a park (`failed` in place, reseated);
 * - the batch: `SetUnitBypass`, `ToggleUnit` and `Toggle.Units` (any on: all off, else all on, one
 *   `Toggled`; none of the kind: nothing) switch the model; `SetUnitParam` clamped as the engine clamps
 *   (ECHO's level 2 reads 1, its time rounds to an index, SYNTH's level stops at 1.5); a param of another
 *   kind and an unseated id dropped;
 * - a cold rig (a bypassed plugin, two ECHOs at different settings) read back from a reset frame;
 * - the OTT face's feed: `unitMeters` at about 60 Hz while an OTT is seated and on, silent when it is
 *   bypassed or gone; with the timer off, one frame per `meterTick`, following DEPTH, UP and DOWN; a frame
 *   scripted through `emit` reaches the feed as scripted;
 * - every `ChainHost` call recorded in order; the editor, tone and reload calls; the injectable id mint;
 *   the stereo pick over four canned inputs; the Tauri and production hosts refusing the chain.
 *
 * Cannot see the native engine (it does not speak the chain yet), any sound, or the UI's chain (not built).
 * Run: pnpm probe chain-fake
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const AMP = { format: 'vst3', path: 'C:\\Fake\\FakeAmp.vst3', id: 'fake.amp' };
const SYNTH = { format: 'clap', path: 'C:\\Fake\\FakeSynth.clap', id: 'fake.synth' };
const LOOKAHEAD = { format: 'vst2', path: 'C:\\Fake\\FakeLookahead.dll', id: 'fake.lookahead' };
const lane = (state) => ({ state, length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });

await probe(async ({ open }) => {
  const { page } = await open({
    init: (p) =>
      p.addInitScript(() => {
        window.__lfEngineFake = true;
        window.__lfChainFake = true;
      }),
  });
  let seq = 1000;
  const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
  await emit({ reset: true, settings: [], events: [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty') } })) });

  await page.evaluate(async () => {
    window.__frames = [];
    window.__lf.platform.engine.subscribe((f) => {
      const units = f.events.some((e) => e.type === 'Toggled' && typeof e.toggle === 'object' && 'Units' in e.toggle);
      if (f.chains || f.unitMeters || units) window.__frames.push({ at: performance.now(), ...JSON.parse(JSON.stringify(f)) });
    });
    window.__platform = await import('/src/platform/index.ts');
    window.__call = async (method, ...args) => {
      if (method === 'importTone') args[1] = new Uint8Array(args[1]);
      try {
        return { ok: (await window.__lf.platform.chain[method](...args)) ?? null };
      } catch (e) {
        return { err: String(e.message) };
      }
    };
  });
  const call = (method, ...args) => page.evaluate(([m, a]) => window.__call(m, ...a), [method, args]);
  /** Start a call without waiting for its answer (`settle` reads it). */
  const start = (key, method, ...args) => page.evaluate(([k, m, a]) => void (window[k] = window.__call(m, ...a)), [key, method, args]);
  const settle = (key) => page.evaluate((k) => window[k], key);
  const send = (...commands) => page.evaluate((c) => window.__platform.sendEngine(...c), commands);
  const frames = () => page.evaluate(() => window.__frames);
  const mark = () => page.evaluate(() => window.__frames.length);
  const since = async (m) => (await frames()).slice(m);
  const chainFrames = async (m) => (await since(m)).filter((f) => f.chains);
  const last = async () => (await frames()).filter((f) => f.chains).at(-1).chains;
  const ids = (c, slot) => c[slot].units.map((u) => u.id);
  const unit = (c, id) => [...c[0].units, ...c[1].units].find((u) => u.id === id);
  /** Each frame's state of `id` (absent: gone), in order. */
  const trail = (fs, id) => fs.map((f) => unit(f.chains, id)?.state ?? 'gone');
  /** Wait until a frame since mark `m` satisfies `pred`. */
  const waitFrames = async (m, pred, what) => {
    for (const end = Date.now() + 5000; !(await since(m)).some(pred); await page.waitForTimeout(20)) {
      if (Date.now() > end) throw new Error(`no frame came: ${what}`);
    }
  };
  /** A call's refusal, or what it answered instead (so a missing refusal reads as such). */
  const refusal = (r) => r.err ?? `no refusal: answered ${JSON.stringify(r.ok)}`;
  /** The calls the probe made, as the fake must have recorded them: method and first argument. */
  const made = [];
  const track = async (method, ...args) => {
    made.push([method, args[0]]);
    return call(method, ...args);
  };

  // ── The id mint: injectable, else 32 random bits, never 0 ──────────────────────────────────────────
  const random = await page.evaluate(() => [0, 1, 2, 3].map(() => window.__platform.mintUnitId()));
  console.log('random ids', random);
  assert.ok(random.every((id) => Number.isSafeInteger(id) && id >= 1 && id <= 0xffffffff), 'a minted id is a non-zero u32');
  assert.equal(new Set(random).size, 4, 'four random ids differ');
  await page.evaluate(() => {
    let n = 0x100;
    window.__lf.native.chain.mint = () => ++n;
  });
  const mint = () => page.evaluate(() => window.__platform.mintUnitId());
  assert.equal(await mint(), 0x101, 'the injected mint answers');
  assert.equal(await mint(), 0x102);
  assert.equal(await page.evaluate(() => window.__platform.engineFake === window.__lf.native), true, 'the probe imports the app’s own platform');

  // ── The picker's canned plugins, the host on ──────────────────────────────────────────────────────
  const scanned = await page.evaluate(() => window.__lf.platform.pluginHost.scanPlugins());
  console.log('scanned', scanned.map((d) => `${d.name} (${d.isEffect ? 'effect' : 'instrument'})`));
  assert.deepEqual(scanned.map((d) => d.id), ['fake.amp', 'fake.synth', 'fake.lookahead']);
  assert.equal(await page.evaluate(() => window.__lf.platform.chain.available), true);

  // ── Add: through `loading`, held there by the seam, then `seated`; a reserved id refused at once ─────
  const amp = await mint();
  let m = await mark();
  await page.evaluate(() => {
    window.__lf.native.chain.holdLoading = new Promise((r) => (window.__releaseLoading = r));
  });
  made.push(['add', 0]);
  await start('__addAmp', 'add', 0, amp, { kind: 'plugin', plugin: AMP }, 0);
  await waitFrames(m, (f) => f.chains && f.chains[0].units.some((u) => u.id === amp && u.state === 'loading'), 'the amp loading');
  assert.equal((await last())[0].units[0].state, 'loading', 'the plate shows loading while the load is held');
  assert.match(refusal(await track('add', 1, amp, { kind: 'echo' }, 0)), /reserved/, 'a reserved id is refused at once, in the other slot too');
  await page.evaluate(() => {
    window.__lf.native.chain.holdLoading = null;
    window.__releaseLoading();
  });
  assert.deepEqual(await settle('__addAmp'), { ok: 'seated' });
  let fs = await chainFrames(m);
  console.log('amp trail', trail(fs, amp));
  assert.deepEqual(trail(fs, amp), ['loading', 'seated'], 'every change answered by a chains frame, loading first');
  const ampState = unit(await last(), amp);
  assert.deepEqual(
    { kind: ampState.kind, plugin: ampState.plugin, bypass: ampState.bypass, source: ampState.source, name: ampState.name, latency: ampState.latency },
    { kind: 'plugin', plugin: AMP, bypass: false, source: false, name: 'Fake Amp', latency: 0 },
  );

  // ── A source at any index lands at the head, replacing the seated one ─────────────────────────────
  const synth = await mint();
  m = await mark();
  assert.deepEqual(await track('add', 0, synth, { kind: 'synth', params: { instrument: 1 } }, 5), { ok: 'seated' });
  fs = await chainFrames(m);
  console.log('synth placed', fs.map((f) => ids(f.chains, 0)));
  assert.deepEqual(ids(fs[0].chains, 0), [amp, synth], 'loading where asked (clamped to the end)');
  assert.deepEqual(ids(await last(), 0), [synth, amp], 'seated at the head');
  assert.equal(unit(await last(), synth).latency, 128, 'SYNTH reports one quantum');
  assert.equal((await last())[0].latency, 128, 'the chain sums its latencies');
  const plugSynth = await mint();
  assert.deepEqual(await track('add', 0, plugSynth, { kind: 'plugin', plugin: SYNTH }, 1), { ok: 'seated' });
  console.log('after the replace', ids(await last(), 0));
  assert.deepEqual(ids(await last(), 0), [plugSynth, amp], 'the plugin instrument replaced SYNTH at the head');
  assert.equal(unit(await last(), plugSynth).source, true);
  assert.equal((await last())[0].latency, 0);
  assert.deepEqual(await track('add', 1, synth, { kind: 'echo' }, 0), { ok: 'seated' }, 'the replaced id is free again');
  assert.deepEqual(await track('remove', synth), { ok: null });

  // ── An effect never goes ahead of a source; a source only heads ───────────────────────────────────
  const echoAhead = await mint();
  m = await mark();
  const ahead = await track('add', 0, echoAhead, { kind: 'echo' }, 0);
  console.log('effect ahead', ahead, trail(await chainFrames(m), echoAhead));
  assert.match(refusal(ahead), /ahead of the source/, 'an effect ahead of a source is refused');
  assert.deepEqual(trail(await chainFrames(m), echoAhead), ['loading', 'gone'], 'refused at activation, its plate gone');
  assert.deepEqual(ids(await last(), 0), [plugSynth, amp]);
  assert.match(refusal(await track('move', amp, 0)), /ahead of the source/, 'a move cannot put an effect ahead');
  assert.match(refusal(await track('move', plugSynth, 1)), /only heads/, 'a source cannot leave the head');
  assert.deepEqual(await track('add', 0, echoAhead, { kind: 'echo' }, 1), { ok: 'seated' }, 'a refused add frees its id');
  assert.deepEqual(await track('move', amp, 2), { ok: null });
  assert.deepEqual(ids(await last(), 0), [plugSynth, echoAhead, amp], 'a move reorders in one frame');

  // ── A ninth unit is refused ───────────────────────────────────────────────────────────────────────
  const kinds = ['echo', 'reverb', 'ring', 'ott', 'echo', 'reverb', 'ring', 'echo'];
  const eight = [];
  for (const kind of kinds) {
    const id = await mint();
    eight.push(id);
    assert.deepEqual(await track('add', 1, id, { kind, bypass: true }, 99), { ok: 'seated' });
  }
  const ninth = await mint();
  m = await mark();
  const refused = await track('add', 1, ninth, { kind: 'echo' }, 0);
  console.log('ninth', refused);
  assert.match(refusal(refused), /holds 8 units/, 'a ninth unit is refused');
  assert.equal((await last())[1].units.length, 8);
  assert.equal((await chainFrames(m)).length, 0, 'refused at once: no plate, no frame');
  m = await mark();
  for (const id of eight) assert.deepEqual(await track('remove', id), { ok: null });
  assert.deepEqual(trail(await chainFrames(m), eight[0]).slice(0, 2), ['leaving', 'gone'], 'a removal goes through leaving');
  assert.deepEqual(ids(await last(), 1), []);

  // ── A hold in `waiting`: a scripted take holds an edit that moves the alignment ───────────────────
  await emit({ events: [{ Lane: { frame: 0, lane: 0, info: lane('Recording') } }] });
  const look = await mint();
  m = await mark();
  assert.deepEqual(await track('add', 0, look, { kind: 'plugin', plugin: LOOKAHEAD }, 3), { ok: 'waiting' }, 'a latent unit waits for the take');
  let c = await last();
  console.log('held', { held: c[0].held, latency: c[0].latency, state: unit(c, look).state });
  assert.equal(unit(c, look).state, 'waiting');
  assert.equal(c[0].held, true);
  assert.equal(c[0].latency, 0, 'the waiting unit is not in the sum yet');
  const quiet = await mint();
  assert.deepEqual(await track('add', 1, quiet, { kind: 'ring' }, 0), { ok: 'seated' }, 'a zero-latency effect in the other slot does not wait');
  await emit({ events: [{ Lane: { frame: 0, lane: 0, info: lane('Playing') } }] });
  c = await last();
  assert.equal(unit(c, look).state, 'seated', 'the take ended: the held edit applies');
  assert.equal(c[0].held, false);
  assert.equal(c[0].latency, 64);
  assert.deepEqual(trail(await chainFrames(m), look).filter((s, i, a) => s !== a[i - 1]), ['loading', 'waiting', 'seated']);
  // The `capture` seam holds a removal that changes the sum, and releases it.
  await page.evaluate(() => (window.__lf.native.chain.capture = true));
  assert.deepEqual(await track('remove', look), { ok: null });
  c = await last();
  assert.equal(unit(c, look).state, 'leaving');
  assert.equal(c[0].held, true);
  await page.evaluate(() => (window.__lf.native.chain.capture = false));
  assert.equal(unit(await last(), look), undefined, 'released, the removal applies');
  assert.deepEqual(await track('remove', quiet), { ok: null });

  // ── A scripted failure, and a park ─────────────────────────────────────────────────────────────────
  const broken = await mint();
  await page.evaluate((id) => window.__lf.native.chain.failures.set(id, 'the DLL did not load'), broken);
  m = await mark();
  const failed = await track('add', 0, broken, { kind: 'plugin', plugin: AMP }, 9);
  console.log('scripted failure', failed, trail(await chainFrames(m), broken));
  assert.deepEqual(failed, { err: 'the DLL did not load' });
  assert.deepEqual(trail(await chainFrames(m), broken), ['loading', 'gone']);
  await page.evaluate((id) => window.__lf.native.chain.park(id, 'came back an instrument'), amp);
  c = await last();
  console.log('parked', unit(c, amp));
  assert.deepEqual([unit(c, amp).state, unit(c, amp).error], ['failed', 'came back an instrument']);
  assert.deepEqual(ids(c, 0), [plugSynth, echoAhead, amp], 'parked in its place');
  const droppedBefore = await page.evaluate(() => window.__lf.native.chain.dropped.length);
  await send({ SetUnitBypass: [amp, true] });
  await page.waitForFunction((n) => window.__lf.native.chain.dropped.length > n, droppedBefore, { timeout: 5000 });
  assert.equal(unit(await last(), amp).bypass, false, 'a parked unit is not seated: its bypass command is dropped');
  await page.evaluate((id) => window.__lf.native.chain.reseat(id), amp);
  c = await last();
  assert.equal(unit(c, amp).state, 'seated');
  assert.equal(unit(c, amp).error, undefined);

  // ── Bypass and params through the batch ───────────────────────────────────────────────────────────
  const echoB = await mint();
  assert.deepEqual(await track('add', 1, echoB, { kind: 'echo', bypass: true }, 0), { ok: 'seated' });
  const bypassOf = async (id) => unit(await last(), id).bypass;
  m = await mark();
  await send({ SetUnitBypass: [amp, true] });
  await waitFrames(m, (f) => f.chains && f.chains[0].units.find((u) => u.id === amp)?.bypass === true, 'SetUnitBypass');
  m = await mark();
  await send({ Action: { ToggleUnit: amp } });
  await waitFrames(m, (f) => f.chains && f.chains[0].units.find((u) => u.id === amp)?.bypass === false, 'ToggleUnit');
  assert.equal(await bypassOf(amp), false);
  // Toggle.Units: one ECHO on (echoAhead), one off (echoB): any on, so all go off; then all come on.
  m = await mark();
  await send({ Action: { Toggle: { Units: 'echo' } } });
  await waitFrames(m, (f) => f.events.some((e) => e.type === 'Toggled'), 'Toggled for the echoes');
  let answer = (await since(m)).find((f) => f.events.some((e) => e.type === 'Toggled'));
  console.log('Toggle.Units echo', JSON.stringify(answer.events), [await bypassOf(echoAhead), await bypassOf(echoB)]);
  assert.deepEqual(answer.events.map((e) => [e.type, e.toggle, e.on]), [['Toggled', { Units: 'echo' }, false]]);
  assert.ok(answer.chains, 'the Toggled rides with the changed chains');
  assert.deepEqual([await bypassOf(echoAhead), await bypassOf(echoB)], [true, true]);
  m = await mark();
  await send({ ActionOn: [3, { Toggle: { Units: 'echo' } }] });
  await waitFrames(m, (f) => f.events.some((e) => e.type === 'Toggled' && e.on), 'Toggled on');
  assert.deepEqual([await bypassOf(echoAhead), await bypassOf(echoB)], [false, false], 'none on: all come on');
  m = await mark();
  await send({ Action: { Toggle: { Units: 'ring' } } }, { SetUnitBypass: [echoB, true] });
  await waitFrames(m, (f) => f.chains, 'the bypass after the empty toggle');
  assert.ok((await since(m)).every((f) => f.events.length === 0), 'a kind with no unit: no Toggled');
  // SetUnitParam, clamped as the engine clamps; another kind's param and an unseated id dropped.
  const dropped0 = await page.evaluate(() => window.__lf.native.chain.dropped.length);
  m = await mark();
  await send(
    { SetUnitParam: [echoB, 'echoLevel', 2] },
    { SetUnitParam: [echoB, 'echoTime', 2.6] },
    { SetUnitParam: [echoB, 'echoFeedback', -1] },
    { SetUnitParam: [echoB, 'ottDepth', 0.3] },
    { SetUnitParam: [amp, 'echoLevel', 0.3] },
    { SetUnitParam: [0x7fff, 'echoLevel', 0.3] },
  );
  await waitFrames(m, (f) => f.chains && f.chains[1].units.find((u) => u.id === echoB)?.params.echoLevel === 1, 'the clamped echo level');
  const echoParams = unit(await last(), echoB).params;
  console.log('echo params', echoParams);
  assert.deepEqual(echoParams, { echoTime: 3, echoFeedback: 0, echoLevel: 1 });
  const droppedNow = await page.evaluate(() => window.__lf.native.chain.dropped.slice());
  assert.deepEqual(droppedNow.slice(dropped0).map((d) => d.SetUnitParam), [[echoB, 'ottDepth', 0.3], [amp, 'echoLevel', 0.3], [0x7fff, 'echoLevel', 0.3]]);
  const synthB = await mint();
  assert.deepEqual(await track('add', 1, synthB, { kind: 'synth', params: { gainLead: 3, instrument: 9 } }, 2), { ok: 'seated' });
  assert.deepEqual(ids(await last(), 1), [synthB, echoB], 'SYNTH heads slot B');
  const synthParams = unit(await last(), synthB).params;
  assert.deepEqual([synthParams.gainLead, synthParams.instrument, synthParams.gainBass], [1.5, 5, 1], 'built clamped, the rest at defaults');
  m = await mark();
  await send({ SetUnitParam: [synthB, 'gainPad', -2] });
  await waitFrames(m, (f) => f.chains && f.chains[1].units[0].params.gainPad === 0, 'SYNTH level clamped');
  for (const id of [synthB, echoB]) assert.deepEqual(await track('remove', id), { ok: null });

  // ── The editor, tones and reload, by unit ─────────────────────────────────────────────────────────
  const amp2 = await mint();
  assert.deepEqual(await track('add', 0, amp2, { kind: 'plugin', plugin: AMP }, 9), { ok: 'seated' });
  assert.deepEqual(await track('openEditor', amp), { ok: null });
  assert.match(refusal(await track('openEditor', amp2)), /belongs to unit/, 'D29: a second instance in the same chain has no editor');
  assert.deepEqual(await track('closeEditor', amp), { ok: null });
  assert.deepEqual(await track('listParams', amp2), { ok: [] });
  assert.deepEqual(await track('setParameter', amp2, 3, 0.5), { ok: null });
  assert.deepEqual(await track('takeTone', amp2), { ok: null });
  const tone = await track('importTone', 0, [1, 2, 3], AMP, amp2);
  console.log('importTone', tone);
  assert.deepEqual(tone.ok.matches, [amp2, amp], 'its own unit first, then the others in chain order');
  assert.equal(tone.ok.name, 'Fake Amp');
  assert.ok(Number.isInteger(tone.ok.token));
  assert.deepEqual((await track('importTone', 1, [1], AMP)).ok, { token: null, name: 'Fake Amp', matches: [] }, 'no unit of its identity there');
  const seqNow = (await frames()).filter((f) => f.chains).at(-1).seq;
  m = await mark();
  assert.deepEqual(await track('reload', amp2, AMP, seqNow, tone.ok.token), { ok: 'reloaded' });
  assert.deepEqual(trail(await chainFrames(m), amp2), ['loading', 'seated']);
  assert.deepEqual(await track('reload', amp2, AMP, 0, 7), { ok: 'moved' }, 'added after `since`: moved');
  await page.evaluate((id) => window.__lf.native.chain.failures.set(id, 'no'), amp2);
  assert.deepEqual(await track('reload', amp2, AMP, seqNow, 8), { ok: 'failed' });
  assert.equal(unit(await last(), amp2).state, 'seated', 'a failed reload leaves the old seated');
  assert.deepEqual(await track('forgetTone', tone.ok.token), { ok: null });
  assert.deepEqual(await track('remove', amp2), { ok: null });

  // ── A cold rig, read back from a reset frame ──────────────────────────────────────────────────────
  const rig = [await mint(), await mint(), await mint()];
  assert.deepEqual(await track('add', 1, rig[0], { kind: 'plugin', plugin: AMP, bypass: true }, 0), { ok: 'seated' });
  assert.deepEqual(await track('add', 1, rig[1], { kind: 'echo', params: { echoTime: 0, echoLevel: 0.3, echoFeedback: 0.2 } }, 1), { ok: 'seated' });
  assert.deepEqual(await track('add', 1, rig[2], { kind: 'echo', params: { echoTime: 3, echoLevel: 0.8 } }, 2), { ok: 'seated' });
  m = await mark();
  await emit({ reset: true, settings: [], events: [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty') } })) });
  const reset = (await since(m)).find((f) => f.reset);
  assert.ok(reset?.chains, 'a reset frame carries the chains');
  const restored = reset.chains[1].units.map(({ id, kind, bypass, params, plugin }) => ({ id, kind, bypass, params, plugin }));
  console.log('restored', JSON.stringify(restored));
  assert.deepEqual(restored, [
    { id: rig[0], kind: 'plugin', bypass: true, params: undefined, plugin: AMP },
    { id: rig[1], kind: 'echo', bypass: false, params: { echoTime: 0, echoFeedback: 0.2, echoLevel: 0.3 }, plugin: undefined },
    { id: rig[2], kind: 'echo', bypass: false, params: { echoTime: 3, echoFeedback: 0.4, echoLevel: 0.8 }, plugin: undefined },
  ]);
  for (const id of rig) assert.deepEqual(await track('remove', id), { ok: null });

  // ── The OTT face's feed ───────────────────────────────────────────────────────────────────────────
  const ott = await mint();
  m = await mark();
  assert.deepEqual(await track('add', 1, ott, { kind: 'ott' }, 0), { ok: 'seated' });
  await page.waitForTimeout(1000);
  let meters = (await since(m)).filter((f) => f.unitMeters);
  const span = (meters.at(-1).at - meters[0].at) / 1000;
  const rate = (meters.length - 1) / span;
  console.log(`unitMeters: ${meters.length} frames over ${span.toFixed(2)} s = ${rate.toFixed(1)} Hz; first`, JSON.stringify(meters[0].unitMeters));
  assert.ok(rate > 45 && rate < 75, `about 60 Hz (${rate.toFixed(1)})`);
  for (const f of meters) {
    assert.equal(f.unitMeters.length, 1);
    const [mt] = f.unitMeters;
    assert.equal(mt.unit, ott);
    assert.equal(mt.out[0], mt.out[1]);
    assert.equal(mt.bands.length, 3);
    for (const b of mt.bands) {
      assert.ok(b.gainMin <= b.gain && b.gain <= b.gainMax && b.gainMin >= -12 && b.gainMax <= 12, JSON.stringify(b));
      assert.ok(b.level >= -120 && b.level <= b.levelPeak, JSON.stringify(b));
    }
  }
  assert.ok(meters.some((f) => f.unitMeters[0].bands.some((b) => Math.abs(b.gain) > 0.5)), 'the simulated part moves the gains');
  const quietFor = async (ms, what) => {
    const q = await mark();
    await page.waitForTimeout(ms);
    assert.equal((await since(q)).filter((f) => f.unitMeters).length, 0, what);
  };
  await send({ SetUnitBypass: [ott, true] });
  await page.waitForFunction((id) => window.__lf.native.chain.chains()[1].units.find((u) => u.id === id).bypass, ott);
  await quietFor(300, 'silent while the OTT is bypassed');
  await send({ Action: { ToggleUnit: ott } });
  m = await mark();
  await waitFrames(m, (f) => f.unitMeters, 'meters again once on');
  assert.deepEqual(await track('remove', ott), { ok: null });
  await quietFor(300, 'silent once the OTT is gone');

  // The timer off: one frame per tick, following the params.
  await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
  const ticks = async (params, n) => {
    const id = await mint();
    assert.deepEqual(await track('add', 1, id, { kind: 'ott', params }, 0), { ok: 'seated' });
    const q = await mark();
    await page.waitForTimeout(150);
    assert.equal((await since(q)).filter((f) => f.unitMeters).length, 0, 'no timer frame while the timer is off');
    for (let k = 0; k < n; k++) await page.evaluate(() => window.__lf.native.chain.meterTick());
    const got = (await since(q)).filter((f) => f.unitMeters).map((f) => f.unitMeters[0]);
    assert.equal(got.length, n, 'one frame per tick');
    assert.deepEqual(await track('remove', id), { ok: null });
    return got.flatMap((u) => u.bands);
  };
  const flat = await ticks({ ottDepth: 0 }, 60);
  assert.ok(flat.every((b) => b.gain === 0 && b.gainMin === 0 && b.gainMax === 0), 'DEPTH 0: no gain');
  const downOnly = await ticks({ ottDepth: 1, ottUpward: 0 }, 120);
  const upOnly = await ticks({ ottDepth: 1, ottDownward: 0 }, 120);
  const range = (bs) => [Math.min(...bs.map((b) => b.gainMin)), Math.max(...bs.map((b) => b.gainMax))];
  console.log('UP 0 gain range', range(downOnly), 'DOWN 0 gain range', range(upOnly));
  assert.ok(range(downOnly)[1] <= 0 && range(downOnly)[0] < -1, 'UP 0: only cuts');
  assert.ok(range(upOnly)[0] >= 0 && range(upOnly)[1] > 1, 'DOWN 0: only lifts');
  const again = await ticks({ ottDepth: 1, ottUpward: 0 }, 120);
  assert.deepEqual(again, downOnly, 'deterministic: the same params, the same frames');
  // A frame scripted through `emit` reaches the feed as scripted.
  const scriptedMeter = { unit: 77, out: [0.5, 0.25], bands: [0, 1, 2].map((i) => ({ gain: 0 - i, gainMin: -1 - i, gainMax: 0 - i, level: -30, levelPeak: -20 })) };
  m = await mark();
  await emit({ unitMeters: [scriptedMeter] });
  assert.deepEqual((await since(m)).find((f) => f.unitMeters)?.unitMeters, [scriptedMeter]);
  await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));

  // ── Every ChainHost call recorded in order ────────────────────────────────────────────────────────
  const recorded = await page.evaluate(() => window.__lf.native.chain.calls.map((c) => [c[0], c[1]]));
  console.log(`${recorded.length} calls recorded`);
  assert.deepEqual(recorded, made);

  // ── The stereo pick over four canned inputs ───────────────────────────────────────────────────────
  assert.equal(await page.evaluate(() => window.__lf.native.inputs.length), 4);
  const pick = (slot, p) => page.evaluate(([s, v]) => window.__lf.platform.engine.setSlotInputPick(s, v).then(() => 'ok', (e) => e.message), [slot, p]);
  assert.equal(await pick(0, [2, 3]), 'ok');
  assert.equal(await pick(1, null), 'ok');
  assert.equal(await pick(1, 0), 'ok');
  assert.match(await pick(0, 4), /lacks/);
  assert.match(await pick(0, [0, 4]), /lacks/);
  assert.deepEqual(await page.evaluate(() => window.__lf.native.slotInputPicks), [[0, [2, 3]], [1, null], [1, 0]]);

  // ── The Tauri and production hosts refuse the chain without logging ──────────────────────────────
  const refusals = await page.evaluate(async () => {
    const { tauriPlatform } = await import('/src/platform/host.tauri.ts');
    const { inertPlatform } = await import('/src/platform/host.inert.ts');
    const why = (p) => p.then(() => 'answered', (e) => e.message);
    return [
      tauriPlatform.chain.available,
      await why(tauriPlatform.chain.add(0, 5, { kind: 'echo' }, 0)),
      await why(tauriPlatform.engine.setSlotInputPick(0, [0, 1])),
      inertPlatform.chain.available,
      await why(inertPlatform.chain.move(5, 0)),
    ];
  });
  console.log('refusals', refusals);
  assert.deepEqual(refusals.slice(0, 2), [false, 'The native engine does not have the chain yet.']);
  assert.match(refusals[2], /does not take a stereo input pick yet/);
  assert.deepEqual(refusals.slice(3), [false, 'Not available in the browser build.']);
});
