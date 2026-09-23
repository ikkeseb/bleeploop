import { For, Show, type JSX, createSignal } from 'solid-js';
import './splitstack.css';

/**
 * A panel in a SplitStack. `render` is a function (NOT pre-built JSX) so the For mapping owns the
 * content's reactive scope — that gives correct mount/cleanup when a panel is added or removed (e.g.
 * the keyboard being hidden), while reordering a panel that *stays* in the array only MOVES its DOM
 * node (For keys by descriptor reference), so a region like the looper never remounts when the
 * keyboard moves around it.
 */
export interface StackPanel {
  id: string;
  render: () => JSX.Element;
  /** min size as a fraction of the usable axis (default 0.08). */
  min?: number;
  /** hard min in px (default 44) — whichever of (min*usable, minPx) is larger wins. Also becomes the
   * grid track's floor, so the min holds for the initial split / nudge / reset / resize, not only drag. */
  minPx?: number;
  /** human label for the divider's accessible name (defaults to the id). */
  label?: string;
  /** When true the panel hugs its content: its grid track becomes `auto` (content height) instead of
   * minmax(minPx, weight·fr), and it takes NO part in the weight math (weight / beginDrag / resetPair /
   * nudge / pairPctA all skip it). No interactive divider is rendered against an autoSize panel — it has
   * nothing to resize — only a static gap that keeps the divider-track rhythm. `min` / `minPx` /
   * `defaultWeight` are ignored on an autoSize panel. */
  autoSize?: boolean;
  /** Optional design-default weight. When BOTH panels of a divider declare one, a reset (double-click /
   * Home / Enter) returns that pair toward the default ratio instead of an equal split; the pair sum is
   * preserved either way, so the rest of the stack doesn't shift. Panels without it (e.g. the slot split)
   * still reset to equal — fully backward-compatible. */
  defaultWeight?: number;
}

const DEFAULT_DIVIDER_PX = 10;

/**
 * A resizable stack of N panels with a draggable divider between each adjacent pair. Works in both
 * orientations (vertical = stacked rows, horizontal = side-by-side columns). Sizes are fr weights keyed
 * by panel id (held + persisted by the caller); dragging a divider transfers weight between exactly the
 * two adjacent panels (their sum is preserved, so the rest of the stack doesn't shift). The panes are
 * never remounted on resize — only the container's inline grid template updates — so stateful children
 * (the looper's RAF canvas loop, capture state) survive a drag untouched.
 */
export function SplitStack(props: {
  orientation: 'vertical' | 'horizontal';
  panels: StackPanel[];
  sizes: () => Record<string, number>;
  onSizes: (next: Record<string, number>) => void;
  dividerPx?: number;
}) {
  let host!: HTMLDivElement;
  const [dragging, setDragging] = createSignal(-1); // index of the divider being dragged, -1 = none
  const dpx = () => props.dividerPx ?? DEFAULT_DIVIDER_PX;
  const vertical = () => props.orientation === 'vertical';
  const weight = (id: string) => {
    const w = props.sizes()[id];
    return typeof w === 'number' && Number.isFinite(w) && w > 0 ? w : 1;
  };

  const template = () => {
    const parts: string[] = [];
    const panels = props.panels;
    panels.forEach((p, i) => {
      // autoSize panels hug their content (an `auto` track — no part in the fr split). Weighted panels
      // are floored at their hard px min (not 0), so a lopsided persisted weight / nudge / reset /
      // viewport shrink can't shrink a pane below its min and clip — the min holds for every resize path,
      // not just a live drag. Auto panels also keep their content when the combined floors exceed
      // the available space; the stack scrolls instead of squeezing an instrument header shut.
      parts.push(p.autoSize ? 'minmax(min-content, auto)' : `minmax(${p.minPx ?? 44}px, ${weight(p.id)}fr)`);
      // One gap track per adjacent pair, ALWAYS — the DOM renders a divider (between two weighted panes)
      // or a static spacer (next to an autoSize pane) into it, so the (n-1)·dpx accounting below holds.
      if (i < panels.length - 1) parts.push(`${dpx()}px`);
    });
    return parts.join(' ');
  };

  // Captured at pointerdown for the active divider (constant during a drag — the panels before it don't
  // move, and only the two adjacent weights change).
  let cap:
    | null
    | {
        idA: string;
        idB: string;
        pxPerWeight: number;
        panelAStart: number; // px from host's leading edge to panel A's leading edge
        pairPx: number; // px(A) + px(B)
        minA: number;
        minB: number;
      } = null;
  let activePointerId: number | null = null;

  const axisSize = (rect: DOMRect) => (vertical() ? rect.height : rect.width);
  const axisPos = (e: PointerEvent, rect: DOMRect) =>
    vertical() ? e.clientY - rect.top : e.clientX - rect.left;
  const paneEls = () => host.querySelectorAll<HTMLElement>(':scope > .splitstack__pane');

  // Geometry of the fr-WEIGHTED region only. autoSize panels hug their content (an `auto` grid track) and
  // take no part in the weight math, so their MEASURED pixel size is subtracted from the usable axis and
  // their (unused) weight is left out of the total. Every adjacent pair contributes one dpx gap track
  // (divider or static spacer), so `(n-1)·dpx` is the exact gap total regardless of autoSize placement.
  // Returns null when geometry is unavailable (no weighted panels / zero usable) → callers bail. When no
  // panel is autoSize this reduces to the pre-autoSize totalW/usable/pxPerWeight verbatim.
  const weightedGeom = () => {
    const panels = props.panels;
    const panes = paneEls();
    let totalW = 0;
    let autoPx = 0;
    panels.forEach((p, i) => {
      if (p.autoSize) {
        const pane = panes[i];
        if (pane) autoPx += axisSize(pane.getBoundingClientRect());
      } else {
        totalW += weight(p.id);
      }
    });
    const usable = axisSize(host.getBoundingClientRect()) - (panels.length - 1) * dpx() - autoPx;
    if (usable <= 0 || totalW <= 0) return null;
    return { usable, totalW, pxPerWeight: usable / totalW };
  };

  const beginDrag = (k: number, e: PointerEvent) => {
    if (!e.isPrimary || e.button !== 0 || activePointerId !== null) return;
    const panels = props.panels;
    const g = weightedGeom();
    if (!g) return;
    const panes = paneEls();
    const pxPerWeight = g.pxPerWeight;
    // Lead = pixels from the host's leading edge to panel k's leading edge. A weighted panel before k
    // contributes weight·pxPerWeight; an autoSize panel before k contributes its MEASURED content size
    // (it isn't in weight space). Plus the k gap tracks (dpx each) that sit before divider k.
    let lead = 0;
    for (let i = 0; i < k; i++) {
      const p = panels[i];
      if (p.autoSize) {
        const pane = panes[i];
        if (pane) lead += axisSize(pane.getBoundingClientRect());
      } else {
        lead += weight(p.id) * pxPerWeight;
      }
    }
    lead += k * dpx();
    // The pair straddling divider k is always two WEIGHTED panels (no interactive divider is rendered
    // against an autoSize pane), so pxA/pxB stay in weight space exactly as before.
    const pxA = weight(panels[k].id) * pxPerWeight;
    const pxB = weight(panels[k + 1].id) * pxPerWeight;
    const minA = Math.max(panels[k].minPx ?? 44, (panels[k].min ?? 0.08) * g.usable);
    const minB = Math.max(panels[k + 1].minPx ?? 44, (panels[k + 1].min ?? 0.08) * g.usable);
    const nextCap = {
      idA: panels[k].id,
      idB: panels[k + 1].id,
      pxPerWeight,
      panelAStart: lead,
      pairPx: pxA + pxB,
      minA,
      minB,
    };
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    activePointerId = e.pointerId;
    cap = nextCap;
    setDragging(k);
    e.preventDefault();
  };

  const onPointerMove = (e: PointerEvent) => {
    if (e.pointerId !== activePointerId || dragging() < 0 || !cap) return;
    // Too-narrow pair (both panes already near their floors): bail rather than overshoot one side's min.
    if (cap.minA + cap.minB >= cap.pairPx) return;
    const rect = host.getBoundingClientRect();
    let newPxA = axisPos(e, rect) - cap.panelAStart;
    newPxA = Math.max(cap.minA, Math.min(cap.pairPx - cap.minB, newPxA));
    const newPxB = cap.pairPx - newPxA;
    props.onSizes({
      ...props.sizes(),
      [cap.idA]: newPxA / cap.pxPerWeight,
      [cap.idB]: newPxB / cap.pxPerWeight,
    });
  };

  const endDrag = (e: PointerEvent) => {
    if (e.pointerId !== activePointerId) return;
    const el = e.currentTarget as HTMLElement;
    if (el.hasPointerCapture(e.pointerId)) el.releasePointerCapture(e.pointerId);
    activePointerId = null;
    setDragging(-1);
    cap = null;
  };

  // Per-pane weight floor (in fr) for the pair at divider k, honoring BOTH the px min (minPx) and the
  // fractional min, computed off live geometry — the SAME floor the drag path clamps to (minA/minB). The
  // keyboard/reset paths use it so a stored weight never implies a px below a pane's floor; otherwise the
  // grid clamps the render to minPx while the persisted fr stays smaller, desyncing the aria %, and the
  // next drag's beginDrag (which rebuilds geometry from the stored weights) grabs with a visible jump.
  // Returns null when geometry is unavailable → callers fall back to the plain fraction-only clamp. (#B)
  const pairFloors = (k: number) => {
    const panels = props.panels;
    // Same weighted geometry the drag path uses (autoSize panes subtracted), so a floor computed here in
    // fr units matches what the grid actually renders for the weighted pair.
    const g = weightedGeom();
    if (!g) return null;
    const floor = (p: StackPanel) => Math.max(p.minPx ?? 44, (p.min ?? 0.08) * g.usable) / g.pxPerWeight;
    return { fa: floor(panels[k]), fb: floor(panels[k + 1]) };
  };
  // Double-click resets the pair (toward its default ratio if both panels declare one, else equal);
  // arrow keys nudge the boundary ~6% of the pair.
  const resetPair = (k: number) => {
    const pa = props.panels[k];
    const pb = props.panels[k + 1];
    const sum = weight(pa.id) + weight(pb.id);
    const f = pairFloors(k);
    // Reset toward the pair's design default ratio when both declare one (the looper-hero stage layout);
    // otherwise an equal split. Sum is preserved so the rest of the stack doesn't shift.
    const da = pa.defaultWeight;
    const db = pb.defaultWeight;
    const target =
      da && db && da > 0 && db > 0 && Number.isFinite(da) && Number.isFinite(db) ? (sum * da) / (da + db) : sum / 2;
    // Never store a weight below either pane's floor (keeps weight↔render consistent).
    const na = f && f.fa + f.fb < sum ? Math.max(f.fa, Math.min(sum - f.fb, target)) : target;
    props.onSizes({ ...props.sizes(), [pa.id]: na, [pb.id]: sum - na });
  };
  const nudge = (k: number, dir: number) => {
    const a = props.panels[k].id;
    const b = props.panels[k + 1].id;
    const sum = weight(a) + weight(b);
    const f = pairFloors(k);
    if (f && f.fa + f.fb >= sum) return; // pair too narrow to honor both floors — leave it as-is
    const lo = f ? f.fa : sum * 0.08;
    const hi = f ? sum - f.fb : sum * 0.92;
    const na = Math.max(lo, Math.min(hi, weight(a) + sum * 0.06 * dir));
    props.onSizes({ ...props.sizes(), [a]: na, [b]: sum - na });
  };
  // A11y: pane A's share of the adjacent pair (0–100), for the separator's aria-valuenow. The 8/92
  // bounds mirror the nudge clamp, so a screen reader announces the split and its adjustable range.
  const pairPctA = (k: number) => {
    const sum = weight(props.panels[k].id) + weight(props.panels[k + 1].id);
    const pct = sum > 0 ? Math.round((weight(props.panels[k].id) / sum) * 100) : 50;
    // Clamp to the declared aria-valuemin/valuemax (8/92): a stale/lopsided persisted weight could put
    // the raw ratio outside [8,92], and ARIA requires valuenow ∈ [valuemin,valuemax] or a screen reader
    // announces a self-contradictory "5%, range 8 to 92".
    return Math.max(8, Math.min(92, pct));
  };

  // A divider is draggable only between two WEIGHTED panels. Against an autoSize pane there's nothing to
  // resize (it hugs content), so that gap renders a static spacer instead. beginDrag / nudge / resetPair /
  // pairPctA are therefore only ever reached with two weighted neighbours — they never read an autoSize
  // panel's weight. (divider k sits between panels k and k+1.)
  const interactiveDivider = (k: number) =>
    k < props.panels.length - 1 && !props.panels[k].autoSize && !props.panels[k + 1].autoSize;

  return (
    <div
      ref={host}
      class="splitstack"
      classList={{
        'splitstack--v': vertical(),
        'splitstack--h': !vertical(),
        'splitstack--dragging': dragging() >= 0,
      }}
      style={
        vertical()
          ? { 'grid-template-rows': template() }
          : { 'grid-template-columns': template() }
      }
    >
      <For each={props.panels}>
        {(panel, i) => (
          <>
            <div class="splitstack__pane">{panel.render()}</div>
            <Show when={i() < props.panels.length - 1}>
              {/* Interactive divider between two weighted panes; a static, non-interactive spacer next to
                  an autoSize pane. Either way it occupies the one dpx gap track the template reserves for
                  this pair, so the pane/gap DOM order stays lockstep with the grid tracks. */}
              <Show
                when={interactiveDivider(i())}
                fallback={<div class="splitstack__gap" aria-hidden="true" />}
              >
              <div
                class="splitstack__divider"
                role="separator"
                aria-orientation={vertical() ? 'horizontal' : 'vertical'}
                aria-label={`Resize ${props.panels[i()].label ?? props.panels[i()].id} and ${
                  props.panels[i() + 1]?.label ?? props.panels[i() + 1]?.id ?? ''
                }`}
                aria-valuenow={pairPctA(i())}
                aria-valuemin={8}
                aria-valuemax={92}
                aria-valuetext={`${pairPctA(i())}%`}
                tabindex="0"
                title="Drag to resize. Double-click, Home, or Enter to reset"
                onPointerDown={(e) => beginDrag(i(), e)}
                onPointerMove={onPointerMove}
                onPointerUp={endDrag}
                onPointerCancel={endDrag}
                onDblClick={() => resetPair(i())}
                onKeyDown={(e) => {
                  const dec = vertical() ? 'ArrowUp' : 'ArrowLeft';
                  const inc = vertical() ? 'ArrowDown' : 'ArrowRight';
                  if (e.key === dec) {
                    nudge(i(), -1);
                    e.preventDefault();
                  } else if (e.key === inc) {
                    nudge(i(), 1);
                    e.preventDefault();
                  } else if (e.key === 'Home' || e.key === 'Enter') {
                    // Keyboard equivalent of double-click-reset (M4) — equalize the adjacent pair.
                    resetPair(i());
                    e.preventDefault();
                  }
                }}
              >
                <span class="splitstack__grip" aria-hidden="true" />
              </div>
              </Show>
            </Show>
          </>
        )}
      </For>
    </div>
  );
}
