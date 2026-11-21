/**
 * The unit faces as free-standing gear (`src/ui/units/UnitFace.tsx`, `face-place.ts`) and the ECHO's own
 * face (`EchoFace.tsx`, its tape `echo-loop.ts`), driven as a player drives them on the browser fake's chain
 * (`docs/unit-contract.md` §10, `src/platform/chain-fake.ts`), the chain asked for with `__lfChainFake`.
 *
 * - the ECHO face: a press on its plate's name opens it under the plate on its strap (no settings popover);
 *   TIME by the keyboard, a position's press and a drag sends `SetUnitParam` with the engine's index (the
 *   face orders by length, 1/16 1/8 1/8. 1/4; the engine's index 3 1 2 0) and reads the feed's value and its
 *   length at the tempo, a new tempo included; with the engine holding the commands (`holdApply`), a drag
 *   turned back inside one gesture sends its way back, quick keys count on from the last step sent, a wheel
 *   event of several notches is one send, and every read-out stays on the feed once let go; FEEDBACK (0 to
 *   95 %) and LEVEL by the keyboard and a drag, a knob following the hand only while the drag holds it (a
 *   release or a lost capture hands it back to the feed); the power switch is the unit's bypass, and the face
 *   shows it; scripted `unitMeters` frames light the tape and OUT and the trace slides toward the past, and
 *   a frame without the unit clears it; a right-only pulse and the silence after it, both between two draws,
 *   are drawn on R and its ladder alone, dissimilar L and R each on its own half; scaled up while silent, the
 *   glass is drawn again at its size; TIME's printed positions take a press, never the focus;
 * - the move: a drag on the face's panel moves it by the pointer's way, the strap goes, nothing is sent and
 *   the face stays open, its size not pinned (nor by a nudge); it keeps its place per kind (closed and
 *   opened again, it opens there; the OTT's face keeps its own home); a move released near home lands home,
 *   the strap back; with the face focused the arrows nudge it and Home sends it home; the OTT's face moves
 *   the same way; each move check starts from home;
 * - the resize: a corner drag scales the face (its width by the corner's way), shrunk past its base size the
 *   narrow face takes over at the base size and never goes under it, grown it stops at the window; at home
 *   from a plate the window clamps (left: slot A's first; right: slot B's) a small corner drag grows it a
 *   little, never the narrow face; - then + by the keys returns to the full face;
 * - a window resize pulls a moved face back into the window; a window too short for the face under its plate
 *   takes it off the strap, fitted, every control in reach; a focused knob keeps its focus across the reflow;
 * - free-standing: no backdrop, a press beside the face reaches the looper (a lane's record key) and the
 *   transport keys play with the face focused, the face open; it closes by its close, Escape in it or its
 *   name again, never an Escape elsewhere; the OTT's and the ECHO's open at once, the last touched on top, a
 *   strap each at home; the strap follows its plate through a reorder; where the row folds, a fold leaves
 *   the face open where it stood, off its strap, and unfolded it hangs home again;
 * - per size (1920x1080, 2560x1440, 1000x700, 960x600): both faces open at home, every legend at 12 px or
 *   more and none clipped (inside the face and inside every box that clips it), their controls reached by
 *   the pointer, the ECHO's tape live on the fake's timer.
 *
 * `--shots=<dir>` writes the ECHO face open at home and moved at 1920x1080 and 1000x700
 * (`echo-<home|moved>-<W>x<H>.png`), each size's OTT and ECHO at home, and both faces open at once at
 * 1920x1080 (`both-faces-1920x1080.png`). Each check is soft (`check`): a
 * run reports every red one, then fails. Cannot see the native engine (it does not speak the chain yet)
 * or any sound.
 * Run: pnpm probe unit-faces
 */
import assert from 'node:assert/strict';
import { arg, probe } from '../harness/probe.ts';

const SHOTS = arg('shots');
const lane = (state) => ({ state, length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
const emptyLanes = () => [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty') } }));
const transport = (bpm) => ({ Transport: { frame: 0, master: 0, bpm, meter: { num: 4, den: 4, beats: 4 }, locked: false } });
const SIZES = [
  [1920, 1080],
  [2560, 1440],
  [1000, 700],
  [960, 600],
];

const failures = [];
let current = null;
let currentSize = null;
async function check(name, fn) {
  try {
    await fn();
    console.log(`PASS ${name}`);
  } catch (e) {
    failures.push(name);
    await current?.screenshot({ path: `logs/unit-faces/red-${failures.length}.png` }).catch(() => {});
    console.log(`FAIL ${name}\n     ${String(e.message).split('\n').slice(0, 4).join('\n     ')}`);
    // leave the next check its own start: the pointer up, the engine applying, the window's size, no face open
    await current?.mouse.up().catch(() => {});
    for (let k = 0; k < 2; k++) await current?.keyboard.press('Escape').catch(() => {});
    await current?.evaluate(() => (window.__lf.native.holdApply = false)).catch(() => {});
    if (currentSize) await current?.setViewportSize(currentSize).catch(() => {});
    for (let k = 0; k < 3; k++) await current?.locator('.uface__close').first().click({ timeout: 1000 }).catch(() => {});
    await current?.waitForTimeout(150).catch(() => {});
  }
}

await probe(async ({ open }) => {
  for (const [W, H] of SIZES) {
    const first = W === 1920;
    const { page } = await open({
      viewport: { width: W, height: H },
      init: (p) =>
        p.addInitScript(() => {
          window.__lfEngineFake = true;
          window.__lfChainFake = true;
          // every size starts with the faces at home
          localStorage.removeItem('lf.unitFaces');
        }),
    });
    current = page;
    currentSize = { width: W, height: H };
    page.setDefaultTimeout(4000);
    let seq = 10;
    const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], ...frame });
    await emit({ reset: true, settings: [], events: emptyLanes() });
    await page.evaluate(() => {
      let n = 0x100;
      window.__lf.native.chain.mint = () => ++n;
    });
    const chains = () => page.evaluate(() => JSON.parse(JSON.stringify(window.__lf.native.chain.chains())));
    const sent = () => page.evaluate(() => JSON.parse(JSON.stringify(window.__lf.native.sent)));
    const unitOf = async (kind) => (await chains())[0].units.find((u) => u.kind === kind)?.id;
    const plate = (id) => page.locator(`.unit[data-unit="${id}"]`);
    const settle = () => page.waitForTimeout(120);
    const add = async (label) => {
      await page.locator('.slot--a .unit-add').click();
      await page.getByRole('menuitem', { name: new RegExp(`^${label}`) }).first().click();
      await settle();
    };
    const show = async () => {
      const fold = page.locator('.slot--a .slot__fold');
      if (await fold.count()) await fold.click();
      await settle();
    };
    const shot = (name) => (SHOTS ? page.screenshot({ path: `${SHOTS}/${name}-${W}x${H}.png` }) : null);
    const echoFace = () => page.getByRole('dialog', { name: 'ECHO, slot A' });
    const ottFace = () => page.getByRole('dialog', { name: 'OTT, slot A' });
    /** Open `kind`'s face from its plate's name, unless it is open (a red check before may have closed it). */
    const openFace = async (kind) => {
      const face = kind === 'echo' ? echoFace() : ottFace();
      if (await face.count()) return;
      await show();
      await plate(await unitOf(kind)).locator('.unit__name').click();
      await face.waitFor({ timeout: 2000 });
      await settle();
    };
    const faceOf = (kind) => (kind === 'echo' ? echoFace() : ottFace());
    /** Close every open face by its close. */
    const closeFace = async () => {
      while ((await page.locator('.uface__close').count()) > 0) await page.locator('.uface__close').first().click();
      await settle();
    };
    /** Open `kind`'s face and send it home at its first size: each check that moves a face starts there. */
    const atHome = async (kind) => {
      await openFace(kind);
      await faceOf(kind).focus();
      await page.keyboard.press('Home');
      await settle();
    };
    const kept = () => page.evaluate(() => JSON.parse(localStorage.getItem('lf.unitFaces') ?? '{}'));
    const hold = (on) => page.evaluate((on) => (window.__lf.native.holdApply = on), on);
    /** The ECHO's TIME sends after the first `from` commands sent: the engine indices. */
    const timeSends = async (from) => (await sent()).slice(from).filter((c) => c.SetUnitParam?.[1] === 'echoTime').map((c) => c.SetUnitParam[2]);
    /** The strap's centre and top, or null. */
    const strapAt = async () => {
      const t = await page.locator('.uface-tie').first().boundingBox().catch(() => null);
      return t && { x: t.x + t.width / 2, y: t.y };
    };
    const box = (sel) => page.locator(sel).boundingBox();
    /** Drag with the mouse from (x, y) by (dx, dy) in `steps`. */
    const drag = async (x, y, dx, dy, steps = 8) => {
      await page.mouse.move(x, y);
      await page.mouse.down();
      await page.mouse.move(x + dx, y + dy, { steps });
      await page.mouse.up();
      await settle();
    };
    /** A point on the face's panel no control takes: under its name, left of the knobs. */
    const panelPoint = async (sel) => {
      const b = await box(sel);
      const s = b.width / 900;
      return { x: b.x + 120 * s, y: b.y + (b.height / s > 400 ? 470 : 325) * s };
    };
    /** The face's smallest type (CSS px as rendered), and every text that is clipped: outside the face or
     * outside a box that clips it. */
    const typeAndClip = (sel) =>
      page.$eval(sel, (face) => {
        const scale = face.getBoundingClientRect().width / face.offsetWidth;
        const fr = face.getBoundingClientRect();
        let min = Infinity;
        let what = '';
        const clipped = [];
        for (const el of face.querySelectorAll('*')) {
          const text = [...el.childNodes].some((n) => n.nodeType === 3 && n.textContent.trim() !== '');
          if (!text) continue;
          const px = parseFloat(getComputedStyle(el).fontSize) * scale;
          if (px < min) [min, what] = [px, `${el.className}: ${el.textContent.trim()}`];
          const r = el.getBoundingClientRect();
          const out = (b) => r.left < b.left - 0.5 || r.right > b.right + 0.5 || r.top < b.top - 0.5 || r.bottom > b.bottom + 0.5;
          let bad = out(fr);
          for (let p = el.parentElement; p && p !== face; p = p.parentElement) {
            const cs = getComputedStyle(p);
            if (cs.overflowX !== 'visible' || cs.overflowY !== 'visible') bad ||= out(p.getBoundingClientRect());
          }
          if (bad) clipped.push(`${el.className}: ${el.textContent.trim()}`);
        }
        return { min, what, clipped, inWindow: fr.left >= -0.5 && fr.top >= -0.5 && fr.right <= innerWidth + 0.5 && fr.bottom <= innerHeight + 0.5 };
      });
    /** Controls of `selector` reached by the pointer: in the window and topmost at their centre. */
    const unreached = (selector) =>
      page.$$eval(selector, (els) =>
        els
          .filter((el) => {
            const r = el.getBoundingClientRect();
            const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
            const inside = r.width > 0 && r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight;
            return !(inside && hit && (hit === el || el.contains(hit)));
          })
          .map((el) => el.getAttribute('aria-label') ?? el.className),
      );
    /** The ECHO's glass: the columns its trace lights at `frac` of the way from the centre line to 0 dBFS
     * (the heads' lines stay under a fifth of full alpha, the trace well over), and OUT's left ladder near
     * its foot. */
    const tape = (frac = 0.89) =>
      page.evaluate((frac) => {
        const cv = document.querySelector('.echo__canvas');
        const d = cv.width / cv.clientWidth;
        const W = cv.clientWidth;
        const H = cv.clientHeight;
        const narrow = H < 200;
        const MX = W - 46;
        const X1 = MX - 22;
        const YC = narrow ? 86 : 118;
        const HALF = narrow ? 50 : 74;
        const ctx = cv.getContext('2d');
        const row = (y) => ctx.getImageData(0, Math.round(y * d), cv.width, 1).data;
        const litOn = (y) => {
          const r = row(y);
          const out = [];
          for (let x = 40; x < X1 - 2; x++) {
            const i = Math.round(x * d) * 4;
            if (r[i] > 150 && r[i + 3] > 100) out.push(x);
          }
          return out;
        };
        // L above the centre line, R below it (its columns start a pixel under the line)
        const lit = litOn(YC - frac * HALF);
        const litR = litOn(YC + 1 + frac * HALF);
        const cell = (x) => [...ctx.getImageData(Math.round(x * d), Math.round((YC + HALF - 6) * d), 1, 1).data];
        // OUT's two ladders: L at MX + 3, R at MX + 18, each 6 px wide; the foot cell's green
        return { lit, litR, newest: lit.length ? Math.max(...lit) : null, meter: cell(MX + 5), meterR: cell(MX + 20) };
      }, frac);

    // the scene: an OTT and an ECHO in slot A
    await add('OTT');
    await add('ECHO');

    if (first) {
      await check('the ECHO face: a press on the plate’s name opens it under the plate, on its strap', async () => {
        await openFace('echo');
        assert.equal(await page.locator('.infx-pop').count(), 0, 'no settings popover');
        const fb = await echoFace().boundingBox();
        const pb = await plate(await unitOf('echo')).boundingBox();
        assert.ok(fb.y > pb.y + pb.height && fb.y < pb.y + pb.height + 30, `under the plate: face top ${fb.y}, plate foot ${pb.y + pb.height}`);
        const tie = await box('.uface-tie');
        assert.ok(tie && Math.abs(tie.x + tie.width / 2 - (pb.x + pb.width / 2)) < 1 && Math.abs(tie.y - (pb.y + pb.height)) < 1, 'the strap hangs from the plate’s foot');
        assert.deepEqual(await echoFace().getByRole('slider').evaluateAll((els) => els.map((e) => e.getAttribute('aria-label'))), ['ECHO time', 'ECHO feedback', 'ECHO level']);
      });
      await check('the ECHO face: TIME sends the engine’s index and reads the feed’s, with its length at the tempo', async () => {
        await openFace('echo');
        const echo = await unitOf('echo');
        const time = page.getByRole('slider', { name: 'ECHO time' });
        const lastTime = async () => (await sent()).filter((c) => c.SetUnitParam && c.SetUnitParam[0] === echo && c.SetUnitParam[1] === 'echoTime').at(-1)?.SetUnitParam[2];
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8, 250 ms', 'the engine’s default, 1/8, at 120');
        await time.focus();
        await page.keyboard.press('ArrowUp');
        await settle();
        assert.equal(await lastTime(), 2, 'up from 1/8: 1/8. is the engine’s 2');
        await page.keyboard.press('ArrowUp');
        await settle();
        assert.equal(await lastTime(), 0, 'then 1/4, the engine’s 0');
        assert.equal((await chains())[0].units.find((u) => u.id === echo).params.echoTime, 0, 'the fake took it');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/4, 500 ms');
        await page.keyboard.press('Home');
        await settle();
        assert.equal(await lastTime(), 3, 'Home: 1/16, the engine’s 3');
        assert.match(await page.locator('.echo .echo__time .uface__val').textContent(), /^1\/16\s*125 ms$/, 'the read-out');
        await page.locator('.echo .echo__st', { hasText: '1/8.' }).click();
        await settle();
        assert.equal(await lastTime(), 2, 'a press on 1/8.');
        assert.match(await page.locator('.echo .echo__st.is-on').textContent(), /^1\/8\.375$/, 'the position lit');
        assert.match(await page.locator('.echo .echo__span').textContent(), /^375\s*ms$/, 'the glass’s span');
        // a drag down by two steps: 1/8. to 1/16
        const kb = await time.boundingBox();
        await drag(kb.x + kb.width / 2, kb.y + kb.height / 2, 0, 74, 6);
        assert.equal(await lastTime(), 3, 'a drag down two steps');
        // a new tempo: the lengths follow
        await emit({ events: [transport(100)] });
        await settle();
        assert.equal(await time.getAttribute('aria-valuetext'), '1/16, 150 ms', '1/16 at 100');
        await emit({ events: [transport(120)] });
        await time.focus();
        await page.keyboard.press('ArrowUp');
        await page.keyboard.press('ArrowUp');
        await settle();
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8., 375 ms');
      });
      /** TIME back to 1/8 (the engine's 1), the engine applying again. */
      const timeAt18 = async () => {
        await hold(false);
        const echo = await unitOf('echo');
        await page.evaluate(async (id) => {
          const { sendEngine } = await import('/src/platform/index.ts');
          sendEngine({ SetUnitParam: [id, 'echoTime', 1] });
        }, echo);
        await settle();
        assert.equal(await page.getByRole('slider', { name: 'ECHO time' }).getAttribute('aria-valuetext'), '1/8, 250 ms', 'TIME starts at 1/8');
      };
      await check('TIME’s commands in order: a drag 1/8 → 1/8. → 1/8 before the engine answers sends its way back; let go, it reads the feed', async () => {
        await openFace('echo');
        await timeAt18();
        const time = page.getByRole('slider', { name: 'ECHO time' });
        await hold(true);
        const from = (await sent()).length;
        const kb = await time.boundingBox();
        const [x, y] = [kb.x + kb.width / 2, kb.y + kb.height / 2];
        await page.mouse.move(x, y);
        await page.mouse.down();
        await page.mouse.move(x, y - 40, { steps: 4 });
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8., 375 ms', 'under the hand: 1/8.');
        await page.mouse.move(x, y, { steps: 4 });
        await page.mouse.up();
        await settle();
        assert.deepEqual(await timeSends(from), [2, 1], 'sent 1/8. (2), then 1/8 (1) again');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8, 250 ms', 'let go: the feed’s value, the engine holding the commands');
        await hold(false);
        await settle();
        assert.equal((await chains())[0].units.find((u) => u.kind === 'echo').params.echoTime, 1, 'the engine ends at 1/8');
      });
      await check('TIME’s commands in order: keys pressed before the engine answers count on from the last step sent', async () => {
        await openFace('echo');
        await timeAt18();
        const time = page.getByRole('slider', { name: 'ECHO time' });
        await hold(true);
        const from = (await sent()).length;
        await time.focus();
        await page.keyboard.press('ArrowUp');
        await page.keyboard.press('ArrowUp');
        await page.keyboard.press('ArrowDown');
        assert.deepEqual(await timeSends(from), [2, 0, 2], '1/8. (2), 1/4 (0), back to 1/8. (2)');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8, 250 ms', 'the read-out stays on the feed');
        await hold(false);
        await settle();
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8., 375 ms', 'the engine’s answer');
      });
      await check('TIME’s commands in order: a wheel event of several notches is one send', async () => {
        await openFace('echo');
        await timeAt18();
        const time = page.getByRole('slider', { name: 'ECHO time' });
        await hold(true);
        const from = (await sent()).length;
        await time.dispatchEvent('wheel', { deltaY: -100 });
        assert.deepEqual(await timeSends(from), [0], 'two notches up from 1/8: 1/4 (0), once');
        await hold(false);
        await settle();
        assert.equal(await time.getAttribute('aria-valuetext'), '1/4, 500 ms');
      });
      await check('the ECHO face: FEEDBACK (0 to 95 %) and LEVEL send SetUnitParam and read the feed’s value', async () => {
        await openFace('echo');
        const echo = await unitOf('echo');
        const fb = page.getByRole('slider', { name: 'ECHO feedback' });
        await fb.focus();
        await page.keyboard.press('End');
        await settle();
        assert.ok((await sent()).some((c) => c.SetUnitParam?.[0] === echo && c.SetUnitParam[1] === 'echoFeedback' && c.SetUnitParam[2] === 0.95), 'End: 0.95');
        assert.equal(await fb.getAttribute('aria-valuenow'), '95');
        assert.equal(await fb.getAttribute('aria-valuemax'), '95');
        await page.keyboard.press('ArrowDown');
        await settle();
        assert.equal((await chains())[0].units.find((u) => u.id === echo).params.echoFeedback, 0.94);
        assert.match(await page.locator('.echo .uface__ctl:has([data-param="echoFeedback"]) .uface__val').textContent(), /^94\s*%$/);
        const level = page.getByRole('slider', { name: 'ECHO level' });
        const lb = await level.boundingBox();
        await drag(lb.x + lb.width / 2, lb.y + lb.height / 2, 0, -44, 4);
        const took = (await chains())[0].units.find((u) => u.id === echo).params.echoLevel;
        assert.ok(took > 0.66 && took < 0.76, `a 44 px drag up lifts LEVEL by about 0.2 (took ${took})`);
        assert.match(await page.locator('.echo .uface__ctl:has([data-param="echoLevel"]) .uface__val').textContent(), new RegExp(`^${Math.round(took * 100)}`));
      });
      await check('a knob’s held value never sticks: it follows the hand in a drag, let go it reads the feed', async () => {
        await openFace('echo');
        const level = page.getByRole('slider', { name: 'ECHO level' });
        const val = page.locator('.echo .uface__ctl:has([data-param="echoLevel"]) .uface__val');
        const feed = await level.getAttribute('aria-valuenow');
        await hold(true);
        const lb = await level.boundingBox();
        const [x, y] = [lb.x + lb.width / 2, lb.y + lb.height / 2];
        await page.mouse.move(x, y);
        await page.mouse.down();
        await page.mouse.move(x, y - 22, { steps: 3 });
        const under = Number(await level.getAttribute('aria-valuenow'));
        assert.ok(under > Number(feed), `under the hand: ${under} over the feed’s ${feed}`);
        await page.mouse.up();
        await settle();
        assert.equal(await level.getAttribute('aria-valuenow'), feed, 'let go: the feed’s value');
        assert.match(await val.textContent(), new RegExp(`^${feed}\\s*%$`), 'the read-out too');
        await hold(false);
        await settle();
        assert.equal(Number(await level.getAttribute('aria-valuenow')), under, 'the engine’s answer');
      });
      await check('a knob’s held value never sticks: a capture lost mid-drag hands it back to the feed', async () => {
        await openFace('echo');
        const fb = page.getByRole('slider', { name: 'ECHO feedback' });
        const feed = await fb.getAttribute('aria-valuenow');
        await hold(true);
        const b = await fb.boundingBox();
        const [x, y] = [b.x + b.width / 2, b.y + b.height / 2];
        await page.mouse.move(x, y);
        await page.mouse.down();
        await page.mouse.move(x, y + 22, { steps: 3 });
        assert.notEqual(await fb.getAttribute('aria-valuenow'), feed, 'under the hand');
        // the window takes the pointer away (an OS dialog, a blur): the capture goes, the button still down
        await fb.evaluate((el) => el.dispatchEvent(new PointerEvent('lostpointercapture', { pointerId: 1, bubbles: false })));
        await settle();
        assert.equal(await fb.getAttribute('aria-valuenow'), feed, 'the capture lost: the feed’s value');
        await page.mouse.up();
        await hold(false);
        await settle();
      });
      await check('the ECHO face: the power switch is the unit’s bypass, and the face shows it', async () => {
        await openFace('echo');
        const echo = await unitOf('echo');
        const pwr = page.locator('.echo .uface__pwr');
        assert.equal(await pwr.getAttribute('aria-pressed'), 'true');
        await pwr.click();
        await settle();
        assert.ok((await sent()).some((c) => c.Action?.ToggleUnit === echo), 'ToggleUnit sent');
        assert.equal((await chains())[0].units.find((u) => u.id === echo).bypass, true, 'the fake bypassed it');
        assert.equal(await pwr.getAttribute('aria-pressed'), 'false');
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--byp')), 'the face drawn bypassed');
        await page.waitForTimeout(400);
        assert.equal(await page.locator('.echo .echo__canvas').evaluate((e) => getComputedStyle(e).opacity), '0.4', 'its glass sits back');
        await pwr.click();
        await settle();
        assert.equal(await pwr.getAttribute('aria-pressed'), 'true', 'on again');
      });
      await check('the ECHO face: scripted unitMeters light the tape and OUT, the trace slides into the past, and clears', async () => {
        await openFace('echo');
        const echo = await unitOf('echo');
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
        await emit({ unitMeters: [] });
        await page.waitForTimeout(300);
        assert.deepEqual((await tape()).lit, [], 'the tape dark without frames');
        for (let i = 0; i < 6; i++) await emit({ unitMeters: [{ unit: echo, out: [0.9, 0.8] }] });
        await page.waitForTimeout(150);
        const a = await tape();
        assert.ok(a.newest !== null, 'the frames light the tape');
        assert.ok(a.meter[1] > 120, `OUT lit: ${a.meter}`);
        await page.waitForTimeout(500);
        const b = await tape();
        const moved = a.newest - b.newest;
        // two bars at 120 over the tape's 744 px: 186 px a second, so about 93 px in half a second
        assert.ok(moved > 60 && moved < 130, `the trace slid toward the past by ${moved} px (from ${a.newest} to ${b.newest})`);
        await emit({ unitMeters: [] });
        await page.waitForTimeout(150);
        assert.deepEqual((await tape()).lit, [], 'a frame without the unit clears it');
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
      });
      /** The tape at rest: the fake's timer off, a frame without the unit, the glass dark. */
      const quiet = async () => {
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
        await emit({ unitMeters: [] });
        await page.waitForTimeout(300);
        assert.deepEqual((await tape(0.3)).lit, [], 'dark to start');
      };
      await check('the tape: a right-only pulse and the silence after it, both before the next draw, are drawn (R, its ladder; L dark)', async () => {
        await openFace('echo');
        await quiet();
        const echo = await unitOf('echo');
        // two frames in one task: no draw between them
        await page.evaluate(
          ([id, seq]) => {
            const f = (n, out) => ({ seq: n, reset: false, events: [], unitMeters: [{ unit: id, out }] });
            window.__lf.native.emit(f(seq + 1, [0, 0.9]));
            window.__lf.native.emit(f(seq + 2, [0, 0]));
          },
          [echo, seq],
        );
        seq += 2;
        await page.waitForTimeout(120);
        const t = await tape(0.8);
        assert.ok(t.litR.length > 0, 'the pulse drawn on R');
        assert.deepEqual(t.lit, [], 'nothing on L');
        assert.ok(t.meterR[1] > 120 && t.meter[1] < 80, `OUT: R lit ${t.meterR}, L dark ${t.meter}`);
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
      });
      await check('the tape: dissimilar L and R are drawn each on its own half and ladder', async () => {
        await openFace('echo');
        await quiet();
        const echo = await unitOf('echo');
        for (let i = 0; i < 6; i++) await emit({ unitMeters: [{ unit: echo, out: [0.9, 0.12] }] });
        await page.waitForTimeout(120);
        const hi = await tape(0.89);
        assert.ok(hi.lit.length > 0 && hi.litR.length === 0, `near full scale: L lit (${hi.lit.length}), R not (${hi.litR.length})`);
        const lo = await tape(0.5);
        assert.ok(lo.litR.length > 0, 'R lit lower down');
        await emit({ unitMeters: [] });
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
      });
      await check('the tape: scaled up while silent, its glass is drawn again at its new size', async () => {
        await atHome('echo');
        await quiet();
        await echoFace().focus();
        for (let k = 0; k < 3; k++) await page.keyboard.press('+');
        await page.waitForTimeout(200);
        const r = await page.locator('.echo__canvas').evaluate((cv) => ({ backing: cv.width, shown: cv.getBoundingClientRect().width * devicePixelRatio }));
        assert.ok(Math.abs(r.backing - r.shown) <= 2, `backing ${r.backing} px for ${r.shown.toFixed(1)} px shown`);
        await page.keyboard.press('Home');
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
      });
      await check('the move: a drag on the panel moves the face, the strap goes, nothing sent, the face stays open, its size not pinned', async () => {
        await atHome('echo');
        const before = await echoFace().boundingBox();
        const n = (await sent()).length;
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, -150, 70);
        const after = await echoFace().boundingBox();
        assert.ok(Math.abs(after.x - before.x + 150) < 1 && Math.abs(after.y - before.y - 70) < 1, `moved by the pointer's way: ${after.x - before.x}, ${after.y - before.y}`);
        assert.equal(await page.locator('.uface-tie').count(), 0, 'the strap is gone');
        assert.equal((await sent()).length, n, 'nothing sent');
        assert.equal(await echoFace().count(), 1, 'still open');
        assert.equal((await kept()).echo?.size, null, 'a move keeps the size as it was (none set)');
        await echoFace().focus();
        await page.keyboard.press('ArrowLeft');
        await settle();
        assert.equal((await kept()).echo?.size, null, 'and so does a nudge');
      });
      await check('the move: kept per kind (closed and opened again at its place; the OTT at its own home)', async () => {
        await atHome('echo');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, -150, 70);
        const moved = await echoFace().boundingBox();
        await closeFace();
        await openFace('echo');
        const again = await echoFace().boundingBox();
        assert.ok(Math.abs(again.x - moved.x) < 1 && Math.abs(again.y - moved.y) < 1, 'the ECHO opens where it was left');
        assert.equal(await page.locator('.uface-tie').count(), 0, 'with no strap');
        await closeFace();
        await openFace('ott');
        const pb = await plate(await unitOf('ott')).boundingBox();
        const ob = await ottFace().boundingBox();
        assert.ok(ob.y > pb.y + pb.height && ob.y < pb.y + pb.height + 30, 'the OTT at its home');
        assert.equal(await page.locator('.uface-tie').count(), 1, 'on its strap');
        await closeFace();
      });
      await check('the move: released near home it lands home, the strap back; the arrows nudge, Home sends it home', async () => {
        await atHome('echo');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, -150, 70);
        const q = await panelPoint('.echo');
        await drag(q.x, q.y, 146, -66);
        assert.equal(await page.locator('.uface-tie').count(), 1, 'home again, on its strap');
        const home = await echoFace().boundingBox();
        await echoFace().focus();
        await page.keyboard.press('ArrowRight');
        await page.keyboard.press('Shift+ArrowDown');
        await settle();
        const nudged = await echoFace().boundingBox();
        assert.ok(Math.abs(nudged.x - home.x - 8) < 1 && Math.abs(nudged.y - home.y - 40) < 1, `nudged 8 right, 40 down: ${nudged.x - home.x}, ${nudged.y - home.y}`);
        assert.equal(await page.locator('.uface-tie').count(), 0);
        await page.keyboard.press('Home');
        await settle();
        const back = await echoFace().boundingBox();
        assert.ok(Math.abs(back.x - home.x) < 1 && Math.abs(back.y - home.y) < 1 && (await page.locator('.uface-tie').count()) === 1, 'Home: home');
      });
      await check('the size by the keys: - takes the full face to the narrow one, + brings the full face back', async () => {
        await atHome('echo');
        const narrow = () => page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow'));
        assert.equal(await narrow(), false, 'the full face to start');
        await page.keyboard.press('-');
        await settle();
        assert.equal(await narrow(), true, '-: the narrow face');
        await page.keyboard.press('+');
        await settle();
        assert.equal(await narrow(), false, '+: the full face again');
        assert.equal(Math.round((await echoFace().boundingBox()).height), 540, 'at its base size');
        await page.keyboard.press('Home');
      });
      await check('the resize: a corner drag scales the face; past its base size the narrow face, never under it; up to the window', async () => {
        await atHome('echo');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, -200, 40);
        const b0 = await echoFace().boundingBox();
        const grip = await box('.echo .uface__grip');
        await drag(grip.x + grip.width / 2, grip.y + grip.height / 2, 180, 108);
        const b1 = await echoFace().boundingBox();
        assert.ok(Math.abs(b1.width - 1080) < 12 && Math.abs(b1.height / b1.width - 0.6) < 0.01, `grown by its corner to about 1080 wide: ${b1.width}x${b1.height}`);
        assert.ok(Math.abs(b1.x - b0.x) < 1 && Math.abs(b1.y - b0.y) < 1, 'from its top-left');
        let t = await typeAndClip('.echo');
        assert.ok(t.min >= 12 && t.clipped.length === 0, `type ${t.min} (${t.what}); clipped ${t.clipped}`);
        const g1 = await box('.echo .uface__grip');
        await drag(g1.x + g1.width / 2, g1.y + g1.height / 2, -700, -500);
        const b2 = await echoFace().boundingBox();
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow')), 'shrunk past its base size: the narrow face');
        assert.ok(Math.abs(b2.width - 900) < 1 && Math.abs(b2.height - 382) < 1, `at the base size, never under: ${b2.width}x${b2.height}`);
        t = await typeAndClip('.echo');
        assert.ok(t.min >= 12 - 1e-6 && t.clipped.length === 0, `narrow: type ${t.min} (${t.what}); clipped ${t.clipped}`);
        const g2 = await box('.echo .uface__grip');
        await drag(g2.x + g2.width / 2, g2.y + g2.height / 2, 3000, 3000);
        const b3 = await echoFace().boundingBox();
        assert.ok(!(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow'))), 'grown back: the full face');
        assert.ok(b3.x + b3.width <= W - 8 + 0.5 && b3.y + b3.height <= H - 8 + 0.5 && b3.width > 1100, `as large as the window holds: ${JSON.stringify(b3)}`);
        await echoFace().focus();
        await page.keyboard.press('Home');
        await settle();
        assert.equal(Math.round((await echoFace().boundingBox()).width), 900, 'Home: its first size too');
      });
      await check('a window resize pulls a moved face back into the window', async () => {
        await atHome('echo');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, 2000, 2000);
        const b = await echoFace().boundingBox();
        assert.ok(Math.abs(b.x + b.width - (W - 8)) < 1 && Math.abs(b.y + b.height - (H - 8)) < 1, `dragged past the corner, it stops in it: ${JSON.stringify(b)}`);
        await page.setViewportSize({ width: 1400, height: 800 });
        await page.waitForTimeout(250);
        const r = await typeAndClip('.echo');
        assert.ok(r.inWindow, `in the smaller window: ${JSON.stringify(await echoFace().boundingBox())}`);
        await page.setViewportSize({ width: W, height: H });
        await page.waitForTimeout(250);
        await echoFace().focus();
        await page.keyboard.press('Home');
        await closeFace();
      });
      await check('the OTT face moves the same way: the strap goes, its place kept', async () => {
        await atHome('ott');
        const before = await ottFace().boundingBox();
        const p = await panelPoint('.ott');
        await drag(p.x, p.y, 300, 60);
        const after = await ottFace().boundingBox();
        assert.ok(Math.abs(after.x - before.x - 300) < 1 && Math.abs(after.y - before.y - 60) < 1, `moved: ${after.x - before.x}, ${after.y - before.y}`);
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap');
        await closeFace();
        await openFace('ott');
        const again = await ottFace().boundingBox();
        assert.ok(Math.abs(again.x - after.x) < 1 && Math.abs(again.y - after.y) < 1, 'it opens where it was left');
        await ottFace().focus();
        await page.keyboard.press('Home');
        await closeFace();
      });

      // ── free-standing ──────────────────────────────────────────────────────────────────────────────
      await check('free-standing: no backdrop; a press beside the face reaches the looper and leaves the face open', async () => {
        await atHome('echo');
        assert.equal(await page.locator('.slot-pop__backdrop').count(), 0, 'no backdrop');
        const rec = page.getByRole('button', { name: 'Track 5 record', exact: true });
        assert.deepEqual(await unreached('.zone--looper [aria-label="Track 5 record"]'), [], 'the lane’s record key is in reach beside the face');
        const n = (await sent()).length;
        await rec.click();
        await settle();
        assert.ok((await sent()).slice(n).some((c) => c.RecDub === 4), `the press reached the looper: ${JSON.stringify((await sent()).slice(n))}`);
        assert.equal(await echoFace().count(), 1, 'the face stays open');
      });
      await check('free-standing: with the face focused, the transport keys play the looper', async () => {
        await atHome('echo');
        const n = (await sent()).length;
        await page.keyboard.press('Space');
        await settle();
        assert.ok((await sent()).slice(n).some((c) => c.Action === 'RecDub'), 'Space sent REC/DUB');
        assert.equal(await echoFace().count(), 1, 'the face stays open');
      });
      await check('free-standing: it closes by its close, by Escape with the focus in it, by its name again; an Escape elsewhere leaves it', async () => {
        const echo = await unitOf('echo');
        await openFace('echo');
        await page.evaluate(() => document.activeElement?.blur());
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await echoFace().count(), 1, 'an Escape with the focus outside leaves it open');
        await page.getByRole('slider', { name: 'ECHO level' }).focus();
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await echoFace().count(), 0, 'Escape in it closes it');
        assert.equal(await page.evaluate(() => document.activeElement?.className), 'unit__name slot-pop__trigger', 'the focus back on the plate’s name');
        await plate(echo).locator('.unit__name').click();
        await echoFace().waitFor();
        await plate(echo).locator('.unit__name').click();
        await settle();
        assert.equal(await echoFace().count(), 0, 'the name again closes it');
        await openFace('echo');
        await page.locator('.echo .uface__close').click();
        await settle();
        assert.equal(await echoFace().count(), 0, 'its close closes it');
      });
      await check('free-standing: the OTT’s and the ECHO’s faces open at once, the last touched on top, a strap each at home', async () => {
        await atHome('echo');
        await atHome('ott');
        assert.equal((await echoFace().count()) + (await ottFace().count()), 2, 'both open');
        assert.equal(await page.locator('.uface-tie').count(), 2, 'a strap each');
        const e = await echoFace().boundingBox();
        const o = await ottFace().boundingBox();
        const ox = (Math.max(e.x, o.x) + Math.min(e.x + e.width, o.x + o.width)) / 2;
        const oy = (Math.max(e.y, o.y) + Math.min(e.y + e.height, o.y + o.height)) / 2;
        const top = () => page.evaluate(([x, y]) => document.elementFromPoint(x, y)?.closest('.uface')?.classList.contains('echo') ? 'echo' : 'ott', [ox, oy]);
        assert.equal(await top(), 'ott', 'the OTT, opened last, on top');
        if (SHOTS) {
          await page.evaluate(() => document.activeElement?.blur());
          await page.waitForTimeout(2500);
          await shot('both-faces');
        }
        // a press on the ECHO's panel where the OTT does not cover it
        const px = e.x < o.x ? e.x + 6 : e.x + e.width - 6;
        await page.mouse.click(px, e.y + e.height * 0.8);
        await settle();
        assert.equal(await top(), 'echo', 'the ECHO, touched, on top');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, 0, 120);
        assert.equal(await page.locator('.uface-tie').count(), 1, 'moved off home, the ECHO’s strap goes, the OTT’s stays');
        await echoFace().focus();
        await page.keyboard.press('Home');
        await closeFace();
      });
      await check('the strap follows its plate when the chain reorders under the open face', async () => {
        const echo = await unitOf('echo');
        await atHome('echo');
        await plate(echo).locator('.plate__menu').click();
        await page.getByRole('menuitem', { name: 'Move left', exact: true }).click();
        await page.waitForTimeout(250);
        assert.equal((await chains())[0].units[0].id, echo, 'the ECHO heads the chain');
        const pb = await plate(echo).boundingBox();
        const t = await strapAt();
        assert.ok(t && Math.abs(t.x - (pb.x + pb.width / 2)) < 1 && Math.abs(t.y - (pb.y + pb.height)) < 1, `the strap under the plate’s new place: ${JSON.stringify(t)} for ${pb.x + pb.width / 2}`);
        await plate(echo).locator('.plate__menu').click();
        await page.getByRole('menuitem', { name: 'Move right', exact: true }).click();
        await page.waitForTimeout(250);
        await closeFace();
      });
      await check('TIME’s printed positions are pressed by the pointer, never focused (inside aria-hidden)', async () => {
        await atHome('echo');
        await page.locator('.echo .echo__st', { hasText: '1/4' }).click();
        await settle();
        const f = await page.evaluate(() => ({ inHidden: document.activeElement?.closest('[aria-hidden="true"]') != null, focusable: [...document.querySelectorAll('.echo__st')].some((e) => e.tabIndex >= -1 && (e.tagName === 'BUTTON' || e.hasAttribute('tabindex'))) }));
        assert.deepEqual(f, { inHidden: false, focusable: false }, 'no focus inside the hidden print');
        assert.equal(await page.getByRole('slider', { name: 'ECHO time' }).getAttribute('aria-valuetext'), '1/4, 500 ms', 'the press took');
      });
      await check('the resize at home, from a plate the window clamps: the corner grows from where the face is drawn, no jump, no narrow face', async () => {
        // the slot's first plate sits near the window's left edge: a step up in size, its face at home is held
        // in from that edge
        await atHome('ott');
        await page.keyboard.press('+');
        await settle();
        const b0 = await ottFace().boundingBox();
        assert.ok(Math.abs(b0.x - 8) < 1, `premise: the OTT at home against the window's left margin (x ${b0.x})`);
        const g = await box('.ott .uface__grip');
        await drag(g.x + g.width / 2, g.y + g.height / 2, 12, 8, 4);
        const b1 = await ottFace().boundingBox();
        assert.ok(!(await page.locator('.ott').evaluate((e) => e.classList.contains('uface--narrow'))), 'still the full face');
        assert.ok(b1.width > b0.width && b1.width - b0.width < 30, `a small drag grows it a little: ${b0.width} -> ${b1.width}`);
        assert.ok(Math.abs(b1.x + b1.width - (g.x + g.width / 2 + 12)) < 14, `its corner by the pointer: ${b1.x + b1.width} vs ${g.x + g.width / 2 + 12}`);
        await ottFace().focus();
        await page.keyboard.press('Home');
        // the right edge: slot B's ECHO, its plate near the window's right, its face at home held in from it;
        // a corner pushed past the edge grows by the downward way alone
        await closeFace();
        await page.locator('.slot--b .unit-add').click();
        await page.getByRole('menuitem', { name: /^ECHO/ }).first().click();
        await settle();
        const echoB = (await chains())[1].units.find((u) => u.kind === 'echo').id;
        await plate(echoB).locator('.unit__name').click();
        const faceB = page.getByRole('dialog', { name: 'ECHO, slot B' });
        await faceB.waitFor();
        await faceB.focus();
        await page.keyboard.press('Home');
        await settle();
        const r0 = await faceB.boundingBox();
        assert.ok(Math.abs(r0.x + r0.width - (W - 8)) < 1 && (await page.locator('.uface-tie').count()) === 1, `premise: at home against the right margin (${r0.x + r0.width})`);
        const gr = await box('.echo .uface__grip');
        await drag(gr.x + gr.width / 2, gr.y + gr.height / 2, 40, 10, 4);
        const r1 = await faceB.boundingBox();
        assert.ok(!(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow'))), 'pushed past the edge, not the narrow face');
        assert.ok(r1.height >= r0.height && r1.height - r0.height < 30, `grown by the way down: ${r0.height} -> ${r1.height}`);
        await faceB.focus();
        await page.keyboard.press('Home');
        await closeFace();
        await plate(echoB).locator('.plate__menu').click();
        await page.getByRole('menuitem', { name: 'Remove', exact: true }).click();
        await page.waitForTimeout(250);
      });
      await check('a focused knob keeps its focus as the face reflows between the full and the narrow layout', async () => {
        await atHome('echo');
        await atHome('ott');
        for (const [kind, name] of [
          ['echo', 'ECHO level'],
          ['echo', 'ECHO time'],
          ['ott', 'OTT depth'],
        ]) {
          await page.getByRole('slider', { name }).focus();
          await page.setViewportSize({ width: 1400, height: 600 });
          await page.waitForTimeout(250);
          assert.ok(await page.locator(`.${kind}`).evaluate((e) => e.classList.contains('uface--narrow')), `${kind}: narrow at 1400x600`);
          assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), name, `${name}: focus kept into the narrow face`);
          await page.setViewportSize({ width: W, height: H });
          await page.waitForTimeout(250);
          assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), name, `${name}: focus kept back into the full face`);
        }
        await closeFace();
      });
      await check('a window too short for the face under its plate: off its strap, fitted to the window, every control inside', async () => {
        await atHome('echo');
        await page.setViewportSize({ width: 1000, height: 440 });
        await page.waitForTimeout(300);
        assert.equal(await page.locator('.uface-tie').count(), 0, 'off its strap');
        const t = await typeAndClip('.echo');
        assert.ok(t.inWindow, `in the window: ${JSON.stringify(await echoFace().boundingBox())}`);
        assert.deepEqual(await unreached('.echo .uface__knob, .echo .uface__pwr, .echo .uface__close, .echo .echo__st'), [], 'every control in reach');
        await page.setViewportSize({ width: W, height: H });
        await page.waitForTimeout(250);
        assert.equal(await page.locator('.uface-tie').count(), 1, 'room again: home on its strap');
        await closeFace();
      });
    }

    // ── a fold: where the row shows one slot at a time, the other's plate folds away under an open face ──
    await show();
    if ((await page.locator('.slot--b .slot__fold').count()) > 0) {
      await check(`a fold at ${W}x${H}: the face stays open where it stood, off its strap, in the window; unfolded, home again`, async () => {
        await atHome('echo');
        const before = await echoFace().boundingBox();
        await page.locator('.slot--b .slot__fold').click();
        await settle();
        assert.equal(await page.locator('.slot--a .slot__fold').count(), 1, 'premise: slot A folded');
        assert.equal(await echoFace().count(), 1, 'still open');
        const b = await echoFace().boundingBox();
        assert.ok(Math.abs(b.x - before.x) < 1 && Math.abs(b.y - before.y) < 1, 'where it stood');
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap without its plate');
        assert.ok((await typeAndClip('.echo')).inWindow, 'in the window');
        await show();
        await settle();
        assert.equal(await page.locator('.uface-tie').count(), 1, 'its plate back: on its strap');
        await closeFace();
      });
    }

    // ── per size: both faces at home, type and clipping, reach, the tape live ─────────────────────────
    for (const kind of ['echo', 'ott']) {
      await check(`${kind.toUpperCase()} at ${W}x${H}: at home, legends 12 px or more and none clipped, its controls in reach`, async () => {
        await openFace(kind);
        const sel = `.${kind}`;
        // home, whatever an earlier check left
        await (kind === 'echo' ? echoFace() : ottFace()).focus();
        await page.keyboard.press('Home');
        await settle();
        assert.equal(await page.locator('.uface-tie').count(), 1, 'at home on its strap');
        const t = await typeAndClip(sel);
        assert.ok(t.inWindow, 'in the window');
        assert.ok(t.min >= 12 - 1e-6, `the smallest type is ${t.min.toFixed(2)} px (${t.what})`);
        assert.deepEqual(t.clipped, [], 'no legend clipped');
        const controls = `${sel} .uface__knob, ${sel} .uface__pwr, ${sel} .uface__close${kind === 'echo' ? `, ${sel} .echo__st` : ''}`;
        assert.deepEqual(await unreached(controls), []);
        if (kind === 'echo') {
          // the accepted face's settings (1/8., 55 %, 50 %), whatever an earlier check left
          await page.evaluate(async (id) => {
            const { sendEngine } = await import('/src/platform/index.ts');
            sendEngine({ SetUnitParam: [id, 'echoTime', 2] }, { SetUnitParam: [id, 'echoFeedback', 0.55] }, { SetUnitParam: [id, 'echoLevel', 0.5] });
          }, await unitOf('echo'));
          // the fake's timer: a note on the first downbeat lights the tape at once
          await page.waitForTimeout(SHOTS ? 4500 : 500);
          assert.ok((await tape(0.6)).newest !== null, 'the tape runs on the fake’s meters');
        } else if (SHOTS) await page.waitForTimeout(2500);
        await page.evaluate(() => document.activeElement?.blur());
        if (SHOTS) await shot(kind === 'echo' ? 'echo-home' : 'ott-home');
        if (SHOTS && kind === 'echo' && (W === 1920 || W === 1000)) {
          // moved off home: over the lanes, its strap gone
          const p = await panelPoint('.echo');
          await drag(p.x, p.y, W === 1920 ? 520 : -60, W === 1920 ? 200 : 140);
          await page.evaluate(() => document.activeElement?.blur());
          await page.waitForTimeout(600);
          await shot('echo-moved');
          await echoFace().focus();
          await page.keyboard.press('Home');
        }
        await closeFace();
      });
    }
    await page.close();
  }
  assert.deepEqual(failures, [], `${failures.length} check(s) red`);
});
