/**
 * The unit faces as free-standing gear (`src/ui/units/UnitFace.tsx`, `face-place.ts`), the ECHO's own
 * face (`EchoFace.tsx`, its tape `echo-loop.ts`) and the RING MOD's own face (`RingFace.tsx`, its carrier
 * `ring-loop.ts`), driven as a player drives them on the browser fake's chain (`docs/unit-contract.md` §10,
 * `src/platform/chain-fake.ts`), the chain asked for with `__lfChainFake`.
 *
 * - the ECHO face: a press on its plate's name opens it under the plate with no strap (no settings popover);
 *   TIME by the keyboard, a position's press and a drag sends `SetUnitParam` with the engine's index (the
 *   face orders by length, 1/16 1/8 1/8. 1/4; the engine's index 3 1 2 0) and reads the feed's value and its
 *   length at the tempo, a new tempo included; with the engine holding the commands (`holdApply`), a drag
 *   turned back inside one gesture (even past a >500 ms pause) sends its way back, quick keys count on from
 *   the last step sent even through one-by-one late reports (an unchanged snapshot answers nothing), a wheel event normalizes `deltaMode` and sends
 *   once, and every read-out stays on the feed once let go; FEEDBACK (0 to 95 %) and LEVEL by the keyboard
 *   and a drag, a knob following the hand only while the drag holds it (a release or a lost capture hands it
 *   back to the feed and returns pointer-only focus to the frame so Space/Enter drive the looper); the power
 *   switch is the unit's bypass, and the face shows it; scripted `unitMeters` frames light the tape and OUT
 *   and the trace slides toward the past, and a frame without the unit clears both traces and both ladders;
 *   a right-only pulse and the silence after it, both between two draws, are drawn on R and its ladder alone,
 *   dissimilar L and R each on its own half; scaled up while silent, the glass is drawn again at a larger
 *   backing size; a beats-only meter change redraws the silent ruler; closing the last face cancels its rAF;
 *   TIME's printed positions take a press while keeping TIME focus, never focusing inside aria-hidden;
 * - the move: a drag on the face's panel moves it by the pointer's way with no strap, nothing is sent and
 *   the face stays open, its size not pinned (nor by a nudge), and a narrow-reflowed face keeps its fit on
 *   release without storing it; it keeps its place per kind (closed and opened again, it opens there; the
 *   OTT's face keeps its own home); a move released near home lands home; with the face focused the arrows
 *   nudge it and Home sends it home; the OTT's face moves the same way; each move check starts from home;
 * - the resize: a corner drag scales the face (its width by the corner's way), shrunk past its base size the
 *   narrow face takes over at the base size and never goes under it, grown it stops at the window; a small
 *   corner drag on a narrow face at 1.2x stays narrow and scales smoothly until deliberate growth; at home
 *   from a plate the window clamps (left: slot A's first; right: slot B's) a small corner drag grows it
 *   positively, never the narrow face; a moved narrow face at the window's foot grows to full when the corner
 *   is pulled past it; - then + by the keys returns to the full face;
 * - a window resize pulls a moved face back into the window; a place saved by a real move and reloaded into a
 *   smaller window is the saved place, clamped, and a large stored place keeps every control reachable; the
 *   fit a move holds clears at the first window change (no return with a round trip); a window too short for the face under its plate or a scrolled plate
 *   above the margin detaches it, fitted to the window, every control in reach; a focused knob keeps its
 *   focus across the reflow;
 * - free-standing: no backdrop, a press beside the face reaches the looper (a lane's record key) and the
 *   transport keys play with the face focused or after a knob drag, the face open; it closes by its close,
 *   Escape in it (returning focus to the plate, the slot's fold button when folded, or a surviving chain
 *   control on feed removal) or its name again, never an Escape elsewhere; the OTT's and the ECHO's open at
 *   once, the last touched on top, no strap; the face at home follows its plate through a reorder; overlays:
 *   the face layer is inert under the tuner, a plate menu closes on Tab or Shift+Tab and moves on from its trigger in that direction, closes when the
 *   focus leaves it (a legacy row dialog does not), faces keep no slot record once closed, and Stage View
 *   hides and makes faces inert then restores them on exit; where the row folds, a fold leaves the face open
 *   where it stood, in the window, closing the last OTT cancels its draw loop (its glass or its miniature last), and the stage view queues
 *   no face draw;
 * - the RING MOD face: a press on its plate's name opens it under the plate with no strap (no popover);
 *   FREQ (log 20–1500 Hz, printed in Hz/kHz with nearest note at A4 = 440) and LEVEL send `SetUnitParam`
 *   and read the feed's value by keyboard and drag; the power switch is the bypass; scripted `unitMeters`
 *   frames light the carrier and OUT (L upper, R lower) and a frame without the unit clears both; FREQ
 *   moves the carrier wavelength and ruler marker; closing the last face cancels its rAF, and Stage View
 *   queues no ring draw;
 * - per size (1920x1080, 2560x1440, 1000x700, 960x600): every face opens at home, no strap, every legend at
 *   12 px or more and none clipped (inside the face and inside every box that clips it), their controls
 *   reached by the pointer, the ECHO's tape and the RING MOD's carrier live on the fake's timer.
 *
 * `--shots=<dir>` writes both faces open at home and moved at 1920x1080 and 1000x700
 * (`<echo|ott>-<home|moved>-<W>x<H>.png`), each size's OTT and ECHO at home, and both faces open at once at
 * 1920x1080 (`both-faces-1920x1080.png`). Each check is soft (`check`): a
 * run reports every red one, then fails. Cannot see the native engine (it does not speak the chain yet)
 * or any sound.
 * Run: pnpm probe unit-faces
 */
import assert from 'node:assert/strict';
import { arg, probe } from '../harness/probe.ts';

const SHOTS = arg('shots');
const CASE = arg('case');
const lane = (state) => ({ state, length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 });
const emptyLanes = () => [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: lane('Empty') } }));
const transport = (bpm, meter = { num: 4, den: 4, beats: 4 }) => ({ Transport: { frame: 0, master: 0, bpm, meter, locked: false } });
const SIZES = [
  [1920, 1080],
  [2560, 1440],
  [1000, 700],
  [960, 600],
];

const failures = [];
let ran = 0;
let current = null;
let currentSize = null;
async function check(name, fn) {
  if (CASE && !CASE.split(',').some((t) => name.toLowerCase().includes(t.toLowerCase()))) return;
  ran++;
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
    const ringFace = () => page.getByRole('dialog', { name: 'RING MOD, slot A' });
    const faceOf = (kind) => (kind === 'echo' ? echoFace() : kind === 'ring' ? ringFace() : ottFace());
    /** Open `kind`'s face from its plate's name, unless it is open (a red check before may have closed it). */
    const openFace = async (kind) => {
      const face = faceOf(kind);
      if (await face.count()) return;
      await show();
      await plate(await unitOf(kind)).locator('.unit__name').click();
      await face.waitFor({ timeout: 2000 });
      await settle();
    };
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
    /** The RING MOD's glass: lit carrier pixels on L (upper) and R (lower) at `frac` of HALF, lobe count
     * along L, the frequency ruler caret's x, and OUT's two ladders near their foot. */
    const ringGlass = (frac = 0.55) =>
      page.evaluate((frac) => {
        const cv = document.querySelector('.ring__canvas');
        const d = cv.width / cv.clientWidth;
        const W = cv.clientWidth;
        const H = cv.clientHeight;
        const narrow = H < 200;
        const MX = W - 46;
        const X1 = MX - 22;
        const YC = narrow ? 82 : 108;
        const HALF = narrow ? 46 : 66;
        const rule = narrow ? 140 : 192;
        const ctx = cv.getContext('2d');
        const row = (y) => ctx.getImageData(0, Math.round(y * d), cv.width, 1).data;
        const scan = (y) => {
          const r = row(y);
          let count = 0;
          let lobes = 0;
          let dark = 99;
          for (let x = 42; x < X1 - 2; x++) {
            const i = Math.round(x * d) * 4;
            const lit = r[i] > 170 && r[i + 3] > 60;
            if (lit) {
              count++;
              // 2 px bars on a 3 px pitch leave a 1 px gap inside a lobe; >= 4 px dark separates lobes
              if (dark >= 4) lobes++;
              dark = 0;
            } else dark++;
          }
          return { count, lobes };
        };
        const upper = scan(YC - frac * HALF);
        const lower = scan(YC + frac * HALF);
        // the carrier caret sits directly above `rule` (rule - 4)
        const mr = row(rule - 4);
        const markerXs = [];
        for (let x = 36; x <= X1 + 4; x++) {
          const i = Math.round(x * d) * 4;
          if (mr[i] > 200 && mr[i + 3] > 180) markerXs.push(x);
        }
        const markerX = markerXs.length ? (Math.min(...markerXs) + Math.max(...markerXs)) / 2 : null;
        const cell = (x) => [...ctx.getImageData(Math.round(x * d), Math.round((YC + HALF - 6) * d), 1, 1).data];
        return { litL: upper.count, lobesL: upper.lobes, litR: lower.count, markerX, meter: cell(MX + 5), meterR: cell(MX + 20) };
      }, frac);

    /** A page of its own at `viewport` with the ECHO's face open in slot A; `kept` is stored as `lf.unitFaces`
     * before the app loads (none: whatever the page keeps stays, so a reload keeps what a move saved). */
    const freshFace = async (viewport, keptJson) => {
      const { page: p2 } = await open({
        viewport,
        init: (p) =>
          p.addInitScript((k) => {
            window.__lfEngineFake = true;
            window.__lfChainFake = true;
            if (k) localStorage.setItem('lf.unitFaces', k);
          }, keptJson ?? null),
      });
      p2.setDefaultTimeout(4000);
      await openEchoIn(p2);
      return p2;
    };
    /** Slot A's ECHO added and its face opened on `p2` (the engine fake's lanes first). */
    const openEchoIn = async (p2) => {
      await p2.evaluate(() => window.__lf.native.emit({ seq: 1, reset: true, settings: [], events: [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 } } })) }));
      await p2.locator('.slot--a .unit-add').click();
      await p2.getByRole('menuitem', { name: /^ECHO/ }).first().click();
      await p2.waitForTimeout(120);
      await p2.locator('.slot--a .unit[data-kind="echo"] .unit__name').click();
      await p2.getByRole('dialog', { name: 'ECHO, slot A' }).waitFor();
      await p2.waitForTimeout(120);
    };

    // the scene: an OTT, an ECHO and a RING MOD in slot A
    await add('OTT');
    await add('ECHO');
    await add('RING MOD');

    if (first) {
      await check('the ECHO face: a press on the plate’s name opens it under the plate, with no strap', async () => {
        await openFace('echo');
        assert.equal(await page.locator('.infx-pop').count(), 0, 'no settings popover');
        const fb = await echoFace().boundingBox();
        const pb = await plate(await unitOf('echo')).boundingBox();
        assert.ok(fb.y > pb.y + pb.height && fb.y < pb.y + pb.height + 30, `under the plate: face top ${fb.y}, plate foot ${pb.y + pb.height}`);
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap drawn between plate and face');
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
      await check('TIME’s commands in order: a drag 1/8 → 1/8. → 1/8 across a >500 ms pause still sends [2, 1] and lands on 1/8', async () => {
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
        await page.waitForTimeout(600);
        await page.mouse.move(x, y, { steps: 4 });
        await page.mouse.up();
        await settle();
        assert.deepEqual(await timeSends(from), [2, 1], 'sent 1/8. (2), then 1/8 (1) even after >500 ms pause');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8, 250 ms', 'let go: the feed’s value');
        await hold(false);
        await settle();
        assert.equal((await chains())[0].units.find((u) => u.kind === 'echo').params.echoTime, 1, 'the engine ends at 1/8');
      });
      await check('TIME’s commands in order: late key reports answered one by one with a fourth key between them send [2, 0, 2, 0] and land on 1/4', async () => {
        await openFace('echo');
        await timeAt18();
        const echo = await unitOf('echo');
        const time = page.getByRole('slider', { name: 'ECHO time' });
        await hold(true);
        const from = (await sent()).length;
        await time.focus();
        await page.keyboard.press('ArrowUp');
        await page.keyboard.press('ArrowUp');
        await page.keyboard.press('ArrowDown');
        assert.deepEqual(await timeSends(from), [2, 0, 2], '1/8. (2), 1/4 (0), back to 1/8. (2)');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8, 250 ms', 'the read-out stays on the feed');
        const emitTime = async (idx) => {
          const cs = await chains();
          const u = cs[0].units.find((x) => x.id === echo);
          u.params = { ...u.params, echoTime: idx };
          await emit({ chains: cs });
          await page.waitForTimeout(40);
        };
        // answer the first two sends one by one before the third lands, then press Up a fourth time
        await emitTime(2);
        await emitTime(0);
        await page.keyboard.press('ArrowUp');
        assert.deepEqual(await timeSends(from), [2, 0, 2, 0], 'fourth Up counts from pending 1/8. (2) to 1/4 (0)');
        await hold(false);
        await settle();
        assert.equal((await chains())[0].units.find((u) => u.id === echo).params.echoTime, 0, 'the engine ends at 1/4 (0)');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/4, 500 ms', 'final feed value is 1/4');
      });
      await check('TIME’s commands in order: an unchanged snapshot is no answer; a late report retires only what it names (from 1/8: [2, 1], snapshot 1/8, then 2, then Up queues from 1/8)', async () => {
        await openFace('echo');
        await timeAt18();
        // the steps the checks before sent have aged out (a step is the base of the next for 500 ms)
        await page.waitForTimeout(600);
        const echo = await unitOf('echo');
        const time = page.getByRole('slider', { name: 'ECHO time' });
        await hold(true);
        const from = (await sent()).length;
        await time.focus();
        await page.keyboard.press('ArrowUp');
        await page.keyboard.press('ArrowDown');
        assert.deepEqual(await timeSends(from), [2, 1], '1/8. (2), then back to 1/8 (1)');
        // a chain snapshot that still reports 1/8: new unit objects, the same step
        await emit({ chains: await chains() });
        await page.waitForTimeout(40);
        // the first send's answer arrives late
        const cs = await chains();
        const u = cs[0].units.find((x) => x.id === echo);
        u.params = { ...u.params, echoTime: 2 };
        await emit({ chains: cs });
        await page.waitForTimeout(40);
        await page.keyboard.press('ArrowUp');
        assert.deepEqual(await timeSends(from), [2, 1, 2], 'the next Up queues from the request 1/8 (to 1/8.: 2), not from the late 2 (to 1/4: 0)');
        await hold(false);
        await settle();
        assert.equal((await chains())[0].units.find((x) => x.id === echo).params.echoTime, 2, 'the engine ends at 1/8. (2)');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8., 375 ms', 'the final feed value');
      });
      await check('TIME’s commands in order: a wheel event of several notches is one send, and deltaMode lines/pages normalize', async () => {
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
        await timeAt18();
        const fromMode = (await sent()).length;
        await time.dispatchEvent('wheel', { deltaY: -1, deltaMode: 1 });
        await settle();
        assert.deepEqual(await timeSends(fromMode), [2], '1 line up from 1/8 sends 1/8. (2)');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8., 375 ms');
        await time.dispatchEvent('wheel', { deltaY: 1, deltaMode: 1 });
        await settle();
        assert.deepEqual(await timeSends(fromMode), [2, 1], '1 line down returns to 1/8 (1)');
        await time.dispatchEvent('wheel', { deltaY: -1, deltaMode: 2 });
        await settle();
        assert.deepEqual(await timeSends(fromMode), [2, 1, 0], '1 page up reaches 1/4 (0) in one send');
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
        const cleared = await tape(0.3);
        assert.deepEqual(cleared.lit, [], 'a frame without the unit clears L trace');
        assert.deepEqual(cleared.litR, [], 'a frame without the unit clears R trace');
        assert.ok(cleared.meter[1] < 80 && cleared.meterR[1] < 80, `a frame without the unit clears both ladders: L ${cleared.meter}, R ${cleared.meterR}`);
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
      await check('the tape: scaled up while silent, its glass is drawn again at a larger backing size', async () => {
        await atHome('echo');
        await quiet();
        const before = await page.locator('.echo__canvas').evaluate((cv) => cv.width);
        await echoFace().focus();
        for (let k = 0; k < 3; k++) await page.keyboard.press('+');
        await page.waitForTimeout(200);
        const r = await page.locator('.echo__canvas').evaluate((cv) => ({ backing: cv.width, shown: cv.getBoundingClientRect().width * devicePixelRatio }));
        assert.ok(r.backing > before, `backing grew: ${before} -> ${r.backing}`);
        assert.ok(Math.abs(r.backing - r.shown) <= 2, `backing ${r.backing} px for ${r.shown.toFixed(1)} px shown`);
        await page.keyboard.press('Home');
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
      });
      await check('the tape: redraws its ruler while silent when the meter’s beats change', async () => {
        await atHome('echo');
        await quiet();
        const rulerTicks = () =>
          page.evaluate(() => {
            const cv = document.querySelector('.echo__canvas');
            const d = cv.width / cv.clientWidth;
            const W = cv.clientWidth;
            const H = cv.clientHeight;
            const narrow = H < 200;
            const X1 = W - 46 - 22;
            const rule = (narrow ? 86 : 118) + (narrow ? 50 : 74) + (narrow ? 8 : 10);
            const ctx = cv.getContext('2d');
            const row = ctx.getImageData(0, Math.round((rule + 2) * d), cv.width, 1).data;
            let count = 0;
            let inTick = false;
            for (let x = 38; x <= X1 + 2; x++) {
              const i = Math.round(x * d) * 4;
              const on = row[i + 3] > 20;
              if (on && !inTick) count++;
              inTick = on;
            }
            return count;
          });
        await emit({ events: [transport(120, { num: 6, den: 8, beats: 2 })] });
        await page.waitForTimeout(120);
        const t2 = await rulerTicks();
        await emit({ events: [transport(120, { num: 4, den: 4, beats: 4 })] });
        await page.waitForTimeout(120);
        const t4 = await rulerTicks();
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
        assert.ok(t4 > t2 + 2, `ruler ticks grew when meter changed 6/8 (2 beats) -> 4/4 (4 beats) at 120 BPM: ${t2} -> ${t4}`);
      });
      await check('the move: a drag on the panel moves the face with no strap, nothing sent, the face stays open, its size not pinned', async () => {
        await atHome('echo');
        const before = await echoFace().boundingBox();
        const n = (await sent()).length;
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, -150, 70);
        const after = await echoFace().boundingBox();
        assert.ok(Math.abs(after.x - before.x + 150) < 1 && Math.abs(after.y - before.y - 70) < 1, `moved by the pointer's way: ${after.x - before.x}, ${after.y - before.y}`);
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap');
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
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap');
        await closeFace();
        await openFace('ott');
        const pb = await plate(await unitOf('ott')).boundingBox();
        const ob = await ottFace().boundingBox();
        assert.ok(ob.y > pb.y + pb.height && ob.y < pb.y + pb.height + 30, 'the OTT at its home');
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap at home');
        await closeFace();
      });
      await check('the move: released near home it lands home; the arrows nudge, Home sends it home', async () => {
        await atHome('echo');
        const home = await echoFace().boundingBox();
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, -150, 70);
        const q = await panelPoint('.echo');
        await drag(q.x, q.y, 146, -66);
        const snapped = await echoFace().boundingBox();
        assert.ok(Math.abs(snapped.x - home.x) < 1 && Math.abs(snapped.y - home.y) < 1 && (await kept()).echo?.at === null, 'snapped back home');
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap');
        await echoFace().focus();
        await page.keyboard.press('ArrowRight');
        await page.keyboard.press('Shift+ArrowDown');
        await settle();
        const nudged = await echoFace().boundingBox();
        assert.ok(Math.abs(nudged.x - home.x - 8) < 1 && Math.abs(nudged.y - home.y - 40) < 1, `nudged 8 right, 40 down: ${nudged.x - home.x}, ${nudged.y - home.y}`);
        assert.ok((await kept()).echo?.at !== null, 'moved off home');
        await page.keyboard.press('Home');
        await settle();
        const back = await echoFace().boundingBox();
        assert.ok(Math.abs(back.x - home.x) < 1 && Math.abs(back.y - home.y) < 1 && (await kept()).echo?.at === null, 'Home: home');
      });
      await check('the move: a narrow-reflowed face keeps its displayed fit on release and nudge without storing a size preference', async () => {
        await page.setViewportSize({ width: 1400, height: 600 });
        await page.waitForTimeout(200);
        await atHome('echo');
        const b0 = await echoFace().boundingBox();
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow')), 'premise: narrow at home at 1400x600');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, 30, -30);
        const b1 = await echoFace().boundingBox();
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow')), 'stays narrow on release');
        assert.ok(Math.abs(b1.height - b0.height) < 1 && Math.abs(b1.y - (b0.y - 30)) < 2, `no jump on release: ${JSON.stringify(b0)} -> ${JSON.stringify(b1)}`);
        assert.equal((await kept()).echo?.size, null, 'size preference stays null');
        // the nudge alone, from a fresh reflowed home face (the drag's own hold gone with Home)
        await echoFace().focus();
        await page.keyboard.press('Home');
        await settle();
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow')), 'premise: narrow at home again');
        const h0 = await echoFace().boundingBox();
        await page.keyboard.press('ArrowUp');
        await settle();
        const b2 = await echoFace().boundingBox();
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow')), 'stays narrow after nudge');
        assert.ok(Math.abs(b2.height - h0.height) < 1 && Math.abs(b2.y - (h0.y - 8)) < 2, `nudge up 8 px without jump: ${h0.y} -> ${b2.y}, ${h0.height} -> ${b2.height}`);
        assert.equal((await kept()).echo?.size, null, 'size preference still null');
        await page.setViewportSize({ width: W, height: H });
        await page.waitForTimeout(250);
        assert.ok(!(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow'))), 'roomy window restores full face (size was not stored)');
        await echoFace().focus();
        await page.keyboard.press('Home');
      });
      await check('the move: the displayed fit holds for the window it was made in and clears at the first change; a round trip does not bring it back', async () => {
        await page.setViewportSize({ width: 1400, height: 600 });
        await page.waitForTimeout(200);
        await atHome('echo');
        const narrow = () => page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow'));
        assert.ok(await narrow(), 'premise: narrow at home at 1400x600');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, 30, -30);
        assert.ok(await narrow(), 'held narrow on release');
        await page.setViewportSize({ width: W, height: H });
        await page.waitForTimeout(250);
        assert.ok(!(await narrow()), 'a roomy window: the full face');
        await page.setViewportSize({ width: 1400, height: 600 });
        await page.waitForTimeout(250);
        assert.ok(!(await narrow()), 'back at 1400x600: the old hold did not return (the moved face has room for the full layout)');
        await page.setViewportSize({ width: W, height: H });
        await page.waitForTimeout(250);
        await echoFace().focus();
        await page.keyboard.press('Home');
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
      await check('the resize: a small corner drag on a narrow face at 1.2x stays narrow and scales smoothly until deliberate growth', async () => {
        await atHome('echo');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, -200, 40);
        // shrink to narrow at 1.0x, then grow along narrow's shape to ~1.2x
        const g0 = await box('.echo .uface__grip');
        await drag(g0.x + g0.width / 2, g0.y + g0.height / 2, -700, -500);
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow')), 'premise: narrow at 1.0x');
        const g1 = await box('.echo .uface__grip');
        await drag(g1.x + g1.width / 2, g1.y + g1.height / 2, 180, 76);
        const b1 = await echoFace().boundingBox();
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow')), `still narrow at ~1.2x (${b1.width.toFixed(0)}x${b1.height.toFixed(0)})`);
        assert.ok(b1.width > 1040 && b1.width < 1130, `narrow grew to ~1.2x: ${b1.width}`);
        // a small drag from narrow 1.2x must stay narrow without flipping to full
        const g2 = await box('.echo .uface__grip');
        await drag(g2.x + g2.width / 2, g2.y + g2.height / 2, 12, 8, 4);
        const b2 = await echoFace().boundingBox();
        assert.ok(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow')), 'small drag from narrow 1.2x stays narrow');
        assert.ok(b2.width > b1.width && b2.width - b1.width < 30 && b2.height - b1.height < 20, `grew smoothly: ${b1.width.toFixed(0)}x${b1.height.toFixed(0)} -> ${b2.width.toFixed(0)}x${b2.height.toFixed(0)}`);
        // deliberate downward/outward growth flips to full
        const g3 = await box('.echo .uface__grip');
        await drag(g3.x + g3.width / 2, g3.y + g3.height / 2, 100, 260);
        assert.ok(!(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow'))), 'deliberate growth switches to full face');
        await echoFace().focus();
        await page.keyboard.press('Home');
      });
      await check('the resize: a moved narrow face at 1.2x touching the window’s foot grows to full when the corner is pulled past the foot (1920x700)', async () => {
        const p2 = await freshFace({ width: 1920, height: 700 }, JSON.stringify({ echo: { at: { x: 400, y: 900 }, size: { narrow: true, k: 1.2 } } }));
        try {
          const f = p2.locator('.echo');
          assert.equal(await f.evaluate((e) => e.classList.contains('uface--narrow')), true, 'premise: narrow');
          const b0 = await f.boundingBox();
          assert.ok(Math.abs(b0.y + b0.height - 692) < 1, `premise: touching the foot margin (${b0.y + b0.height})`);
          assert.ok(Math.abs(b0.width - 1080) < 4, `premise: 1.2x (${b0.width})`);
          const g = await p2.locator('.echo .uface__grip').boundingBox();
          const [x, y] = [g.x + g.width / 2, g.y + g.height / 2];
          await p2.mouse.move(x, y);
          await p2.mouse.down();
          await p2.mouse.move(x, y + 150, { steps: 8 });
          await p2.mouse.up();
          await p2.waitForTimeout(150);
          assert.equal(await f.evaluate((e) => e.classList.contains('uface--narrow')), false, 'pulled past the foot: the full face');
          const b1 = await f.boundingBox();
          assert.ok(b1.y >= 7.5 && b1.y + b1.height <= 692.5, `fitted to the window: ${b1.y}..${b1.y + b1.height}`);
          assert.equal((await p2.evaluate(() => JSON.parse(localStorage.getItem('lf.unitFaces')))).echo.size.narrow, false, 'kept as the full face');
        } finally {
          await p2.close();
        }
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
      await check('reloading a large saved position into a smaller viewport keeps the face in window and controls reachable', async () => {
        const { page: p2 } = await open({
          viewport: { width: 1000, height: 700 },
          init: (p) =>
            p.addInitScript(() => {
              window.__lfEngineFake = true;
              window.__lfChainFake = true;
              localStorage.setItem('lf.unitFaces', JSON.stringify({ echo: { at: { x: 1600, y: 900 }, size: { narrow: false, k: 1.4 } } }));
            }),
        });
        try {
          p2.setDefaultTimeout(4000);
          await p2.evaluate(() => window.__lf.native.emit({ seq: 1, reset: true, settings: [], events: [0, 1, 2, 3, 4].map((i) => ({ Lane: { frame: 0, lane: i, info: { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 } } })) }));
          await p2.locator('.slot--a .unit-add').click();
          await p2.getByRole('menuitem', { name: /^ECHO/ }).first().click();
          await p2.waitForTimeout(120);
          await p2.locator('.slot--a .unit[data-kind="echo"] .unit__name').click();
          const f2 = p2.getByRole('dialog', { name: 'ECHO, slot A' });
          await f2.waitFor();
          await p2.waitForTimeout(120);
          const inWin = await p2.$eval('.echo', (el) => {
            const r = el.getBoundingClientRect();
            return r.left >= -0.5 && r.top >= -0.5 && r.right <= innerWidth + 0.5 && r.bottom <= innerHeight + 0.5;
          });
          assert.ok(inWin, 'reloaded face clamped inside 1000x700 window');
          const missed = await p2.$$eval('.echo .uface__knob, .echo .uface__pwr, .echo .uface__close, .echo .echo__st', (els) =>
            els
              .filter((el) => {
                const r = el.getBoundingClientRect();
                const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
                const inside = r.width > 0 && r.left >= 0 && r.top >= 0 && r.right <= innerWidth && r.bottom <= innerHeight;
                return !(inside && hit && (hit === el || el.contains(hit)));
              })
              .map((el) => el.getAttribute('aria-label') ?? el.className),
          );
          assert.deepEqual(missed, [], 'every control reachable after reload into smaller viewport');
        } finally {
          await p2.close();
        }
      });
      await check('a place saved by a real move and reloaded into a smaller window is the saved place, clamped (1920x1080 to 1000x700)', async () => {
        const p2 = await freshFace({ width: 1920, height: 1080 }, null);
        try {
          const f = p2.locator('.echo');
          const b0 = await f.boundingBox();
          // a press on the panel under the name, left of the knobs, carries the face right and down
          const sc = b0.width / 900;
          const [x, y] = [b0.x + 120 * sc, b0.y + (b0.height / sc > 400 ? 470 : 325) * sc];
          await p2.mouse.move(x, y);
          await p2.mouse.down();
          await p2.mouse.move(x + 600, y + 300, { steps: 8 });
          await p2.mouse.up();
          await p2.waitForTimeout(150);
          const saved = (await p2.evaluate(() => JSON.parse(localStorage.getItem('lf.unitFaces')))).echo;
          assert.ok(saved.at && saved.at.x > 400 && saved.at.y > 300, `a real move saved a place: ${JSON.stringify(saved)}`);
          await p2.setViewportSize({ width: 1000, height: 700 });
          await p2.reload();
          await openEchoIn(p2);
          const b1 = await p2.locator('.echo').boundingBox();
          const wantLeft = Math.max(8, Math.min(saved.at.x, 1000 - 8 - b1.width));
          const wantTop = Math.max(8, Math.min(saved.at.y, 700 - 8 - b1.height));
          assert.ok(wantLeft < saved.at.x - 100 && wantTop < saved.at.y - 100, 'premise: the window clamps both');
          assert.ok(Math.abs(b1.x - wantLeft) < 1 && Math.abs(b1.y - wantTop) < 1, `the saved place ${JSON.stringify(saved.at)}, clamped to ${wantLeft},${wantTop}; drawn at ${b1.x},${b1.y}`);
          // not the face's home: the saved place was read, and it is not under the plate
          const pb = await p2.locator('.slot--a .unit[data-kind="echo"]').boundingBox();
          assert.ok(Math.abs(b1.y - (pb.y + pb.height + 19)) > 20, `not at home (${b1.y} against the plate’s foot ${pb.y + pb.height})`);
        } finally {
          await p2.close();
        }
      });
      await check('the OTT face moves the same way: no strap, its place kept', async () => {
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
      await check('free-standing: after a knob drag (or lost capture), Space and Enter drive the looper; a Tab-focused slider keeps its focus', async () => {
        await atHome('echo');
        const level = page.getByRole('slider', { name: 'ECHO level' });
        const lb = await level.boundingBox();
        await drag(lb.x + lb.width / 2, lb.y + lb.height / 2, 0, -24, 4);
        const n1 = (await sent()).length;
        await page.keyboard.press('Space');
        await settle();
        assert.ok((await sent()).slice(n1).some((c) => c.Action === 'RecDub'), 'Space after knob drag sends RecDub');
        const n2 = (await sent()).length;
        await page.keyboard.press('Enter');
        await settle();
        assert.ok((await sent()).slice(n2).some((c) => c.Action === 'PlayStop'), 'Enter after knob drag sends PlayStop');
        // lost pointer capture on TIME knob also returns pointer-only focus to the frame
        const time = page.getByRole('slider', { name: 'ECHO time' });
        const tb = await time.boundingBox();
        await page.mouse.move(tb.x + tb.width / 2, tb.y + tb.height / 2);
        await page.mouse.down();
        await page.mouse.move(tb.x + tb.width / 2, tb.y + tb.height / 2 - 40, { steps: 3 });
        await time.evaluate((el) => el.dispatchEvent(new PointerEvent('lostpointercapture', { pointerId: 1, bubbles: false })));
        // before the button is released, so a pointer-up cannot stand in for the capture-loss handler
        assert.equal(await page.evaluate(() => document.activeElement?.classList.contains('uface')), true, 'the capture lost: focus on the frame while the button is still down');
        await page.mouse.up();
        await settle();
        const n3 = (await sent()).length;
        await page.keyboard.press('Space');
        await settle();
        assert.ok((await sent()).slice(n3).some((c) => c.Action === 'RecDub'), 'Space after lostpointercapture sends RecDub');
        // a keyboard user's focus on a slider stays
        const fb = page.getByRole('slider', { name: 'ECHO feedback' });
        await fb.focus();
        await page.keyboard.press('ArrowUp');
        await settle();
        assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('aria-label')), 'ECHO feedback', 'keyboard slider focus preserved');
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
      await check('feed removal with focus in face returns focus to a surviving chain control via returnFocus', async () => {
        await page.locator('.slot--b .unit-add').click();
        await page.getByRole('menuitem', { name: /^ECHO/ }).first().click();
        await settle();
        const echoB = (await chains())[1].units.find((u) => u.kind === 'echo').id;
        await plate(echoB).locator('.unit__name').click();
        const faceB = page.getByRole('dialog', { name: 'ECHO, slot B' });
        await faceB.waitFor();
        await faceB.getByRole('slider', { name: 'ECHO level' }).focus();
        // wait past ChainLine's 1 s lostAt window so only FaceHost's returnFocus can land focus back in slot B
        await page.waitForTimeout(1150);
        const cs = await chains();
        cs[1].units = cs[1].units.filter((u) => u.id !== echoB);
        await emit({ chains: cs });
        await settle();
        assert.equal(await faceB.count(), 0, 'removed unit closes its face');
        const focusedInB = await page.evaluate(() => document.activeElement?.closest('.slot--b') != null && document.activeElement !== document.body);
        assert.ok(focusedInB, `focus returned to a surviving slot B control, got ${await page.evaluate(() => `${document.activeElement?.tagName}.${document.activeElement?.className}`)}`);
        const n = (await sent()).length;
        await page.keyboard.press('Space');
        await settle();
        assert.ok((await sent()).slice(n).some((c) => c.Action === 'RecDub'), 'returnFocus lets Space drive the looper');
      });
      await check('closing faces keeps no slot record: open, close, unit removed leave none; an open face replaced by another of its kind leaves one', async () => {
        await closeFace();
        const records = () => page.evaluate(async () => (await import('/src/ui/units/FaceHost.tsx')).faceSlotRecords());
        const r0 = await records();
        await page.locator('.slot--b .unit-add').click();
        await page.getByRole('menuitem', { name: /^ECHO/ }).first().click();
        await settle();
        const echoB = (await chains())[1].units.find((u) => u.kind === 'echo').id;
        // open, close: no record stays
        await plate(echoB).locator('.unit__name').click();
        await page.getByRole('dialog', { name: 'ECHO, slot B' }).waitFor();
        assert.equal(await records(), r0 + 1, 'an open face keeps its last slot (for the feed’s removal)');
        await page.getByRole('dialog', { name: 'ECHO, slot B' }).locator('.uface__close').click();
        await settle();
        assert.equal(await records(), r0, 'closed: no record stays');
        // open, close, then the unit leaves the feed: still none
        const cs = await chains();
        cs[1].units = cs[1].units.filter((u) => u.id !== echoB);
        await emit({ chains: cs });
        await settle();
        assert.equal(await records(), r0, 'the unit removed after its face closed: no record');
        // an open face replaced by another of its kind: one record, not two
        await page.locator('.slot--b .unit-add').click();
        await page.getByRole('menuitem', { name: /^ECHO/ }).first().click();
        await settle();
        const echoB2 = (await chains())[1].units.find((u) => u.kind === 'echo').id;
        await openFace('echo');
        assert.equal(await records(), r0 + 1, 'slot A’s ECHO face open: one record');
        await plate(echoB2).locator('.unit__name').click();
        await page.getByRole('dialog', { name: 'ECHO, slot B' }).waitFor();
        await settle();
        assert.equal(await echoFace().count(), 0, 'premise: slot A’s face was replaced');
        assert.equal(await records(), r0 + 1, 'replaced by slot B’s: one record');
        await closeFace();
        const cs2 = await chains();
        cs2[1].units = cs2[1].units.filter((u) => u.id !== echoB2);
        await emit({ chains: cs2 });
        await settle();
        assert.equal(await records(), r0, 'closed and removed: none');
      });
      await check('overlays: face layer is inert under the tuner', async () => {
        await atHome('echo');
        await echoFace().focus();
        await page.keyboard.press('n');
        await page.locator('.tn').waitFor({ timeout: 2000 });
        await settle();
        assert.equal(await page.locator('.uface-layer').evaluate((el) => el.inert), true, 'face layer inert under tuner');
        for (let k = 0; k < 4; k++) {
          await page.keyboard.press('Tab');
          assert.equal(await page.evaluate(() => document.activeElement?.closest('.uface') != null), false, 'Tab under tuner never enters face');
        }
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await page.locator('.tn').count(), 0, 'Escape closed tuner');
        assert.equal(await echoFace().count(), 1, 'face stayed open after tuner closed');
        await closeFace();
      });
      await check('overlays: Tab and Shift+Tab in a plate menu close it and move on from its trigger, each in its own direction', async () => {
        await atHome('echo');
        const trigger = page.locator('.slot--a .unit-add');
        const who = () => page.evaluate(() => `${document.activeElement?.tagName}|${document.activeElement?.className}|${document.activeElement?.getAttribute('aria-label')}|${document.activeElement?.closest('.unit')?.getAttribute('data-unit')}`);
        // the controls either side of the trigger, as the browser's own Tab finds them
        await trigger.focus();
        await page.keyboard.press('Tab');
        const after = await who();
        await trigger.focus();
        await page.keyboard.press('Shift+Tab');
        const before = await who();
        await trigger.focus();
        const at = await who();
        assert.notEqual(after, at, 'premise: a control after the trigger');
        assert.notEqual(before, at, 'premise: a control before the trigger');
        assert.notEqual(before, after, 'premise: the two differ');
        for (const [key, want, name] of [['Tab', after, 'after'], ['Shift+Tab', before, 'before']]) {
          await trigger.click();
          await page.locator('.slot-pop').waitFor({ timeout: 2000 });
          await settle();
          assert.equal(await page.evaluate(() => document.activeElement?.getAttribute('role')?.startsWith('menuitem') ?? false), true, 'premise: focus in the menu');
          await page.keyboard.press(key);
          await settle();
          assert.equal(await page.locator('.slot-pop').count(), 0, `${key} closed the menu`);
          assert.equal(await who(), want, `${key} lands on the control ${name} its trigger`);
          assert.equal(await page.evaluate(() => document.activeElement?.closest('.uface') != null), false, `${key} did not reach the face under the backdrop`);
        }
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await echoFace().count(), 1, 'Escape after a menu dismiss leaves the face open');
        await closeFace();
      });
      await check('overlays: a menu closes when the focus leaves it by the pointer or a script; a legacy row dialog does not', async () => {
        await atHome('echo');
        const trigger = page.locator('.slot--a .unit-add');
        await trigger.click();
        await page.locator('.slot-pop').waitFor({ timeout: 2000 });
        await settle();
        // a control outside takes the focus (a pointer press on one, or a script)
        await page.evaluate(() => document.querySelector('.slot--a .slot__id, .slot--a .slot__fold, .slot--a .plate__menu')?.focus());
        await settle();
        assert.equal(await page.locator('.slot-pop').count(), 0, 'the focus left the menu: it closed');
        // a legacy row dialog: REVERB’s Parameters
        await add('REVERB');
        const reverb = await unitOf('reverb');
        await plate(reverb).locator('.unit__name').click();
        const dlg = page.getByRole('dialog', { name: 'REVERB parameters' });
        await dlg.waitFor({ timeout: 2000 });
        await settle();
        await page.evaluate(() => document.querySelector('.slot--a .slot__id, .slot--a .slot__fold, .slot--a .unit-add')?.focus());
        await settle();
        assert.equal(await dlg.count(), 1, 'the focus left the dialog: it stays open');
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await dlg.count(), 0, 'Escape closes it');
        const cs = await chains();
        cs[0].units = cs[0].units.filter((u) => u.id !== reverb);
        await emit({ chains: cs });
        await settle();
        await closeFace();
      });
      await check('stage view hides and makes faces inert; on exit the same faces return where they were', async () => {
        await atHome('echo');
        const p = await panelPoint('.echo');
        await drag(p.x, p.y, -120, 60);
        await atHome('ott');
        const eb = await echoFace().boundingBox();
        const ob = await ottFace().boundingBox();
        await ottFace().focus();
        await page.keyboard.press('b');
        await page.locator('.sv').waitFor({ timeout: 2000 });
        await settle();
        const hiddenState = await page.locator('.uface-layer').evaluate((el) => ({ hidden: el.hidden, inert: el.inert, display: getComputedStyle(el).display }));
        assert.deepEqual(hiddenState, { hidden: true, inert: true, display: 'none' }, 'face layer hidden and inert in stage view');
        for (let k = 0; k < 3; k++) {
          await page.keyboard.press('Tab');
          assert.equal(await page.evaluate(() => document.activeElement?.closest('.uface') != null), false, 'Tab in stage view never enters hidden face');
        }
        await page.keyboard.press('b');
        await settle();
        assert.equal(await page.locator('.sv').count(), 0, 'stage view exited');
        assert.equal((await echoFace().count()) + (await ottFace().count()), 2, 'both faces survived stage view');
        const ea = await echoFace().boundingBox();
        const oa = await ottFace().boundingBox();
        assert.ok(Math.abs(ea.x - eb.x) < 1 && Math.abs(ea.y - eb.y) < 1, `ECHO back where it was: ${JSON.stringify(eb)} -> ${JSON.stringify(ea)}`);
        assert.ok(Math.abs(oa.x - ob.x) < 1 && Math.abs(oa.y - ob.y) < 1, `OTT back where it was: ${JSON.stringify(ob)} -> ${JSON.stringify(oa)}`);
        await echoFace().focus();
        await page.keyboard.press('Home');
        await closeFace();
      });
      await check('stage view: the faces’ draw loops queue nothing while it hides them, and each draws again on exit', async () => {
        await atHome('echo');
        await atHome('ott');
        await page.evaluate(() => {
          const origRaf = window.requestAnimationFrame.bind(window);
          window.__drawSpy = { origRaf, echo: 0, ott: 0 };
          window.requestAnimationFrame = (cb) => {
            // the ECHO loop's frame draws glasses; the OTT loop's draws glasses and miniatures
            const src = String(cb);
            const kind = src.includes('drawGlass') ? (src.includes('minis') ? 'ott' : 'echo') : null;
            return origRaf((ts) => {
              if (kind && window.__drawSpy) window.__drawSpy[kind]++;
              cb(ts);
            });
          };
        });
        try {
          const ran = () => page.evaluate(() => ({ echo: window.__drawSpy.echo, ott: window.__drawSpy.ott }));
          await page.waitForTimeout(120);
          const shown = await ran();
          assert.ok(shown.echo > 0 && shown.ott > 0, `premise: both faces draw while shown ${JSON.stringify(shown)}`);
          await ottFace().focus();
          await page.keyboard.press('b');
          await page.locator('.sv').waitFor({ timeout: 2000 });
          await settle();
          const hidden0 = await ran();
          await page.waitForTimeout(300);
          assert.deepEqual(await ran(), hidden0, 'no face draw callback ran while the stage view hid the faces');
          await page.keyboard.press('b');
          await page.locator('.sv').waitFor({ state: 'detached', timeout: 2000 });
          await page.waitForTimeout(200);
          const back = await ran();
          assert.ok(back.echo > hidden0.echo, `the ECHO draws again after the stage view (${hidden0.echo} -> ${back.echo})`);
          assert.ok(back.ott > hidden0.ott, `the OTT draws again after the stage view (${hidden0.ott} -> ${back.ott})`);
          assert.equal((await echoFace().count()) + (await ottFace().count()), 2, 'both faces still open');
        } finally {
          await page.evaluate(() => {
            if (!window.__drawSpy) return;
            window.requestAnimationFrame = window.__drawSpy.origRaf;
            delete window.__drawSpy;
          });
        }
        await closeFace();
      });
      await check('free-standing: the OTT’s and the ECHO’s faces open at once, the last touched on top, no strap', async () => {
        await atHome('echo');
        await atHome('ott');
        assert.equal((await echoFace().count()) + (await ottFace().count()), 2, 'both open');
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap on either face');
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
        assert.equal(await page.locator('.uface-tie').count(), 0, 'still no strap');
        await echoFace().focus();
        await page.keyboard.press('Home');
        await closeFace();
      });
      await check('the face at home follows its plate when the chain reorders or shifts under it (no strap)', async () => {
        const echo = await unitOf('echo');
        const ott = await unitOf('ott');
        await atHome('echo');
        const before = await echoFace().boundingBox();
        await plate(echo).locator('.plate__menu').click();
        await page.getByRole('menuitem', { name: 'Move left', exact: true }).click();
        await page.waitForTimeout(250);
        assert.equal((await chains())[0].units[0].id, echo, 'the ECHO heads the chain');
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap');
        const pb = await plate(echo).boundingBox();
        const after = await echoFace().boundingBox();
        assert.ok(after.x < before.x - 20, `face moved left with its plate: ${before.x} -> ${after.x}`);
        const wantLeft = Math.max(8, Math.min(pb.x + pb.width / 2 - 333 * (after.width / 900), W - 8 - after.width));
        assert.ok(Math.abs(after.x - wantLeft) < 2 && after.y > pb.y + pb.height && after.y < pb.y + pb.height + 30, `face under plate’s new place: x ${after.x} (want ${wantLeft}), y ${after.y}`);
        // move ECHO back right, then remove the OTT ahead of it so ECHO's own DOM node never re-inserts (only slotChain shifts its x)
        await plate(echo).locator('.plate__menu').click();
        await page.getByRole('menuitem', { name: 'Move right', exact: true }).click();
        await page.waitForTimeout(250);
        const beforeShift = await echoFace().boundingBox();
        const csOrig = await chains();
        const csWithoutOtt = JSON.parse(JSON.stringify(csOrig));
        csWithoutOtt[0].units = csWithoutOtt[0].units.filter((u) => u.id !== ott);
        await emit({ chains: csWithoutOtt });
        await page.waitForTimeout(200);
        const pbShift = await plate(echo).boundingBox();
        const afterShift = await echoFace().boundingBox();
        const wantShift = Math.max(8, Math.min(pbShift.x + pbShift.width / 2 - 333 * (afterShift.width / 900), W - 8 - afterShift.width));
        assert.ok(afterShift.x < beforeShift.x - 20 && Math.abs(afterShift.x - wantShift) < 2, `face followed plate shift without re-insert: ${beforeShift.x} -> ${afterShift.x} (want ${wantShift})`);
        await emit({ chains: csOrig });
        await page.waitForTimeout(200);
        await closeFace();
      });
      await check('TIME’s printed positions are pressed by the pointer, keep TIME focus, and are never focused inside aria-hidden', async () => {
        await atHome('echo');
        const time = page.getByRole('slider', { name: 'ECHO time' });
        await time.focus();
        await page.locator('.echo .echo__st', { hasText: '1/4' }).click();
        await settle();
        const f = await page.evaluate(() => ({
          inHidden: document.activeElement?.closest('[aria-hidden="true"]') != null,
          focusable: [...document.querySelectorAll('.echo__st')].some((e) => e.tabIndex >= -1 && (e.tagName === 'BUTTON' || e.hasAttribute('tabindex'))),
          activeLabel: document.activeElement?.getAttribute('aria-label'),
        }));
        assert.deepEqual(f, { inHidden: false, focusable: false, activeLabel: 'ECHO time' }, 'TIME slider keeps focus; nothing focused inside hidden print');
        assert.equal(await time.getAttribute('aria-valuetext'), '1/4, 500 ms', 'the press took');
        await page.keyboard.press('ArrowDown');
        await settle();
        assert.equal(await time.getAttribute('aria-valuetext'), '1/8., 375 ms', 'ArrowDown still steers TIME');
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
        assert.ok(Math.abs(r0.x + r0.width - (W - 8)) < 1 && (await page.locator('.uface-tie').count()) === 0, `premise: at home against the right margin (${r0.x + r0.width})`);
        const gr = await box('.echo .uface__grip');
        await drag(gr.x + gr.width / 2, gr.y + gr.height / 2, 40, 10, 4);
        const r1 = await faceB.boundingBox();
        assert.ok(!(await page.locator('.echo').evaluate((e) => e.classList.contains('uface--narrow'))), 'pushed past the edge, not the narrow face');
        assert.ok(r1.height > r0.height && r1.height - r0.height < 30, `grown positively by the way down: ${r0.height} -> ${r1.height}`);
        await faceB.focus();
        await page.keyboard.press('Home');
        await closeFace();
        await plate(echoB).locator('.plate__menu').click();
        await page.getByRole('menuitem', { name: 'Remove', exact: true }).click();
        await page.waitForTimeout(250);
        // pointer pushed past the right window edge is clamped to the window margin (same size as stopping at W - 8)
        await atHome('echo');
        const pm = await panelPoint('.echo');
        await drag(pm.x, pm.y, 450, 0);
        const startEdge = await echoFace().boundingBox();
        const gEdge = await box('.echo .uface__grip');
        const toMargin = W - 8 - (gEdge.x + gEdge.width / 2);
        await drag(gEdge.x + gEdge.width / 2, gEdge.y + gEdge.height / 2, toMargin, 0, 4);
        const atMargin = await echoFace().boundingBox();
        await echoFace().focus();
        await page.keyboard.press('Home');
        await settle();
        const pm2 = await panelPoint('.echo');
        await drag(pm2.x, pm2.y, 450, 0);
        const gPast = await box('.echo .uface__grip');
        await drag(gPast.x + gPast.width / 2, gPast.y + gPast.height / 2, toMargin + 600, 0, 4);
        const pastMargin = await echoFace().boundingBox();
        assert.ok(Math.abs(pastMargin.width - atMargin.width) < 4, `pointer past right edge clamped to margin: atMargin ${atMargin.width.toFixed(1)} vs pastMargin ${pastMargin.width.toFixed(1)} (start ${startEdge.x.toFixed(0)})`);
        await echoFace().focus();
        await page.keyboard.press('Home');
        await closeFace();
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
      await check('a window too short for the face under its plate: detached, fitted to the window, every control inside', async () => {
        await atHome('echo');
        await page.setViewportSize({ width: 1000, height: 440 });
        await page.waitForTimeout(300);
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap');
        const pbShort = await plate(await unitOf('echo')).boundingBox();
        const fbShort = await echoFace().boundingBox();
        assert.ok(fbShort.y < pbShort.y + pbShort.height, `detached above under-plate home (${fbShort.y} vs plate foot ${pbShort.y + pbShort.height})`);
        const t = await typeAndClip('.echo');
        assert.ok(t.inWindow, `in the window: ${JSON.stringify(fbShort)}`);
        assert.deepEqual(await unreached('.echo .uface__knob, .echo .uface__pwr, .echo .uface__close, .echo .echo__st'), [], 'every control in reach');
        await page.setViewportSize({ width: W, height: H });
        await page.waitForTimeout(250);
        const pbTall = await plate(await unitOf('echo')).boundingBox();
        const fbTall = await echoFace().boundingBox();
        assert.ok(fbTall.y > pbTall.y + pbTall.height, 'room again: back under the plate');
        await closeFace();
      });
      await check('a scrolled plate at 1000x350 never pulls the face’s close button above the viewport', async () => {
        // exact review geometry: plate bottom at -60 in a 1000x350 window must detach (top >= 8), never top -41
        const direct = await page.evaluate(async () => {
          const { placeFace } = await import('/src/ui/units/face-place.ts');
          return placeFace({ plate: { left: 200, width: 140, bottom: -60 }, win: { w: 1000, h: 350 }, ui: 1, size: null, at: null });
        });
        assert.ok(direct.top >= 8 && !direct.home, `detached above-margin home at 1000x350: ${JSON.stringify(direct)}`);
        // live DOM at 1000x350 with the plate scrolled above the viewport (plate bottom <= -60)
        await atHome('echo');
        await page.setViewportSize({ width: 1000, height: 350 });
        await page.waitForTimeout(250);
        await page.evaluate(() => {
          const stack = document.querySelector('.splitstack--v');
          if (stack) {
            stack.style.paddingBottom = '40px';
            stack.scrollTop = stack.scrollHeight;
          }
        });
        await page.waitForTimeout(250);
        const pb = await plate(await unitOf('echo')).boundingBox();
        const fb = await echoFace().boundingBox();
        try {
          assert.ok(pb && pb.y + pb.height <= -59, `premise: plate scrolled above viewport (plate bottom ${pb?.y + pb?.height})`);
          assert.ok(fb.y >= 8 - 0.5, `face top clamped inside viewport margin: top ${fb.y}`);
          assert.deepEqual(await unreached('.echo .uface__close, .echo .uface__pwr, .echo .uface__knob'), [], 'close button and controls reachable');
        } finally {
          await page.evaluate(() => {
            const stack = document.querySelector('.splitstack--v');
            if (stack) {
              stack.style.paddingBottom = '';
              stack.scrollTop = 0;
            }
          });
          await page.setViewportSize({ width: W, height: H });
          await page.waitForTimeout(200);
          await closeFace();
        }
      });
      await check('the ECHO draw loop cancels its queued frame on last close', async () => {
        await atHome('echo');
        await page.evaluate(() => {
          const origRaf = window.requestAnimationFrame.bind(window);
          const origCancel = window.cancelAnimationFrame.bind(window);
          window.__echoSpy = { origRaf, origCancel, lastId: 0, cancelled: [], ranAfter: 0, closed: false };
          window.requestAnimationFrame = (cb) => {
            const isEcho = String(cb).includes('drawGlass') && !String(cb).includes('minis');
            const id = origRaf((ts) => {
              if (isEcho && window.__echoSpy.closed) window.__echoSpy.ranAfter++;
              cb(ts);
            });
            if (isEcho) window.__echoSpy.lastId = id;
            return id;
          };
          window.cancelAnimationFrame = (id) => {
            window.__echoSpy.cancelled.push(id);
            return origCancel(id);
          };
        });
        try {
          await page.waitForTimeout(80);
          await page.locator('.echo .uface__close').click();
          await page.evaluate(() => {
            window.__echoSpy.closed = true;
          });
          await page.waitForTimeout(100);
          const res = await page.evaluate(() => ({ lastId: window.__echoSpy.lastId, cancelled: window.__echoSpy.cancelled, ranAfter: window.__echoSpy.ranAfter }));
          assert.ok(res.lastId > 0 && res.cancelled.includes(res.lastId), `queued ECHO rAF ${res.lastId} cancelled: ${JSON.stringify(res.cancelled)}`);
          assert.equal(res.ranAfter, 0, `no ECHO draw callback ran after close (${res.ranAfter})`);
        } finally {
          await page.evaluate(() => {
            if (!window.__echoSpy) return;
            window.requestAnimationFrame = window.__echoSpy.origRaf;
            window.cancelAnimationFrame = window.__echoSpy.origCancel;
            delete window.__echoSpy;
          });
        }
      });
      // ── the RING MOD face ("the carrier") ──────────────────────────────────────────────────────────
      await check('the RING MOD face: a press on the plate’s name opens it under the plate, with no strap', async () => {
        await closeFace();
        await openFace('ring');
        assert.equal(await page.locator('.infx-pop').count(), 0, 'no settings popover');
        const fb = await ringFace().boundingBox();
        const pb = await plate(await unitOf('ring')).boundingBox();
        assert.ok(fb.y > pb.y + pb.height && fb.y < pb.y + pb.height + 30, `under the plate: face top ${fb.y}, plate foot ${pb.y + pb.height}`);
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap drawn between plate and face');
        assert.deepEqual(
          await ringFace().getByRole('slider').evaluateAll((els) => els.map((e) => e.getAttribute('aria-label'))),
          ['RING MOD frequency', 'RING MOD level'],
        );
      });
      await check('the RING MOD face: FREQ (log 20–1500 Hz, Hz/kHz + note) and LEVEL send SetUnitParam and read the feed', async () => {
        await openFace('ring');
        const ring = await unitOf('ring');
        const freq = page.getByRole('slider', { name: 'RING MOD frequency' });
        const freqVal = page.locator('.ring .uface__ctl:has([data-param="ringFreq"]) .uface__val');
        const glassRead = page.locator('.ring .ring__read');
        assert.equal(await freq.getAttribute('aria-valuetext'), '440 Hz, A4', 'default 440 Hz · A4');
        assert.match(await freqVal.textContent(), /^440\s*Hz\s*·\s*A4$/);
        assert.match(await glassRead.textContent(), /^440\s*Hz\s*·\s*A4$/);
        await freq.focus();
        await page.keyboard.press('ArrowUp');
        await settle();
        const upHz = (await chains())[0].units.find((u) => u.id === ring).params.ringFreq;
        assert.ok(upHz > 440 && upHz < 470, `log ArrowUp from 440 Hz stepped smoothly (got ${upHz})`);
        assert.ok((await sent()).some((c) => c.SetUnitParam?.[0] === ring && c.SetUnitParam[1] === 'ringFreq' && c.SetUnitParam[2] === upHz), 'SetUnitParam ringFreq sent');
        await page.keyboard.press('Home');
        await settle();
        assert.equal((await chains())[0].units.find((u) => u.id === ring).params.ringFreq, 20, 'Home: 20 Hz');
        assert.equal(await freq.getAttribute('aria-valuetext'), '20 Hz, D#0');
        await page.keyboard.press('End');
        await settle();
        assert.equal((await chains())[0].units.find((u) => u.id === ring).params.ringFreq, 1500, 'End: 1500 Hz');
        assert.equal(await freq.getAttribute('aria-valuetext'), '1.50 kHz, F#6');
        assert.match(await freqVal.textContent(), /^1\.50\s*kHz\s*·\s*F#6$/);
        // log drag down halfway (110 px): from 1500 Hz lands near geometric midpoint sqrt(20*1500) ≈ 173 Hz
        const fb = await freq.boundingBox();
        await drag(fb.x + fb.width / 2, fb.y + fb.height / 2, 0, 110, 6);
        const midHz = (await chains())[0].units.find((u) => u.id === ring).params.ringFreq;
        assert.ok(midHz > 120 && midHz < 240, `half-travel log drag landed near geometric mid ~173 Hz (got ${midHz})`);
        await freq.dblclick();
        await settle();
        assert.equal((await chains())[0].units.find((u) => u.id === ring).params.ringFreq, 440, 'double-click resets to 440 Hz');
        // LEVEL knob: keyboard and drag
        const level = page.getByRole('slider', { name: 'RING MOD level' });
        await level.focus();
        await page.keyboard.press('End');
        await settle();
        assert.ok((await sent()).some((c) => c.SetUnitParam?.[0] === ring && c.SetUnitParam[1] === 'ringLevel' && c.SetUnitParam[2] === 1), 'End: 1');
        assert.equal(await level.getAttribute('aria-valuenow'), '100');
        await page.keyboard.press('ArrowDown');
        await settle();
        assert.equal((await chains())[0].units.find((u) => u.id === ring).params.ringLevel, 0.99);
        assert.match(await page.locator('.ring .uface__ctl:has([data-param="ringLevel"]) .uface__val').textContent(), /^99\s*%$/);
        const lb = await level.boundingBox();
        await drag(lb.x + lb.width / 2, lb.y + lb.height / 2, 0, 110, 4);
        const took = (await chains())[0].units.find((u) => u.id === ring).params.ringLevel;
        assert.ok(took > 0.44 && took < 0.56, `a 110 px drag down lowers LEVEL by ~0.5 (took ${took})`);
      });
      await check('the RING MOD face: the power switch is the unit’s bypass, and the face shows it', async () => {
        await openFace('ring');
        const ring = await unitOf('ring');
        const pwr = page.locator('.ring .uface__pwr');
        assert.equal(await pwr.getAttribute('aria-pressed'), 'true');
        await pwr.click();
        await settle();
        assert.ok((await sent()).some((c) => c.Action?.ToggleUnit === ring), 'ToggleUnit sent');
        assert.equal((await chains())[0].units.find((u) => u.id === ring).bypass, true, 'the fake bypassed it');
        assert.equal(await pwr.getAttribute('aria-pressed'), 'false');
        assert.ok(await page.locator('.ring').evaluate((e) => e.classList.contains('uface--byp')), 'the face drawn bypassed');
        await page.waitForTimeout(300);
        assert.equal(await page.locator('.ring .ring__canvas').evaluate((e) => getComputedStyle(e).opacity), '0.4', 'its glass sits back');
        await pwr.click();
        await settle();
        assert.equal(await pwr.getAttribute('aria-pressed'), 'true', 'on again');
      });
      await check('the RING MOD face: scripted unitMeters light the carrier and OUT (L/R), FREQ moves the wave and marker, and omission clears it', async () => {
        await openFace('ring');
        const ring = await unitOf('ring');
        await page.evaluate(() => (window.__lf.native.chain.meterTimer = false));
        try {
          await emit({ unitMeters: [] });
          await page.waitForTimeout(150);
          const rest = await ringGlass();
          assert.equal(rest.litL, 0, 'silent: upper carrier flat and dim');
          assert.equal(rest.litR, 0, 'silent: lower carrier flat and dim');
          assert.ok(rest.meter[1] < 80 && rest.meterR[1] < 80, `silent: OUT ladders dark (L ${rest.meter}, R ${rest.meterR})`);
          // loud stereo frames light both halves and both OUT ladders
          for (let i = 0; i < 4; i++) await emit({ unitMeters: [{ unit: ring, out: [0.85, 0.85] }] });
          await page.waitForTimeout(120);
          const lit440 = await ringGlass();
          assert.ok(lit440.litL > 40 && lit440.litR > 40, `lit stereo carrier: L ${lit440.litL}, R ${lit440.litR}`);
          assert.ok(lit440.meter[1] > 120 && lit440.meterR[1] > 120, `OUT lit: L ${lit440.meter}, R ${lit440.meterR}`);
          // right-only frame lights lower half and R ladder while upper half stays flat
          await emit({ unitMeters: [] });
          await page.waitForTimeout(60);
          await emit({ unitMeters: [{ unit: ring, out: [0, 0.85] }] });
          await page.waitForTimeout(120);
          const rightOnly = await ringGlass();
          assert.equal(rightOnly.litL, 0, 'right-only: L half stays flat');
          assert.ok(rightOnly.litR > 40, `right-only: R half lit (${rightOnly.litR})`);
          assert.ok(rightOnly.meter[1] < 80 && rightOnly.meterR[1] > 120, `right-only OUT: L ${rightOnly.meter}, R ${rightOnly.meterR}`);
          // changing FREQ from 20 Hz to 1500 Hz packs more carrier lobes across the glass and shifts the ruler marker
          await emit({ unitMeters: [{ unit: ring, out: [0.85, 0.85] }] });
          await page.evaluate(async (id) => {
            const { sendEngine } = await import('/src/platform/index.ts');
            sendEngine({ SetUnitParam: [id, 'ringFreq', 20] });
          }, ring);
          await page.waitForTimeout(120);
          const at20 = await ringGlass(0.5);
          await page.evaluate(async (id) => {
            const { sendEngine } = await import('/src/platform/index.ts');
            sendEngine({ SetUnitParam: [id, 'ringFreq', 1500] });
          }, ring);
          await page.waitForTimeout(120);
          const at1500 = await ringGlass(0.5);
          assert.ok(at1500.lobesL > at20.lobesL + 10, `carrier frequency packs lobes on log scale: 20 Hz (${at20.lobesL}) -> 1500 Hz (${at1500.lobesL})`);
          assert.ok(at20.markerX !== null && at1500.markerX !== null && at1500.markerX - at20.markerX > 650, `ruler marker spans scale: ${at20.markerX} -> ${at1500.markerX}`);
          await page.evaluate(async (id) => {
            const { sendEngine } = await import('/src/platform/index.ts');
            sendEngine({ SetUnitParam: [id, 'ringFreq', 440] });
          }, ring);
          // a frame without the unit clears both halves and both OUT ladders
          await emit({ unitMeters: [] });
          await page.waitForTimeout(120);
          const cleared = await ringGlass();
          assert.equal(cleared.litL, 0, 'omission clears L carrier');
          assert.equal(cleared.litR, 0, 'omission clears R carrier');
          assert.ok(cleared.meter[1] < 80 && cleared.meterR[1] < 80, `omission clears OUT: L ${cleared.meter}, R ${cleared.meterR}`);
        } finally {
          await page.evaluate(() => (window.__lf.native.chain.meterTimer = true));
        }
      });
      await check('the RING MOD draw loop cancels its queued frame on last close and queues nothing under the stage view', async () => {
        await atHome('ring');
        await page.evaluate(() => {
          const origRaf = window.requestAnimationFrame.bind(window);
          const origCancel = window.cancelAnimationFrame.bind(window);
          window.__ringSpy = { origRaf, origCancel, lastId: 0, cancelled: [], ran: 0, ranAfter: 0, closed: false };
          window.requestAnimationFrame = (cb) => {
            const isRing = String(cb).includes('drawRingGlass');
            const id = origRaf((ts) => {
              if (isRing && window.__ringSpy) {
                window.__ringSpy.ran++;
                if (window.__ringSpy.closed) window.__ringSpy.ranAfter++;
              }
              cb(ts);
            });
            if (isRing && window.__ringSpy) window.__ringSpy.lastId = id;
            return id;
          };
          window.cancelAnimationFrame = (id) => {
            if (window.__ringSpy) window.__ringSpy.cancelled.push(id);
            return origCancel(id);
          };
        });
        try {
          await page.waitForTimeout(100);
          const shown = await page.evaluate(() => window.__ringSpy.ran);
          assert.ok(shown > 0, `premise: RING MOD draws while shown (${shown})`);
          // stage view suspends the ring draw loop and resumes it on exit
          await ringFace().focus();
          await page.keyboard.press('b');
          await page.locator('.sv').waitFor({ timeout: 2000 });
          await settle();
          const hidden0 = await page.evaluate(() => window.__ringSpy.ran);
          await page.waitForTimeout(250);
          assert.equal(await page.evaluate(() => window.__ringSpy.ran), hidden0, 'no RING MOD draw callback ran while stage view hid the face');
          await page.keyboard.press('b');
          await page.locator('.sv').waitFor({ state: 'detached', timeout: 2000 });
          await page.waitForTimeout(150);
          const resumed = await page.evaluate(() => window.__ringSpy.ran);
          assert.ok(resumed > hidden0, `RING MOD draws again after stage view (${hidden0} -> ${resumed})`);
          // closing the last RING MOD face cancels its queued rAF
          await page.locator('.ring .uface__close').click();
          await page.evaluate(() => {
            window.__ringSpy.closed = true;
          });
          await page.waitForTimeout(100);
          const res = await page.evaluate(() => ({ lastId: window.__ringSpy.lastId, cancelled: window.__ringSpy.cancelled, ranAfter: window.__ringSpy.ranAfter }));
          assert.ok(res.lastId > 0 && res.cancelled.includes(res.lastId), `queued RING MOD rAF ${res.lastId} cancelled: ${JSON.stringify(res.cancelled)}`);
          assert.equal(res.ranAfter, 0, `no RING MOD draw callback ran after close (${res.ranAfter})`);
        } finally {
          await page.evaluate(() => {
            if (!window.__ringSpy) return;
            window.requestAnimationFrame = window.__ringSpy.origRaf;
            window.cancelAnimationFrame = window.__ringSpy.origCancel;
            delete window.__ringSpy;
          });
        }
      });
    }

    // ── a fold: where the row shows one slot at a time, the other's plate folds away under an open face ──
    await show();
    if ((await page.locator('.slot--b .slot__fold').count()) > 0) {
      await check(`a fold at ${W}x${H}: the face stays open where it stood, and Escape returns focus to the fold button`, async () => {
        await atHome('echo');
        const before = await echoFace().boundingBox();
        await page.locator('.slot--b .slot__fold').click();
        await settle();
        assert.equal(await page.locator('.slot--a .slot__fold').count(), 1, 'premise: slot A folded');
        assert.equal(await echoFace().count(), 1, 'still open');
        const b = await echoFace().boundingBox();
        assert.ok(Math.abs(b.x - before.x) < 1 && Math.abs(b.y - before.y) < 1, 'where it stood');
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap');
        assert.ok((await typeAndClip('.echo')).inWindow, 'in the window');
        // Escape in a folded slot's face returns focus to the slot's fold button through returnFocus
        await echoFace().getByRole('slider', { name: 'ECHO level' }).focus();
        await page.keyboard.press('Escape');
        await settle();
        assert.equal(await echoFace().count(), 0, 'Escape closed folded face');
        const onFold = await page.evaluate(() => document.activeElement === document.querySelector('.slot--a .slot__fold'));
        assert.ok(onFold, `focus returned to slot A fold button, got ${await page.evaluate(() => document.activeElement?.className)}`);
        const n = (await sent()).length;
        await page.keyboard.press('Space');
        await settle();
        assert.ok((await sent()).slice(n).some((c) => c.Action === 'RecDub'), 'returnFocus lets Space arm REC without unfolding');
        assert.equal(await page.locator('.slot--a .slot__fold').count(), 1, 'slot A still folded after Space');
        await show();
        await settle();
        await closeFace();
      });
      await check(`the OTT draw loop at ${W}x${H} cancels its queued frame when closed after its slot folds`, async () => {
        await show();
        await atHome('ott');
        await page.locator('.slot--b .slot__fold').click();
        await settle();
        assert.equal(await page.locator('.slot--a .slot__fold').count(), 1, 'slot A folded (OTT miniature unregistered)');
        await page.evaluate(() => {
          const origRaf = window.requestAnimationFrame.bind(window);
          const origCancel = window.cancelAnimationFrame.bind(window);
          window.__ottSpy = { origRaf, origCancel, lastId: 0, cancelled: [], ranAfter: 0, closed: false };
          window.requestAnimationFrame = (cb) => {
            const isOtt = String(cb).includes('minis');
            const id = origRaf((ts) => {
              if (isOtt && window.__ottSpy.closed) window.__ottSpy.ranAfter++;
              cb(ts);
            });
            if (isOtt) window.__ottSpy.lastId = id;
            return id;
          };
          window.cancelAnimationFrame = (id) => {
            window.__ottSpy.cancelled.push(id);
            return origCancel(id);
          };
        });
        try {
          await page.waitForTimeout(80);
          await page.locator('.ott .uface__close').click();
          await page.evaluate(() => {
            window.__ottSpy.closed = true;
          });
          await page.waitForTimeout(100);
          const res = await page.evaluate(() => ({ lastId: window.__ottSpy.lastId, cancelled: window.__ottSpy.cancelled, ranAfter: window.__ottSpy.ranAfter }));
          assert.ok(res.lastId > 0 && res.cancelled.includes(res.lastId), `queued OTT rAF ${res.lastId} cancelled: ${JSON.stringify(res.cancelled)}`);
          assert.equal(res.ranAfter, 0, `no OTT draw callback ran after close (${res.ranAfter})`);
        } finally {
          await page.evaluate(() => {
            if (!window.__ottSpy) return;
            window.requestAnimationFrame = window.__ottSpy.origRaf;
            window.cancelAnimationFrame = window.__ottSpy.origCancel;
            delete window.__ottSpy;
          });
        }
        await show();
        await settle();
        await closeFace();
      });
      await check(`the OTT draw loop at ${W}x${H} cancels its queued frame when its miniature is the last to go (the face closed first, then the slot folds)`, async () => {
        await show();
        await atHome('ott');
        await page.locator('.ott .uface__close').click();
        await settle();
        assert.equal(await ottFace().count(), 0, 'premise: the face is closed (the glass gone, the miniature still drawn)');
        await page.evaluate(() => {
          const origRaf = window.requestAnimationFrame.bind(window);
          const origCancel = window.cancelAnimationFrame.bind(window);
          window.__ottSpy = { origRaf, origCancel, lastId: 0, cancelled: [], ranAfter: 0, closed: false };
          window.requestAnimationFrame = (cb) => {
            const isOtt = String(cb).includes('minis');
            const id = origRaf((ts) => {
              if (isOtt && window.__ottSpy.closed) window.__ottSpy.ranAfter++;
              cb(ts);
            });
            if (isOtt) window.__ottSpy.lastId = id;
            return id;
          };
          window.cancelAnimationFrame = (id) => {
            window.__ottSpy.cancelled.push(id);
            return origCancel(id);
          };
        });
        try {
          await page.waitForTimeout(80);
          assert.ok(await page.evaluate(() => window.__ottSpy.lastId > 0), 'premise: the miniature keeps the loop queued');
          await page.locator('.slot--b .slot__fold').click();
          await settle();
          assert.equal(await page.locator('.slot--a .slot__fold').count(), 1, 'slot A folded (the OTT miniature unregistered)');
          await page.evaluate(() => {
            window.__ottSpy.closed = true;
          });
          await page.waitForTimeout(100);
          const res = await page.evaluate(() => ({ lastId: window.__ottSpy.lastId, cancelled: window.__ottSpy.cancelled, ranAfter: window.__ottSpy.ranAfter }));
          assert.ok(res.cancelled.includes(res.lastId), `queued OTT rAF ${res.lastId} cancelled: ${JSON.stringify(res.cancelled)}`);
          assert.equal(res.ranAfter, 0, `no OTT draw callback ran after the miniature went (${res.ranAfter})`);
        } finally {
          await page.evaluate(() => {
            if (!window.__ottSpy) return;
            window.requestAnimationFrame = window.__ottSpy.origRaf;
            window.cancelAnimationFrame = window.__ottSpy.origCancel;
            delete window.__ottSpy;
          });
        }
        await show();
        await settle();
      });
    }

    // ── per size: every face at home, type and clipping, reach, live glass ────────────────────────────
    for (const kind of ['echo', 'ott', 'ring']) {
      await check(`${kind.toUpperCase()} at ${W}x${H}: at home, no strap, legends 12 px or more and none clipped, its controls in reach`, async () => {
        await openFace(kind);
        const sel = `.${kind}`;
        // home, whatever an earlier check left
        await faceOf(kind).focus();
        await page.keyboard.press('Home');
        await settle();
        assert.equal(await page.locator('.uface-tie').count(), 0, 'no strap at home');
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
        } else if (kind === 'ring') {
          await page.evaluate(async (id) => {
            const { sendEngine } = await import('/src/platform/index.ts');
            sendEngine({ SetUnitParam: [id, 'ringFreq', 440] }, { SetUnitParam: [id, 'ringLevel', 0.5] });
          }, await unitOf('ring'));
          // wait until the fake's timer lights the carrier brightly
          await page.waitForFunction(() => {
            const cv = document.querySelector('.ring__canvas');
            if (!cv || cv.width === 0) return false;
            const d = cv.width / cv.clientWidth;
            const H = cv.clientHeight;
            const YC = H < 200 ? 82 : 108;
            const HALF = H < 200 ? 46 : 66;
            const row = cv.getContext('2d').getImageData(0, Math.round((YC - 0.55 * HALF) * d), cv.width, 1).data;
            let n = 0;
            for (let x = 42; x < cv.clientWidth - 70; x++) if (row[Math.round(x * d) * 4] > 170 && row[Math.round(x * d) * 4 + 3] > 60) n++;
            return n > 40;
          }, { timeout: 3000 });
          assert.ok((await ringGlass(0.55)).litL > 40, 'the carrier runs on the fake’s meters');
        } else if (SHOTS) await page.waitForTimeout(2500);
        await page.evaluate(() => document.activeElement?.blur());
        if (SHOTS) await shot(`${kind}-home`);
        if (SHOTS && kind === 'ring' && W === 1920) {
          // narrow layout at 1920x1080, glass alive
          await faceOf(kind).focus();
          await page.keyboard.press('-');
          await settle();
          const tn = await typeAndClip(sel);
          assert.ok(tn.min >= 12 - 1e-6, `narrow smallest type ${tn.min.toFixed(2)} px (${tn.what})`);
          assert.deepEqual(tn.clipped, [], 'no legend clipped in narrow');
          await page.evaluate(() => document.activeElement?.blur());
          await page.waitForFunction(() => {
            const cv = document.querySelector('.ring__canvas');
            if (!cv || cv.width === 0) return false;
            const d = cv.width / cv.clientWidth;
            const row = cv.getContext('2d').getImageData(0, Math.round((82 - 0.55 * 46) * d), cv.width, 1).data;
            let n = 0;
            for (let x = 42; x < cv.clientWidth - 70; x++) if (row[Math.round(x * d) * 4] > 170 && row[Math.round(x * d) * 4 + 3] > 60) n++;
            return n > 40;
          }, { timeout: 3000 });
          await shot('ring-narrow');
          await faceOf(kind).focus();
          await page.keyboard.press('Home');
          await settle();
        }
        if (SHOTS && (W === 1920 || W === 1000)) {
          // moved off home: over the lanes
          const p = await panelPoint(sel);
          await drag(p.x, p.y, W === 1920 ? 520 : -60, W === 1920 ? 200 : 140);
          await page.evaluate(() => document.activeElement?.blur());
          await page.waitForTimeout(600);
          await shot(`${kind}-moved`);
          await faceOf(kind).focus();
          await page.keyboard.press('Home');
        }
        await closeFace();
      });
    }
    await page.close();
  }
  assert.ok(ran > 0, `no checks matched --case=${CASE}`);
  assert.deepEqual(failures, [], `${failures.length} check(s) red`);
});
