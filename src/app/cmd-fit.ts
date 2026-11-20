/**
 * OWNS: the command bar's one-row / two-row decision.
 *
 * The bar's controls are all min-content (nowrap pills, fixed sliders) and body is overflow:hidden, so
 * a window narrower than their sum would push master + the tools off-screen. A media query cannot
 * decide this: the row's width changes with FIXED's bars and the words that change (the loop read-out),
 * so a bar that fits idle can clip busy. Measure the ONE-ROW requirement instead — the sum of
 * the children's intrinsic widths + gaps + padding (the spacer counts as its min-width) — against the
 * bar's width, and flag `.cmd--stack`; CSS (app.css + transport.css) drops the modes cluster to a
 * second row. Child widths are intrinsic in both arrangements, so the flag cannot oscillate.
 * Re-measured on bar resize, on any DOM change inside it (FIXED's bars appearing, a label changing) and when
 * a web font finishes loading (Geist is requested only once the bar has text, after the first measure, and
 * its metrics change every child's width without touching the bar's box or its DOM).
 */
const SPACER_MIN_PX = 8;
const BRAND_DOT_PX = 11; // the folded brand's width (app.css `.cmd--stack-brand .brand`)
// The master group's own rungs, tightest last (transport.css "Master rungs"): the volume slider's width,
// then the volume's numeric value.
const MASTER_RUNGS = ['cmd--fit-vol', 'cmd--fit-val'] as const;

export function installCmdFit(bar: HTMLElement): () => void {
  const measure = (): void => {
    // The master rungs change the master group's width, so every pass starts without them: the widths
    // below are intrinsic, and the rungs are re-added one by one at the end (no paint in between).
    for (const rung of MASTER_RUNGS) if (bar.classList.contains(rung)) bar.classList.remove(rung);
    const cs = getComputedStyle(bar);
    const gap = parseFloat(cs.columnGap) || 0;
    let need = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
    let modes = 0;
    let tools = 0;
    let pair = 0;
    let brand = 0;
    let master: HTMLElement | null = null;
    let n = 0;
    for (const child of Array.from(bar.children) as HTMLElement[]) {
      if (child.classList.contains('cmd__sr')) continue; // visually-hidden live region (absolute)
      const w = child.classList.contains('transport__grow')
        ? SPACER_MIN_PX
        : child.classList.contains('brand')
          ? child.scrollWidth // unfolded, also while folded; one reading in both states, so the fold cannot flicker
          : child.getBoundingClientRect().width;
      if (child.classList.contains('transport__modes')) modes = w;
      if (child.classList.contains('tools')) tools = w;
      if (child.classList.contains('transport__pair')) pair += w + gap;
      if (child.classList.contains('brand')) brand = w;
      if (child.classList.contains('transport__master')) master = child;
      need += w;
      n++;
    }
    need += gap * Math.max(0, n - 1);
    const width = bar.clientWidth + 0.5;
    const stack = need > width;
    // Second rung: with the modes on row 2, can the REST still share row 1? If not, the tools cluster
    // joins the modes on row 2 (right-aligned) instead of wrapping onto a third row of its own, so the
    // bar stays two rows while row 2 fits and the stage below does not jump when a loop readout
    // appears. The stacked bar wraps (`.cmd--stack`, app.css), so a row 2 that does not fit would break
    // onto a third row (`verify/probes/layout-reachability.mjs` holds two rows from 960 px with every
    // mode on). Third rung: TAP (`.transport__pair`) holds row 1 only while it still fits without the
    // tools; otherwise it joins the modes. Last rung: if row 1 still overflows with all three on row 2
    // (FADE and IN FX at 960 and 1100 px), the brand folds to its dot (app.css). Every rung
    // measures intrinsic widths (the folded brand reports its unfolded scrollWidth), so none can see its
    // own effect.
    const row1 = need - modes - gap;
    const stackTools = stack && row1 > width;
    const stackPair = stackTools && row1 - tools - gap > width;
    const stackBrand = stackPair && row1 - tools - gap - pair > width;
    bar.classList.toggle('cmd--stack', stack);
    bar.classList.toggle('cmd--stack-tools', stackTools);
    bar.classList.toggle('cmd--stack-pair', stackPair);
    bar.classList.toggle('cmd--stack-brand', stackBrand);
    // Master rungs: if row 1 still overflows with the brand folded, the master group gives up width
    // itself, one rung at a time, until it fits. Each rung's saving is read off the group's box as it lands; the pass began without them,
    // so a rung never decides from its own effect.
    if (stackBrand && master) {
      let over = row1 - tools - gap - pair - (brand - BRAND_DOT_PX) - width;
      for (const rung of MASTER_RUNGS) {
        if (over <= 0) break;
        const before = master.getBoundingClientRect().width;
        bar.classList.add(rung);
        over -= before - master.getBoundingClientRect().width;
      }
    }
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
  document.fonts.addEventListener('loadingdone', schedule);
  measure();
  return () => {
    document.fonts.removeEventListener('loadingdone', schedule);
    ro.disconnect();
    mo.disconnect();
    if (raf) cancelAnimationFrame(raf);
  };
}
