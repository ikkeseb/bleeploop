/**
 * OWNS: the command bar's one-row / two-row decision.
 *
 * The bar's controls are all min-content (nowrap pills, fixed sliders) and body is overflow:hidden, so
 * a window narrower than their sum would push master + the tools off-screen. A media query cannot
 * decide this: the row's width changes with what the bar holds (the UPDATE pill, the native-only
 * rescan, the brand's font). Measure the ONE-ROW requirement instead — the sum of the children's
 * intrinsic widths + gaps + padding (the spacer counts as its min-width) — against the bar's width, and
 * flag `.cmd--stack`; CSS (app.css + transport.css) drops the TAKE chip to a second row. The chip and
 * the read-outs beside it each hold one width whatever they say, so a mode or a take never changes the
 * arrangement. Child widths are intrinsic in both arrangements, so the flag cannot oscillate.
 * Re-measured on bar resize and on any DOM change inside it (a pill mounting, a label changing).
 */
const SPACER_MIN_PX = 8;

export function installCmdFit(bar: HTMLElement): () => void {
  const measure = (): void => {
    const cs = getComputedStyle(bar);
    const gap = parseFloat(cs.columnGap) || 0;
    let need = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
    let take = 0;
    let tools = 0;
    let tap = 0;
    let n = 0;
    for (const child of Array.from(bar.children) as HTMLElement[]) {
      if (child.classList.contains('cmd__sr')) continue; // visually-hidden live region (absolute)
      const w = child.classList.contains('transport__grow')
        ? SPACER_MIN_PX
        : child.classList.contains('brand')
          ? child.scrollWidth // unfolded, also while folded; one reading in both states, so the fold cannot flicker
          : child.getBoundingClientRect().width;
      if (child.classList.contains('transport__take')) take = w;
      if (child.classList.contains('tools')) tools = w;
      if (child.classList.contains('transport__tap')) tap = w;
      need += w;
      n++;
    }
    need += gap * Math.max(0, n - 1);
    const width = bar.clientWidth + 0.5;
    const stack = need > width;
    // Second rung: with TAKE on row 2, can the REST still share row 1? If not, the tools cluster joins
    // it on row 2 (right-aligned) instead of wrapping onto a third row of its own, so the bar stays two
    // rows and the stage below does not jump when a loop readout appears. Third rung: TAP
    // (`.transport__tap`) holds row 1 only while it still fits without the tools; otherwise it joins
    // TAKE. Last rung: if row 1 still overflows with all three on row 2 (under 1280 px), the brand folds
    // to its dot (app.css). Row 2 then holds TAP, TAKE and the tools, which fit from 960 px up,
    // so the bar is never more than two rows. Every rung measures intrinsic widths (the folded brand
    // reports its unfolded scrollWidth), so none can see its own effect.
    const row1 = need - take - gap;
    const stackTools = stack && row1 > width;
    const stackTap = stackTools && row1 - tools - gap > width;
    const stackBrand = stackTap && row1 - tools - gap - tap - gap > width;
    bar.classList.toggle('cmd--stack', stack);
    bar.classList.toggle('cmd--stack-tools', stackTools);
    bar.classList.toggle('cmd--stack-tap', stackTap);
    bar.classList.toggle('cmd--stack-brand', stackBrand);
  };
  let raf = 0;
  const schedule = (): void => {
    if (raf) return;
    raf = requestAnimationFrame(() => {
      raf = 0;
      measure();
    });
  };
  const ro = new ResizeObserver(schedule);
  ro.observe(bar);
  const mo = new MutationObserver(schedule);
  mo.observe(bar, { childList: true, subtree: true, characterData: true });
  measure();
  return () => {
    ro.disconnect();
    mo.disconnect();
    if (raf) cancelAnimationFrame(raf);
  };
}
