/**
 * A SplitStack drag survives its divider unmounting mid-drag. On the web engine fake at 1280x820 with the
 * keyboard below the looper, a real mouse drag (Playwright's mouse) holds the looper/keyboard divider,
 * the keyboard is hidden through the app's own layout store (`__lf.layoutStore`) so that divider leaves
 * the DOM while it holds pointer capture, the button is released and the keyboard restored. Then the
 * stage host must not carry `splitstack--dragging`, a buttonless hover over the remounted divider must
 * leave the stored stage weights alone (stale drag state resized on hover), and a second drag must move
 * the boundary under the pointer (a captured drag, not one that stops once the pointer leaves the
 * divider). Also checks the first drag resized before the unmount (the drag path itself works). Sees
 * only the DOM and the layout store's weights; says nothing about the touch path or a placement change
 * through the command-bar buttons. Run: pnpm probe splitstack-drag
 */
import assert from 'node:assert/strict';
import { probe } from '../harness/probe.ts';

await probe(async ({ open }) => {
  const { page } = await open({
    viewport: { width: 1280, height: 820 },
    init: (p) => p.addInitScript(() => void (window.__lfEngineFake = true)),
  });
  await page.waitForFunction(() => window.__lf.native.opened.length === 1, undefined, { timeout: 5000 });
  await page.evaluate(() => {
    window.__lf.layoutStore.setKeyboardPlacement('bottom');
    window.__lf.layoutStore.setStageSizes({ keyboard: 0.55, looper: 2.0 });
  });
  const divider = page.getByRole('separator', { name: /looper/i });
  await divider.waitFor({ state: 'visible' });
  const sizes = () => page.evaluate(() => ({ ...window.__lf.layoutStore.stageSizes() }));
  const center = async () => {
    const box = await divider.boundingBox();
    assert.ok(box, 'no divider box');
    return { x: box.x + box.width / 2, y: box.y + box.height / 2 };
  };
  const dragging = () => page.evaluate(() => document.querySelectorAll('.splitstack--dragging').length);

  // Drag 1: grab, move (proves the drag resizes), then unmount the held divider and release.
  const s0 = await sizes();
  let c = await center();
  await page.mouse.move(c.x, c.y);
  await page.mouse.down();
  await page.mouse.move(c.x, c.y - 60, { steps: 4 });
  const s1 = await sizes();
  assert.notDeepEqual(s1, s0, 'the first drag did not resize the stage at all');
  assert.equal(await dragging(), 1, 'host lacks splitstack--dragging during a drag');
  await page.evaluate(() => window.__lf.layoutStore.setKeyboardPlacement('hidden'));
  await page.waitForFunction(() => !document.querySelector('.splitstack__divider'));
  await page.mouse.up();
  await page.evaluate(() => window.__lf.layoutStore.setKeyboardPlacement('bottom'));
  await divider.waitFor({ state: 'visible' });
  await page.waitForTimeout(100);

  const afterUnmount = await dragging();

  // A hover with no button over the remounted divider must not resize (stale drag state would).
  const s2 = await sizes();
  c = await center();
  await page.mouse.move(c.x, c.y - 2);
  await page.mouse.move(c.x, c.y + 2, { steps: 2 });
  const hover = await sizes();

  // Drag 2 on the remounted divider: the boundary must follow the pointer (captured drag).
  c = await center();
  await page.mouse.move(c.x, c.y);
  await page.mouse.down();
  await page.mouse.move(c.x, c.y + 80, { steps: 8 });
  await page.mouse.up();
  const s3 = await sizes();
  const end = await center();
  const stuck = await dragging();
  console.log(JSON.stringify({ s0, s1, afterUnmount, s2, hover, s3, target: c.y + 80, dividerY: end.y, draggingHosts: stuck }));
  assert.equal(afterUnmount, 0, 'the stage host keeps splitstack--dragging after the held divider unmounted');
  assert.deepEqual(hover, s2, 'a buttonless hover over the remounted divider resized the stage (stale drag state)');
  // The pointer marks panel A's trailing edge, so the divider's centre sits half a divider (5 px) past it.
  assert.ok(Math.abs(end.y - (c.y + 80)) < 12, `the second drag's boundary did not follow the pointer (at ${end.y}, pointer at ${c.y + 80})`);
  assert.equal(stuck, 0, 'a stage host still carries splitstack--dragging after the drags ended');
  await page.close();
}, { launch: {} });
