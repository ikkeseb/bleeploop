import { Show, createEffect, createSignal } from 'solid-js';
import { Portal } from 'solid-js/web';
import { trimLane } from '../state/engine-store';
import { looper } from '../state/audio';
import { stageOpen } from '../stage/stage-store';
import { loopWholeBars, trimGate } from './gates';
import { EscapeCloses } from './shared';
import '../transport/transport.css';
import './trim.css';

/**
 * ✂ TRIM (F16), a lane pill after ⧉ COPY: the track keeps its first N bars as heard and repeats them
 * across the loop, whose length stays (`Looper::trim` in lf-engine). One UNDO gives the loop back. Engine
 * mode only; shown while the lane holds a committed loop of two whole bars or more.
 *
 * The pill opens a small popover under itself, IN FX's pattern (`src/ui/transport/InputFx.tsx`): KEEP
 * FIRST [−] N [+] BARS and a TRIM button. N starts at half the loop's bars, rounded down, and stays in
 * 1..bars−1. TRIM sends the command and closes; Escape, an outside click or the stage view closes it.
 * A keyboard close (Escape, or TRIM pressed by key) hands focus back to the pill on the app's behalf
 * (`returnFocus`), so Space after it still arms REC.
 */

function TrimControl(props: { index: number; disabled: boolean; returnFocus?: (el: HTMLElement | undefined) => void }) {
  const [open, setOpen] = createSignal(false);
  const [keep, setKeep] = createSignal(1);
  const bars = () => loopWholeBars();
  // The popover's N, inside 1..bars−1 even when the loop changes under it (a multiply elsewhere).
  const n = () => Math.max(1, Math.min(bars() - 1, keep()));
  const track = props.index + 1;
  let trigger: HTMLButtonElement | undefined;
  const close = (keyboard: boolean) => {
    setOpen(false);
    if (keyboard) props.returnFocus?.(trigger);
  };
  const toggle = () => {
    if (!open()) setKeep(Math.max(1, Math.floor(bars() / 2)));
    setOpen((v) => !v);
  };
  // `disabled` is the lane's live gate (a reactive prop), so the open popover's TRIM greys out when a
  // pedal starts an overdub or a stop under it, and a press that races it still sends nothing. The
  // engine's own check (`Looper::trim`) stays the last word. A keyboard press (a click with detail 0,
  // as `src/app/transport-keys.ts` reads it) hands focus back to the pill, as Escape does; a pointer
  // press leaves it where the transport keys' blur put it.
  const trim = (e: MouseEvent) => {
    if (props.disabled) return;
    trimLane(props.index, n());
    close(e.detail === 0);
  };
  // The stage view hides the lanes: the popover goes with them.
  createEffect(() => {
    if (stageOpen()) setOpen(false);
  });
  // The gate closing under an open popover disables its TRIM; a disabled button drops focus to body,
  // where the next Space is the transport's, so the focus moves to the dialog instead and Escape still
  // hands it back to the lane.
  let panel: HTMLDivElement | undefined;
  createEffect(() => {
    if (open() && props.disabled && panel && panel.contains(document.activeElement)) panel.focus();
  });
  const id = `lf-trim-popover-${props.index}`;
  return (
    <>
      <button
        ref={trigger}
        class="lp-pb lp-pb--trim"
        classList={{ 'is-open': open() }}
        disabled={props.disabled}
        aria-label={`Trim track ${track}`}
        aria-haspopup="dialog"
        aria-expanded={open()}
        aria-controls={id}
        title="Keep the first bars and repeat them across the loop (undo gives the whole loop back)"
        onClick={toggle}
      >
        <span>
          ✂<span class="lp-pb__word"> TRIM</span>
        </span>
      </button>
      <Show when={open()}>
        <EscapeCloses close={() => close(true)} />
        <Portal>
          <div class="settings-popover__backdrop" onClick={() => close(false)}>
            <div
              class="trim-popover"
              id={id}
              role="dialog"
              aria-label={`Trim track ${track}`}
              tabindex={-1}
              ref={(el) => {
                panel = el;
                queueMicrotask(() => el.focus());
              }}
              onClick={(e) => e.stopPropagation()}
            >
              <div class="trim" role="group" aria-label="Bars to keep">
                <span class="trim__label">KEEP FIRST</span>
                <button
                  class="transport__step"
                  aria-label="Keep fewer bars"
                  disabled={n() <= 1}
                  onClick={() => setKeep(n() - 1)}
                >
                  −
                </button>
                <span class="trim__val" aria-live="polite">
                  {n()}
                </span>
                <button
                  class="transport__step"
                  aria-label="Keep more bars"
                  disabled={n() >= bars() - 1}
                  onClick={() => setKeep(n() + 1)}
                >
                  +
                </button>
                <span class="trim__unit">{n() === 1 ? 'BAR' : 'BARS'}</span>
                <button
                  class="trim__go"
                  aria-label={`Trim track ${track} to its first ${n()} ${n() === 1 ? 'bar' : 'bars'}`}
                  title={`Keep bars 1–${n()} of ${bars()} and repeat them across the loop`}
                  disabled={props.disabled}
                  onClick={trim}
                >
                  TRIM
                </button>
              </div>
            </div>
          </div>
        </Portal>
      </Show>
    </>
  );
}

/** The lane's ✂ TRIM pill and its popover, over a committed loop of two whole bars or more; nothing
 * otherwise. Disabled while the lane stops at the loop end (as ↶ UNDO and ↺ REV are). */
export function Trim(props: { index: number; returnFocus?: (el: HTMLElement | undefined) => void }) {
  const track = looper.track(props.index);
  const shown = () => track().canReverse && loopWholeBars() >= 2;
  return (
    <Show when={shown()}>
      <TrimControl index={props.index} disabled={!trimGate(props.index).ok} returnFocus={props.returnFocus} />
    </Show>
  );
}
