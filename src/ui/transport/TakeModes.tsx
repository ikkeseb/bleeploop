import { For, Show, createEffect, createMemo, createSignal } from 'solid-js';
import { Portal } from 'solid-js/web';
import { clock, looper, sampleRate } from '../state/audio';
import { laterTakeBars } from '../state/engine-store';
import { EscapeCloses, masterBars } from '../looper/shared';
import { autoRecGate, fixedGate, retakeGate } from '../looper/gates';
import { stageOpen } from '../stage/stage-store';
import { takePhrase, type TakeTerm } from './take-phrase';
import '../looper/fxpanel.css';
import './take-modes.css';

/**
 * TAKE: the record modes behind one command-bar chip. The chip reads the modes that are on as one
 * phrase ("FIXED 4 · CLICK", "FREE" with none; the grammar is `take-phrase.ts`'s) and opens a popover
 * with their five keys and the settings that go with them: FIXED's bars, RETAKE, AUTO REC's
 * sensitivity, CLICK's level and END STOP. Every key sends the engine's toggle and shows the feed's
 * `Toggled` (`state/engine-store.ts` `toggleSetting`), the same facade calls its pedal action
 * (`src/app/actions.ts`) reaches. A setting is live only while its mode is on; off, it stays in view,
 * dimmed, so the popover never changes size.
 *
 * The chip holds one width (an unseen sizer under the phrase, as the loop read-out does): turning a mode
 * never moves the controls beside it, and `src/app/cmd-fit.ts` measures the same width in every state.
 */

/** The chip's sizer: the widest phrase `takePhrase` can produce in pixels at the chip's type, measured
 * over all 1,662 phrases of 1 to 150 bars (144 px; its digits are tabular, so any two-digit FIXED with
 * AUTO REC and more after it). */
const WIDEST_PHRASE = 'FIXED 00 · AUTO REC +0';

export function TakeModes(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  const [open, setOpen] = createSignal(false);
  let trigger: HTMLButtonElement | undefined;
  // Non-modal, as IN FX's popover (`InputFx.tsx`): focus moves into the panel on open; a keyboard close
  // hands it back to the chip on the app's behalf (`returnFocus`, so Space after Escape still arms REC);
  // a pointer close leaves it where the pointer went. The play path stays live behind it.
  const close = (keyboard: boolean) => {
    setOpen(false);
    if (keyboard) props.returnFocus?.(trigger);
  };
  // The stage view covers the command bar but not this portal: opening it closes the popover.
  createEffect(() => {
    if (stageOpen()) setOpen(false);
  });

  // ----- FIXED length, shown as what the next take will actually record -----
  // Up to the loop's bar count a later take is that many bars and repeats across the loop; past it the
  // loop grows to it in whole loops (multiply), so the stepper moves a bar at a time up to the loop and a
  // whole loop at a time above it. FIXED off, a take runs until the press and may grow the loop too (E10:
  // the stop picks the nearest whole number of loops). `nextTakeMaxBars()` bounds it: 32 before a loop,
  // then the longest take the looper records. The chip and the stepper show the EFFECTIVE value so the
  // UI never promises a take the looper will not record; the signal itself is left alone, so a longer
  // loop restores the user's choice.
  const hasMaster = () => looper.masterLengthFrames() > 0;
  const loopBars = () => masterBars(looper.masterLengthFrames(), clock.bpm(), sampleRate(), clock.meter().beats);
  const maxTakeBars = createMemo(() => Math.max(1, Math.floor(looper.nextTakeMaxBars())));
  const takeLoopBars = () => (hasMaster() ? loopBars() : 0);
  const canMultiply = () => hasMaster() && maxTakeBars() > loopBars();
  const effectiveFixedBars = createMemo(() =>
    Math.min(maxTakeBars(), laterTakeBars(looper.fixedLengthBars(), takeLoopBars(), maxTakeBars())),
  );
  const stepFixedBars = (dir: 1 | -1) => {
    const bars = effectiveFixedBars();
    const loop = takeLoopBars();
    const byLoop = loop > 0 && (dir > 0 ? bars >= loop : bars > loop);
    looper.setFixedLengthBars(Math.max(1, Math.min(maxTakeBars(), bars + dir * (byLoop ? loop : 1))));
  };
  // RETAKE passes roll at master length whatever FIXED says, so FIXED is meaningless there.
  const fixedIgnored = createMemo(() => looper.retakeEnabled() && hasMaster());
  const fixedDisabled = createMemo(() => !fixedGate().ok);
  // RETAKE and AUTO REC: the gates their pedal actions are refused by (`src/app/actions.ts`).
  const retakeDisabled = createMemo(() => !retakeGate().ok);
  const autoRecDisabled = createMemo(() => !autoRecGate().ok);
  const fixedTitle = () =>
    fixedIgnored()
      ? 'RETAKE takes roll at the full loop length, so FIXED is ignored while it is on'
      : canMultiply()
        ? 'Length of the next take in bars. Shorter than the loop: it repeats across the loop. Longer: the loop grows to it in whole loops, and the other tracks repeat. Off: the take runs until you stop it, and a take stopped past the loop grows it to the nearest whole number of loops'
        : hasMaster()
          ? 'Length of the next take, in bars (at most the loop). A shorter take repeats across the loop'
          : 'Length of the first take, in bars (count-in + auto-stop on the downbeat)';

  // ----- the chip's phrase -----
  const phrase = createMemo(() =>
    takePhrase({
      fixedBars: looper.fixedLengthEnabled() ? effectiveFixedBars() : null,
      retake: looper.retakeEnabled(),
      autoRec: looper.autoRecordEnabled(),
      click: clock.metronomeOn(),
      endStop: looper.loopEndStopEnabled(),
    }),
  );
  // A term that does not apply now reads faint: FIXED under a RETAKE over a loop, AUTO REC once the tempo
  // is locked (it starts only a first take).
  const termIdle = (t: TakeTerm) => (t.key === 'fixed' && fixedIgnored()) || (t.key === 'autoRec' && clock.bpmLocked());
  // The whole list, uncut, for the chip's description.
  const allOn = () =>
    [
      looper.fixedLengthEnabled() ? `FIXED ${effectiveFixedBars()}` : 'FREE',
      looper.retakeEnabled() && 'RETAKE',
      looper.autoRecordEnabled() && 'AUTO REC',
      clock.metronomeOn() && 'CLICK',
      looper.loopEndStopEnabled() && 'END STOP',
    ]
      .filter(Boolean)
      .join(', ');

  const clickPct = () => Math.round(clock.clickVolume() * 100);

  return (
    <>
      {/* One stable name; the phrase is the description (a name that changed with every mode would read
          as a new control). */}
      <button
        ref={trigger}
        class="transport__tgl transport__take"
        aria-label="Take modes"
        aria-haspopup="dialog"
        aria-expanded={open()}
        aria-controls="lf-take-popover"
        title={`How the next take records: ${allOn()}`}
        onClick={() => setOpen((v) => !v)}
      >
        <span class="transport__take-k" aria-hidden="true">
          TAKE
        </span>
        <span class="transport__take-v" aria-hidden="true">
          <span class="transport__take-fit">{WIDEST_PHRASE}</span>
          <span class="transport__take-phrase">
            <For each={phrase().terms}>
              {(t, i) => (
                <>
                  <Show when={i() > 0}>
                    <span class="transport__take-sep"> · </span>
                  </Show>
                  <span
                    class="transport__take-term"
                    classList={{ 'is-free': t.key === 'free', 'is-idle': termIdle(t) }}
                  >
                    {t.text}
                  </span>
                </>
              )}
            </For>
            <Show when={phrase().more > 0}>
              <span class="transport__take-more"> +{phrase().more}</span>
            </Show>
          </span>
        </span>
        <span class="transport__take-caret" aria-hidden="true" />
      </button>
      <Show when={open()}>
        <EscapeCloses close={() => close(true)} />
        <Portal>
          <div class="settings-popover__backdrop" onClick={() => close(false)}>
            <div
              class="take-pop"
              id="lf-take-popover"
              role="dialog"
              aria-label="Take modes"
              tabindex={-1}
              ref={(el) => queueMicrotask(() => el.focus())}
              onClick={(e) => e.stopPropagation()}
            >
              {/* Each key keeps ONE aria-label and says on/off through aria-pressed alone; a label that
                  flips as well reads "Click off, pressed". */}
              {/* FIXED: the length of the NEXT take in bars. Before a loop that is the first take
                  (count-in, then auto-stop on the downbeat); after it a shorter take repeats across the
                  loop and a longer one grows it in whole loops. Read at arm, so it is locked only while a
                  capture is live, or while RETAKE (whose passes are master-length) overrides it. */}
              <button
                class="transport__tgl take-pop__key"
                classList={{ 'is-on': looper.fixedLengthEnabled() }}
                aria-label="Fixed take length"
                aria-pressed={looper.fixedLengthEnabled()}
                disabled={fixedDisabled()}
                onClick={() => looper.toggleFixedLength()}
                title={fixedTitle()}
              >
                FIXED
              </button>
              <div
                class="transport__bars take-pop__set"
                classList={{ 'is-off': !looper.fixedLengthEnabled() }}
                role="group"
                aria-label="Take length in bars"
              >
                <button
                  class="transport__step"
                  aria-label="Fewer bars"
                  disabled={!looper.fixedLengthEnabled() || fixedDisabled()}
                  onClick={() => stepFixedBars(-1)}
                >
                  −
                </button>
                <span class="transport__bars-val" aria-live="polite">
                  {effectiveFixedBars()}
                  <span class="transport__bars-unit">{effectiveFixedBars() === 1 ? 'bar' : 'bars'}</span>
                </span>
                <button
                  class="transport__step"
                  aria-label="More bars"
                  disabled={!looper.fixedLengthEnabled() || fixedDisabled() || effectiveFixedBars() >= maxTakeBars()}
                  onClick={() => stepFixedBars(1)}
                  title={hasMaster() ? (canMultiply() ? 'Past the loop: whole loops' : 'At most the loop length') : undefined}
                >
                  +
                </button>
              </div>

              {/* RETAKE: a take with a known length (FIXED first take, any later take) keeps rolling; the
                  stop gesture keeps the last complete pass. Read at arm, so locked while a capture is live. */}
              <button
                class="transport__tgl take-pop__key"
                classList={{ 'is-on': looper.retakeEnabled() }}
                aria-label="Retake"
                aria-pressed={looper.retakeEnabled()}
                disabled={retakeDisabled()}
                onClick={() => looper.toggleRetake()}
                title="Keep recording round the loop until you stop. STOP, REC/DUB or REC on another track keeps the last complete pass. First track needs FIXED."
              >
                RETAKE
              </button>
              <span />

              {/* AUTO REC replaces the first-track count-in with a level arm (the detector listens to the
                  record tap and keeps a short onset look-back; later tracks still use the master boundary
                  arm). Its sensitivity stays editable while LISTENING, so a noisy guitar chain can be
                  tuned without cancelling the arm; the cyan tick on the record level is its trigger. */}
              <button
                class="transport__tgl take-pop__key"
                classList={{ 'is-on': looper.autoRecordEnabled() }}
                aria-label="Auto record"
                aria-pressed={looper.autoRecordEnabled()}
                disabled={autoRecDisabled()}
                onClick={() => looper.toggleAutoRecord()}
                title="Start recording when you start playing, instead of counting in (first track). SENS is how quiet a sound may be and still start it: higher = more sensitive."
              >
                AUTO REC
              </button>
              <label class="fxp-param take-pop__set" classList={{ 'is-off': !looper.autoRecordEnabled() }}>
                <span class="fxp-param__label">Sens</span>
                <input
                  class="lf-range fxp-param__range"
                  type="range"
                  min="1"
                  max="100"
                  step="1"
                  value={looper.autoRecordSensitivity()}
                  style={{ '--fill': `${looper.autoRecordSensitivity()}%` }}
                  disabled={!looper.autoRecordEnabled() || clock.bpmLocked()}
                  aria-label="Auto record sensitivity"
                  aria-valuetext={`${looper.autoRecordSensitivity()} of 100`}
                  title="Higher starts recording from quieter input"
                  onInput={(e) => looper.setAutoRecordSensitivity(Number(e.currentTarget.value))}
                />
                <span class="fxp-param__val">{looper.autoRecordSensitivity()}</span>
              </label>

              <button
                class="transport__tgl take-pop__key"
                classList={{ 'is-on': clock.metronomeOn() }}
                aria-label="Metronome click"
                aria-pressed={clock.metronomeOn()}
                onClick={() => clock.toggleMetronome()}
                title="Metronome click: its own level, never recorded, silent while nothing runs"
              >
                CLICK
              </button>
              <label class="fxp-param take-pop__set" classList={{ 'is-off': !clock.metronomeOn() }}>
                <span class="fxp-param__label">Level</span>
                <input
                  class="lf-range fxp-param__range"
                  type="range"
                  min="0"
                  max="100"
                  step="1"
                  value={clickPct()}
                  style={{ '--fill': `${clickPct()}%` }}
                  disabled={!clock.metronomeOn()}
                  aria-label="Click volume"
                  aria-valuetext={`${clickPct()} percent`}
                  title="Click volume"
                  onInput={(e) => clock.setClickVolume(Number(e.currentTarget.value) / 100)}
                />
                <span class="fxp-param__val">{clickPct()}</span>
              </label>

              {/* END STOP: how every stop lands. */}
              <button
                class="transport__tgl take-pop__key"
                classList={{ 'is-on': looper.loopEndStopEnabled() }}
                aria-label="Stop playing loops at loop end"
                aria-pressed={looper.loopEndStopEnabled()}
                onClick={() => looper.toggleLoopEndStop()}
                title="Stop playback at loop end. Press STOP again for immediate stop. Recording and overdub still commit and stop immediately."
              >
                END STOP
              </button>
              <span />
            </div>
          </div>
        </Portal>
      </Show>
    </>
  );
}
