/**
 * The chain in the slot row and the OTT's face (`src/ui/units/`), driven as a player drives them, on the
 * browser fake's chain (`docs/unit-contract.md` §10, `src/platform/chain-fake.ts`): the row draws each slot's
 * chain from the feed's `chains` while `platform.chain.available` (here: the probe asks for the chain with
 * `__lfChainFake`; every other probe keeps the fixed topology, the native build's look today).
 *
 * - add: each built-in through the `+` menu (ECHO, REVERB, RING MOD, OTT), a synth and a plugin; each is
 *   one `add` call at the chain's end and a plate in the chain's order, its mark beside its name;
 * - the contract's refusals as the slot's tag: a ninth unit, an effect moved ahead of a source;
 * - move: by the plate's menu (keyboard-reachable) and by dragging a plate along the line (past a movement
 *   threshold, with a drop marker; Escape cancels and sends nothing); each a single `move` call, the new
 *   order in the `chains` frame and in the DOM;
 * - bypass: the lamp sends `ToggleUnit`, the plate shows the feed's bypass; remove by the menu;
 * - states on the plate and in its accessible name: `loading` (held by the seam), `waiting` while a take
 *   holds a latency edit (the chain `held`), `failed` with its reason;
 * - the OTT face: a press on the plate's name opens it under the row; DEPTH by the keyboard and by a drag
 *   sends `SetUnitParam` and shows the feed's value; its glass and the plate's miniature follow scripted
 *   `unitMeters` frames (the read-outs, the ladders' pixels, the miniature's bars);
 * - every plate's lamp, name and menu, the `+`, and the face's knobs, lamp and close reachable (in the
 *   window, inside what clips them, not covered) at 1920x1080, 2560x1440, 1000x700 and 960x600 (there a
 *   chain of eight, scrolling along its line).
 *
 * Each check is soft (`check`): a run reports every red one, then fails. `--shots=<dir>` writes the four
 * sizes' screenshots (a chain of four in slot A, the OTT face open, a drag in flight, a failed plate).
 * Cannot see the native engine (it does not speak the chain yet) or any sound.
 * Run: pnpm probe chain-ui
 */
import assert from 'node:assert/strict';
import { arg, probe } from '../harness/probe.ts';

const SHOTS = arg('shots');
const lane = (state) => ({ state, length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
const emptyLanes = () => [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty') } }));
const SIZES = [
  [1920, 1080],
  [2560, 1440],
  [1000, 700],
  [960, 600],
];

const failures = [];
/** The page a check runs on. */
let current = null;
/** A soft check: its name and PASS, or FAIL with why; the probe fails at the end if any did. */
async function check(name, fn) {
  try {
    await fn();
    console.log(`PASS ${name}`);
  } catch (e) {
    failures.push(name);
    // what the page showed when it went red, for the reader of the log
    await current?.screenshot({ path: `logs/chain-ui/red-${failures.length}.png` }).catch(() => {});
    console.log(`FAIL ${name}\n     ${String(e.message).split('\n').slice(0, 4).join('\n     ')}`);
  }
}

await probe(async ({ open }) => {
  for (const [W, H] of SIZES) {
    const first = W === 1920;
    const { page, consumeConsoleErrors } = await open({
      viewport: { width: W, height: H },
      init: (p) =>
        p.addInitScript(() => {
          window.__lfEngineFake = true;
          window.__lfChainFake = true;
        }),
    });
    current = page;
    // a soft check's missing element fails in seconds, never the default half minute
    page.setDefaultTimeout(4000);
    let seq = 10;
    const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
    await emit({ reset: true, settings: [], events: emptyLanes() });
    await page.evaluate(() => {
      let n = 0x100;
      window.__lf.native.chain.mint = () => ++n;
    });
    const chains = () => page.evaluate(() => JSON.parse(JSON.stringify(window.__lf.native.chain.chains())));
    const calls = () => page.evaluate(() => JSON.parse(JSON.stringify(window.__lf.native.chain.calls)));
    const sent = () => page.evaluate(() => JSON.parse(JSON.stringify(window.__lf.native.sent)));
    const ids = async (slot) => (await chains())[slot].units.map((u) => u.id);
    const domIds = (slot) => page.$$eval(`.slot--${slot} .unit`, (els) => els.map((e) => Number(e.dataset.unit)));
    const plate = (id) => page.locator(`.unit[data-unit="${id}"]`);
    const settle = () => page.waitForTimeout(120);
    /** Add through the slot's `+` menu: the menu item whose name starts with `label`. */
    const add = async (slot, label) => {
      await page.locator(`.slot--${slot} .unit-add`).click();
      await page.getByRole('menuitem', { name: new RegExp(`^${label}`) }).first().click();
      await settle();
    };
    const menu = async (id, item) => {
      await plate(id).locator('.plate__menu').click();
      await page.getByRole('menuitem', { name: item, exact: true }).click();
      await settle();
    };
    /** Show slot `slot`'s whole line where the row folded it to a plate. */
    const show = async (slot) => {
      const fold = page.locator(`.slot--${slot} .slot__fold`);
      if (await fold.count()) await fold.click();
      await settle();
    };
    const tag = (slot) => page.locator(`.slot--${slot} .unit-err`).textContent({ timeout: 1000 }).catch(() => null);
    const label = (id) => plate(id).getAttribute('aria-label');
    const shot = (name) => (SHOTS ? page.screenshot({ path: `${SHOTS}/chain-ui-${name}-${W}x${H}.png` }) : null);
    /** Every element of `selector` in the window, inside every box that clips it, and the topmost thing at its
     * centre (a scroller brings it in). */
    const reach = (selector) =>
      page.$$eval(selector, (els) =>
        els.map((el) => {
          el.scrollIntoView({ block: 'nearest', inline: 'nearest' });
          const r = el.getBoundingClientRect();
          const x = r.left + r.width / 2;
          const y = r.top + r.height / 2;
          const inside = r.width > 0 && r.height > 0 && r.left >= 0 && r.top >= 0 && r.right <= innerWidth + 0.5 && r.bottom <= innerHeight + 0.5;
          const hit = document.elementFromPoint(x, y);
          // inside every box that clips it too (a chain's line that cannot scroll hides what passes its end)
          let shown = true;
          for (let p = el.parentElement; p; p = p.parentElement) {
            const cs = getComputedStyle(p);
            if (cs.overflowX === 'visible' && cs.overflowY === 'visible') continue;
            const b = p.getBoundingClientRect();
            if (r.left < b.left - 0.5 || r.right > b.right + 0.5 || r.top < b.top - 0.5 || r.bottom > b.bottom + 0.5) shown = false;
          }
          return { what: el.getAttribute('aria-label') ?? el.className, ok: inside && shown && !!hit && (hit === el || el.contains(hit)) };
        }),
      );
    const unreached = async (selector) => (await reach(selector)).filter((r) => !r.ok).map((r) => r.what);

    // ── add each built-in, then a plugin: one `add` each, at the chain's end, in order ─────────────────
    if (first) {
      await check('add: each built-in through the + menu, one add call each at the chain end', async () => {
        for (const name of ['ECHO', 'REVERB', 'RING MOD', 'OTT']) await add('a', name);
        const c = await chains();
        assert.deepEqual(c[0].units.map((u) => [u.kind, u.state]), [['echo', 'seated'], ['reverb', 'seated'], ['ring', 'seated'], ['ott', 'seated']]);
        const adds = (await calls()).filter((x) => x[0] === 'add');
        assert.deepEqual(adds.map((x) => [x[1], x[3].kind, x[4]]), [[0, 'echo', 0], [0, 'reverb', 1], [0, 'ring', 2], [0, 'ott', 3]]);
        assert.deepEqual(await domIds('a'), await ids(0), 'the DOM draws the chain in its order');
        assert.deepEqual(await page.$$eval('.slot--a .unit .plate__name', (els) => els.map((e) => e.textContent)), ['ECHO', 'REVERB', 'RING MOD', 'OTT']);
        assert.equal(await page.locator('.slot--a .unit[data-kind="ott"] .unit-mini--ott > i').count(), 3, "the OTT plate's miniature");
      });
      await check('bypass: the lamp sends ToggleUnit and the plate shows the feed’s bypass', async () => {
        const echo = (await ids(0))[0];
        const lamp = plate(echo).locator('.plate__lamp');
        assert.equal(await lamp.getAttribute('aria-pressed'), 'true');
        await lamp.click();
        await settle();
        assert.ok((await sent()).some((c) => c.Action?.ToggleUnit === echo), 'ToggleUnit sent');
        assert.equal((await chains())[0].units[0].bypass, true, 'the fake bypassed it');
        assert.equal(await lamp.getAttribute('aria-pressed'), 'false', 'the lamp reads bypassed');
        assert.match(await label(echo), /bypassed/, 'its accessible name says so');
        assert.ok(await plate(echo).evaluate((e) => e.classList.contains('plate--byp')));
        await lamp.click();
        await settle();
        assert.equal((await chains())[0].units[0].bypass, false, 'and back on');
      });
      await check('move by the menu: Move right is one move call, the order in the frame and the DOM', async () => {
        const [a, b, c, d] = await ids(0);
        const before = (await calls()).length;
        await plate(a).locator('.plate__menu').focus();
        await page.keyboard.press('Enter');
        await page.getByRole('menuitem', { name: 'Move right', exact: true }).focus();
        await page.keyboard.press('Enter');
        await settle();
        const made = (await calls()).slice(before);
        assert.deepEqual(made, [['move', a, 1]]);
        assert.deepEqual(await ids(0), [b, a, c, d]);
        assert.deepEqual(await domIds('a'), [b, a, c, d]);
        await menu(a, 'Move left');
        assert.deepEqual(await ids(0), [a, b, c, d], 'Move left back');
      });
      await check('move by drag: past the threshold, a drop marker, one move call; order in the frame and the DOM', async () => {
        const [a, b, c, d] = await ids(0);
        const from = await plate(a).locator('.plate__name').boundingBox();
        const to = await plate(c).boundingBox();
        const before = (await calls()).length;
        await page.mouse.move(from.x + 10, from.y + from.height / 2);
        await page.mouse.down();
        await page.mouse.move(from.x + 13, from.y + from.height / 2);
        assert.equal(await page.locator('.unit-drop').count(), 0, 'under the threshold: no drag');
        await page.mouse.move(to.x + to.width * 0.75, to.y + to.height / 2, { steps: 8 });
        assert.equal(await page.locator('.unit-drop').count(), 1, 'the drop marker shows');
        assert.ok(await plate(a).evaluate((e) => e.classList.contains('unit--dragging')), 'the plate follows the pointer');
        if (SHOTS) await shot('drag');
        await page.mouse.up();
        await settle();
        assert.deepEqual((await calls()).slice(before), [['move', a, 2]], 'one move to after the third');
        assert.deepEqual(await ids(0), [b, c, a, d]);
        assert.deepEqual(await domIds('a'), [b, c, a, d]);
        assert.equal(await page.locator('.unit-drop').count(), 0, 'the marker goes');
      });
      await check('move by drag: Escape cancels, nothing sent', async () => {
        const order = await ids(0);
        const from = await plate(order[0]).locator('.plate__name').boundingBox();
        const to = await plate(order[3]).boundingBox();
        const before = (await calls()).length;
        await page.mouse.move(from.x + 10, from.y + from.height / 2);
        await page.mouse.down();
        await page.mouse.move(to.x + to.width * 0.8, to.y + to.height / 2, { steps: 8 });
        await page.keyboard.press('Escape');
        assert.equal(await page.locator('.unit-drop').count(), 0, 'Escape drops the marker');
        await page.mouse.up();
        await settle();
        assert.deepEqual((await calls()).slice(before), [], 'no call');
        assert.deepEqual(await ids(0), order);
        await plate(order[0]).locator('.plate__menu').click();
        assert.ok(await page.getByRole('menuitem', { name: 'Remove', exact: true }).isVisible(), "the plate's next press opens its menu");
        await page.keyboard.press('Escape');
      });
      await check('the OTT face: a press on the name opens it under the row', async () => {
        const ott = (await chains())[0].units.find((u) => u.kind === 'ott').id;
        await plate(ott).locator('.unit__name').click();
        const face = page.getByRole('dialog', { name: 'OTT, slot A' });
        await face.waitFor({ timeout: 2000 });
        const fb = await face.boundingBox();
        const pb = await plate(ott).boundingBox();
        assert.ok(fb.y > pb.y + pb.height, 'it hangs under the plate');
        assert.equal(await face.getByRole('slider').count(), 4, 'DEPTH, TIME, UP, DOWN');
      });
      await check('the OTT face: DEPTH by the keyboard and by a drag sends SetUnitParam, shows the feed’s value', async () => {
        const ott = (await chains())[0].units.find((u) => u.kind === 'ott').id;
        const depth = page.getByRole('slider', { name: 'OTT depth' });
        await depth.focus();
        await page.keyboard.press('ArrowUp');
        await settle();
        assert.ok((await sent()).some((c) => c.SetUnitParam && c.SetUnitParam[0] === ott && c.SetUnitParam[1] === 'ottDepth' && Math.abs(c.SetUnitParam[2] - 0.51) < 1e-9), 'SetUnitParam ottDepth 0.51');
        assert.equal((await chains())[0].units.find((u) => u.id === ott).params.ottDepth, 0.51, 'the fake took it');
        assert.equal(await depth.getAttribute('aria-valuenow'), '51');
        const kb = await depth.boundingBox();
        await page.mouse.move(kb.x + kb.width / 2, kb.y + kb.height / 2);
        await page.mouse.down();
        await page.mouse.move(kb.x + kb.width / 2, kb.y + kb.height / 2 - 44, { steps: 4 });
        await page.mouse.up();
        await settle();
        const took = (await chains())[0].units.find((u) => u.id === ott).params.ottDepth;
        assert.ok(took > 0.66 && took < 0.76, `a 44 px drag up lifts DEPTH by about 0.2 (took ${took})`);
        assert.equal(await depth.getAttribute('aria-valuenow'), String(Math.round(took * 100)));
        assert.match(await page.locator('.ott__ctl--hero .ott__val').textContent(), new RegExp(`^${Math.round(took * 100)}`), 'the number under it');
      });
      await check('the OTT face: its glass and the plate’s miniature follow scripted unitMeters frames', async () => {
        const ott = (await chains())[0].units.find((u) => u.kind === 'ott').id;
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
        await page.waitForTimeout(100);
        const band = (gain) => ({ gain, gainMin: gain, gainMax: gain, level: -20, levelPeak: -18 });
        await emit({ unitMeters: [{ unit: ott, out: [0.5, 0.5], bands: [band(6), band(-4), band(0)] }] });
        await page.waitForTimeout(150);
        const read = await page.$$eval('.ott__gv', (els) => els.map((e) => `${e.firstChild.textContent}|${e.dataset.dir}`));
        assert.deepEqual(read, ['+6.0|up', '−4.0|dn', '0.0|']);
        // the NOW ladders: band 0's first cell over the 0 dB line lit warm white, band 1's first under it amber
        const px = await page.evaluate(() => {
          const cv = document.querySelector('.ott__canvas');
          const d = cv.width / cv.clientWidth;
          const at = (x, y) => [...cv.getContext('2d').getImageData(Math.round(x * d), Math.round(y * d), 1, 1).data];
          return { up: at(250, 116), dn: at(520, 124), still: at(800, 116) };
        });
        assert.ok(px.up[0] > 230 && px.up[1] > 220 && px.up[2] > 200, `band 0 lifts warm white: ${px.up}`);
        assert.ok(px.dn[0] > 230 && px.dn[1] > 150 && px.dn[1] < 200 && px.dn[2] < 90, `band 1 cuts amber: ${px.dn}`);
        assert.ok(px.still[0] < 40, `band 2 at 0 dB lights no cell: ${px.still}`);
        const g = await plate(ott).locator('.unit-mini--ott').evaluate((e) => [0, 1, 2].map((b) => e.style.getPropertyValue(`--g${b}`)));
        assert.deepEqual(g, ['6', '-4', '0'], "the plate's miniature");
        await emit({ unitMeters: [{ unit: ott, out: [0.5, 0.5], bands: [band(-2), band(3), band(1)] }] });
        await page.waitForTimeout(150);
        assert.deepEqual(await page.$$eval('.ott__gv', (els) => els.map((e) => e.firstChild.textContent)), ['−2.0', '+3.0', '+1.0'], 'the next frame');
      });
      await check('the OTT face: Escape closes it, focus back on the name', async () => {
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await page.getByRole('dialog', { name: 'OTT, slot A' }).count(), 0);
      });
      await check('remove by the menu: the plate goes', async () => {
        const ring = (await chains())[0].units.find((u) => u.kind === 'ring').id;
        await menu(ring, 'Remove');
        await page.waitForTimeout(200);
        assert.ok(!(await ids(0)).includes(ring), 'gone from the chain');
        assert.equal(await plate(ring).count(), 0, 'gone from the DOM');
        assert.ok((await calls()).some((x) => x[0] === 'remove' && x[1] === ring));
      });
      await check('loading: held by the seam, the plate says so; then seated', async () => {
        await page.evaluate(() => {
          window.__lf.native.chain.holdLoading = new Promise((r) => (window.__release = r));
        });
        await add('a', 'Fake Amp');
        const amp = (await ids(0)).at(-1);
        // a red check here never leaves the seam holding every later add
        const release = () =>
          page.evaluate(() => {
            window.__lf.native.chain.holdLoading = null;
            window.__release?.();
          });
        try {
          assert.equal(await plate(amp).getAttribute('data-state'), 'loading');
        assert.equal(await plate(amp).locator('.unit__status').textContent(), 'loading…');
        assert.match(await label(amp), /loading/);
          assert.ok(await plate(amp).locator('.plate__lamp').evaluate((e) => e.classList.contains('is-busy') && e.disabled));
        } finally {
          await release();
        }
        await page.waitForTimeout(200);
        assert.equal(await plate(amp).getAttribute('data-state'), 'seated');
        assert.equal(await plate(amp).locator('.plate__fmt').textContent(), 'VST3', "a plugin's mark is its format");
      });
      await check('a held edit: a latency plugin waits for the take, the plate says so; applied once it ends', async () => {
        await page.evaluate(() => (window.__lf.native.chain.capture = true));
        // a red check here never leaves the take holding every later edit
        const end = async () => {
          await page.evaluate(() => (window.__lf.native.chain.capture = false));
          await emit({ events: [{ Lane: { frame: 0, lane: 0, info: lane('Empty') } }] });
        };
        let la;
        try {
          await add('a', 'Fake Lookahead');
          la = (await ids(0)).at(-1);
        assert.equal((await chains())[0].held, true, 'the chain is held');
        assert.equal(await plate(la).getAttribute('data-state'), 'waiting');
        assert.equal(await plate(la).locator('.unit__status').textContent(), 'waits for the take');
        assert.match(await label(la), /waits for the take/);
        if (SHOTS) await shot('held');
        } finally {
          await end();
        }
        await page.waitForTimeout(250);
        assert.equal(await plate(la).getAttribute('data-state'), 'seated', 'applied when the take ends');
      });
      await check('the refusal: a ninth unit comes back as the slot’s tag', async () => {
        while ((await ids(0)).length < 8) await add('a', 'ECHO');
        await add('a', 'REVERB');
        assert.equal((await ids(0)).length, 8);
        assert.equal(await tag('a'), 'chain full: 8 units');
        consumeConsoleErrors('[chain] add refused');
      });
      await check('a failed plate: the reason on the plate and in its name; removed by hand', async () => {
        // make room, then script the next add's failure
        for (const id of (await chains())[0].units.filter((u) => u.kind === 'echo').map((u) => u.id)) await menu(id, 'Remove');
        await page.waitForTimeout(200);
        const next = await page.evaluate(() => {
          const id = window.__lf.native.chain.mint() + 1;
          window.__lf.native.chain.failures.set(id, 'FakeAmp.vst3 crashed on load');
          return id;
        });
        await add('a', 'Fake Amp');
        consumeConsoleErrors('[chain] add refused');
        assert.equal(await plate(next).getAttribute('data-state'), 'failed');
        assert.equal(await plate(next).locator('.unit__status').textContent(), 'failed: FakeAmp.vst3 crashed on load');
        assert.match(await label(next), /failed: FakeAmp\.vst3 crashed on load/);
        assert.equal(await plate(next).locator('.plate__lamp').isDisabled(), true);
        if (SHOTS) await shot('failed');
        await menu(next, 'Remove');
        await page.waitForTimeout(200);
        assert.equal(await plate(next).count(), 0);
        consumeConsoleErrors('[chain] add refused');
      });
      await check('the refusal: an effect moved ahead of the source comes back as the slot’s tag', async () => {
        await show('b');
        await add('b', 'Bass');
        await add('b', 'OTT');
        const [synth, ott] = await ids(1);
        assert.equal((await chains())[1].units[0].source, true, 'the synth heads B');
        assert.equal(await plate(synth).locator('.plate__name').textContent(), 'Bass', "a synth's plate reads its instrument");
        await menu(ott, 'Move left');
        assert.deepEqual(await ids(1), [synth, ott], 'nothing moved');
        assert.equal(await tag('b'), 'the source heads the chain');
        consumeConsoleErrors('[chain] move refused');
      });
    } else {
      // the scene for the other sizes: a chain of four in slot A, a synth in B
      for (const name of ['Fake Amp', 'OTT', 'ECHO', 'REVERB']) await add('a', name);
      // the smallest window holds a full chain: eight units, the line scrolls (the row's last fold)
      if (W === 960) {
        for (const name of ['RING MOD', 'ECHO', 'ECHO', 'REVERB']) await add('a', name);
        assert.equal(await page.locator('.slot--a .unit-line--scroll').count(), 1, 'a chain of eight scrolls at 960x600');
      }
      await show('b');
      await add('b', 'Bass');
      await show('a');
    }

    // ── reach: every plate's lamp, name and menu, the +, the face's controls, at this size ─────────────
    if (first) {
      // the size scene: four units in A
      await show('a');
      for (const id of await ids(0)) await menu(id, 'Remove');
      await page.waitForTimeout(200);
      for (const name of ['Fake Amp', 'OTT', 'ECHO', 'REVERB']) await add('a', name);
    }
    await check(`reach at ${W}x${H}: every plate's lamp, name and menu and the + in both slots`, async () => {
      for (const slot of ['a', 'b']) {
        await show(slot);
        const missed = await unreached(`.slot--${slot} .unit .plate__lamp, .slot--${slot} .unit .unit__name, .slot--${slot} .unit .plate__menu, .slot--${slot} .unit-add`);
        assert.deepEqual(missed, [], `slot ${slot.toUpperCase()} misses: ${missed.join(", ")}`);
        assert.ok((await page.locator(`.slot--${slot} .unit`).count()) >= (slot === 'a' ? 4 : 1), `slot ${slot.toUpperCase()} draws its plates`);
      }
    });
    await show('a');
    if (SHOTS) await shot('chain');
    await check(`reach at ${W}x${H}: the OTT face's knobs, lamp and close`, async () => {
      const ott = (await chains())[0].units.find((u) => u.kind === 'ott').id;
      await plate(ott).locator('.unit__name').click();
      await page.getByRole('dialog', { name: 'OTT, slot A' }).waitFor({ timeout: 2000 });
      await page.waitForTimeout(150);
      if (SHOTS) await shot('ott');
      assert.deepEqual(await unreached('.ott .ott__knob, .ott .ott__pwr, .ott .ott__close'), []);
      await page.locator('.ott__close').click();
      await settle();
    });
    await page.close();
  }
  assert.deepEqual(failures, [], `${failures.length} check(s) red`);
});
