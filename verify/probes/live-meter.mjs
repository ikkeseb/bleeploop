/**
 * The live/hot input meter, in engine mode on the web engine fake (the `engine-seam` pattern: feed frames
 * scripted with `__lf.native.emit`), with the native-only chrome on (`host.web.ts` served with
 * `available: true`, as `contact-sheet` does). Both slots Off, so each takes its own input.
 *
 * Proves, at 1280×820:
 * - each slot card meters its OWN input (the feed's `meter.peak[slot]`): GO LIVE lights the card's meter
 *   and the INPUT LIVE pill, the meter's `--lvl` follows the slot's level on the −60..0 dBFS span, and
 *   a slot not live stays dark whatever its input reads;
 * - the peak tick holds ~1.5 s after a peak, then falls toward the level, and lands on it; a peak whose
 *   frame and the next quiet one land between two draws is held all the same;
 * - CLIP lights only on the engine's clip (a sample at full scale), never for a level near it, on that
 *   slot alone; it lights even when the clip's frame and the next one land between two draws (the
 *   store's clip counter), and goes out ~1.5 s later, the other slot going live or not meanwhile;
 * - the selected lane names the live slots ("A LIVE", "A+B LIVE", "B LIVE"), the other lanes nothing,
 *   and the badge follows the selection and GO LIVE off;
 * - a GO LIVE (or a stop) whose batch never reaches the host leaves the slot as it was and says so (a
 *   single toast, GO LIVE's and not the outbox's, and the release log), since the engine echoes no live flag;
 * - at 1000×700, 1280×820 and 1920×1080: the CLIP lamp, the badge and the pill are at least their
 *   readable size (a size up at 1920), the "A+B LIVE" badge stays clear of the lane's REC core, and the
 *   meter runs most of the card's width.
 *
 * Cannot see: the engine's meter (per-slot peaks and clips: `src-tauri/src/engine_io/tests.rs`
 * `the_meter_reads_each_slots_own_capture_channel`), WebView2, a real guitar, or whether the meter
 * reads from where the player stands (`STATUS.md` § Not heard yet).
 * Run: pnpm probe live-meter
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

const RATE = 48000;
const EMPTY = { state: 'Empty', length: 0, armed: false, autoArmed: false, canUndo: false, canReverse: false, reversed: false, stopAt: null, fading: false, retakePass: 0 };
const VIEWPORTS = [[1280, 820], [1000, 700], [1920, 1080]];
/** A linear level's place on the meter's −60..0 dBFS span. */
const frac = (linear) => Math.min(1, Math.max(0, (20 * Math.log10(linear) + 60) / 60));
const meter = (a, b, clip = [false, false]) => ({ meter: { peak: [a, b], clip } });

await probe(async ({ open }) => {
  for (const [width, height] of VIEWPORTS) {
    const tag = `${width}x${height}`;
    const { page, consoleErrors, consumeConsoleErrors } = await open({
      viewport: { width, height },
      init: async (p) => {
        await p.route('**/src/platform/host.web.ts', async (route) => {
          const response = await route.fetch();
          await route.fulfill({ response, body: (await response.text()).replace('available: false', 'available: true') });
        });
        await p.addInitScript(() => void (window.__lfEngineFake = true));
      },
    });
    await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
    let seq = 0;
    const emit = (frame) => page.evaluate((f) => window.__lf.native.emit(f), { seq: ++seq, reset: false, events: [], device: [], peaks: [], anchor: null, ...frame });
    await emit({
      reset: true,
      settings: [],
      events: [
        ...[0, 1, 2, 3, 4].map((lane) => ({ Lane: { frame: 0, lane, info: EMPTY } })),
        { Transport: { frame: 0, master: 0, bpm: 120, locked: false } },
        { Selected: { frame: 0, lane: 0 } },
      ],
      anchor: { frame: 0, atMs: Date.now(), rate: RATE, grid: 0 },
      ...meter(0, 0),
    });

    const picker = (slot) => page.getByRole('combobox', { name: `Source for slot ${'AB'[slot - 1]}`, exact: true });
    const live = (slot) => page.getByRole('button', { name: `Live input for slot ${'AB'[slot - 1]}`, exact: true });
    const card = (slot) => page.locator('.slot').nth(slot - 1);
    const cardMeter = (slot) => card(slot).locator('.slot__meter');
    const lane = (i) => page.locator('.lp-lane').nth(i);
    /** A card meter's drawn state: the level and peak it was written, its CLIP lamp, lit or dark. */
    const read = (slot) => cardMeter(slot).evaluate((el) => ({
      live: el.classList.contains('is-live'),
      lvl: Number(el.style.getPropertyValue('--lvl') || 0),
      pk: Number(el.style.getPropertyValue('--pk') || 0),
      clip: el.classList.contains('is-clip'),
    }));
    const badge = (i) => lane(i).locator('.lp-lane__src');
    const badges = async () => Promise.all([0, 1, 2, 3, 4].map(async (i) => ((await badge(i).count()) ? await badge(i).textContent() : '')));
    /** Wait until slot `slot`'s meter satisfies `want` (the rAF loop draws it). */
    const until = async (slot, what, want) => {
      const deadline = Date.now() + 4000;
      for (let m = await read(slot); !want(m); m = await read(slot)) {
        if (Date.now() > deadline) assert.fail(`${tag}: ${what}: slot ${slot} meter reads ${JSON.stringify(m)}`);
        await page.waitForTimeout(30);
      }
    };

    // Both slots Off: each takes its own input, so each card shows its meter, dark until it goes live.
    await picker(1).selectOption('off');
    await picker(2).selectOption('off');
    await cardMeter(1).waitFor();
    await cardMeter(2).waitFor();
    await emit(meter(0.5, 0.25));
    await page.waitForTimeout(200);
    assert.deepEqual(await read(1), { live: false, lvl: 0, pk: 0, clip: false }, `${tag}: a slot not live: its meter dark whatever its input`);
    assert.deepEqual(await badges(), ['', '', '', '', ''], `${tag}: nothing live: no lane names a source`);

    // GO LIVE on A: its pill, its meter at its own level, the selected lane's badge.
    await live(1).click();
    await page.getByRole('button', { name: 'Live input for slot A', exact: true, pressed: true }).waitFor();
    assert.equal((await live(1).textContent()).trim(), 'INPUT LIVE');
    await emit(meter(0.5, 0.0316));
    await until(1, 'A live at −6 dBFS', (m) => m.live && Math.abs(m.lvl - frac(0.5)) < 0.01);
    assert.deepEqual(await read(2), { live: false, lvl: 0, pk: 0, clip: false }, `${tag}: B not live stays dark`);
    assert.deepEqual(await badges(), ['A LIVE', '', '', '', ''], `${tag}: the selected lane names A`);

    // B live too: each card its own level; the badge names both.
    await live(2).click();
    await page.getByRole('button', { name: 'Live input for slot B', exact: true, pressed: true }).waitFor();
    await emit(meter(0.5, 0.0316));
    await until(2, 'B live at −30 dBFS', (m) => m.live && Math.abs(m.lvl - frac(0.0316)) < 0.01);
    const [a, b] = [await read(1), await read(2)];
    console.log(tag, 'per-slot levels', JSON.stringify({ a, b }));
    assert.ok(Math.abs(a.lvl - frac(0.5)) < 0.01, `${tag}: A reads its own input, not B's: ${a.lvl}`);
    assert.deepEqual(await badges(), ['A+B LIVE', '', '', '', ''], `${tag}: both slots named`);

    // Readable from where the player stands, at every size: the measured type and the badge's room.
    const sizes = await page.evaluate(() => {
      const px = (el) => parseFloat(getComputedStyle(el).fontSize);
      const card = document.querySelector('.slot');
      const lane = document.querySelector('.lp-lane');
      const box = (el) => el.getBoundingClientRect();
      return {
        clip: px(card.querySelector('.slot__meter-clip')),
        pill: px(card.querySelector('.tgl.live')),
        badge: px(lane.querySelector('.lp-lane__src')),
        badgeRight: box(lane.querySelector('.lp-lane__src')).right,
        coreLeft: box(lane.querySelector('.lp-core')).left,
        pairRight: box(lane.querySelector('.lp-lane__pair')).right,
        leftRight: box(lane.querySelector('.lp-lane__left')).right,
        track: box(card.querySelector('.slot__meter-track')).width,
        card: box(card).width,
      };
    });
    console.log(tag, 'sizes', JSON.stringify(sizes));
    const large = width >= 1920;
    assert.ok(sizes.clip >= (large ? 12 : 10.5), `${tag}: the CLIP lamp's type: ${sizes.clip}px`);
    assert.ok(sizes.badge >= (large ? 12 : 10), `${tag}: the lane's LIVE badge's type: ${sizes.badge}px`);
    assert.ok(sizes.pill >= (large ? 13 : 12), `${tag}: the INPUT LIVE pill's type: ${sizes.pill}px`);
    assert.ok(sizes.badgeRight + 3 <= sizes.coreLeft, `${tag}: "A+B LIVE" clear of the REC core (badge ends ${sizes.badgeRight}, core starts ${sizes.coreLeft})`);
    assert.ok(sizes.pairRight <= sizes.leftRight + 0.5, `${tag}: the wider number box keeps PLAY and CLR inside the lane's left cluster`);
    assert.ok(sizes.track >= 0.7 * sizes.card, `${tag}: the meter runs most of the card (${sizes.track} of ${sizes.card})`);

    if (width === 1280) {
      // The badge follows the selection.
      await emit({ events: [{ Selected: { frame: 0, lane: 2 } }] });
      await page.waitForFunction(() => document.querySelectorAll('.lp-lane')[2].querySelector('.lp-lane__src'));
      assert.deepEqual(await badges(), ['', '', 'A+B LIVE', '', ''], 'the badge moves to the selected lane');

      // The peak holds, then falls to the level: A from −6 dBFS down to −20, sampled every frame.
      await emit(meter(0.5, 0));
      await until(1, 'the peak at −6 dBFS', (m) => Math.abs(m.pk - frac(0.5)) < 0.01);
      const trace = await page.evaluate(async (f) => {
        window.__lf.native.emit(f);
        const t0 = performance.now();
        const el = document.querySelector('.slot .slot__meter');
        const out = [];
        while (performance.now() - t0 < 2800) {
          await new Promise((r) => requestAnimationFrame(r));
          out.push({ t: performance.now() - t0, pk: Number(el.style.getPropertyValue('--pk')), lvl: Number(el.style.getPropertyValue('--lvl')) });
        }
        return out;
      }, { seq: ++seq, reset: false, events: [], device: [], peaks: [], anchor: null, ...meter(0.1, 0) });
      const [hi, lo] = [frac(0.5), frac(0.1)];
      const held = trace.filter((s) => s.t > 100 && s.t < 1350);
      const falling = trace.filter((s) => s.t > 1600 && s.t < 1850);
      const landed = trace.filter((s) => s.t > 2400);
      console.log('peak trace', JSON.stringify({ held: held.map((s) => s.pk.toFixed(3)).slice(0, 3), falling: falling.map((s) => s.pk.toFixed(3)), landed: landed.at(-1) }));
      assert.ok(held.length > 10 && held.every((s) => Math.abs(s.pk - hi) < 0.01 && Math.abs(s.lvl - lo) < 0.01), 'the peak holds at −6 dBFS for its hold, the level at −20');
      assert.ok(falling.some((s) => s.pk > lo + 0.02 && s.pk < hi - 0.02), 'then falls');
      assert.ok(landed.length > 0 && landed.every((s) => Math.abs(s.pk - lo) < 0.01), 'and lands on the level');
      // A peak whose frame and the next quiet one land between two draws still holds the peak tick.
      await page.evaluate(([p, q]) => {
        window.__lf.native.emit(p);
        window.__lf.native.emit(q);
      }, [
        { seq: ++seq, reset: false, events: [], device: [], peaks: [], anchor: null, ...meter(0.5, 0) },
        { seq: ++seq, reset: false, events: [], device: [], peaks: [], anchor: null, ...meter(0.1, 0) },
      ]);
      await page.waitForTimeout(300);
      const missed = await read(1);
      assert.ok(Math.abs(missed.pk - hi) < 0.01 && Math.abs(missed.lvl - lo) < 0.01, `a peak between two draws is held: ${JSON.stringify(missed)}`);

      // Near full scale is no clip: −0.4 dBFS, the engine reports none.
      await emit(meter(0.95, 0));
      await page.waitForTimeout(300);
      assert.equal((await read(1)).clip, false, 'a level near full scale does not light CLIP');
      // A clip on A, followed at once by a quiet frame, both before the next draw: CLIP lights on A alone,
      // and the peak holds at full scale.
      await page.evaluate(([c, q]) => {
        window.__lf.native.emit(c);
        window.__lf.native.emit(q);
      }, [
        { seq: ++seq, reset: false, events: [], device: [], peaks: [], anchor: null, ...meter(1, 0.1, [true, false]) },
        { seq: ++seq, reset: false, events: [], device: [], peaks: [], anchor: null, ...meter(0.1, 0.1) },
      ]);
      await until(1, 'CLIP after a clip between two draws', (m) => m.clip && m.pk >= 0.999);
      assert.equal((await read(2)).clip, false, "A's clip does not light B's lamp");
      // B off and on again during A's hold: A's lamp and held peak are A's alone, and the lamp goes out.
      const toggledAt = Date.now();
      for (const pressed of [false, true]) {
        await live(2).click();
        await page.getByRole('button', { name: 'Live input for slot B', exact: true, pressed }).waitFor();
      }
      const kept = await read(1);
      assert.ok(kept.clip && kept.pk >= 0.999, `B toggled ${Date.now() - toggledAt} ms into A's hold: A's CLIP and peak hold: ${JSON.stringify(kept)}`);
      await until(1, 'CLIP goes out after its hold', (m) => !m.clip);

      // GO LIVE off: the meter goes dark and the badge drops the slot.
      await live(1).click();
      await page.getByRole('button', { name: 'Live input for slot A', exact: true, pressed: false }).waitFor();
      await until(1, 'A dark once not live', (m) => !m.live && m.lvl === 0 && m.pk === 0);
      assert.equal((await live(1).textContent()).trim(), 'GO LIVE');
      assert.deepEqual(await badges(), ['', '', 'B LIVE', '', ''], 'the badge names B alone');
      await live(2).click();
      await page.getByRole('button', { name: 'Live input for slot B', exact: true, pressed: false }).waitFor();
      assert.deepEqual(await badges(), ['', '', '', '', ''], 'nothing live: no badge');

      // A lost GO LIVE: the batch never reaches the host (the fake loses it and its one resend), so the
      // slot stays GO LIVE and the press says so; a lost stop leaves it INPUT LIVE the same way.
      const lose = () => page.evaluate(() => void (window.__lf.native.failSends = 2));
      const sentLive = () => page.evaluate(() => window.__lf.native.sent.filter((c) => c?.SetSlotLive?.[0] === 0).length);
      const before = await sentLive();
      // Every toast on screen by its message (`src/notify.ts`): the outbox's generic one must not join GO LIVE's.
      const told = () => page.evaluate(async () => (await import('/src/notify.ts')).toasts().map((t) => t.message));
      await lose();
      await live(1).click();
      await page.locator('.toast', { hasText: 'Could not go live. Press GO LIVE again.' }).waitFor({ timeout: 5000 });
      await page.getByRole('button', { name: 'Live input for slot A', exact: true, pressed: false }).waitFor();
      await page.waitForFunction(() => !document.querySelector('[aria-label="Live input for slot A"]')?.disabled);
      assert.equal((await live(1).textContent()).trim(), 'GO LIVE', 'a lost GO LIVE leaves the slot GO LIVE');
      assert.equal(await sentLive(), before, 'the engine fake applied no SetSlotLive from the lost batch');
      assert.equal(consumeConsoleErrors('[PluginControls] go-live toggle failed').length, 1, 'the lost GO LIVE reaches the release log');
      assert.deepEqual(await told(), ['Could not go live. Press GO LIVE again.'], 'a lost GO LIVE is told once, by GO LIVE');
      await live(1).click();
      await page.getByRole('button', { name: 'Live input for slot A', exact: true, pressed: true }).waitFor();
      await lose();
      await live(1).click();
      await page.locator('.toast', { hasText: 'Could not stop the live input. Press INPUT LIVE again.' }).waitFor({ timeout: 5000 });
      await page.waitForFunction(() => !document.querySelector('[aria-label="Live input for slot A"]')?.disabled);
      assert.equal((await live(1).textContent()).trim(), 'INPUT LIVE', 'a lost stop leaves the slot INPUT LIVE');
      assert.equal(consumeConsoleErrors('[PluginControls] go-live toggle failed').length, 1, 'the lost stop reaches the release log');
      assert.deepEqual(
        (await told()).filter((m) => m !== 'Could not go live. Press GO LIVE again.'),
        ['Could not stop the live input. Press INPUT LIVE again.'],
        'a lost stop is told once, by INPUT LIVE',
      );
      assert.equal(consumeConsoleErrors('[platform] input batch failed').length, 2, 'the outbox logged both lost batches');
    }
    assert.deepEqual(consoleErrors, [], `${tag}: no console errors`);
    await page.context().close();
  }
});
