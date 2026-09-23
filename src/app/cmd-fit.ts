/**
 * OWNS: the command bar's one-row / two-row decision.
 *
 * The bar's controls are all min-content (nowrap pills, fixed sliders) and body is overflow:hidden, so
 * a window narrower than their sum would push master + the tools off-screen. A media query cannot
 * decide this: the row's width changes with the contextual CLICK / FIXED controls,
 * so a bar that fits idle at 1440 clips busy. Measure the ONE-ROW requirement instead — the sum of
 * the children's intrinsic widths + gaps + padding (the spacer counts as its min-width) — against the
 * bar's width, and flag `.cmd--stack`; CSS (app.css + transport.css) drops the modes cluster to a
 * second row. Child widths are intrinsic in both arrangements, so the flag cannot oscillate.
 * Re-measured on bar resize and on any DOM change inside it (a slider sliding in, a label changing).
 */
const SPACER_MIN_PX = 8;

export function installCmdFit(bar: HTMLElement): () => void {
  const measure = (): void => {
    const cs = getComputedStyle(bar);
    const gap = parseFloat(cs.columnGap) || 0;
    let need = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
    let modes = 0;
    let n = 0;
    for (const child of Array.from(bar.children) as HTMLElement[]) {
      if (child.classList.contains('cmd__sr')) continue; // visually-hidden live region (absolute)
      const w = child.classList.contains('transport__grow') ? SPACER_MIN_PX : child.getBoundingClientRect().width;
      if (child.classList.contains('transport__modes')) modes = w;
      need += w;
      n++;
    }
    need += gap * Math.max(0, n - 1);
    const width = bar.clientWidth + 0.5;
    const stack = need > width;
    // Second rung: with the modes on row 2, can the REST still share row 1? If not, the tools cluster
    // joins the modes on row 2 (right-aligned) instead of wrapping onto a third row of its own — the
    // bar is never taller than two rows, so the stage below never jumps when a loop readout appears.
    // Both rungs measure intrinsic widths, so neither can see its own effect.
    const stackTools = stack && need - modes - gap > width;
    bar.classList.toggle('cmd--stack', stack);
    bar.classList.toggle('cmd--stack-tools', stackTools);
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
