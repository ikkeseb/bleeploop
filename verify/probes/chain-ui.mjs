/**
 * The chain in the slot row and the OTT's face (`src/ui/units/`), driven as a player drives them, on the
 * browser fake's chain (`docs/unit-contract.md` §10, `src/platform/chain-fake.ts`): the row draws each slot's
 * chain from the feed's `chains` while `platform.chain.available` (here: the probe asks for the chain with
 * `__lfChainFake`; every other probe keeps the fixed topology, the native build's look today).
 *
 * - add: each built-in through the `+` menu (ECHO, REVERB, RING MOD, OTT), a synth and a plugin; each is
 *   one `add` call at the chain's end and a plate in the chain's order, its mark beside its name;
 * - the contract's known refusals judged in the row, the slot's tag and no call (a ninth unit, an effect
 *   moved ahead of a source by the menu or a drag, a source moved);
 * - move: by the plate's menu, reached by the keyboard's arrows, focus kept on the moved plate's menu; by
 *   dragging a plate (one move per drop, the same index whatever the pointer's step, slow or fast); a
 *   press released outside before it was a drag, Escape (the pointer let go, a keyboard press after it
 *   not eaten), a lost capture and a plate unmounted mid-drag each leave no drag behind and send nothing;
 * - the plate's menu keeps its focused entry through a `chains` report;
 * - bypass: the lamp sends `ToggleUnit`, the plate shows the feed's bypass; remove by the menu, focus to a
 *   surviving plate's menu;
 * - states on the plate, in its accessible name and said in the slot's live region: `loading`, `waiting`
 *   while a take holds a latency edit, a held removal, `failed` (a failed add) and parked, apart, the full
 *   reason in the plate's menu and its chevron's description; a unit that is not seated offers no
 *   Parameters, no listParams goes out for it, and an open face of a unit that gets parked closes;
 * - LIVE follows the applied head: a synth still loading does not take it away;
 * - the slot's level and the note target in chain mode: `SetSlotGain`, `{ Slot: n }`, never
 *   `SetInstrumentGain`; a plugin's editor window closed by its own close clears the plate's editor press;
 * - the row's popovers hang from their plate (the name's Parameters, the `+` menu);
 * - the OTT face: a press on the plate's name opens it under the row; DEPTH by the keyboard and by a drag
 *   sends `SetUnitParam` and shows the feed's value; its glass (read-outs, NOW ladders, history, level
 *   meters) and the plate's visible miniature follow scripted `unitMeters` frames, and clear when a
 *   complete frame omits the unit;
 * - per size (1920x1080, 2560x1440, 1000x700, 960x600; a chain of four in slot A, at 960 also of eight):
 *   each unit's expected controls present, reached by the pointer (in the window, inside every box that
 *   clips it, topmost) and by Tab; the `+` always in sight; a scrolling line's cue outside the plates and
 *   no mask over their text; the face's legends at 12 px or more and its controls in reach;
 * - with the chain flag off, a `chains` frame changes nothing the UI holds.
 *
 * Each check is soft (`check`): a run reports every red one, then fails. `--shots=<dir>` writes each size's
 * screenshots (`fix2-<scene>-<W>x<H>.png`: the chain of four, the face open with its meters live, a drag in
 * flight, a failed and a parked plate; at 960 the chain of eight). Cannot see the native engine (it does not
 * speak the chain yet) or any sound.
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
    // a red check leaves no button held and no popover open for the next one
    await current?.mouse.up().catch(() => {});
    for (let k = 0; k < 2; k++) await current?.keyboard.press('Escape').catch(() => {});
    await current?.waitForTimeout(150).catch(() => {});
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
    const unitOf = async (kind, slot = 0) => (await chains())[slot].units.find((u) => u.kind === kind)?.id;
    /** What has the focus: its classes and its plate's unit, or `body`. */
    const active = () =>
      page.evaluate(() => {
        const a = document.activeElement;
        if (!a || a === document.body) return 'body';
        const p = a.closest('.unit');
        return `${a.className}${p ? `@${p.dataset.unit}` : ''}`;
      });
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
    const said = (slot) => page.locator(`.unit-said[data-slot="${slot}"]`).textContent();
    const label = (id) => plate(id).getAttribute('aria-label');
    const shot = (name) => (SHOTS ? page.screenshot({ path: `${SHOTS}/fix2-${name}-${W}x${H}.png` }) : null);
    /** The centre of plate `id`'s name, where a drag takes it. */
    const grip = async (id) => {
      const b = await plate(id).locator('.plate__name').boundingBox();
      return { x: b.x + 10, y: b.y + b.height / 2 };
    };
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
    /** The smallest legend or number the face draws, in CSS pixels as rendered (its type times its scale). */
    const faceTypeMin = () =>
      page.evaluate(() => {
        const face = document.querySelector('.ott');
        const scale = face.getBoundingClientRect().width / face.offsetWidth;
        let min = Infinity;
        let what = '';
        for (const el of face.querySelectorAll('*')) {
          const text = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim() !== '');
          if (!text) continue;
          const px = parseFloat(getComputedStyle(el).fontSize) * scale;
          if (px < min) [min, what] = [px, `${el.className}: ${el.textContent.trim()}`];
        }
        return { min, what };
      });

    if (first) {
      // ── add each built-in: one `add` each, at the chain's end, in order ───────────────────────────────
      await check('add: each built-in through the + menu, one add call each at the chain end', async () => {
        for (const name of ['ECHO', 'REVERB', 'RING MOD', 'OTT']) await add('a', name);
        const c = await chains();
        assert.deepEqual(c[0].units.map((u) => [u.kind, u.state]), [['echo', 'seated'], ['reverb', 'seated'], ['ring', 'seated'], ['ott', 'seated']]);
        const adds = (await calls()).filter((x) => x[0] === 'add');
        assert.deepEqual(adds.map((x) => [x[1], x[3].kind, x[4]]), [[0, 'echo', 0], [0, 'reverb', 1], [0, 'ring', 2], [0, 'ott', 3]]);
        assert.deepEqual(await domIds('a'), await ids(0), 'the DOM draws the chain in its order');
        assert.deepEqual(await page.$$eval('.slot--a .unit .plate__name', (els) => els.map((e) => e.textContent)), ['ECHO', 'REVERB', 'RING MOD', 'OTT']);
        assert.equal(await page.locator('.slot--a .unit[data-kind="ott"] .unit-mini--ott > i').count(), 3, "the OTT plate's miniature");
        assert.match(await said(0), /OTT ready/, 'the slot says what came');
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
      await check('move by the menu: the arrows reach Move right, one move call, focus stays on the moved plate', async () => {
        const [a, b, c, d] = await ids(0);
        const before = (await calls()).length;
        await plate(a).locator('.plate__menu').focus();
        await page.keyboard.press('Enter');
        await page.getByRole('menu').waitFor();
        // the first enabled entry has the focus; the arrows walk the rest (Move left is disabled at the head)
        assert.equal(await page.evaluate(() => document.activeElement.textContent), 'Parameters');
        await page.keyboard.press('ArrowDown');
        assert.equal(await page.evaluate(() => document.activeElement.textContent), 'Move right', 'ArrowDown skips the disabled Move left');
        await page.keyboard.press('Enter');
        await settle();
        const made = (await calls()).slice(before);
        assert.deepEqual(made, [['move', a, 1]]);
        assert.deepEqual(await ids(0), [b, a, c, d]);
        assert.deepEqual(await domIds('a'), [b, a, c, d]);
        assert.equal(await active(), `plate__menu slot-pop__trigger@${a}`, "focus stays on the moved plate's menu");
        await menu(a, 'Move left');
        assert.deepEqual(await ids(0), [a, b, c, d], 'Move left back');
      });
      await check('the plate menu keeps its focused entry through a chains report', async () => {
        const [a, b] = await ids(0);
        await plate(a).locator('.plate__menu').focus();
        await page.keyboard.press('Enter');
        await page.keyboard.press('ArrowDown');
        await page.evaluate(() => (document.activeElement.__kept = true));
        // two reports while the menu is open (another plate parked, then reseated)
        await page.evaluate((id) => window.__lf.native.chain.park(id, 'probe'), b);
        await settle();
        await page.evaluate((id) => window.__lf.native.chain.reseat(id), b);
        await settle();
        assert.equal(await page.evaluate(() => document.activeElement.__kept === true && document.activeElement.textContent), 'Move right', 'the same entry, still focused');
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await active(), `plate__menu slot-pop__trigger@${a}`, 'Escape hands focus back to the chevron');
      });

      // ── the drag: one move per drop, judged at the release; every way a press ends leaves nothing behind ──
      for (const steps of [2, 60]) {
        await check(`move by drag (${steps} steps): past the threshold, a marker, one move to the gap released in`, async () => {
          const [a, b, c, d] = await ids(0);
          const from = await grip(a);
          const to = await plate(c).boundingBox();
          const before = (await calls()).length;
          await page.mouse.move(from.x, from.y);
          await page.mouse.down();
          await page.mouse.move(from.x + 3, from.y);
          assert.equal(await page.locator('.unit-drop').count(), 0, 'under the threshold: no drag');
          // 40 % into the third plate: before its middle, so the gap before it
          await page.mouse.move(to.x + to.width * 0.4, to.y + to.height / 2, { steps });
          assert.equal(await page.locator('.unit-drop').count(), 1, 'the drop marker shows');
          assert.ok(await plate(a).evaluate((e) => e.classList.contains('unit--dragging')), 'the plate follows the pointer');
          if (SHOTS && steps === 60) await shot('drag');
          await page.mouse.up();
          await settle();
          assert.deepEqual((await calls()).slice(before), [['move', a, 1]], 'one move, to before the third plate');
          assert.deepEqual(await ids(0), [b, a, c, d]);
          assert.deepEqual(await domIds('a'), [b, a, c, d]);
          assert.equal(await page.locator('.unit-drop').count(), 0, 'the marker goes');
          await menu(a, 'Move left');
          assert.deepEqual(await ids(0), [a, b, c, d]);
        });
      }
      await check('move by drag: a press released outside before it was a drag leaves no press behind', async () => {
        const order = await ids(0);
        const from = await grip(order[0]);
        const before = (await calls()).length;
        await page.mouse.move(from.x, from.y);
        await page.mouse.down();
        await page.mouse.move(from.x, from.y + 300);
        await page.mouse.up();
        await settle();
        // the button is up: crossing the plates again is no drag
        const far = await plate(order[3]).boundingBox();
        await page.mouse.move(from.x, from.y);
        await page.mouse.move(far.x + far.width * 0.8, far.y + far.height / 2, { steps: 10 });
        assert.equal(await page.locator('.unit-drop, .unit--dragging').count(), 0, 'no drag without a button');
        await page.mouse.move(from.x, from.y + 300);
        await settle();
        assert.deepEqual((await calls()).slice(before), [], 'no call');
        assert.deepEqual(await ids(0), order);
      });
      await check('move by drag: Escape cancels, lets the pointer go, sends nothing, eats no keyboard press', async () => {
        const order = await ids(0);
        const from = await grip(order[0]);
        const to = await plate(order[3]).boundingBox();
        const before = (await calls()).length;
        await page.mouse.move(from.x, from.y);
        await page.mouse.down();
        await page.mouse.move(to.x + to.width * 0.8, to.y + to.height / 2, { steps: 8 });
        await page.keyboard.press('Escape');
        assert.equal(await page.locator('.unit-drop').count(), 0, 'Escape drops the marker');
        assert.equal(await plate(order[0]).evaluate((e) => e.hasPointerCapture(1)), false, 'the pointer is let go');
        // released away from the plates: no click of its own to eat
        await page.mouse.move(to.x, to.y + 300);
        await page.mouse.up();
        await settle();
        assert.deepEqual((await calls()).slice(before), [], 'no call');
        assert.deepEqual(await ids(0), order);
        // the next keyboard press on the plate is its own
        await plate(order[0]).locator('.unit__name').focus();
        await page.keyboard.press('Enter');
        await page.getByRole('dialog', { name: 'ECHO parameters' }).waitFor({ timeout: 1500 });
        await page.keyboard.press('Escape');
        await settle();
        await plate(order[0]).locator('.plate__menu').click();
        assert.ok(await page.getByRole('menuitem', { name: 'Remove', exact: true }).isVisible(), "the plate's next press opens its menu");
        await page.keyboard.press('Escape');
      });
      await check('move by drag: a lost pointer capture ends the drag, nothing sent', async () => {
        const order = await ids(0);
        const from = await grip(order[0]);
        const to = await plate(order[2]).boundingBox();
        const before = (await calls()).length;
        await page.mouse.move(from.x, from.y);
        await page.mouse.down();
        await page.mouse.move(to.x + to.width * 0.8, to.y + to.height / 2, { steps: 8 });
        assert.equal(await page.locator('.unit-drop').count(), 1, 'a drag in flight');
        await plate(order[0]).evaluate((e) => e.releasePointerCapture(1));
        await settle();
        assert.equal(await page.locator('.unit-drop, .unit--dragging, .unit-line--drag').count(), 0, 'the capture lost: the drag is gone');
        await page.mouse.up();
        await settle();
        assert.deepEqual((await calls()).slice(before), [], 'no call');
        assert.deepEqual(await ids(0), order);
      });
      await check('move by drag: a plate removed mid-drag leaves no drag behind', async () => {
        await add('a', 'REVERB');
        const order = await ids(0);
        const gone = order.at(-1);
        const from = await grip(gone);
        const to = await plate(order[1]).boundingBox();
        await page.mouse.move(from.x, from.y);
        await page.mouse.down();
        await page.mouse.move(to.x + 4, to.y + to.height / 2, { steps: 8 });
        assert.equal(await page.locator('.unit-line--drag').count(), 1, 'a drag in flight');
        await page.evaluate((id) => window.__lf.native.chain.host.remove(id), gone);
        await page.waitForTimeout(200);
        assert.equal(await plate(gone).count(), 0, 'the plate went');
        assert.equal(await page.locator('.unit-line--drag, .unit-drop').count(), 0, 'no drag left on the line');
        const before = (await calls()).length;
        await page.mouse.up();
        await settle();
        assert.deepEqual((await calls()).slice(before), [], 'no call');
      });

      // ── the OTT face ─────────────────────────────────────────────────────────────────────────────────
      await check('the OTT face: a press on the name opens it under the row', async () => {
        const ott = await unitOf('ott');
        await plate(ott).locator('.unit__name').click();
        const face = page.getByRole('dialog', { name: 'OTT, slot A' });
        await face.waitFor({ timeout: 2000 });
        const fb = await face.boundingBox();
        const pb = await plate(ott).boundingBox();
        assert.ok(fb.y > pb.y + pb.height, 'it hangs under the plate');
        assert.equal(await face.getByRole('slider').count(), 4, 'DEPTH, TIME, UP, DOWN');
      });
      await check('the OTT face: DEPTH by the keyboard and by a drag sends SetUnitParam, shows the feed’s value', async () => {
        const ott = await unitOf('ott');
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
      await check('the OTT face: its glass (read-outs, ladders, history, level meters) and the plate’s miniature follow unitMeters; a complete frame without it clears them', async () => {
        const ott = await unitOf('ott');
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
        await page.waitForTimeout(100);
        const band = (gain) => ({ gain, gainMin: gain, gainMax: gain, level: -20, levelPeak: -18 });
        // a second of the same reading fills the right of each band's history
        for (let i = 0; i < 60; i++) await emit({ unitMeters: [{ unit: ott, out: [0.5, 0.5], bands: [band(6), band(-4), band(0)] }] });
        await page.waitForTimeout(150);
        const read = await page.$$eval('.ott__gv', (els) => els.map((e) => `${e.firstChild.textContent}|${e.dataset.dir}`));
        assert.deepEqual(read, ['+6.0|up', '−4.0|dn', '0.0|']);
        const pixels = () =>
          page.evaluate(() => {
            const cv = document.querySelector('.ott__canvas');
            const d = cv.width / cv.clientWidth;
            const at = (x, y) => [...cv.getContext('2d').getImageData(Math.round(x * d), Math.round(y * d), 1, 1).data];
            // the brightest of a few columns (the history draws 2 px columns on a 3 px pitch)
            const near = (x, y) => [0, 1, 2, 3].map((k) => at(x + k, y)).reduce((a, b) => (b[0] + b[1] > a[0] + a[1] ? b : a));
            // the geometry ott-loop.ts draws: X0 40, the 0 dB line at 120, 7 px a dB; band i's meter at its
            // right edge less 18, its ladder 58 wide left of it, its history left of that
            const W = cv.clientWidth;
            const bx = (i) => 40 + ((W - 6 - 40) / 3) * i;
            const mx = (i) => Math.round(bx(i + 1)) - 12 - 6;
            const lx = (i) => mx(i) - 10 - 58;
            return {
              up: at(250, 116),
              dn: at(520, 124),
              still: at(800, 116),
              hist0: near(lx(0) - 14, 120 - 4 * 7),
              hist1: near(lx(1) - 14, 120 + 3 * 7),
              meterLo: at(mx(0) + 3, 120 + 12 * 7 - 6),
              meterHi: at(mx(0) + 3, 120 - 12 * 7 + 4),
            };
          });
        const px = await pixels();
        assert.ok(px.up[0] > 230 && px.up[1] > 220 && px.up[2] > 200, `band 0's ladder lifts warm white: ${px.up}`);
        assert.ok(px.dn[0] > 230 && px.dn[1] > 150 && px.dn[1] < 200 && px.dn[2] < 90, `band 1's ladder cuts amber: ${px.dn}`);
        assert.ok(px.still[0] < 40, `band 2 at 0 dB lights no cell: ${px.still}`);
        assert.ok(px.hist0[0] > 120 && px.hist0[3] > 0, `band 0's history lit above the line: ${px.hist0}`);
        assert.ok(px.hist1[0] > 120 && px.hist1[2] < px.hist1[0] * 0.7, `band 1's history amber under the line: ${px.hist1}`);
        assert.ok(px.meterLo[0] > 60, `band 0's level meter lit at its foot: ${px.meterLo}`);
        assert.ok(px.meterHi[0] < 40, `and unlit near 0 dBFS: ${px.meterHi}`);
        const mini = await plate(ott).locator('.unit-mini--ott').evaluate((e) => {
          const h = (i, pseudo) => parseFloat(getComputedStyle(e.children[i], pseudo).height);
          const r = e.getBoundingClientRect();
          return { g: [0, 1, 2].map((b) => e.style.getPropertyValue(`--g${b}`)), up0: h(0, '::before'), dn1: h(1, '::after'), up2: h(2, '::before'), seen: r.width > 0 && r.height > 0 };
        });
        assert.deepEqual(mini.g, ['6', '-4', '0'], "the plate's miniature reads the bands");
        assert.ok(mini.seen && mini.up0 > 3 && mini.dn1 > 2 && mini.up2 === 0, `and draws them: ${JSON.stringify(mini)}`);
        await emit({ unitMeters: [{ unit: ott, out: [0.5, 0.5], bands: [band(-2), band(3), band(1)] }] });
        await page.waitForTimeout(150);
        assert.deepEqual(await page.$$eval('.ott__gv', (els) => els.map((e) => e.firstChild.textContent)), ['−2.0', '+3.0', '+1.0'], 'the next frame');
        // a complete frame that omits the OTT (it no longer sounds): no frozen reading
        await emit({ unitMeters: [] });
        await page.waitForTimeout(150);
        assert.deepEqual(await page.$$eval('.ott__gv', (els) => els.map((e) => e.firstChild.textContent)), ['0.0', '0.0', '0.0'], 'the read-outs clear');
        assert.deepEqual(await plate(ott).locator('.unit-mini--ott').evaluate((e) => [0, 1, 2].map((b) => e.style.getPropertyValue(`--g${b}`))), ['0', '0', '0'], 'the miniature clears');
        const cleared = await pixels();
        assert.ok(cleared.hist0[3] === 0 || cleared.hist0[0] < 40, `the history goes dark: ${cleared.hist0}`);
      });
      await check('the OTT face: Escape closes it, focus back on the name', async () => {
        const ott = await unitOf('ott');
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await page.getByRole('dialog', { name: 'OTT, slot A' }).count(), 0);
        assert.equal(await active(), `unit__name slot-pop__trigger@${ott}`);
      });
      await check('the row’s popovers hang from their plate: the name’s Parameters and the + menu', async () => {
        const echo = await unitOf('echo');
        await plate(echo).locator('.unit__name').click();
        const panel = page.getByRole('dialog', { name: 'ECHO parameters' });
        await panel.waitFor();
        const pb = await plate(echo).boundingBox();
        const fb = await panel.boundingBox();
        assert.ok(fb.y >= pb.y + pb.height && fb.y <= pb.y + pb.height + 14, `under the ECHO plate: panel top ${fb.y}, plate bottom ${pb.y + pb.height}`);
        assert.ok(fb.x >= pb.x - 20 && fb.x <= pb.x + pb.width, `beside it: panel left ${fb.x}, plate ${pb.x}..${pb.x + pb.width}`);
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await active(), `unit__name slot-pop__trigger@${echo}`, 'focus back on the name');
        const plus = page.locator('.slot--a .unit-add');
        await plus.click();
        const menuBox = await page.getByRole('menu', { name: 'Add to slot A' }).boundingBox();
        const ab = await plus.boundingBox();
        assert.ok(menuBox.y >= ab.y + ab.height && menuBox.y <= ab.y + ab.height + 14, `under the +: menu top ${menuBox.y}, + bottom ${ab.y + ab.height}`);
        await page.keyboard.press('Escape');
        await settle();
      });
      await check('remove by the menu: the plate goes, focus to a surviving plate', async () => {
        const ring = await unitOf('ring');
        await plate(ring).locator('.plate__menu').focus();
        await page.keyboard.press('Enter');
        await page.keyboard.press('End');
        assert.equal(await page.evaluate(() => document.activeElement.textContent), 'Remove');
        await page.keyboard.press('Enter');
        await page.waitForTimeout(200);
        assert.ok(!(await ids(0)).includes(ring), 'gone from the chain');
        assert.equal(await plate(ring).count(), 0, 'gone from the DOM');
        assert.ok((await calls()).some((x) => x[0] === 'remove' && x[1] === ring));
        assert.match(await active(), /^plate__menu slot-pop__trigger@\d+$/, `focus on a surviving plate's menu (${await active()})`);
        assert.match(await said(0), /RING MOD removed/);
      });

      // ── states, eligibility, the editor window ──────────────────────────────────────────────────────
      await check('loading: the plate says so, offers no Parameters or editor; then seated', async () => {
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
          assert.equal(await plate(amp).locator('.unit__name').isDisabled(), true, 'its name opens nothing yet');
          await plate(amp).locator('.plate__menu').click();
          assert.equal(await page.getByRole('menuitemcheckbox', { name: 'Parameters', exact: true }).isDisabled(), true, 'Parameters waits for the seat');
          assert.equal(await page.getByRole('menuitemcheckbox', { name: 'Editor', exact: true }).isDisabled(), true, 'so does the editor');
          await page.keyboard.press('Escape');
        } finally {
          await release();
        }
        await page.waitForTimeout(200);
        assert.equal(await plate(amp).getAttribute('data-state'), 'seated');
        assert.equal(await plate(amp).locator('.plate__fmt').textContent(), 'VST3', "a plugin's mark is its format");
        assert.ok(!(await calls()).some((x) => x[0] === 'listParams' && x[1] === amp), 'no listParams before the seat');
        assert.match(await said(0), /Fake Amp ready/);
      });
      await check('the editor window: its own close clears the plate’s editor press', async () => {
        const amp = await unitOf('plugin');
        const ed = plate(amp).locator('.plate__ed');
        await ed.click();
        await settle();
        assert.equal(await ed.getAttribute('aria-pressed'), 'true', 'open');
        await page.evaluate((id) => window.__lf.native.chain.closeWindow(id), amp);
        await settle();
        assert.equal(await ed.getAttribute('aria-pressed'), 'false', 'closed by the window');
      });
      await check('a held edit: a latency plugin waits for the take; its Parameters wait; applied once it ends', async () => {
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
          assert.match(await said(0), /Fake Lookahead waits for the take/);
          assert.equal(await plate(la).locator('.unit__name').isDisabled(), true, 'no Parameters while it waits');
        } finally {
          await end();
        }
        await page.waitForTimeout(250);
        assert.equal(await plate(la).getAttribute('data-state'), 'seated', 'applied when the take ends');
        assert.ok(!(await calls()).some((x) => x[0] === 'listParams'), 'no listParams went out for a unit not seated');
      });
      await check('a held removal: the plate says it goes when the take ends, said; then it goes', async () => {
        const la = (await chains())[0].units.find((u) => u.name === 'Fake Lookahead').id;
        await page.evaluate(() => (window.__lf.native.chain.capture = true));
        const end = async () => {
          await page.evaluate(() => (window.__lf.native.chain.capture = false));
          await emit({ events: [{ Lane: { frame: 0, lane: 0, info: lane('Empty') } }] });
        };
        try {
          await menu(la, 'Remove');
          assert.equal(await plate(la).getAttribute('data-state'), 'leaving');
          assert.equal(await plate(la).locator('.unit__status').textContent(), 'removed when the take ends');
          assert.match(await label(la), /removed when the take ends/);
          assert.match(await said(0), /Fake Lookahead removed when the take ends/);
        } finally {
          await end();
        }
        await page.waitForTimeout(250);
        assert.equal(await plate(la).count(), 0, 'gone once the take ended');
        assert.match(await said(0), /Fake Lookahead removed/);
      });
      await check('parked: apart from a failed add, said; an open face closes, focus to the plate’s menu; reseated, said', async () => {
        const ott = await unitOf('ott');
        await plate(ott).locator('.unit__name').click();
        await page.getByRole('dialog', { name: 'OTT, slot A' }).waitFor();
        const sets = (await sent()).filter((c) => c.SetUnitParam).length;
        await page.evaluate((id) => window.__lf.native.chain.park(id, 'the restart failed'), ott);
        await settle();
        assert.equal(await page.getByRole('dialog', { name: 'OTT, slot A' }).count(), 0, 'the face closed');
        assert.equal(await active(), `plate__menu slot-pop__trigger@${ott}`, "focus on the plate's menu");
        assert.equal(await plate(ott).getAttribute('data-state'), 'failed');
        assert.ok(await plate(ott).evaluate((e) => e.classList.contains('unit--parked')), 'drawn parked');
        assert.equal(await plate(ott).locator('.unit__status').textContent(), 'parked: the restart failed');
        assert.match(await label(ott), /parked: the restart failed/);
        assert.match(await said(0), /OTT parked: the restart failed/);
        assert.equal(await plate(ott).locator('.unit__name').isDisabled(), true, 'no face while parked');
        assert.equal((await sent()).filter((c) => c.SetUnitParam).length, sets, 'nothing sent to it');
        if (SHOTS) await shot('parked');
        await page.evaluate((id) => window.__lf.native.chain.reseat(id), ott);
        await settle();
        assert.equal(await plate(ott).getAttribute('data-state'), 'seated');
        assert.match(await said(0), /OTT back/);
      });
      await check('a failed add: struck, its reason on the plate, said, and whole in its menu and the chevron’s description', async () => {
        const reason = 'FakeAmp.vst3 crashed on load: the module returned no factory for its class id';
        const next = await page.evaluate((r) => {
          const id = window.__lf.native.chain.mint() + 1;
          window.__lf.native.chain.failures.set(id, r);
          return id;
        }, reason);
        await add('a', 'Fake Amp');
        consumeConsoleErrors('[chain] add refused');
        assert.equal(await plate(next).getAttribute('data-state'), 'failed');
        assert.ok(!(await plate(next).evaluate((e) => e.classList.contains('unit--parked'))), 'not drawn parked');
        assert.equal(await plate(next).locator('.unit__status').textContent(), `failed: ${reason}`);
        assert.match(await label(next), /failed: FakeAmp\.vst3 crashed on load/);
        assert.match(await said(0), /Fake Amp failed: FakeAmp\.vst3 crashed/);
        assert.equal(await plate(next).locator('.plate__lamp').isDisabled(), true);
        const chevron = plate(next).locator('.plate__menu');
        const described = await chevron.evaluate((e) => document.getElementById(e.getAttribute('aria-describedby'))?.textContent);
        assert.equal(described, `failed: ${reason}`, "the chevron's description holds the whole reason");
        if (SHOTS) await shot('failed');
        await chevron.focus();
        await page.keyboard.press('Enter');
        assert.equal(await page.locator('.slot-menu__note').textContent(), `failed: ${reason}`, 'the menu reads it whole');
        await page.keyboard.press('End');
        await page.keyboard.press('Enter');
        await page.waitForTimeout(200);
        assert.equal(await plate(next).count(), 0);
        consumeConsoleErrors('[chain] add refused');
      });
      await check('known refusals: a ninth unit is the slot’s tag, no add call', async () => {
        while ((await ids(0)).length < 8) await add('a', 'ECHO');
        const before = (await calls()).filter((x) => x[0] === 'add').length;
        await add('a', 'REVERB');
        assert.equal((await ids(0)).length, 8);
        assert.equal(await tag('a'), 'chain full: 8 units');
        assert.equal((await calls()).filter((x) => x[0] === 'add').length, before, 'nothing sent');
      });
      await check('LIVE follows the applied head: a synth still loading leaves it; seated, it goes', async () => {
        await show('b');
        const liveB = page.locator('.slot--b .slot__live');
        assert.equal(await liveB.count(), 1, 'an empty chain takes its input');
        await page.evaluate(() => {
          window.__lf.native.chain.holdLoading = new Promise((r) => (window.__release = r));
        });
        try {
          await add('b', 'Bass');
          assert.equal((await chains())[1].units[0].state, 'loading');
          assert.equal(await liveB.count(), 1, 'LIVE stays while the synth loads');
        } finally {
          await page.evaluate(() => {
            window.__lf.native.chain.holdLoading = null;
            window.__release?.();
          });
        }
        await page.waitForTimeout(200);
        assert.equal(await liveB.count(), 0, 'the seated synth heads the chain: no LIVE');
      });
      await check('known refusals: a source or an effect moved against the source is the slot’s tag, no move call', async () => {
        await add('b', 'OTT');
        const [synth, ott] = await ids(1);
        assert.equal((await chains())[1].units[0].source, true, 'the synth heads B');
        assert.equal(await plate(synth).locator('.plate__name').textContent(), 'Bass', "a synth's plate reads its instrument");
        const moves = async () => (await calls()).filter((x) => x[0] === 'move').length;
        const before = await moves();
        await menu(ott, 'Move left');
        assert.equal(await tag('b'), 'the source heads the chain');
        await menu(synth, 'Move right');
        assert.equal(await tag('b'), 'a source heads its chain');
        // a drag of the OTT ahead of the synth: no marker there, and its drop is refused in the row
        const from = await grip(ott);
        const to = await plate(synth).boundingBox();
        await page.mouse.move(from.x, from.y);
        await page.mouse.down();
        await page.mouse.move(to.x + 4, to.y + to.height / 2, { steps: 8 });
        assert.equal(await page.locator('.unit-drop').count(), 0, 'no marker where the chain refuses');
        await page.mouse.up();
        await settle();
        assert.equal(await tag('b'), 'the source heads the chain');
        assert.deepEqual(await ids(1), [synth, ott], 'nothing moved');
        assert.equal(await moves(), before, 'no move call');
      });
      await check('chain mode: notes to the slot, its level as SetSlotGain, never SetInstrumentGain', async () => {
        await page.evaluate(() => window.__lf.setActiveSlot(1));
        await settle();
        const targets = await page.evaluate(() => JSON.parse(JSON.stringify(window.__lf.native.inputSent)).filter((e) => e.selectTarget));
        assert.deepEqual(targets.at(-1).selectTarget, { slot: 1, target: { Slot: 1 } }, 'B is active: its notes to slot B');
        await page.evaluate(() => window.__lf.setActiveSlot(0));
        await show('a');
        const before = (await sent()).length;
        await page.locator('.slot--a .slot__vol input').focus();
        await page.keyboard.press('ArrowDown');
        await settle();
        const gains = (await sent()).slice(before).filter((c) => c.SetSlotGain);
        assert.ok(gains.length > 0 && gains.at(-1).SetSlotGain[0] === 0 && gains.at(-1).SetSlotGain[1] < 1, `SetSlotGain for A: ${JSON.stringify(gains)}`);
        await emit({ reset: true, settings: [], events: emptyLanes() });
        await settle();
        assert.ok(!(await sent()).some((c) => c.SetInstrumentGain), 'no SetInstrumentGain in chain mode');
        const last = (await sent()).filter((c) => c.SetSlotGain).slice(-2).map((c) => c.SetSlotGain[0]).sort();
        assert.deepEqual(last, [0, 1], 'a reset resends both slots’ levels');
      });
    } else {
      // the scene for the other sizes: a chain of four in slot A, a synth in B
      for (const name of ['Fake Amp', 'OTT', 'ECHO', 'REVERB']) await add('a', name);
      await show('b');
      await add('b', 'Bass');
      await show('a');
    }

    // ── per size: a chain of four in slot A ─────────────────────────────────────────────────────────
    if (first) {
      // the size scene: four units in A
      await show('a');
      for (const id of await ids(0)) await menu(id, 'Remove');
      await page.waitForTimeout(200);
      for (const name of ['Fake Amp', 'OTT', 'ECHO', 'REVERB']) await add('a', name);
    }
    /** Each unit's controls by its state and the row's fold: its lamp, its name (a unit with parameters),
     * its editor glyph (a plugin, not compact) and its menu; every one reached by the pointer and by Tab. */
    const reachUnits = async (slot, idx) => {
      await show(slot);
      const compact = (await page.locator(`.slot--${slot} .unit-line--compact`).count()) > 0;
      const units = (await chains())[idx].units;
      const want = [];
      for (const u of units) {
        const p = `.slot--${slot} .unit[data-unit="${u.id}"]`;
        const expected = [`${p} .plate__lamp`, `${p} .unit__name`, `${p} .plate__menu`];
        if (u.kind === 'plugin' && !compact) expected.push(`${p} .plate__ed`);
        for (const sel of expected) {
          assert.equal(await page.locator(sel).count(), 1, `${u.name} draws ${sel.split(' ').at(-1)}`);
          want.push(sel);
        }
      }
      want.push(`.slot--${slot} .unit-add`);
      const missed = await unreached(want.join(', '));
      assert.deepEqual(missed, [], `slot ${slot.toUpperCase()} misses by the pointer: ${missed.join(', ')}`);
      // by Tab: from the slot's letter, every control comes up in turn
      await page.locator(`.slot--${slot} .slot__id`).focus();
      const seen = new Set();
      for (let k = 0; k < 60; k++) {
        await page.keyboard.press('Tab');
        const at = await page.evaluate((list) => list.findIndex((s) => document.activeElement === document.querySelector(s)), want);
        if (at >= 0) seen.add(want[at]);
        if (seen.size === want.length) break;
      }
      const unseen = want.filter((s) => !seen.has(s));
      assert.deepEqual(unseen, [], `slot ${slot.toUpperCase()} misses by Tab`);
    };
    await check(`reach at ${W}x${H}: each unit's expected controls and the +, by the pointer and by Tab, in both slots`, async () => {
      await reachUnits('a', 0);
      await reachUnits('b', 1);
    });
    await show('a');
    await page.evaluate(() => document.activeElement?.blur());
    if (SHOTS) await shot('chain');
    await check(`the line at ${W}x${H}: no mask over the plates; where it scrolls, its cue outside them and the + outside the scroller`, async () => {
      const line = await page.locator('.slot--a .unit-line').evaluate((e) => ({ mask: getComputedStyle(e).maskImage, scroll: e.classList.contains('unit-line--scroll') }));
      assert.equal(line.mask, 'none', 'nothing dims the plates');
      assert.equal(await page.locator('.slot--a .unit-line .unit-add').count(), 0, 'the + is never inside the line');
      if (!line.scroll) return;
      const cue = await page.evaluate(() => {
        const ln = document.querySelector('.slot--a .unit-line');
        const plates = [...ln.querySelectorAll('.unit')].map((p) => p.getBoundingClientRect());
        const read = () =>
          ['l', 'r'].map((s) => {
            const el = document.querySelector(`.slot--a .unit-more--${s}`);
            const r = el.getBoundingClientRect();
            const lr = ln.getBoundingClientRect();
            return { op: Number(getComputedStyle(el).opacity), outside: r.right <= lr.left + 0.5 || r.left >= lr.right - 0.5 };
          });
        ln.scrollLeft = 0;
        return { plates: plates.length, start: read() };
      });
      assert.ok(cue.start[0].outside && cue.start[1].outside, 'the cues sit outside the scroller');
      await page.waitForTimeout(100);
      const at = (to) =>
        page.evaluate(async (to) => {
          const ln = document.querySelector('.slot--a .unit-line');
          ln.scrollLeft = to === 'end' ? ln.scrollWidth : 0;
          await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
          return ['l', 'r'].map((s) => Number(getComputedStyle(document.querySelector(`.slot--a .unit-more--${s}`)).opacity));
        }, to);
      const [l0, r0] = await at('start');
      assert.ok(l0 < 0.2 && r0 > 0.8, `at the start the cue points right: ${l0}, ${r0}`);
      const [l1, r1] = await at('end');
      assert.ok(l1 > 0.8 && r1 < 0.2, `at the end it points left: ${l1}, ${r1}`);
      await at('start');
    });
    await check(`the OTT face at ${W}x${H}: its legends at 12 px or more, its knobs, lamp and close in reach, meters live`, async () => {
      const ott = await unitOf('ott');
      await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
      await plate(ott).locator('.unit__name').click();
      await page.getByRole('dialog', { name: 'OTT, slot A' }).waitFor({ timeout: 2000 });
      // the glass shows three seconds: let most of them fill for the shot
      await page.waitForTimeout(SHOTS ? 2500 : 600);
      if (SHOTS) await shot('ott');
      const type = await faceTypeMin();
      assert.ok(type.min >= 12 - 1e-6, `the smallest type is ${type.min.toFixed(2)} px (${type.what})`);
      assert.deepEqual(await unreached('.ott .ott__knob, .ott .ott__pwr, .ott .ott__close'), []);
      assert.notEqual(await page.locator('.ott__gv').first().evaluate((e) => e.firstChild.textContent), '0.0', 'the meters run');
      await page.locator('.ott__close').click();
      await settle();
    });
    if (!first) {
      await check(`a drag in flight and a failed and a parked plate at ${W}x${H}`, async () => {
        const [amp, ott, echo] = await ids(0);
        await page.evaluate((id) => window.__lf.native.chain.park(id, 'the restart failed'), ott);
        const next = await page.evaluate(() => {
          const id = window.__lf.native.chain.mint() + 1;
          window.__lf.native.chain.failures.set(id, 'FakeLookahead.dll crashed on load');
          return id;
        });
        await add('a', 'Fake Lookahead');
        consumeConsoleErrors('[chain] add refused');
        assert.equal(await plate(next).getAttribute('data-state'), 'failed');
        assert.ok(await plate(ott).evaluate((e) => e.classList.contains('unit--parked')));
        await page.evaluate(() => document.activeElement?.blur());
        // a narrow line scrolls: each plate in sight for its own shot
        await plate(ott).scrollIntoViewIfNeeded();
        if (SHOTS) await shot('parked');
        await plate(next).scrollIntoViewIfNeeded();
        if (SHOTS) await shot('failed');
        await menu(next, 'Remove');
        await page.evaluate((id) => window.__lf.native.chain.reseat(id), ott);
        await page.waitForTimeout(200);
        await plate(echo).scrollIntoViewIfNeeded();
        await plate(amp).scrollIntoViewIfNeeded();
        const to = await plate(ott).boundingBox();
        const grab = await grip(amp);
        await page.mouse.move(grab.x, grab.y);
        await page.mouse.down();
        await page.mouse.move(to.x + to.width * 0.7, to.y + to.height / 2, { steps: 20 });
        assert.equal(await page.locator('.unit-drop').count(), 1, 'the marker shows');
        if (SHOTS) await shot('drag');
        await page.keyboard.press('Escape');
        await page.mouse.up();
        await settle();
        consumeConsoleErrors('[chain] add refused');
      });
    }
    // the smallest window holds a full chain: eight units, the line scrolls (the row's last fold)
    if (W === 960) {
      for (const name of ['RING MOD', 'ECHO', 'ECHO', 'REVERB']) await add('a', name);
      await check('960x600: a chain of eight scrolls, every unit and the + in reach, the cue outside the plates', async () => {
        assert.equal(await page.locator('.slot--a .unit-line--scroll').count(), 1, 'a chain of eight scrolls at 960x600');
        await reachUnits('a', 0);
        await show('a');
        await page.evaluate(() => {
          document.activeElement?.blur();
          document.querySelector('.slot--a .unit-line').scrollLeft = 0;
        });
        await page.waitForTimeout(100);
        if (SHOTS) await shot('chain8');
        assert.equal(await page.locator('.slot--a .unit-line').evaluate((e) => getComputedStyle(e).maskImage), 'none');
        assert.ok((await page.locator('.slot--a .unit-more--r').evaluate((e) => Number(getComputedStyle(e).opacity))) > 0.8, 'the cue points to the rest');
      });
    }
    await page.close();
  }

  // ── the chain flag off: a `chains` frame changes nothing the UI holds ───────────────────────────────
  {
    const { page } = await open({
      viewport: { width: 1920, height: 1080 },
      init: (p) =>
        p.addInitScript(() => {
          window.__lfEngineFake = true;
        }),
    });
    current = page;
    await check('flag off: the fixed topology, and a chains frame is no state of the row', async () => {
      const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: 1, reset: false, events: [], ...frame });
      await emit({ reset: true, settings: [], events: emptyLanes() });
      const unit = { id: 7, kind: 'echo', params: {}, bypass: false, source: false, name: 'ECHO', latency: 0, state: 'seated' };
      await emit({ chains: [{ units: [unit], latency: 0, held: false }, { units: [], latency: 0, held: false }], unitMeters: [{ unit: 7, out: [0, 0] }] });
      await page.waitForTimeout(100);
      assert.equal(await page.locator('.unit').count(), 0, 'no chain drawn');
      assert.equal(await page.evaluate(() => window.__lf.chainRow()[0].units.length), 0, 'the report is not applied');
    });
    await page.close();
  }
  assert.deepEqual(failures, [], `${failures.length} check(s) red`);
});
