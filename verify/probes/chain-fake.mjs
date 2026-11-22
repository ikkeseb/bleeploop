/**
 * The browser fake's chain per slot (`docs/unit-contract.md` §10; `src/platform/chain-fake.ts`), driven
 * through the page as the UI will drive it: `__lf.platform.chain` (the `ChainHost`), the batch through the
 * platform's `sendEngine`, the feed through `__lf.platform.engine.subscribe` and the seams on
 * `__lf.native.chain`. It proves the fake answers as the contract says the engine will:
 *
 * - structure: the canned plugins in the picker's scan; an add answered through `loading`, then `seated`;
 *   a reserved id refused at once in either slot; a source at any index landing at the head, replacing the
 *   seated source (its id freed), in a full chain too (every frame decodable); an effect ahead of a source
 *   refused (add and move); a ninth unit refused (at once, or at activation when only it could tell); a
 *   move to another index; a removal through `leaving`; a hold in `loading` (`holdLoading`) and in
 *   `waiting` (a scripted recording lane, and the `capture` seam: a latent edit waits, `held`; a
 *   zero-latency effect in the other slot does not; a held move published at once, its unit `held`, and
 *   judged again when it applies; a held removal settling only once applied); a scripted failure (a
 *   `failed` plate in its place, the id free, removed by hand) and a park (`failed` in place, reseated);
 *   a scripted `failed` plate without `parked` is a failed add (no seat in the sum, no source, gone at once);
 * - the batch: `SetUnitBypass`, `ToggleUnit` and `Toggle.Units` (any on: all off, else all on, one
 *   `Toggled`; none of the kind: nothing) switch the model; `SetUnitParam` clamped as the engine clamps
 *   (ECHO's level 2 reads 1, its time rounds to an index, SYNTH's level stops at 1.5); a param of another
 *   kind and an unseated id dropped;
 * - D29 by unit across both slots (a move keeps the owner; orphaned; unordered); tones by §8 (a tone file
 *   read and identity-checked, its own unit first in either slot, an unmatched tone kept as the slot's file
 *   a new unit starts from, a token bound to its plugin); reload (the old seat counted while it loads, its plate
 *   `reloading`, a latency change held by a take, `since` against a reset frame's seq);
 * - a warm rig read back from a reset frame; a cold rig ingested from one (the model, its reservations,
 *   its batch); a reset settling a held move and a call still loading;
 * - the meters: one per seated unit on every frame the fake makes; the OTT face's `unitMeters` at about
 *   60 Hz on one live interval exactly while an OTT is seated and on (none when bypassed, removed, reset or
 *   the timer is off); with the timer off, one frame per `meterTick`, following DEPTH, TIME, UP and DOWN;
 *   the ECHO's `out` per channel, a note on each downbeat and its repeats at TIME (the engine's index:
 *   1/4, 1/8, 1/16), FEEDBACK and LEVEL and the scripted tempo, deterministic, its timer live while it is
 *   on; a frame scripted through `emit` reaches the feed as scripted;
 * - every `ChainHost` call recorded in order with all its arguments; the injectable id mint; the stereo
 *   pick over four canned inputs; the Tauri and production hosts refusing the chain; under Tauri the
 *   unit commands never reach `input_send` (the IPC spied: zero invokes for them).
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
const emptyLanes = () => [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty') } }));

/** A tone file as `src-tauri/src/host/tone.rs` `encode` writes one, as a plain array (it crosses `evaluate`). */
function tone(plugin, name, state) {
  const out = [0x42, 0x4c, 0x54, 0x4e, 1, 0];
  const put = (b) => out.push(b.length & 0xff, (b.length >> 8) & 0xff, (b.length >> 16) & 0xff, b.length >>> 24, ...b);
  for (const field of [plugin.format, plugin.path, plugin.id, name]) put([...new TextEncoder().encode(field)]);
  put(state);
  let h = 0xcbf29ce484222325n;
  for (const b of out) h = BigInt.asUintN(64, (h ^ BigInt(b)) * 0x100000001b3n);
  for (let k = 0n; k < 8n; k++) out.push(Number((h >> (8n * k)) & 0xffn));
  return out;
}

await probe(async ({ open }) => {
  const { page } = await open({
    init: (p) =>
      p.addInitScript(() => {
        window.__lfEngineFake = true;
        window.__lfChainFake = true;
        // The meters' timer: every interval at 60 Hz the page holds live, so a leaked one shows.
        window.__intervals = new Set();
        const set = window.setInterval;
        const clear = window.clearInterval;
        window.setInterval = function (fn, ms, ...rest) {
          const id = set.call(window, fn, ms, ...rest);
          if (ms === 1000 / 60) window.__intervals.add(id);
          return id;
        };
        window.clearInterval = function (id) {
          window.__intervals.delete(id);
          return clear.call(window, id);
        };
      }),
  });
  /** The feed's seq is one sequence: a scripted frame numbers on from the newest frame the page saw. */
  let seq = 1000;
  const emit = async (frame) => {
    seq = Math.max(seq, await page.evaluate(() => Math.max(0, ...(window.__frames ?? []).map((f) => f.seq)))) + 1;
    await page.evaluate((f) => window.__lf.native.emit(f), { seq, reset: false, events: [], ...frame });
  };
  await emit({ reset: true, settings: [], events: emptyLanes() });

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
        const answer = await window.__lf.platform.chain[method](...args);
        return { ok: answer instanceof Uint8Array ? [...answer] : (answer ?? null) };
      } catch (e) {
        return { err: String(e.message) };
      }
    };
  });
  /** The calls the probe made, as the fake must have recorded them: the method and every argument. */
  const made = [];
  const call = (method, ...args) => {
    made.push([method, ...args]);
    return page.evaluate(([m, a]) => window.__call(m, ...a), [method, args]);
  };
  /** Start a call without waiting for its answer (`settle` reads it; `settled` whether it has). */
  const start = (key, method, ...args) => {
    made.push([method, ...args]);
    return page.evaluate(
      ([k, m, a]) => {
        window[`${k}Done`] = false;
        window[k] = window.__call(m, ...a).finally(() => (window[`${k}Done`] = true));
      },
      [key, method, args],
    );
  };
  /** A started call's answer; one that never settles fails here, never hangs the probe. */
  const settle = async (key) => {
    await page.waitForFunction((k) => window[`${k}Done`], key, { timeout: 5000 }).catch(() => assert.fail(`${key} never settled`));
    return page.evaluate((k) => window[k], key);
  };
  const settled = (key) => page.evaluate((k) => window[`${k}Done`], key);
  const send = (...commands) => page.evaluate((c) => window.__platform.sendEngine(...c), commands);
  const frames = () => page.evaluate(() => window.__frames);
  const mark = () => page.evaluate(() => window.__frames.length);
  const since = async (m) => (await frames()).slice(m);
  const chainFrames = async (m) => (await since(m)).filter((f) => f.chains);
  /** The timer's frames: meters alone, no chains. */
  const timerFrames = async (m) => (await since(m)).filter((f) => f.unitMeters && !f.chains);
  const lastFrame = async () => (await frames()).filter((f) => f.chains).at(-1);
  const last = async () => (await lastFrame()).chains;
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
  const intervals = () => page.evaluate(() => window.__intervals.size);
  const hold = () =>
    page.evaluate(() => {
      window.__lf.native.chain.holdLoading = new Promise((r) => (window.__releaseLoading = r));
    });
  const unhold = () =>
    page.evaluate(() => {
      window.__lf.native.chain.holdLoading = null;
      window.__releaseLoading();
    });
  const capture = (on) => page.evaluate((v) => (window.__lf.native.chain.capture = v), on);
  /** Each frame decoded as the native feed's would be (`decodeFeedFrame`): its error, or 'ok'. */
  const decoded = (fs) =>
    page.evaluate(
      (list) =>
        list.map(({ at: _at, ...f }) => {
          try {
            window.__platform.decodeFeedFrame(f);
            return 'ok';
          } catch (e) {
            return String(e.message);
          }
        }),
      fs,
    );

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
  await hold();
  await start('__addAmp', 'add', 0, amp, { kind: 'plugin', plugin: AMP }, 0);
  await waitFrames(m, (f) => f.chains && f.chains[0].units.some((u) => u.id === amp && u.state === 'loading'), 'the amp loading');
  assert.equal((await last())[0].units[0].state, 'loading', 'the plate shows loading while the load is held');
  assert.match(refusal(await call('add', 1, amp, { kind: 'echo' }, 0)), /reserved/, 'a reserved id is refused at once, in the other slot too');
  await unhold();
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
  assert.deepEqual(await call('add', 0, synth, { kind: 'synth', params: { instrument: 1 } }, 5), { ok: 'seated' });
  fs = await chainFrames(m);
  console.log('synth placed', fs.map((f) => ids(f.chains, 0)));
  assert.deepEqual(ids(fs[0].chains, 0), [amp, synth], 'loading where asked (clamped to the end)');
  assert.deepEqual(ids(await last(), 0), [synth, amp], 'seated at the head');
  assert.equal(unit(await last(), synth).latency, 128, 'SYNTH reports one quantum');
  assert.equal((await last())[0].latency, 128, 'the chain sums its latencies');
  const plugSynth = await mint();
  assert.deepEqual(await call('add', 0, plugSynth, { kind: 'plugin', plugin: SYNTH }, 1), { ok: 'seated' });
  console.log('after the replace', ids(await last(), 0));
  assert.deepEqual(ids(await last(), 0), [plugSynth, amp], 'the plugin instrument replaced SYNTH at the head');
  assert.equal(unit(await last(), plugSynth).source, true);
  assert.equal((await last())[0].latency, 0);
  assert.deepEqual(await call('add', 1, synth, { kind: 'echo' }, 0), { ok: 'seated' }, 'the replaced id is free again');
  assert.deepEqual(await call('remove', synth), { ok: null });

  // ── An effect never goes ahead of a source; a source only heads; a move to another index ──────────
  const echoAhead = await mint();
  m = await mark();
  const ahead = await call('add', 0, echoAhead, { kind: 'echo' }, 0);
  console.log('effect ahead', ahead, trail(await chainFrames(m), echoAhead));
  assert.match(refusal(ahead), /ahead of the source/, 'an effect ahead of a source is refused');
  assert.deepEqual(trail(await chainFrames(m), echoAhead), ['loading', 'gone'], 'refused at activation, its plate gone');
  assert.deepEqual(ids(await last(), 0), [plugSynth, amp]);
  assert.match(refusal(await call('move', amp, 0)), /ahead of the source/, 'a move cannot put an effect ahead');
  assert.match(refusal(await call('move', plugSynth, 1)), /only heads/, 'a source cannot leave the head');
  assert.deepEqual(await call('add', 0, echoAhead, { kind: 'echo' }, 2), { ok: 'seated' }, 'a refused add frees its id');
  assert.deepEqual(ids(await last(), 0), [plugSynth, amp, echoAhead]);
  m = await mark();
  assert.deepEqual(await call('move', echoAhead, 1), { ok: null });
  assert.deepEqual(ids(await last(), 0), [plugSynth, echoAhead, amp], 'a move to another index reorders');
  assert.equal((await chainFrames(m)).length, 1, 'in one frame');
  assert.deepEqual(await call('move', echoAhead, 2), { ok: null });
  assert.deepEqual(ids(await last(), 0), [plugSynth, amp, echoAhead], 'and back');

  // ── A ninth unit: refused at once, or at activation when only activation tells a source ───────────
  const synthHead = await mint();
  assert.deepEqual(await call('add', 1, synthHead, { kind: 'synth' }, 0), { ok: 'seated' });
  const seven = [];
  for (const kind of ['echo', 'reverb', 'ring', 'ott', 'echo', 'reverb', 'ring']) {
    const id = await mint();
    seven.push(id);
    assert.deepEqual(await call('add', 1, id, { kind, bypass: true }, 99), { ok: 'seated' });
  }
  assert.equal((await last())[1].units.length, 8);
  // A full chain headed by a source: a plugin may resolve as the source that replaces it (§4).
  const plugHead = await mint();
  m = await mark();
  assert.deepEqual(await call('add', 1, plugHead, { kind: 'plugin', plugin: SYNTH }, 4), { ok: 'seated' }, 'a plugin instrument replaces the source of a full chain');
  fs = await chainFrames(m);
  console.log('full-chain replace', fs.map((f) => f.chains[1].units.length), await decoded(fs));
  assert.deepEqual(ids(await last(), 1), [plugHead, ...seven], 'at the head, SYNTH gone');
  assert.ok(fs.some((f) => f.chains[1].units.length === 9), 'the install shows loading beside the eight it may replace');
  assert.ok((await decoded(fs)).every((r) => r === 'ok'), 'every frame of it decodes');
  const ampNine = await mint();
  m = await mark();
  assert.match(refusal(await call('add', 1, ampNine, { kind: 'plugin', plugin: AMP }, 8)), /holds 8 units/, 'an effect there is a ninth: refused at activation');
  assert.deepEqual(trail(await chainFrames(m), ampNine), ['loading', 'gone']);
  // A full chain with no source head: a ninth is refused at once, whatever its kind.
  assert.deepEqual(await call('remove', plugHead), { ok: null });
  const eighth = await mint();
  assert.deepEqual(await call('add', 1, eighth, { kind: 'echo', bypass: true }, 99), { ok: 'seated' });
  const eight = [...seven, eighth];
  const ninth = await mint();
  m = await mark();
  const refused = await call('add', 1, ninth, { kind: 'echo' }, 0);
  console.log('ninth', refused);
  assert.match(refusal(refused), /holds 8 units/, 'a ninth unit is refused');
  assert.match(refusal(await call('add', 1, ninth, { kind: 'plugin', plugin: SYNTH }, 0)), /holds 8 units/, 'a plugin too, with no source to replace');
  assert.equal((await last())[1].units.length, 8);
  assert.equal((await chainFrames(m)).length, 0, 'refused at once: no plate, no frame');
  m = await mark();
  for (const id of eight) assert.deepEqual(await call('remove', id), { ok: null });
  assert.deepEqual(trail(await chainFrames(m), eight[0]).slice(0, 2), ['leaving', 'gone'], 'a removal goes through leaving');
  assert.deepEqual(ids(await last(), 1), []);

  // ── A hold in `waiting`: a scripted take holds an edit that moves the alignment ───────────────────
  await emit({ events: [{ Lane: { frame: 0, lane: 0, info: lane('Recording') } }] });
  const look = await mint();
  m = await mark();
  assert.deepEqual(await call('add', 0, look, { kind: 'plugin', plugin: LOOKAHEAD }, 3), { ok: 'waiting' }, 'a latent unit waits for the take');
  let c = await last();
  console.log('held', { held: c[0].held, latency: c[0].latency, state: unit(c, look).state });
  assert.equal(unit(c, look).state, 'waiting');
  assert.equal(c[0].held, true);
  assert.equal(c[0].latency, 0, 'the waiting unit is not in the sum yet');
  const quiet = await mint();
  assert.deepEqual(await call('add', 1, quiet, { kind: 'ring' }, 0), { ok: 'seated' }, 'a zero-latency effect in the other slot does not wait');
  await emit({ events: [{ Lane: { frame: 0, lane: 0, info: lane('Playing') } }] });
  c = await last();
  assert.equal(unit(c, look).state, 'seated', 'the take ended: the held edit applies');
  assert.equal(c[0].held, false);
  assert.equal(c[0].latency, 64);
  assert.deepEqual(trail(await chainFrames(m), look).filter((s, i, a) => s !== a[i - 1]), ['loading', 'waiting', 'seated']);
  // The `capture` seam holds a removal that changes the sum; it settles once it applied.
  await capture(true);
  await start('__rmLook', 'remove', look);
  await page.waitForFunction((id) => window.__lf.native.chain.chains()[0].units.find((u) => u.id === id)?.state === 'leaving', look);
  c = await last();
  assert.equal(unit(c, look).state, 'leaving');
  assert.equal(c[0].held, true);
  await page.waitForTimeout(50);
  assert.equal(await settled('__rmLook'), false, 'a held removal has not settled while the unit is still in');
  await capture(false);
  assert.deepEqual(await settle('__rmLook'), { ok: null });
  assert.equal(unit(await last(), look), undefined, 'released, the removal applies');
  assert.deepEqual(await call('remove', quiet), { ok: null });
  // A held add, then its removal: the engine's port is ordered, so the install is not withdrawn. The unit
  // seats, then leaves; `remove` settles once both applied.
  await capture(true);
  const ghostHeld = await mint();
  m = await mark();
  assert.deepEqual(await call('add', 0, ghostHeld, { kind: 'plugin', plugin: LOOKAHEAD }, 1), { ok: 'waiting' }, 'a latent unit waits for the take');
  await start('__rmHeldAdd', 'remove', ghostHeld);
  await page.waitForFunction((id) => window.__lf.native.chain.chains()[0].units.find((u) => u.id === id)?.state === 'leaving', ghostHeld);
  c = await last();
  assert.deepEqual([c[0].held, c[0].latency, unit(c, ghostHeld).held], [true, 0, true], 'during the take: held, the unit not in the sum');
  await page.waitForTimeout(50);
  assert.equal(await settled('__rmHeldAdd'), false, 'the removal waits behind the held add');
  await capture(false);
  assert.deepEqual(await settle('__rmHeldAdd'), { ok: null });
  assert.equal(unit(await last(), ghostHeld), undefined, 'the final chain lacks the unit');
  assert.deepEqual(
    trail(await chainFrames(m), ghostHeld).filter((s, i, a) => s !== a[i - 1]),
    ['loading', 'waiting', 'leaving', 'gone'],
    'the held add was not withdrawn: it left through the removal',
  );
  // A held move is judged again when it applies: its unit was replaced by the held edit before it.
  const srcB = await mint();
  const lookB = await mint();
  assert.deepEqual(await call('add', 1, srcB, { kind: 'plugin', plugin: SYNTH }, 0), { ok: 'seated' });
  assert.deepEqual(await call('add', 1, lookB, { kind: 'plugin', plugin: LOOKAHEAD }, 1), { ok: 'seated' });
  await capture(true);
  const synthRep = await mint();
  assert.deepEqual(await call('add', 1, synthRep, { kind: 'synth' }, 2), { ok: 'waiting' }, 'SYNTH replacing a zero-latency source moves the sum');
  m = await mark();
  await start('__heldMove', 'move', srcB, 0);
  await waitFrames(m, (f) => f.chains && unit(f.chains, srcB)?.held === true, 'the queued move published at once, its unit held');
  await start('__heldRm', 'remove', lookB);
  await page.waitForTimeout(50);
  assert.deepEqual([await settled('__heldMove'), await settled('__heldRm')], [false, false], 'both wait behind the held add');
  await capture(false);
  const heldMove = await settle('__heldMove');
  console.log('held move after the replace', heldMove);
  assert.match(refusal(heldMove), /not seated/, 'the move of the replaced source is refused when it applies');
  assert.deepEqual(await settle('__heldRm'), { ok: null });
  assert.deepEqual(ids(await last(), 1), [synthRep], 'one source, at the head');
  assert.deepEqual(await call('remove', synthRep), { ok: null });

  // ── A scripted failure, and a park ─────────────────────────────────────────────────────────────────
  const broken = await mint();
  await page.evaluate((id) => window.__lf.native.chain.failures.set(id, 'the DLL did not load'), broken);
  m = await mark();
  const failed = await call('add', 0, broken, { kind: 'plugin', plugin: AMP }, 9);
  console.log('scripted failure', failed, trail(await chainFrames(m), broken));
  assert.deepEqual(failed, { err: 'the DLL did not load' });
  assert.deepEqual(trail(await chainFrames(m), broken), ['loading', 'failed'], 'the failed plate stays in its place');
  c = await last();
  assert.deepEqual([unit(c, broken).error, ids(c, 0)], ['the DLL did not load', [plugSynth, amp, echoAhead, broken]]);
  assert.equal(c[0].latency, 0, 'no seat: not in the sum');
  m = await mark();
  assert.deepEqual(await call('remove', broken), { ok: null }, 'the player removes the plate');
  assert.deepEqual(trail(await chainFrames(m), broken), ['gone']);
  await page.evaluate((id) => window.__lf.native.chain.park(id, 'came back an instrument'), amp);
  c = await last();
  console.log('parked', unit(c, amp));
  assert.deepEqual([unit(c, amp).state, unit(c, amp).error], ['failed', 'came back an instrument']);
  assert.deepEqual(ids(c, 0), [plugSynth, amp, echoAhead], 'parked in its place');
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
  assert.deepEqual(await call('add', 1, echoB, { kind: 'echo', bypass: true }, 0), { ok: 'seated' });
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
  assert.deepEqual(await call('add', 1, synthB, { kind: 'synth', params: { gainLead: 3, instrument: 9 } }, 2), { ok: 'seated' });
  assert.deepEqual(ids(await last(), 1), [synthB, echoB], 'SYNTH heads slot B');
  const synthParams = unit(await last(), synthB).params;
  assert.deepEqual([synthParams.gainLead, synthParams.instrument, synthParams.gainBass], [1.5, 5, 1], 'built clamped, the rest at defaults');
  m = await mark();
  await send({ SetUnitParam: [synthB, 'gainPad', -2] });
  await waitFrames(m, (f) => f.chains && f.chains[1].units[0].params.gainPad === 0, 'SYNTH level clamped');
  for (const id of [synthB, echoB]) assert.deepEqual(await call('remove', id), { ok: null });

  // ── D29 by unit, across both slots ────────────────────────────────────────────────────────────────
  const amp2 = await mint();
  assert.deepEqual(await call('add', 0, amp2, { kind: 'plugin', plugin: AMP }, 9), { ok: 'seated' });
  assert.deepEqual(await call('openEditor', amp), { ok: null });
  assert.match(refusal(await call('openEditor', amp2)), new RegExp(`belongs to unit ${amp}$`), 'a second instance in the same chain has no editor');
  assert.deepEqual(await call('move', amp2, 1), { ok: null });
  assert.deepEqual(ids(await last(), 0), [plugSynth, amp2, amp, echoAhead]);
  assert.deepEqual(await call('openEditor', amp), { ok: null }, 'a move keeps the owner');
  assert.match(refusal(await call('openEditor', amp2)), /belongs to unit/);
  const ampB = await mint();
  assert.deepEqual(await call('add', 1, ampB, { kind: 'plugin', plugin: AMP }, 0), { ok: 'seated' });
  assert.match(refusal(await call('openEditor', ampB)), new RegExp(`belongs to unit ${amp}$`), 'the other slot cannot claim the file');
  assert.deepEqual(await call('closeEditor', amp), { ok: null });
  assert.deepEqual(await call('listParams', amp2), { ok: [] });
  assert.deepEqual(await call('setParameter', amp2, 3, 0.5), { ok: null });
  assert.deepEqual(await call('takeTone', amp2), { ok: null }, 'no tone loaded: none to take');
  // Unordered: two loads of one file in flight at once, one per slot: neither can be ordered.
  const [lookA, lookC] = [await mint(), await mint()];
  await hold();
  await start('__lookA', 'add', 0, lookA, { kind: 'plugin', plugin: LOOKAHEAD }, 9);
  await start('__lookC', 'add', 1, lookC, { kind: 'plugin', plugin: LOOKAHEAD }, 9);
  await page.waitForFunction(() => window.__lf.native.chain.chains().flatMap((c) => c.units).filter((u) => u.state === 'loading').length === 2);
  await unhold();
  assert.deepEqual([await settle('__lookA'), await settle('__lookC')], [{ ok: 'seated' }, { ok: 'seated' }]);
  for (const id of [lookA, lookC]) assert.match(refusal(await call('openEditor', id)), /unknown unit/, 'unordered: refused for both');
  for (const id of [lookA, lookC]) assert.deepEqual(await call('remove', id), { ok: null });

  // ── Tones (§8) and reload ─────────────────────────────────────────────────────────────────────────
  const ampTone = tone(AMP, 'Crunch', [1, 2, 3]);
  let t = await call('importTone', 0, ampTone, AMP, amp);
  console.log('importTone', t);
  assert.deepEqual(t.ok.matches, [amp, amp2], 'its own unit first, then the others in chain order');
  assert.equal(t.ok.name, 'Crunch', 'the name the tone file stores');
  const tokenA = t.ok.token;
  assert.ok(Number.isInteger(tokenA));
  t = await call('importTone', 1, ampTone, AMP, amp);
  assert.deepEqual(t.ok.matches, [amp, ampB], 'its own unit wherever it is seated, then its slot’s');
  assert.deepEqual(await call('forgetTone', t.ok.token), { ok: null });
  assert.deepEqual(await call('remove', ampB), { ok: null });
  assert.match(refusal(await call('importTone', 0, [1, 2, 3], AMP)), /cannot be read/, 'malformed bytes are refused');
  const corrupt = ampTone.slice();
  corrupt[corrupt.length - 10] ^= 1; // a state byte: the checksum no longer matches
  assert.match(refusal(await call('importTone', 0, corrupt, AMP)), /checksum/);
  assert.match(refusal(await call('importTone', 0, tone(LOOKAHEAD, 'L', [1]), AMP)), /belongs to L/, 'a tone of another plugin is refused');
  // A tone with no match is kept as the slot's file; the next unit of that plugin there starts from it.
  const lookTone = tone(LOOKAHEAD, 'Tight', [9, 9]);
  assert.deepEqual((await call('importTone', 1, lookTone, LOOKAHEAD)).ok, { token: null, name: 'Tight', matches: [] });
  const lookD = await mint();
  assert.deepEqual(await call('add', 1, lookD, { kind: 'plugin', plugin: LOOKAHEAD }, 0), { ok: 'seated' });
  assert.deepEqual((await call('takeTone', lookD)).ok, tone(LOOKAHEAD, 'Fake Lookahead', [9, 9]), 'it started from the slot’s file');
  // A token is bound to its plugin.
  assert.match(refusal(await call('add', 1, await mint(), { kind: 'plugin', plugin: LOOKAHEAD }, 0, tokenA)), /belongs to vst3/);
  assert.match(refusal(await call('reload', amp2, AMP, 0, 999)), /not parked/);
  // Reload: through `loading`, the tone applied; `moved` when re-added since; `failed` leaves the old seated.
  const seqNow = (await lastFrame()).seq;
  m = await mark();
  assert.deepEqual(await call('reload', amp2, AMP, seqNow, tokenA), { ok: 'reloaded' });
  assert.deepEqual(trail(await chainFrames(m), amp2), ['loading', 'seated']);
  assert.deepEqual((await call('takeTone', amp2)).ok, tone(AMP, 'Fake Amp', [1, 2, 3]), 'the reload applied the tone');
  const token2 = (await call('importTone', 0, ampTone, AMP, amp2)).ok.token;
  assert.deepEqual(await call('reload', amp2, AMP, 0, token2), { ok: 'moved' }, 'added after `since`: moved');
  await page.evaluate((id) => window.__lf.native.chain.failures.set(id, 'no'), amp2);
  assert.deepEqual(await call('reload', amp2, AMP, seqNow, token2), { ok: 'failed' });
  assert.equal(unit(await last(), amp2).state, 'seated', 'a failed reload leaves the old seated');
  assert.deepEqual(await call('forgetTone', token2), { ok: null });
  // The old seat stays applied while its replacement loads; a latency change waits for the take.
  const lookE = await mint();
  assert.deepEqual(await call('add', 0, lookE, { kind: 'plugin', plugin: LOOKAHEAD }, 9), { ok: 'seated' });
  assert.equal((await last())[0].latency, 64);
  const tokenL = (await call('importTone', 0, tone(LOOKAHEAD, 'L', [5]), LOOKAHEAD, lookE)).ok.token;
  await hold();
  m = await mark();
  await start('__reloadL', 'reload', lookE, LOOKAHEAD, (await lastFrame()).seq, tokenL);
  await waitFrames(m, (f) => f.chains && unit(f.chains, lookE)?.state === 'loading', 'the reload loading');
  assert.equal((await last())[0].latency, 64, 'the old seat counts while the new one loads');
  assert.equal(unit(await last(), lookE).reloading, true, 'the plate says its old instance plays on, applied');
  await unhold();
  assert.deepEqual(await settle('__reloadL'), { ok: 'reloaded' });
  assert.equal(unit(await last(), lookE).reloading, undefined, 'landed: no longer reloading');
  const tokenToAmp = (await call('importTone', 0, ampTone, AMP)).ok.token;
  await capture(true);
  m = await mark();
  await start('__reloadAmp', 'reload', lookE, AMP, (await lastFrame()).seq, tokenToAmp);
  await waitFrames(m, (f) => f.chains && unit(f.chains, lookE)?.state === 'waiting', 'the reseat waiting');
  c = await last();
  assert.deepEqual([c[0].latency, c[0].held, await settled('__reloadAmp')], [64, true, false], 'a reseat that moves the sum waits for the take');
  assert.equal(unit(c, lookE).reloading, true, 'waiting, the old instance still applied');
  await capture(false);
  assert.deepEqual(await settle('__reloadAmp'), { ok: 'reloaded' });
  c = await last();
  assert.deepEqual([c[0].latency, unit(c, lookE).plugin, unit(c, lookE).name], [0, AMP, 'Fake Amp']);
  // Orphaned: the owner's instance gone while others hold the file: refused for every unit.
  assert.deepEqual(await call('remove', amp), { ok: null });
  assert.match(refusal(await call('openEditor', amp2)), new RegExp(`unit ${amp}'s previous load`), 'orphaned');
  for (const id of [amp2, lookE, lookD]) assert.deepEqual(await call('remove', id), { ok: null });

  // ── A warm rig, read back from a reset frame; `since` against its seq ─────────────────────────────
  const rig = [await mint(), await mint(), await mint()];
  assert.deepEqual(await call('add', 1, rig[0], { kind: 'plugin', plugin: AMP, bypass: true }, 0), { ok: 'seated' });
  assert.deepEqual(await call('add', 1, rig[1], { kind: 'echo', params: { echoTime: 0, echoLevel: 0.3, echoFeedback: 0.2 } }, 1), { ok: 'seated' });
  assert.deepEqual(await call('add', 1, rig[2], { kind: 'echo', params: { echoTime: 3, echoLevel: 0.8 } }, 2), { ok: 'seated' });
  const restoredOf = (cs) => cs[1].units.map(({ id, kind, bypass, params, plugin }) => ({ id, kind, bypass, params, plugin }));
  const rigRecords = [
    { id: rig[0], kind: 'plugin', bypass: true, params: undefined, plugin: AMP },
    { id: rig[1], kind: 'echo', bypass: false, params: { echoTime: 0, echoFeedback: 0.2, echoLevel: 0.3 }, plugin: undefined },
    { id: rig[2], kind: 'echo', bypass: false, params: { echoTime: 3, echoFeedback: 0.4, echoLevel: 0.8 }, plugin: undefined },
  ];
  m = await mark();
  await emit({ reset: true, settings: [], events: emptyLanes() });
  const resetSeq = seq;
  const reset = (await since(m)).find((f) => f.reset);
  assert.ok(reset?.chains, 'a reset frame carries the chains');
  console.log('restored', JSON.stringify(restoredOf(reset.chains)));
  assert.deepEqual(restoredOf(reset.chains), rigRecords);
  assert.deepEqual(reset.unitMeters.map((u) => u.unit), [plugSynth, echoAhead, ...rig], 'and a meter per seated unit');
  const fresh = await mint();
  assert.deepEqual(await call('add', 1, fresh, { kind: 'plugin', plugin: AMP }, 9), { ok: 'seated' });
  assert.ok((await lastFrame()).seq > resetSeq, 'the fake numbers on from the reset frame’s seq');
  const tokenR = (await call('importTone', 1, ampTone, AMP)).ok.token;
  assert.deepEqual(await call('reload', fresh, AMP, resetSeq, tokenR), { ok: 'moved' }, 'added after the reset frame: moved');
  assert.deepEqual(await call('reload', rig[0], AMP, resetSeq, tokenR), { ok: 'reloaded' }, 'a unit from before it reloads');
  for (const id of [...rig, fresh]) assert.deepEqual(await call('remove', id), { ok: null });

  // ── The meters: one per seated unit; the OTT face's on its own 60 Hz timer ─────────────────────────
  // the ECHO left on in slot A sits bypassed here: an ECHO's meters are their own checks (below)
  await send({ SetUnitBypass: [echoAhead, true] });
  await page.waitForFunction((id) => window.__lf.native.chain.chains()[0].units.find((u) => u.id === id).bypass, echoAhead);
  const seatedIds = () => page.evaluate(() => window.__lf.native.chain.chains().flatMap((c) => c.units.filter((u) => u.state === 'seated' || u.state === 'leaving').map((u) => u.id)));
  assert.deepEqual((await lastFrame()).unitMeters.map((u) => u.unit), await seatedIds(), 'a chains frame carries a meter per seated unit');
  assert.ok((await lastFrame()).unitMeters.every((u) => u.out[0] === 0 && u.out[1] === 0 && u.bands === undefined), 'silent, no bands, with no OTT');
  assert.equal(await intervals(), 0, 'no OTT: no timer');
  const ott = await mint();
  m = await mark();
  assert.deepEqual(await call('add', 1, ott, { kind: 'ott' }, 0), { ok: 'seated' });
  assert.equal(await intervals(), 1, 'an OTT seated and on: one timer');
  await page.waitForTimeout(1000);
  let meters = await timerFrames(m);
  const span = (meters.at(-1).at - meters[0].at) / 1000;
  const rate = (meters.length - 1) / span;
  console.log(`unitMeters: ${meters.length} frames over ${span.toFixed(2)} s = ${rate.toFixed(1)} Hz; first`, JSON.stringify(meters[0].unitMeters));
  assert.ok(rate > 45 && rate < 75, `about 60 Hz (${rate.toFixed(1)})`);
  const everyone = await seatedIds();
  for (const f of meters) {
    assert.deepEqual(f.unitMeters.map((u) => u.unit), everyone, 'a meter per seated unit on every timer frame');
    for (const u of f.unitMeters.filter((x) => x.unit !== ott)) assert.deepEqual(u, { unit: u.unit, out: [0, 0] }, 'the others read silent');
    const mt = f.unitMeters.find((u) => u.unit === ott);
    assert.equal(mt.out[0], mt.out[1]);
    assert.equal(mt.bands.length, 3);
    for (const b of mt.bands) {
      assert.ok(b.gainMin <= b.gain && b.gain <= b.gainMax && b.gainMin >= -12 && b.gainMax <= 12, JSON.stringify(b));
      assert.ok(b.level >= -120 && b.level <= b.levelPeak, JSON.stringify(b));
    }
  }
  assert.ok(meters.some((f) => f.unitMeters.find((u) => u.unit === ott).bands.some((b) => Math.abs(b.gain) > 0.5)), 'the simulated part moves the gains');
  const quietFor = async (ms, what) => {
    const q = await mark();
    await page.waitForTimeout(ms);
    assert.equal((await timerFrames(q)).length, 0, what);
    assert.equal(await intervals(), 0, `${what}: no live interval`);
  };
  await send({ SetUnitBypass: [ott, true] });
  await page.waitForFunction((id) => window.__lf.native.chain.chains()[1].units.find((u) => u.id === id).bypass, ott);
  await quietFor(300, 'silent while the OTT is bypassed');
  const restMeter = (await lastFrame()).unitMeters.find((u) => u.unit === ott);
  assert.ok(restMeter.bands.every((b) => b.gain === 0 && b.level === -120), 'a bypassed OTT reads its bands at rest');
  await send({ Action: { ToggleUnit: ott } });
  m = await mark();
  await waitFrames(m, (f) => f.unitMeters && !f.chains, 'meters again once on');
  assert.equal(await intervals(), 1);
  await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
  await quietFor(150, 'the timer switched off with an OTT on');
  await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
  assert.equal(await intervals(), 1, 'on again');
  assert.deepEqual(await call('remove', ott), { ok: null });
  await quietFor(300, 'silent once the OTT is gone');

  // The timer off: one frame per tick, following the params.
  await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
  const ticks = async (params, n) => {
    const id = await mint();
    assert.deepEqual(await call('add', 1, id, { kind: 'ott', params }, 0), { ok: 'seated' });
    const q = await mark();
    await page.waitForTimeout(150);
    assert.equal((await timerFrames(q)).length, 0, 'no timer frame while the timer is off');
    assert.equal(await intervals(), 0);
    for (let k = 0; k < n; k++) await page.evaluate(() => window.__lf.native.chain.meterTick());
    const got = (await timerFrames(q)).map((f) => f.unitMeters.find((u) => u.unit === id));
    assert.equal(got.length, n, 'one frame per tick');
    assert.deepEqual(await call('remove', id), { ok: null });
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
  // TIME scales the release: a slow one holds the cut longer, so more of it shows over the same frames.
  const fast = await ticks({ ottDepth: 1, ottUpward: 0, ottTime: 0 }, 120);
  const slow = await ticks({ ottDepth: 1, ottUpward: 0, ottTime: 1 }, 120);
  const held = (bs) => bs.reduce((s, b) => s - b.gain, 0) / bs.length;
  console.log('TIME 0 mean cut', held(fast).toFixed(3), 'TIME 1 mean cut', held(slow).toFixed(3));
  assert.ok(held(slow) > held(fast) + 0.1, 'TIME 1 holds more cut than TIME 0');
  // The ECHO's meters: `out` per channel from a plucked note on each bar's downbeat and its repeats, through
  // the unit's own params at the scripted tempo, one frame per tick while the timer is off.
  const echoTicks = async (params, n) => {
    const id = await mint();
    assert.deepEqual(await call('add', 1, id, { kind: 'echo', params }, 0), { ok: 'seated' });
    const q = await mark();
    for (let k = 0; k < n; k++) await page.evaluate(() => window.__lf.native.chain.meterTick());
    const got = (await timerFrames(q)).map((f) => f.unitMeters.find((u) => u.unit === id));
    assert.equal(got.length, n, 'one ECHO frame per tick');
    assert.deepEqual(await call('remove', id), { ok: null });
    return got.map((u) => u.out);
  };
  /** Frame `f` (from 1) holds the output over ((f - 1) / 60, f / 60]: a repeat at `s` seconds rises in the
   * frame after its onset's, above the frame before it. */
  const rises = (outs, s) => {
    const f = Math.floor(s * 60) + 1;
    return outs[f][0] > outs[f - 2][0] * 1.5 + 0.02;
  };
  const e8 = await echoTicks({ echoTime: 1, echoFeedback: 0.5, echoLevel: 1 }, 150);
  console.log('ECHO 1/8 at 120, frames 1, 15-17, 30-32:', [0, 14, 15, 16, 29, 30, 31].map((i) => e8[i][0].toFixed(3)).join(' '));
  assert.ok(e8.every(([l, r]) => Number.isFinite(l) && l >= 0 && l <= 1 && Math.abs(r - l * 0.94) < 1e-9), 'L in 0..1, R a touch under it');
  assert.ok(e8[0][0] > 0.4, `the note on the downbeat: ${e8[0][0]}`);
  assert.ok(rises(e8, 0.25) && rises(e8, 0.5) && rises(e8, 0.75), 'a repeat every 1/8 (0.25 s at 120)');
  assert.ok(rises(e8, 2), 'the next bar’s note');
  const peak = (outs, s) => Math.max(...outs.slice(Math.floor(s * 60), Math.floor(s * 60) + 4).map((o) => o[0]));
  const ratio = peak(e8, 0.5) / peak(e8, 0.25);
  assert.ok(ratio > 0.4 && ratio < 0.65, `each repeat FEEDBACK (0.5) of the one before: ${ratio.toFixed(3)}`);
  const e4 = await echoTicks({ echoTime: 0, echoFeedback: 0.5, echoLevel: 1 }, 60);
  assert.ok(!rises(e4, 0.25) && rises(e4, 0.5), 'the engine’s index 0 is 1/4: no repeat at 0.25 s, one at 0.5 s');
  const e16 = await echoTicks({ echoTime: 3, echoFeedback: 0.5, echoLevel: 1 }, 30);
  assert.ok(rises(e16, 0.125), 'index 3 is 1/16: a repeat at 0.125 s');
  const fb0 = await echoTicks({ echoTime: 1, echoFeedback: 0, echoLevel: 1 }, 60);
  assert.ok(rises(fb0, 0.25) && !rises(fb0, 0.5), 'FEEDBACK 0: one repeat');
  const dry = await echoTicks({ echoTime: 1, echoFeedback: 0.5, echoLevel: 0 }, 60);
  assert.ok(!rises(dry, 0.25) && dry[0][0] === e8[0][0], 'LEVEL 0: the note alone');
  assert.deepEqual(await echoTicks({ echoTime: 1, echoFeedback: 0.5, echoLevel: 1 }, 150), e8, 'deterministic: the same params, the same frames');
  await emit({ events: [{ Transport: { frame: 0, master: 0, bpm: 60, meter: { num: 4, den: 4, beats: 4 }, locked: false } }] });
  const at60 = await echoTicks({ echoTime: 1, echoFeedback: 0.5, echoLevel: 1 }, 60);
  assert.ok(!rises(at60, 0.25) && rises(at60, 0.5), 'the scripted tempo: 1/8 at 60 BPM is 0.5 s');
  await emit({ reset: true, settings: [], events: emptyLanes() });
  const afterReset = await echoTicks({ echoTime: 1, echoFeedback: 0.5, echoLevel: 1 }, 60);
  assert.ok(!rises(afterReset, 0.25) && rises(afterReset, 0.5), 'a reset without a Transport keeps the scripted tempo (60 BPM)');
  await emit({ events: [{ Transport: { frame: 0, master: 0, bpm: 120, meter: { num: 4, den: 4, beats: 4 }, locked: false } }] });
  // the timer runs while an ECHO is seated and on, and stops when it is bypassed
  await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
  const echoOn = await mint();
  assert.deepEqual(await call('add', 1, echoOn, { kind: 'echo' }, 0), { ok: 'seated' });
  assert.equal(await intervals(), 1, 'an ECHO seated and on: one timer');
  await send({ SetUnitBypass: [echoOn, true] });
  await page.waitForFunction((id) => window.__lf.native.chain.chains()[1].units.find((u) => u.id === id).bypass, echoOn);
  await quietFor(200, 'silent while the ECHO is bypassed');
  assert.deepEqual((await lastFrame()).unitMeters.find((u) => u.unit === echoOn).out, [0, 0], 'a bypassed ECHO reads silent');
  assert.deepEqual(await call('remove', echoOn), { ok: null });
  await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
  // A frame scripted through `emit` reaches the feed as scripted.
  const scriptedMeter = { unit: 77, out: [0.5, 0.25], bands: [0, 1, 2].map((i) => ({ gain: 0 - i, gainMin: -1 - i, gainMax: 0 - i, level: -30, levelPeak: -20 })) };
  m = await mark();
  await emit({ unitMeters: [scriptedMeter] });
  assert.deepEqual((await since(m)).find((f) => f.unitMeters)?.unitMeters, [scriptedMeter]);
  await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));

  // ── A cold rig from a reset frame; a reset settles what waited and what was loading ────────────────
  const ottLive = await mint();
  assert.deepEqual(await call('add', 1, ottLive, { kind: 'ott' }, 0), { ok: 'seated' });
  assert.equal(await intervals(), 1);
  const lookF = await mint();
  await capture(true);
  assert.deepEqual(await call('add', 1, lookF, { kind: 'plugin', plugin: LOOKAHEAD }, 1), { ok: 'waiting' });
  await start('__strandMove', 'move', ottLive, 0);
  const stale = await mint();
  await hold();
  m = await mark();
  await start('__staleAdd', 'add', 0, stale, { kind: 'echo' }, 9);
  await waitFrames(m, (f) => f.chains && unit(f.chains, stale)?.state === 'loading', 'the stale add loading');
  const cold = [0x7000_0001, 0x7000_0002, 0x7000_0003];
  const coldRig = [
    { units: [], latency: 0, held: false },
    {
      units: [
        { id: cold[0], kind: 'plugin', plugin: LOOKAHEAD, bypass: true, source: false, name: 'Fake Lookahead', latency: 64, state: 'seated' },
        { id: cold[1], kind: 'echo', params: { echoTime: 0, echoFeedback: 0.2, echoLevel: 0.3 }, bypass: false, source: false, name: 'ECHO', latency: 0, state: 'seated' },
        { id: cold[2], kind: 'echo', params: { echoTime: 3, echoFeedback: 0.4, echoLevel: 0.8 }, bypass: false, source: false, name: 'ECHO', latency: 0, state: 'seated' },
      ],
      latency: 64,
      held: false,
    },
  ];
  await emit({ reset: true, settings: [], events: emptyLanes(), chains: coldRig });
  await unhold();
  await capture(false);
  const [strand, staleAnswer] = [await settle('__strandMove'), await settle('__staleAdd')];
  console.log('across the reset', strand, staleAnswer);
  assert.match(refusal(strand), /reset/, 'a held move is settled, refused, by the reset');
  assert.match(refusal(staleAnswer), /reset/, 'a call still loading answers refused, never seated');
  assert.deepEqual(await page.evaluate(() => window.__lf.native.chain.chains()), coldRig, 'the model is the cold rig');
  assert.equal(await intervals(), 1, 'its OTT gone; its two ECHOs on hold the one timer');
  assert.match(refusal(await call('add', 0, cold[1], { kind: 'echo' }, 0)), /reserved/, 'its ids are reserved');
  m = await mark();
  await send({ Action: { ToggleUnit: cold[0] } }, { SetUnitParam: [cold[2], 'echoLevel', 0.5] });
  await waitFrames(m, (f) => f.chains && unit(f.chains, cold[2])?.params.echoLevel === 0.5, 'the batch on the cold rig');
  assert.equal(unit(await last(), cold[0]).bypass, false, 'its plugin takes the batch');
  m = await mark();
  await emit({ reset: true, settings: [], events: emptyLanes() });
  const back = (await since(m)).find((f) => f.reset).chains;
  assert.deepEqual(restoredOf(back), [
    { id: cold[0], kind: 'plugin', bypass: false, params: undefined, plugin: LOOKAHEAD },
    { id: cold[1], kind: 'echo', bypass: false, params: { echoTime: 0, echoFeedback: 0.2, echoLevel: 0.3 }, plugin: undefined },
    { id: cold[2], kind: 'echo', bypass: false, params: { echoTime: 3, echoFeedback: 0.4, echoLevel: 0.5 }, plugin: undefined },
  ], 'the next reset frame reads it back');
  for (const id of cold) assert.deepEqual(await call('remove', id), { ok: null });

  // ── A scripted `failed` plate: parked only when it says so; else a failed add (no seat, no latency, no
  // source, its id free), as the decoder and the UI read it ─────────────────────────────────────────
  const [ghost, after] = [0x7100_0001, 0x7100_0002];
  const ghostRig = [
    {
      units: [
        { id: ghost, kind: 'plugin', plugin: SYNTH, bypass: false, source: true, name: 'Fake Synth', latency: 128, state: 'failed', error: 'the DLL did not load' },
        { id: after, kind: 'echo', params: { echoTime: 0, echoFeedback: 0.2, echoLevel: 0.3 }, bypass: false, source: false, name: 'ECHO', latency: 0, state: 'seated' },
      ],
      latency: 0,
      held: false,
    },
    { units: [], latency: 0, held: false },
  ];
  await emit({ reset: true, settings: [], events: emptyLanes(), chains: ghostRig });
  m = await mark();
  await send({ Action: { ToggleUnit: after } });
  await waitFrames(m, (f) => f.chains && unit(f.chains, after)?.bypass === true, 'the batch on the scripted rig');
  c = await last();
  assert.equal(c[0].latency, 0, 'a failed add has no seat in the sum');
  assert.deepEqual(await call('move', after, 0), { ok: null }, 'no source heads the chain: an effect may go to the front');
  assert.deepEqual(ids(await last(), 0), [after, ghost]);
  m = await mark();
  assert.deepEqual(await call('remove', ghost), { ok: null });
  assert.deepEqual(trail(await chainFrames(m), ghost), ['gone'], 'it goes at once, no removal through leaving');
  assert.deepEqual(await call('remove', after), { ok: null });

  // ── Every ChainHost call recorded in order, with every argument ───────────────────────────────────
  const recorded = await page.evaluate(() => window.__lf.native.chain.calls.map((c) => c.map((a) => (a instanceof Uint8Array ? [...a] : a))));
  console.log(`${recorded.length} calls recorded`);
  assert.equal(recorded.length, made.length);
  recorded.forEach((r, k) => assert.deepEqual(r, made[k], `call ${k}`));

  // ── The stereo pick over four canned inputs ───────────────────────────────────────────────────────
  assert.equal(await page.evaluate(() => window.__lf.native.inputs.length), 4);
  const pick = (slot, p) => page.evaluate(([s, v]) => window.__lf.platform.engine.setSlotInputChannel(s, v).then(() => 'ok', (e) => e.message), [slot, p]);
  assert.equal(await pick(0, [2, 3]), 'ok');
  assert.equal(await pick(1, null), 'ok');
  assert.equal(await pick(1, 0), 'ok');
  assert.match(await pick(0, 4), /lacks/);
  assert.match(await pick(0, [0, 4]), /lacks/);
  assert.deepEqual(await page.evaluate(() => window.__lf.native.slotInputChannels), [[0, [2, 3]], [1, null], [1, 0]]);
  // With no device open the pick is refused, as native refuses it, and nothing is recorded.
  const closed = await page.evaluate(async () => {
    const engine = window.__lf.platform.engine;
    await engine.close();
    const why = await engine.setSlotInputChannel(0, 1).then(() => 'ok', (e) => e.message);
    await engine.open(window.__lf.native.opened.at(-1));
    return why;
  });
  assert.equal(closed, 'no audio device is open', 'a pick after close() is refused');
  assert.deepEqual(await page.evaluate(() => window.__lf.native.slotInputChannels), [[0, [2, 3]], [1, null], [1, 0]], 'and not recorded');

  // ── The Tauri and production hosts refuse the chain without logging ──────────────────────────────
  const refusals = await page.evaluate(async () => {
    const { tauriPlatform } = await import('/src/platform/host.tauri.ts');
    const { inertPlatform } = await import('/src/platform/host.inert.ts');
    const why = (p) => p.then(() => 'answered', (e) => e.message);
    return [
      tauriPlatform.chain.available,
      await why(tauriPlatform.chain.add(0, 5, { kind: 'echo' }, 0)),
      inertPlatform.chain.available,
      await why(inertPlatform.chain.move(5, 0)),
    ];
  });
  console.log('refusals', refusals);
  assert.deepEqual(refusals.slice(0, 2), [false, 'The native chain host is off (enable lf.chainNative in DEV).']);
  assert.deepEqual(refusals.slice(2), [false, 'Not available in the browser build.']);

  // ── Under Tauri the unit commands never reach `input_send` (the IPC spied) ────────────────────────
  // A second instance of the platform module, evaluated with Tauri's globals present, over a spy.
  const ipc = await page.evaluate(async () => {
    const invokes = [];
    window.__TAURI_INTERNALS__ = {
      transformCallback: () => 1,
      unregisterCallback() {},
      invoke: async (cmd, args) => {
        invokes.push([cmd, cmd === 'input_send' ? args.items : cmd === 'engine_set_slot_input_channel' ? args : null]);
        return cmd === 'midi_subscribe' ? 1 : null;
      },
    };
    globalThis.isTauri = true;
    let tauri;
    try {
      tauri = await import('/src/platform/index.ts?under-tauri');
    } finally {
      delete globalThis.isTauri;
    }
    try {
      tauri.subscribeMidi(() => {});
      const unitOnly = await tauri.sendEngine(
        { SetUnitBypass: [5, true] },
        { SetUnitParam: [5, 'echoLevel', 1] },
        { Action: { ToggleUnit: 5 } },
        { Action: { Toggle: { Units: 'echo' } } },
        { ActionOn: [2, { ToggleUnit: 5 }] },
      );
      const mixed = await tauri.sendEngine({ SetUnitBypass: [5, true] }, 'PlayAll');
      const direct = await tauri.platform.input.send(1, [{ engine: { SetUnitParam: [5, 'echoLevel', 1] } }]).then(() => 'sent', (e) => e.message);
      await tauri.platform.engine.setSlotInputChannel(0, [0, 1]);
      return { kind: tauri.platform.kind, available: tauri.platform.chain.available, unitOnly, mixed, direct, invokes };
    } finally {
      delete window.__TAURI_INTERNALS__;
    }
  });
  console.log('under Tauri', JSON.stringify(ipc));
  assert.deepEqual([ipc.kind, ipc.available], ['tauri', false]);
  assert.deepEqual([ipc.unitOnly, ipc.mixed], [false, false], 'a call that carried a unit command resolves false');
  assert.equal(ipc.direct, 'The native chain host is off (enable lf.chainNative in DEV).', 'the IPC boundary refuses a batch with one');
  assert.deepEqual(ipc.invokes.filter(([cmd]) => cmd === 'input_send').map(([, items]) => items), [[{ engine: 'PlayAll' }]], 'zero unit commands invoked: only the other command left');
  assert.deepEqual(ipc.invokes.filter(([cmd]) => cmd === 'engine_set_slot_input_channel'), [['engine_set_slot_input_channel', { slot: 0, channel: [0, 1] }]],
    'the stereo pick goes to the native engine as the one input-pick command, its slot and pair whole');

  // ── Full-page Tauri boot, reload & ChainHost IPC spies (flag on vs flag off) ────────────────────
  const AMP_DESC = { ...AMP, name: 'Fake Amp', isEffect: true };
  const LOOK_DESC = { ...LOOKAHEAD, name: 'Fake Lookahead', isEffect: true };
  const SAVED_AMP = { format: AMP.format, path: AMP.path, id: AMP.id, name: AMP_DESC.name };

  /** Runs before the app on every load in the Tauri spy pages: answers boot and chain_* IPC. */
  function installTauriBootSpy({ descriptors, resetChains }) {
    const invokes = [];
    const callbacks = new Map();
    const listeners = new Map();
    let nextCb = 1;
    let nextEvent = 1;
    const status = {
      backend: 'Wasapi',
      sampleRate: 48000,
      block: 128,
      inputName: 'Fake In',
      outputName: 'Fake Out',
      alignFrames: 256,
      inputFrames: 128,
      inputOpen: true,
    };
    const lanes = [0, 1, 2, 3, 4].map((lane) => ({
      Lane: {
        frame: 0,
        lane,
        info: { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 },
      },
    }));
    window.__tauriInvokes = invokes;
    window.__tauriEmitEvent = (event, payload) => {
      for (const { id, handler } of listeners.get(event) ?? []) {
        callbacks.get(handler)?.({ event, id, payload });
      }
    };
    window.__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener() {} };
    window.__TAURI_INTERNALS__ = {
      transformCallback(fn) {
        const id = nextCb++;
        callbacks.set(id, fn);
        return id;
      },
      unregisterCallback(id) {
        callbacks.delete(id);
      },
      invoke: async (cmd, args, options) => {
        const isRawBytes = args instanceof Uint8Array;
        invokes.push({
          cmd,
          args: cmd === 'engine_feed' || cmd === 'midi_subscribe' ? null : isRawBytes ? [...args] : args,
          isRawBytes,
          headers: options?.headers ?? null,
        });
        if (cmd === 'host_init') return 7;
        if (cmd === 'midi_subscribe') return 1;
        if (cmd === 'plugin:event|listen') {
          const id = nextEvent++;
          const list = listeners.get(args.event) ?? [];
          list.push({ id, handler: args.handler });
          listeners.set(args.event, list);
          return id;
        }
        if (cmd === 'plugin:event|unlisten') {
          const list = listeners.get(args.event);
          if (list) listeners.set(args.event, list.filter((e) => e.id !== args.eventId));
          return null;
        }
        if (cmd === 'plugin_asio_status') return { status: 'not-compiled', detail: '' };
        if (cmd === 'plugin_list_input_devices' || cmd === 'plugin_list_output_devices' || cmd === 'plugin_list_loaded' || cmd === 'plugin_list_params') return [];
        if (cmd === 'engine_open' || cmd === 'engine_status') return status;
        if (cmd === 'engine_feed') {
          const cb = callbacks.get(args.channel.id);
          const frame = {
            seq: 1,
            reset: true,
            catchUp: false,
            events: lanes,
            settings: [],
            device: [],
            anchor: null,
            meter: null,
            peaks: [],
            scope: null,
            tuner: null,
            ...(resetChains ? { chains: resetChains } : {}),
          };
          setTimeout(() => cb?.({ index: 0, message: frame }), 0);
          return null;
        }
        if (cmd === 'engine_feed_ack') return false;
        if (cmd === 'plugin_scan_paths') return descriptors.filter((d) => args.paths.includes(d.path));
        if (cmd === 'plugin_scan') return descriptors;
        if (cmd === 'plugin_load') {
          const descriptor = descriptors.find((d) => d.path === args.path && d.id === args.id) ?? descriptors[0];
          return { slot: args.slot, descriptor };
        }
        if (cmd === 'chain_add') {
          if (window.__chainAddHook) return window.__chainAddHook(args);
          return args.at === 0 ? 'seated' : 'waiting';
        }
        if (cmd === 'chain_reload') return 'reloaded';
        if (cmd === 'chain_list_params') return [{ id: 3, name: 'Gain', minValue: 0, maxValue: 1, defaultValue: 0.5, value: 0.5 }];
        if (cmd === 'chain_tone_take') return args.unit === 11 ? new Uint8Array([1, 2, 3]).buffer : new ArrayBuffer(0);
        if (cmd === 'chain_tone_import') return { token: args[0] === 9 ? 5 : null, name: 'Fake Amp', matches: args[0] === 9 ? [11] : [] };
        return null;
      },
    };
    globalThis.isTauri = true;
  }

  // 1) Flag on: page reload with two chain plugins — host_init runs, no plugin_unload, no plugin_load,
  // and the UI draws the reset frame's chains; then spy every ChainHost call, chain_tone_import's raw
  // body and headers, unit commands on input_send, and Toggle.Units dropped before the batch.
  const twoPluginChains = [
    {
      units: [
        { id: 0x101, kind: 'plugin', plugin: AMP, bypass: false, source: false, name: 'Fake Amp', latency: 64, state: 'seated' },
        { id: 0x102, kind: 'plugin', plugin: LOOKAHEAD, bypass: false, source: false, name: 'Fake Lookahead', latency: 128, state: 'seated' },
      ],
      latency: 192,
      held: false,
    },
    { units: [], latency: 0, held: false },
  ];
  const { page: chainPage } = await open({
    viewport: { width: 1920, height: 1080 },
    storage: {
      'lf.chainNative': '1',
      'lf.rigRecall': JSON.stringify([SAVED_AMP, null]),
    },
    init: (p) => p.addInitScript(installTauriBootSpy, { descriptors: [AMP_DESC, LOOK_DESC], resetChains: twoPluginChains }),
  });
  await chainPage.waitForFunction(() => window.__tauriInvokes?.some((i) => i.cmd === 'plugin_scan'), undefined, { timeout: 5000 });
  const initialLoadInvokes = await chainPage.evaluate(() => window.__tauriInvokes.map((i) => i.cmd));
  assert.equal(initialLoadInvokes.filter((c) => c === 'plugin_load').length, 0, 'initial boot skips recallRig (no plugin_load)');
  // Reload with the same two chain plugins on the host's reset frame.
  await chainPage.reload();
  await chainPage.waitForFunction(() => '__lf' in window && window.__tauriInvokes?.some((i) => i.cmd === 'plugin_scan'), undefined, { timeout: 10_000 });
  await chainPage.waitForSelector('.unit[data-unit="257"]', { timeout: 5000 });
  await chainPage.waitForSelector('.unit[data-unit="258"]', { timeout: 5000 });
  const reloadCheck = await chainPage.evaluate(() => ({
    kind: window.__lf.platform.kind,
    available: window.__lf.platform.chain.available,
    invokes: window.__tauriInvokes,
    chainIds: window.__lf.chainRow()[0].units.map((u) => u.id),
    plateNames: [...document.querySelectorAll('.unit')].map((el) => el.querySelector('.unit__name')?.textContent?.trim()),
  }));
  console.log('reload with two chain plugins', JSON.stringify(reloadCheck.invokes.map((i) => i.cmd)), reloadCheck.plateNames);
  assert.deepEqual([reloadCheck.kind, reloadCheck.available], ['tauri', true]);
  assert.ok(reloadCheck.invokes.some((i) => i.cmd === 'host_init'), 'reload boot invokes host_init');
  assert.equal(reloadCheck.invokes.filter((i) => i.cmd === 'plugin_unload').length, 0, 'reload with chain plugins invokes no plugin_unload');
  assert.equal(reloadCheck.invokes.filter((i) => i.cmd === 'plugin_load').length, 0, 'reload skips recallRig (no plugin_load)');
  assert.deepEqual(reloadCheck.chainIds, [0x101, 0x102], 'reset frame chains decoded into chain state');
  assert.deepEqual(reloadCheck.plateNames, ['Fake Amp', 'Fake Lookahead'], 'UI draws the reset frame chains');

  const ipcOn = await chainPage.evaluate(async ([amp]) => {
    localStorage.removeItem('lf.chainNative');
    const availableAfterClear = window.__lf.platform.chain.available;
    window.__tauriInvokes.length = 0;
    const plat = await import('/src/platform/index.ts');
    const chain = window.__lf.platform.chain;
    const added0 = await chain.add(0, 11, { kind: 'plugin', plugin: amp }, 0);
    const added1 = await chain.add(1, 12, { kind: 'echo', params: { echoLevel: 0.4 }, bypass: true }, 1, 99);
    await chain.remove(11);
    const reloaded = await chain.reload(11, amp, 42, 55);
    await chain.move(12, 0);
    await chain.openEditor(11);
    await chain.closeEditor(11);
    const closedUnits = [];
    const stopClosed = chain.onEditorClosed((u) => closedUnits.push(u));
    await new Promise((r) => setTimeout(r, 0));
    window.__tauriEmitEvent('chain:editor-closed', { unit: 11 });
    stopClosed();
    await new Promise((r) => setTimeout(r, 0));
    window.__tauriEmitEvent('chain:editor-closed', { unit: 12 });
    const params = await chain.listParams(11);
    await chain.setParameter(11, 3, 0.75);
    const toneSome = await chain.takeTone(11);
    const toneNone = await chain.takeTone(12);
    const nonAscii = { format: 'vst3', path: 'C:\\Fak\u00e9\\Amp.vst3', id: 'fake.amp' };
    const impWithUnit = await chain.importTone(0, new Uint8Array([9, 8, 7]), nonAscii, 11);
    const impNoUnit = await chain.importTone(1, new Uint8Array([4, 5]), amp);
    await chain.forgetTone(5);

    const unitOk = await plat.sendEngine(
      { SetUnitBypass: [5, true] },
      { SetUnitParam: [5, 'echoLevel', 0.5] },
      { Action: { ToggleUnit: 5 } },
      { ActionOn: [2, { ToggleUnit: 5 }] },
    );
    const kindOnly = await plat.sendEngine(
      { Action: { Toggle: { Units: 'echo' } } },
      { ActionOn: [1, { Toggle: { Units: 'ott' } }] },
    );
    const kindMixed = await plat.sendEngine(
      { Action: { Toggle: { Units: 'echo' } } },
      { SetUnitBypass: [5, false] },
    );
    const directUnit = await window.__lf.platform.input.send(1, [{ engine: { SetUnitBypass: [6, true] } }]).then(() => 'sent', (e) => e.message);
    const directKind = await window.__lf.platform.input.send(1, [{ engine: { Action: { Toggle: { Units: 'echo' } } } }]).then(() => 'sent', (e) => e.message);
    return {
      kind: window.__lf.platform.kind,
      availableAfterClear,
      added0,
      added1,
      reloaded,
      closedUnits,
      params,
      toneSome: toneSome ? [...toneSome] : null,
      toneNone,
      impWithUnit,
      impNoUnit,
      unitOk,
      kindOnly,
      kindMixed,
      directUnit,
      directKind,
      invokes: [...window.__tauriInvokes],
    };
  }, [AMP]);
  console.log('under Tauri chainNative=1', JSON.stringify(ipcOn.invokes.map((i) => i.cmd)));
  assert.deepEqual([ipcOn.kind, ipcOn.availableAfterClear], ['tauri', true], 'lf.chainNative=1 enables tauriChainHost, read once per page');
  assert.equal(ipcOn.added0, 'seated');
  assert.equal(ipcOn.added1, 'waiting');
  assert.equal(ipcOn.reloaded, 'reloaded');
  assert.deepEqual(ipcOn.closedUnits, [11], 'onEditorClosed subscribes to chain:editor-closed and unwraps { unit }');
  assert.deepEqual(ipcOn.params, [{ id: 3, name: 'Gain', minValue: 0, maxValue: 1, defaultValue: 0.5, value: 0.5 }]);
  assert.deepEqual(ipcOn.toneSome, [1, 2, 3]);
  assert.equal(ipcOn.toneNone, null, 'empty chain_tone_take response resolves null');
  assert.deepEqual(ipcOn.impWithUnit, { token: 5, name: 'Fake Amp', matches: [11] });
  assert.deepEqual(ipcOn.impNoUnit, { token: null, name: 'Fake Amp', matches: [] });

  const chainInvokes = ipcOn.invokes.filter((i) => i.cmd.startsWith('chain_'));
  assert.deepEqual(
    chainInvokes.map(({ cmd, args, isRawBytes, headers }) => ({ cmd, args, isRawBytes, headers })),
    [
      { cmd: 'chain_add', args: { slot: 0, unit: 11, spec: { kind: 'plugin', plugin: AMP }, at: 0, toneToken: null, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_add', args: { slot: 1, unit: 12, spec: { kind: 'echo', params: { echoLevel: 0.4 }, bypass: true }, at: 1, toneToken: 99, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_remove', args: { unit: 11, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_reload', args: { unit: 11, plugin: AMP, since: 42, toneToken: 55, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_move', args: { unit: 12, to: 0, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_open_editor', args: { unit: 11, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_close_editor', args: { unit: 11, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_list_params', args: { unit: 11, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_set_param', args: { unit: 11, paramId: 3, value: 0.75, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_tone_take', args: { unit: 11, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      { cmd: 'chain_tone_take', args: { unit: 12, frontendEpoch: 7 }, isRawBytes: false, headers: null },
      {
        cmd: 'chain_tone_import',
        args: [9, 8, 7],
        isRawBytes: true,
        headers: {
          slot: '0',
          plugin: '{"format":"vst3","path":"C:\\\\Fak\\u00e9\\\\Amp.vst3","id":"fake.amp"}',
          epoch: '7',
          unit: '11',
        },
      },
      {
        cmd: 'chain_tone_import',
        args: [4, 5],
        isRawBytes: true,
        headers: {
          slot: '1',
          plugin: JSON.stringify(AMP),
          epoch: '7',
        },
      },
      { cmd: 'chain_tone_forget', args: { token: 5, frontendEpoch: 7 }, isRawBytes: false, headers: null },
    ],
    'every chain_* command name, argument shape, raw tone body and headers match chain_cmds.rs',
  );
  assert.ok(
    ipcOn.invokes.some((i) => i.cmd === 'plugin:event|listen' && i.args.event === 'chain:editor-closed'),
    'onEditorClosed registers chain:editor-closed',
  );
  assert.ok(
    ipcOn.invokes.some((i) => i.cmd === 'plugin:event|unlisten' && i.args.event === 'chain:editor-closed'),
    'onEditorClosed unsubscribe unlistens chain:editor-closed',
  );

  // Rejected cross-slot duplicate add must not overwrite an existing unit's slot queue mapping.
  const crossSlotCheck = await chainPage.evaluate(async ([amp]) => {
    const chain = window.__lf.platform.chain;
    await chain.add(0, 21, { kind: 'plugin', plugin: amp }, 0);
    let releaseSlowAdd;
    const slowAddGate = new Promise((r) => {
      releaseSlowAdd = r;
    });
    window.__chainAddHook = async (args) => {
      if (args.slot === 1 && args.unit === 21) throw new Error('unit 21 is reserved');
      if (args.slot === 0 && args.unit === 22) {
        await slowAddGate;
        return 'seated';
      }
      return 'seated';
    };
    const dupErr = await chain.add(1, 21, { kind: 'plugin', plugin: amp }, 0).then(
      () => null,
      (e) => String(e?.message ?? e),
    );
    window.__tauriInvokes.length = 0;
    const slowAdd = chain.add(0, 22, { kind: 'plugin', plugin: amp }, 1);
    const queuedMove = chain.move(21, 0);
    await new Promise((r) => setTimeout(r, 25));
    const moveStartedEarly = window.__tauriInvokes.some((i) => i.cmd === 'chain_move' && i.args.unit === 21);
    releaseSlowAdd();
    await Promise.all([slowAdd, queuedMove]);
    const moveFinished = window.__tauriInvokes.some((i) => i.cmd === 'chain_move' && i.args.unit === 21);
    window.__chainAddHook = null;
    return { dupErr, moveStartedEarly, moveFinished };
  }, [AMP]);
  assert.ok(crossSlotCheck.dupErr?.includes('reserved'), `rejected duplicate add must reject, got: ${crossSlotCheck.dupErr}`);
  assert.equal(crossSlotCheck.moveStartedEarly, false, 'rejected cross-slot add must leave unit 21 mapped to slot 0 queue');
  assert.equal(crossSlotCheck.moveFinished, true, 'unit 21 move runs once slot 0 finishes');

  assert.equal(ipcOn.unitOk, true, 'unit commands (non-Toggle.Units) succeed under Tauri when lf.chainNative=1');
  assert.deepEqual([ipcOn.kindOnly, ipcOn.kindMixed], [false, false], 'Toggle.Units is dropped before the batch under Tauri');
  assert.equal(ipcOn.directUnit, 'sent', 'tauriInput.send lets unit commands through when lf.chainNative=1');
  assert.notEqual(ipcOn.directKind, 'sent', 'tauriInput.send refuses Toggle.Units before IPC');
  const sentBatchesOn = ipcOn.invokes.filter((i) => i.cmd === 'input_send').map((i) => i.args.items);
  assert.deepEqual(
    sentBatchesOn,
    [
      [
        { engine: { SetUnitBypass: [5, true] } },
        { engine: { SetUnitParam: [5, 'echoLevel', 0.5] } },
        { engine: { Action: { ToggleUnit: 5 } } },
        { engine: { ActionOn: [2, { ToggleUnit: 5 }] } },
      ],
      [{ engine: { SetUnitBypass: [5, false] } }],
      [{ engine: { SetUnitBypass: [6, true] } }],
    ],
    'unit commands reach input_send and zero Toggle.Units are invoked',
  );

  // 2) Flag off: boot recalls the saved amp-sim via plugin_load as today.
  const { page: legacyPage } = await open({
    viewport: { width: 1920, height: 1080 },
    storage: {
      'lf.rigRecall': JSON.stringify([SAVED_AMP, null]),
    },
    init: (p) => p.addInitScript(installTauriBootSpy, { descriptors: [AMP_DESC], resetChains: null }),
  });
  await legacyPage.waitForFunction(() => window.__tauriInvokes?.some((i) => i.cmd === 'plugin_load'), undefined, { timeout: 5000 });
  const legacyBoot = await legacyPage.evaluate(() => ({
    kind: window.__lf.platform.kind,
    available: window.__lf.platform.chain.available,
    invokes: window.__tauriInvokes,
  }));
  console.log('legacy boot invokes', JSON.stringify(legacyBoot.invokes.map((i) => i.cmd)));
  assert.deepEqual([legacyBoot.kind, legacyBoot.available], ['tauri', false]);
  assert.ok(legacyBoot.invokes.some((i) => i.cmd === 'host_init'), 'legacy boot invokes host_init');
  assert.deepEqual(
    legacyBoot.invokes.filter((i) => i.cmd === 'plugin_load').map((i) => i.args),
    [{ slot: 0, path: AMP.path, id: AMP.id, frontendEpoch: 7, toneToken: null }],
    'flag off boot recalls the saved amp-sim via plugin_load',
  );
});
