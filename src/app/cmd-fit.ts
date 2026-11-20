/**
 * OWNS: the command bar's one-row / two-row decision.
 *
 * The bar's parts are all min-content (fixed-width keys, the display, the meter) and body is
 * overflow:hidden, so a window narrower than their sum would push master and the tools off-screen. A
 * media query cannot decide this: the sum moves with --ui, the fonts and the tools the build shows (the
 * native build's rescan and gear, UPDATE while one is offered). Measure the ONE-ROW requirement instead
 * (the children's intrinsic widths, the bar's gaps and padding, each group spacer at its minimum)
 * against the bar's width, and take the first rung that fits (transport.css):
 * 1. one row, as drawn at 1920 (docs/redesign/main.html);
 * 2. `.cmd--fold-brand`: one row with the logotype folded to its mark;
 * 3. `.cmd--stack`: two rows (docs/redesign/narrow.html): the time over the record modes, the transport
 *    over the master, the tools down the right edge, the brand folded.
 * Every part keeps its width in all three (only heights change), and the folded brand reports its
 * unfolded scrollWidth, so a rung never decides from its own effect and the flag cannot oscillate.
 * Re-measured on bar resize, on any DOM change inside it (a word changing, UPDATE appearing) and when a
 * web font finishes loading (its metrics change every child's width without touching the bar's box or
 * its DOM).
 */
const SPACER = 'transport__gap';

export function installCmdFit(bar: HTMLElement): () => void {
  const measure = (): void => {
    // The stacked bar is a grid with other gaps, padding and tool columns: measure from a one-row pass
    // with its class off. Its height is pinned for that pass, so nothing under it moves: a lane stack the
    // bar's height change made taller for a moment would have its scroll clamped by the browser (a lane
    // scrolled half into view jumped to the top whenever the bar re-measured: a word changing, ▶ ALL
    // turning ■ ALL).
    const wasStacked = bar.classList.contains('cmd--stack');
    if (wasStacked) {
      bar.style.height = `${bar.getBoundingClientRect().height}px`;
      bar.classList.remove('cmd--stack');
    }
    const cs = getComputedStyle(bar);
    const gap = parseFloat(cs.columnGap) || 0;
    let need = parseFloat(cs.paddingLeft) + parseFloat(cs.paddingRight);
    let brandSave = 0;
    let n = 0;
    for (const child of Array.from(bar.children) as HTMLElement[]) {
      if (getComputedStyle(child).position === 'absolute') continue; // the visually-hidden live region
      let w: number;
      if (child.classList.contains(SPACER)) w = parseFloat(getComputedStyle(child).minWidth) || 0;
      else if (child.classList.contains('brand')) {
        // unfolded, also while folded: one reading in both states, so the fold cannot flicker
        w = child.scrollWidth;
        brandSave = w - (child.querySelector<HTMLElement>('.brand__dot')?.getBoundingClientRect().width ?? 0);
      } else w = child.getBoundingClientRect().width;
      need += w;
      n++;
    }
    need += gap * Math.max(0, n - 1);
    const width = bar.clientWidth + 0.5;
    const stack = need - brandSave > width;
    bar.classList.toggle('cmd--fold-brand', !stack && need > width);
    bar.classList.toggle('cmd--stack', stack);
    if (wasStacked) bar.style.height = '';
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
