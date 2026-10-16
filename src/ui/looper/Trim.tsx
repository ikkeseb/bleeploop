import { createEffect, createSignal } from 'solid-js';
import { trimLane } from '../state/engine-store';
import { loopWholeBars, trimGate } from './gates';
import '../transport/transport.css';
import './trim.css';

/**
 * ✂ TRIM (F16), the EDIT popover's third view (`Edit.tsx`): the track keeps its first N bars as heard and
 * repeats them across the loop, whose length stays (`Looper::trim` in lf-engine). One UNDO gives the loop
 * back. KEEP FIRST [−] N [+] BARS and a TRIM button; N starts at half the loop's bars, rounded down, and
 * stays in 1..bars−1. TRIM sends the command and closes the popover (`done`, true for a press by key, so
 * the shell hands focus back to the EDIT pill).
 */
export function TrimDialog(props: { index: number; id: string; done: (keyboard: boolean) => void }) {
  const [keep, setKeep] = createSignal(Math.max(1, Math.floor(loopWholeBars() / 2)));
  const bars = () => loopWholeBars();
  // N, inside 1..bars−1 even when the loop changes under it (a multiply elsewhere).
  const n = () => Math.max(1, Math.min(bars() - 1, keep()));
  const track = props.index + 1;
  // The lane's live gate, so the open dialog's TRIM greys out when a pedal starts an overdub or a stop
  // under it, and a press that races it still sends nothing. The engine's own check (`Looper::trim`)
  // stays the last word. A keyboard press (a click with detail 0, as `src/app/transport-keys.ts` reads
  // it) hands focus back to the pill, as Escape does; a pointer press leaves it where the transport
  // keys' blur put it.
  const disabled = () => !trimGate(props.index).ok;
  const trim = (e: MouseEvent) => {
    if (disabled()) return;
    trimLane(props.index, n());
    props.done(e.detail === 0);
  };
  // The gate closing disables TRIM; a disabled button drops focus to body, where the next Space is the
  // transport's, so the focus moves to the dialog instead and Escape still hands it back to the lane.
  let panel: HTMLDivElement | undefined;
  createEffect(() => {
    if (disabled() && panel && panel.contains(document.activeElement)) panel.focus();
  });
  return (
    <div
      class="edit-popover"
      id={props.id}
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
        <button class="transport__step" aria-label="Keep fewer bars" disabled={n() <= 1} onClick={() => setKeep(n() - 1)}>
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
          disabled={disabled()}
          onClick={trim}
        >
          TRIM
        </button>
      </div>
    </div>
  );
}
