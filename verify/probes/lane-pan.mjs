/**
 * The lane's pan control (`Pan` in `src/ui/looper/Looper.tsx`), in engine mode on the web engine fake
 * (`src/platform/host.web.ts`, the `engine-seam` pattern; the fake applies `SetPan` and reports each
 * change as the engine's `Mix`). The probe drives the real control:
 *
 * - (a) gestures: a drag sends `SetPan` and the read-out follows; a pointer landing inside the detent
 *   (2 around the centre) centres it, one just past it does not; the arrow keys step by 1 across the
 *   detent (R 3, R 2, R 1, C, L 1, L 2), Page Up/Down by 10, Home and End are hard left and right; the 0
 *   key centres it while it is focused and does nothing to it otherwise; a double-click and an
 *   Alt-click centre it (the Alt-click sends nothing but the centre);
 * - (b) the overlay (3b's rule, `lane-mix` covers the machinery): a key gesture whose report waits shows
 *   its value over an unequal `Mix`, the report ends it, and a later `Mix` moves the control;
 * - (c) layout at 1000x700, 1280x820 and 1920x1080 with six pills on lane 1: the pan is 44x24, on one
 *   mix row with the volume fader, and every control and read-out of each lane's right cluster lies
 *   inside it, overlaps no other, is not clipped and is under its own centre point (the reach test of
 *   `layout-reachability`), the read-outs at their widest (`L 100`, `−40.0 dB`);
 * - (d) accessibility: each lane's pan is a slider named `Track N pan` whose `aria-valuetext` is its
 *   read-out, disabled on an EMPTY lane as the volume is;
 * - (e) autosave: a jam saved and clean saves again when only a lane's pan changes (its fingerprint
 *   holds the pan), once (the archive's pan: `engine-session`).
 *
 * Cannot see the native engine (the pan's sound, its glide: lf-engine `tests/pan.rs`), WebView2's pointer
 * handling or the feel of the detent: the owner's hand and ear judge that. Run: pnpm probe lane-pan
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const BAR = 2 * RATE; // one 4/4 bar at 120 BPM
const MASTER = 2 * BAR; // two bars, so TRIM shows

const lane = (state, extra = {}) => ({
  state,
  length: state === 'Empty' ? 0 : MASTER,
  armed: false,
  autoArmed: false,
  canUndo: false,
  canReverse: state !== 'Empty',
  reversed: false,
  stopAt: null,
  fading: false,
  retakePass: 0,
  ...extra,
});
const laneEvent = (i, info) => ({ Lane: { frame: 0, lane: i, info } });
const STATES = [lane('Playing', { canUndo: true }), lane('Playing'), lane('Stopped'), lane('Empty'), lane('Empty')];

await probe(async ({ open }) => {
  let seq = 0;
  /** Open the app at `viewport` on the fake, with `STATES` on the feed. */
  const boot = async (viewport) => {
    const app = await open({ viewport, init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)) });
    const { page } = app;
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    await page.evaluate((f) => window.__lf.native.emit(f), {
      seq: ++seq,
      reset: true,
      settings: [],
      events: [{ Transport: { frame: 0, master: MASTER, bpm: 120, locked: true } }, ...STATES.map((s, i) => laneEvent(i, s)), { Selected: { frame: 0, lane: 0 } }],
      anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      meter: { peak: 0, clip: false },
      peaks: [],
    });
    await page.locator('.lp-lane').first().and(page.locator('[data-state="play"]')).waitFor({ timeout: 5000 });
    return app;
  };

  // ── (a) gestures, (b) the overlay, (d) accessibility ─────────────────────────────────────────────────
  {
    const { page, consoleErrors } = await boot({ width: 1600, height: 900 });
    const pan = (i) => page.getByRole('slider', { name: `Track ${i + 1} pan`, exact: true });
    const readout = (i) => page.locator('.lp-lane').nth(i).locator('.lp-pan__val');
    const shown = async (i) => ({ value: Number(await pan(i).inputValue()), text: await readout(i).textContent(), valuetext: await pan(i).getAttribute('aria-valuetext') });
    const applied = (i) => page.evaluate((i) => window.__lf.looper.trackPan(i), i);
    const sent = () => page.evaluate(() => window.__lf.native.sent.slice());
    const clearSent = () => page.evaluate(() => void (window.__lf.native.sent.length = 0));
    const pans = async () => (await sent()).filter((c) => c.SetPan).map((c) => c.SetPan);
    const seam = (name, on) => page.evaluate(([name, on]) => void (window.__lf.native[name] = on), [name, on]);
    /** Wait until the engine's pan of lane `i` is `v`. */
    const engineHas = (i, v) => page.waitForFunction(([i, v]) => window.__lf.looper.trackPan(i) === v, [i, v], { timeout: 5000 });
    const blur = () => page.evaluate(() => (document.activeElement instanceof HTMLElement ? document.activeElement.blur() : undefined));
    // The values a pointer gesture puts on the range before the control's handler reads them.
    await page.evaluate(() => {
      window.__raw = [];
      window.addEventListener('input', (e) => e.target instanceof HTMLInputElement && e.target.closest('.lp-lane__pan') && window.__raw.push(Number(e.target.value)), true);
    });
    const raw = () => page.evaluate(() => window.__raw.splice(0));

    // (d) accessibility, at rest.
    const access = [];
    for (let i = 0; i < 5; i++) {
      const s = await shown(i);
      access.push({ lane: i + 1, ...s, disabled: await pan(i).isDisabled(), volumeDisabled: await page.getByRole('slider', { name: `Track ${i + 1} volume`, exact: true }).isDisabled() });
    }
    console.log('(d) accessibility', JSON.stringify(access));
    for (const a of access) {
      assert.deepEqual([a.value, a.text, a.valuetext], [0, 'C', 'C'], `lane ${a.lane} starts centred, its valuetext the read-out`);
      assert.equal(a.disabled, a.lane >= 4, `lane ${a.lane}'s pan is disabled exactly on an EMPTY lane`);
      assert.equal(a.disabled, a.volumeDisabled, `lane ${a.lane}'s pan is disabled as its volume is`);
    }
    assert.deepEqual(
      await pan(0).evaluate((el) => [el.min, el.max, el.step, el.type]),
      ['-100', '100', '1', 'range'],
      'a range over -100..100',
    );

    // (a) A drag to the right sends its pan; the read-out and the valuetext follow.
    const box = await pan(0).boundingBox();
    const y = box.y + box.height / 2;
    await clearSent();
    await page.mouse.move(box.x + box.width / 2, y);
    await page.mouse.down();
    await page.mouse.move(box.x + box.width * 0.85, y, { steps: 4 });
    await page.mouse.up();
    await page.waitForFunction(() => window.__lf.native.sent.some((c) => c.SetPan), undefined, { timeout: 5000 });
    const dragged = { shown: await shown(0), sent: await pans() };
    await engineHas(0, dragged.sent.at(-1)[1]);
    console.log('(a) drag', JSON.stringify(dragged));
    assert.ok(dragged.shown.value > 40, 'the drag panned right');
    assert.deepEqual([dragged.shown.text, dragged.shown.valuetext], [`R ${dragged.shown.value}`, `R ${dragged.shown.value}`], 'the read-out and valuetext say R n');
    assert.deepEqual(dragged.sent.at(-1), [0, dragged.shown.value / 100], 'the last send is the shown value');
    await raw();

    // The pointer's detent: inside it (a raw 1 or 2) the drag lands on the centre; just past it, not. The
    // thumb's centre travels the box less the 11 px thumb over 200 steps.
    const xAt = (v) => box.x + 5.5 + ((v + 100) / 200) * (box.width - 11);
    const dragTo = async (v) => {
      await page.mouse.move(box.x + box.width * 0.85, y);
      await page.mouse.down();
      await page.mouse.move(xAt(v), y, { steps: 6 });
      await page.mouse.up();
      return { raw: (await raw()).at(-1), shown: await shown(0), sent: (await pans()).at(-1) };
    };
    const inside = await dragTo(1.8);
    const past = await dragTo(4.2);
    const insideLeft = await dragTo(-1.8);
    console.log('(a) detent', JSON.stringify({ inside, past, insideLeft }));
    assert.ok(inside.raw >= 1 && inside.raw <= 2, `the pointer landed inside the detent but off the centre (raw ${inside.raw})`);
    assert.deepEqual([inside.shown.text, inside.sent], ['C', [0, 0]], 'inside the detent the drag centres');
    assert.ok(insideLeft.raw <= -1 && insideLeft.raw >= -2, `and on the left (raw ${insideLeft.raw})`);
    assert.deepEqual([insideLeft.shown.text, insideLeft.sent], ['C', [0, 0]], 'inside the detent on the left too');
    assert.ok(past.raw > 2, `past the detent (raw ${past.raw})`);
    assert.deepEqual([past.shown.text, past.sent], [`R ${past.raw}`, [0, past.raw / 100]], 'past the detent the drag keeps its value');
    await engineHas(0, 0);

    // The detent is the pointer's alone: an input with no pointer down (an assistive technology's
    // increment) keeps a value inside it.
    const unpointed = [];
    for (const v of [2, 1, -1, -2]) {
      await clearSent();
      await pan(0).evaluate((el, value) => {
        el.value = String(value);
        el.dispatchEvent(new Event('input', { bubbles: true }));
      }, v);
      unpointed.push({ v, text: (await shown(0)).text, sent: (await pans()).at(-1) });
    }
    console.log('(a) input with no pointer', JSON.stringify(unpointed));
    assert.deepEqual(
      unpointed.map(({ text, sent }) => [text, sent]),
      [['R 2', [0, 0.02]], ['R 1', [0, 0.01]], ['L 1', [0, -0.01]], ['L 2', [0, -0.02]]],
      'an input with no pointer down steps inside the detent',
    );
    await pan(0).evaluate((el) => {
      el.value = '0';
      el.dispatchEvent(new Event('input', { bubbles: true }));
    });
    await engineHas(0, 0);

    // The keys: focused, from R 3 the arrows step across the detent; Page by 10; Home and End; 0 centres.
    await pan(0).focus();
    await page.keyboard.press('End');
    await page.keyboard.press('Home');
    await page.keyboard.press('0');
    for (let k = 0; k < 3; k++) await page.keyboard.press('ArrowRight');
    await clearSent();
    const steps = [];
    for (const key of ['ArrowLeft', 'ArrowLeft', 'ArrowLeft', 'ArrowDown', 'ArrowLeft', 'PageDown', 'PageUp', 'ArrowUp']) {
      await page.keyboard.press(key);
      steps.push((await shown(0)).text);
    }
    const stepSent = await pans();
    console.log('(a) keys', JSON.stringify({ steps, stepSent }));
    assert.deepEqual(steps, ['R 2', 'R 1', 'C', 'L 1', 'L 2', 'L 12', 'L 2', 'L 1'], 'the arrow keys step by 1 across the detent, Page by 10');
    assert.deepEqual(stepSent.map(([, v]) => v), [0.02, 0.01, 0, -0.01, -0.02, -0.12, -0.02, -0.01], 'each step sends its value');
    await clearSent();
    await page.keyboard.press('End');
    const end = (await shown(0)).text;
    await page.keyboard.press('Home');
    const home = (await shown(0)).text;
    await page.keyboard.press('0');
    const zero = (await shown(0)).text;
    const homeEndSent = await sent();
    console.log('(a) Home/End/0', JSON.stringify({ end, home, zero, homeEndSent }));
    assert.deepEqual([end, home, zero], ['R 100', 'L 100', 'C'], 'End and Home go hard right and left, 0 centres');
    assert.deepEqual(homeEndSent, [{ SetPan: [0, 1] }, { SetPan: [0, -1] }, { SetPan: [0, 0] }], 'and send only that');
    // 0 is the focused control's: unfocused, it leaves the pan alone.
    await page.keyboard.press('End');
    await engineHas(0, 1);
    await blur();
    await clearSent();
    await page.keyboard.press('0');
    await page.waitForTimeout(100);
    const unfocused = { shown: (await shown(0)).text, sent: await pans() };
    console.log('(a) 0 unfocused', JSON.stringify(unfocused));
    assert.deepEqual(unfocused, { shown: 'R 100', sent: [] }, 'the 0 key centres only a focused pan');

    // A double-click centres it.
    await clearSent();
    await pan(0).dblclick({ position: { x: box.width - 6, y: box.height / 2 } });
    await page.waitForFunction(() => window.__lf.native.sent.some((c) => c.SetPan?.[1] === 0), undefined, { timeout: 5000 });
    const dbl = { shown: await shown(0), sent: await pans() };
    console.log('(a) double-click', JSON.stringify(dbl));
    assert.equal(dbl.shown.text, 'C', 'a double-click centres it');
    assert.deepEqual(dbl.sent.at(-1), [0, 0]);
    await engineHas(0, 0);
    // An Alt-click far from the centre centres it, and sends nothing else.
    await pan(0).focus();
    await page.keyboard.press('Home');
    await engineHas(0, -1);
    await blur();
    await clearSent();
    await pan(0).click({ modifiers: ['Alt'], position: { x: box.width - 4, y: box.height / 2 } });
    await page.waitForFunction(() => window.__lf.native.sent.some((c) => c.SetPan), undefined, { timeout: 5000 });
    const alt = { shown: await shown(0), sent: await pans() };
    console.log('(a) Alt-click', JSON.stringify(alt));
    assert.equal(alt.shown.text, 'C', 'an Alt-click centres it');
    assert.ok(alt.sent.length >= 1 && alt.sent.every(([l, v]) => l === 0 && v === 0), 'and sends only the centre');
    await engineHas(0, 0);

    // ── (b) the overlay: a key gesture whose report waits ───────────────────────────────────────────
    await seam('holdEcho', true);
    await pan(1).focus();
    await page.keyboard.press('End');
    const pending = { shown: (await shown(1)).text, applied: await applied(1) };
    await page.evaluate(async (seq) => {
      const { defaultLaneMix } = await import('/src/platform/host.web.ts');
      window.__lf.native.emit({ seq, reset: false, events: [{ Mix: { frame: 0, lane: 1, mix: { ...defaultLaneMix(), pan: -0.5 } } }] });
    }, ++seq);
    const overUnequal = { shown: (await shown(1)).text, applied: await applied(1) };
    await seam('holdEcho', false);
    await engineHas(1, 1);
    const reported = (await shown(1)).text;
    await blur();
    await seam('holdEcho', true);
    await page.evaluate(async (seq) => {
      const { defaultLaneMix } = await import('/src/platform/host.web.ts');
      window.__lf.native.emit({ seq, reset: false, events: [{ Mix: { frame: 0, lane: 1, mix: { ...defaultLaneMix(), pan: -0.5 } } }] });
    }, ++seq);
    const follows = (await shown(1)).text;
    await seam('holdEcho', false);
    await engineHas(1, 1);
    console.log('(b) overlay', JSON.stringify({ pending, overUnequal, reported, follows, back: (await shown(1)).text }));
    assert.deepEqual(pending, { shown: 'R 100', applied: 0 }, 'the gesture shows its value before the engine has it');
    assert.deepEqual(overUnequal, { shown: 'R 100', applied: -0.5 }, 'an unequal Mix does not move it');
    assert.equal(reported, 'R 100', 'the report leaves it there');
    assert.equal(follows, 'L 50', 'settled, the control follows the engine again');
    assert.equal((await shown(1)).text, 'R 100');

    // ── (e) autosave: a pan change alone dirties the jam ────────────────────────────────────────────
    const snapshots = () => page.evaluate(() => window.__lf.native.snapshots.length);
    /** Wait until no snapshot was asked for `quietMs` (autosave is clean). */
    const autosaveSettles = async (quietMs) => {
      let count = await snapshots();
      let since = Date.now();
      for (const end = Date.now() + 20000; Date.now() < end; ) {
        await page.waitForTimeout(250);
        const now = await snapshots();
        if (now !== count) [count, since] = [now, Date.now()];
        else if (Date.now() - since >= quietMs) return count;
      }
      throw new Error('autosave never settled');
    };
    // The engine's snapshot: lane 1's loop with the lane's mix as the fake holds it.
    await page.evaluate(async ([master]) => {
      const { encodeSessionBytes } = await import('/src/platform/engine-wire.ts');
      window.__lf.native.snapshotBytes = encodeSessionBytes(
        { rate: 48000, masterLengthFrames: master, bpm: 120, tracks: [{ index: 0, frames: master, reversed: false, state: 'Playing' }] },
        [new Float32Array(master).fill(0.25)],
      ).buffer;
    }, [MASTER]);
    await page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [laneEvent(1, lane('Empty')), laneEvent(2, lane('Empty'))] });
    const clean = await autosaveSettles(3000);
    await page.evaluate(() => window.__lf.looper.setPan(0, 0.4));
    await engineHas(0, 0.4);
    const after = await autosaveSettles(3000);
    console.log('(e) autosave', JSON.stringify({ clean, after, saves: after - clean }));
    assert.equal(after - clean, 1, 'a pan change alone saves the jam once');

    assert.deepEqual(consoleErrors, [], 'no console errors');
    await page.close();
  }

  // ── (c) layout ───────────────────────────────────────────────────────────────────────────────────────
  for (const [width, height] of [[1000, 700], [1280, 820], [1920, 1080]]) {
    const { page, consoleErrors } = await boot({ width, height });
    // The read-outs at their widest: lane 2 hard left at -40 dB.
    await page.evaluate(() => {
      window.__lf.looper.setPan(1, -1);
      window.__lf.looper.setVolume(1, 0.01);
    });
    await page.waitForFunction(() => window.__lf.looper.trackPan(1) === -1 && window.__lf.looper.trackVolume(1) === 0.01, undefined, { timeout: 5000 });
    await page.waitForTimeout(150);
    const m = await page.evaluate(() => {
      const rect = (r) => ({ left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height });
      /** A read-out's box is its text's (a flex item may be narrower than the text it shows). */
      const boxOf = (el) => {
        if (el.matches('button, input')) return el.getBoundingClientRect();
        const range = document.createRange();
        range.selectNodeContents(el);
        return range.getBoundingClientRect();
      };
      return [...document.querySelectorAll('.lp-lane')].map((laneEl, i) => {
        const cluster = laneEl.querySelector('.lp-lane__right');
        const c = cluster.getBoundingClientRect();
        const items = [...cluster.querySelectorAll('button, input, .lp-vdb, .lp-pan__val')].filter((el) => el.checkVisibility());
        const boxes = items.map((el) => ({ name: el.getAttribute('aria-label') ?? el.textContent.trim(), el, r: boxOf(el) }));
        const problems = [];
        for (const { name, el, r } of boxes) {
          if (r.left < c.left - 0.5 || r.right > c.right + 0.5 || r.top < c.top - 0.5 || r.bottom > c.bottom + 0.5) problems.push(`${name} leaves the cluster`);
          // Clipped by an ancestor that does not show overflow, or by the window.
          let left = Math.max(0, r.left), right = Math.min(innerWidth, r.right), top = Math.max(0, r.top), bottom = Math.min(innerHeight, r.bottom);
          for (let a = el.parentElement; a; a = a.parentElement) {
            const style = getComputedStyle(a), b = a.getBoundingClientRect();
            if (style.overflowX !== 'visible') { left = Math.max(left, b.left); right = Math.min(right, b.right); }
            if (style.overflowY !== 'visible') { top = Math.max(top, b.top); bottom = Math.min(bottom, b.bottom); }
          }
          const fraction = (Math.max(0, right - left) * Math.max(0, bottom - top)) / (r.width * r.height);
          if (fraction < 0.98) problems.push(`${name} is clipped (${fraction.toFixed(2)} visible)`);
          if (el.matches('button, input')) {
            const hit = document.elementFromPoint((left + right) / 2, (top + bottom) / 2);
            if (!hit || !(hit === el || el.contains(hit))) problems.push(`${name} is not under its own centre`);
          }
        }
        for (let a = 0; a < boxes.length; a++) {
          for (let b = a + 1; b < boxes.length; b++) {
            const p = boxes[a].r, q = boxes[b].r;
            const w = Math.min(p.right, q.right) - Math.max(p.left, q.left), h = Math.min(p.bottom, q.bottom) - Math.max(p.top, q.top);
            if (w > 0.5 && h > 0.5) problems.push(`${boxes[a].name} overlaps ${boxes[b].name}`);
          }
        }
        const pan = laneEl.querySelector('.lp-pan'), vol = laneEl.querySelector('.lp-vbar');
        const pr = pan.getBoundingClientRect(), vr = vol.getBoundingClientRect();
        return {
          lane: i + 1,
          cluster: rect(c),
          pills: laneEl.querySelectorAll('.lp-lane__mods > *').length,
          pan: rect(pr),
          fader: rect(vr),
          oneRow: Math.abs((pr.top + pr.bottom) / 2 - (vr.top + vr.bottom) / 2) < 1,
          readouts: [laneEl.querySelector('.lp-vdb')?.textContent, laneEl.querySelector('.lp-pan__val')?.textContent],
          problems,
        };
      });
    });
    const name = `${width}x${height}`;
    await page.screenshot({ path: `logs/probes/lane-pan-${name}.png` });
    for (const l of m) console.log('(c)', name, JSON.stringify({ ...l, cluster: Math.round(l.cluster.width), pan: [l.pan.width, l.pan.height], fader: Math.round(l.fader.width) }));
    const expectedWidth = Math.min(264, Math.max(240, width * 0.2));
    assert.equal(m[0].pills, 6, 'lane 1 shows six pills (FX, MUTE, UNDO, REV, COPY, TRIM)');
    assert.deepEqual(m[1].readouts, ['−40.0 dB', 'L 100'], 'lane 2 shows the widest read-outs');
    for (const l of m) {
      assert.ok(Math.abs(l.cluster.width - expectedWidth) < 0.5, `${name} lane ${l.lane}: the cluster is clamp(240px, 20vw, 264px) wide (${l.cluster.width})`);
      assert.deepEqual([l.pan.width, l.pan.height], [44, 24], `${name} lane ${l.lane}: the pan is 44x24`);
      assert.ok(l.oneRow, `${name} lane ${l.lane}: the pan sits on the fader's row`);
      assert.ok(l.fader.width >= 60, `${name} lane ${l.lane}: the fader keeps a usable length (${l.fader.width})`);
      assert.deepEqual(l.problems, [], `${name} lane ${l.lane}: nothing in the right cluster overlaps, clips or leaves it`);
    }
    assert.deepEqual(consoleErrors, [], 'no console errors');
    await page.close();
  }
});
