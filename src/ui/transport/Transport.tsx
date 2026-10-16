import { For, Show, createEffect, createMemo, createSignal, onCleanup } from 'solid-js';
import { clock, looper, master, sampleRate } from '../state/audio';
import { FADE_BARS, engineFade, laterTakeBars } from '../state/engine-store';
import { anyTrackIn, beatIndices, createTwoStepConfirm, masterBars } from '../looper/shared';
import { autoRecGate, fadeGate, fixedGate, retakeGate, tapGate } from '../looper/gates';
import { meterFrac, registerInputMeter, registerPhaseDial, unregisterInputMeter, unregisterPhaseDial } from '../looper/waveform';
import { autoRecordThreshold } from '../state/auto-record';
import { InputFx } from './InputFx';
import { BarPopoverPanel, createBarPopover } from './BarPopover';
import '../looper/fxpanel.css';
import './transport.css';

/**
 * Command-bar transport cluster. Renders as a Fragment so its children become direct
 * flex items of the `.cmd` header in app.tsx — the internal `.grow` spacer then pushes master + the
 * app.tsx tool icons to the far right. Left→right: BPM group (34px numeral + steppers + the meter
 * picker over the beat dots) · TAP ·
 * the record modes CLICK / FIXED / RETAKE / AUTO REC / END STOP (each with its lamp; CLICK's volume and
 * AUTO's sensitivity in a popover off their pill) · loop ring-dial readout · ■/▶ ALL + FADE and its bars
 * + ✕ ALL (two-step) + the record level (raw input is a slot set to Off and live) · IN FX (`InputFx.tsx`) ·
 * spacer · master mute + slider + value. EXPORT / IMPORT are icon tools in app.tsx's `.tools`
 * cluster (`SessionTools.tsx`).
 *
 * The ALL / loop-readout / master controls live here alone — this command-bar cluster is their
 * single home (the looper zone has no head row).
 */
export function Transport(props: { returnFocus?: (el: HTMLElement | undefined) => void }) {
  const anyStopping = createMemo(() =>
    Array.from({ length: looper.trackCount }, (_, i) => looper.track(i)().stopAt !== null).some(Boolean),
  );
  // Local state for inline BPM editing.
  const [editing, setEditing] = createSignal(false);
  const [editValue, setEditValue] = createSignal('');

  function startEdit() {
    setEditValue(String(clock.bpm()));
    setEditing(true);
  }

  function commitEdit() {
    // Removing the focused field can fire blur again, including after Escape cancels it.
    if (!editing()) return;
    const n = parseFloat(editValue());
    setEditing(false);
    if (!Number.isNaN(n)) clock.setBpm(n);
  }

  function onBpmKeyDown(e: KeyboardEvent) {
    // The commit unmounts the focused field, so the window's transport keys would find focus on the
    // body and read this Enter as PLAY/STOP: it stops here.
    if (e.key === 'Enter') {
      e.stopPropagation();
      commitEdit();
    }
    if (e.key === 'Escape') setEditing(false);
  }

  function onBpmInput(e: Event) {
    setEditValue((e.target as HTMLInputElement).value);
  }

  // Auto-focus the input when editing starts.
  let inputRef: HTMLInputElement | undefined;
  function refInput(el: HTMLInputElement) {
    inputRef = el;
  }

  function startEditFocused() {
    startEdit();
    // Focus after Solid re-renders.
    setTimeout(() => inputRef?.select(), 0);
  }

  // One-shot "tempo just locked" pulse. The engine freezes BPM when the first take's count-in starts
  // (AUTO REC: at its onset), and the feed's `Transport` event then says `locked`; without a visible cue
  // that freeze is silent/surprising.
  // Fire a brief pulse only on the false->true edge (not on mount, not on unlock).
  const [justLocked, setJustLocked] = createSignal(false);
  let prevLocked = clock.bpmLocked();
  let pulseTimer: ReturnType<typeof setTimeout> | undefined;
  createEffect(() => {
    const locked = clock.bpmLocked();
    if (locked && !prevLocked) {
      setJustLocked(true);
      clearTimeout(pulseTimer);
      pulseTimer = setTimeout(() => setJustLocked(false), 1500);
    }
    prevLocked = locked;
  });

  // ----- loop ring-dial readout (moved off Looper's lp__head; same bars/secs math) -----
  const masterFrames = () => looper.masterLengthFrames();
  const hasMaster = () => masterFrames() > 0;
  const loopBars = () => masterBars(masterFrames(), clock.bpm(), sampleRate(), clock.meter().beats);
  const loopSecs = () => masterFrames() / sampleRate();
  // r=16 ⇒ circumference 2π·16 ≈ 100.53; the progress arc fills as the loop phase goes 0→1. The arc is
  // driven by the waveform rAF loop (registerPhaseDial), not a signal — invariant 6.
  const DIAL_C = 2 * Math.PI * 16;

  // ----- global transport (■/▶ ALL, FADE, ✕ ALL two-step) wired to the shared looper store -----
  // Memoized so each five-lane scan runs once per state change, not once per consuming control per render.
  const anyLive = createMemo(() => anyTrackIn('PLAYING', 'OVERDUBBING', 'RECORDING'));

  // ----- FIXED length, shown as what the next take will actually record -----
  // Up to the loop's bar count a later take is that many bars and repeats across the loop; past it the
  // loop grows to it in whole loops (multiply), so the stepper moves a bar at a time up to the loop and a
  // whole loop at a time above it. FIXED off, a take runs until the press and may grow the loop too (E10:
  // the stop picks the nearest whole number of loops). `nextTakeMaxBars()` bounds it: 32 before a loop,
  // then the longest take the looper records. The label and the stepper
  // show the EFFECTIVE value so the UI never promises a take the looper will not record; the signal
  // itself is left alone, so a longer loop restores the user's choice.
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
  // RETAKE passes roll at master length whatever FIXED says, so the group is meaningless there.
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

  // ----- FADE: every playing lane fades out over FADE's bars and stops on the bar line; a
  // second press while they fade stops them at once. Its bars step through FADE_BARS, read at the press.
  const fadeState = createMemo(() => fadeGate());
  const fadeBars = () => engineFade.bars();
  const stepFadeBars = (dir: 1 | -1) => {
    const k = FADE_BARS.indexOf(fadeBars() as (typeof FADE_BARS)[number]);
    engineFade.setBars(FADE_BARS[Math.max(0, Math.min(FADE_BARS.length - 1, k + dir))]);
  };
  const fadeTitle = () => {
    const g = fadeState();
    if (!g.ok) return `Fade out: ${g.reason}`;
    if (engineFade.fading()) return 'Fading out. Press again to stop now';
    return `Fade every playing track out over ${fadeBars()} ${fadeBars() === 1 ? 'bar' : 'bars'}, then stop on the bar line`;
  };

  // Two-step clear-all — same latch as the per-track CLR; the guard keeps it from arming with nothing to clear.
  const clearAll = createTwoStepConfirm(() => looper.clearAll());
  const onClearAll = () => {
    if (!hasMaster() && !anyLive()) return;
    clearAll.trigger();
  };

  onCleanup(() => clearTimeout(pulseTimer));

  // ----- CLICK's volume and AUTO REC's sensitivity: set-and-forget, so each lives in a popover off its
  // pill's read-out (BarPopover.tsx), live while the setting is (CLICK on; AUTO on before a loop). A
  // popover whose read-out goes dead (the mode switched off by a key or a pedal, the tempo locking)
  // closes with it.
  const clickVol = () => Math.round(clock.clickVolume() * 100);
  const clickPop = createBarPopover(props.returnFocus);
  const clickPopDisabled = () => !clock.metronomeOn();
  const sensPop = createBarPopover(props.returnFocus);
  const sensPopDisabled = () => !looper.autoRecordEnabled() || clock.bpmLocked();
  createEffect(() => {
    if (clickPopDisabled()) clickPop.close(false);
  });
  createEffect(() => {
    if (sensPopDisabled()) sensPop.close(false);
  });

  // ----- the meter: picked before a loop, locked with the tempo (the engine drops a SetMeter while a
  // loop exists). A compound meter (x/8) counts its dotted quarters: BPM, TAP, the count-in and the dots.
  const meterName = (m: { num: number; den: number }) => `${m.num}/${m.den}`;
  const compound = () => clock.meter().den === 8;
  const meterTitle = () =>
    clock.bpmLocked()
      ? 'Meter locked to the loop. Clear all to change'
      : compound()
        ? `Time signature. BPM and TAP count dotted quarters, ${clock.meter().beats} a bar`
        : 'Time signature';

  return (
    <>
      {/* BPM group — big mono numeral, ± steppers, unit + meter picker over the beat dots. Read-only once a
          loop fixes the tempo and the meter (lock glyph + disabled steppers and picker + one-shot pulse
          make the freeze legible). */}
      <div
        class="transport__bpm-group"
        classList={{
          'transport__bpm-group--locked': clock.bpmLocked(),
          'transport__bpm-group--just-locked': justLocked(),
        }}
      >
        {clock.bpmLocked() && (
          <span
            class="transport__bpm-lock"
            title="Tempo locked to the loop. Clear all to change"
            aria-hidden="true"
          >
            <svg viewBox="0 0 24 24" width="11" height="11">
              <path
                fill="currentColor"
                d="M12 1.8a4.2 4.2 0 0 0-4.2 4.2V9H6.6A1.6 1.6 0 0 0 5 10.6v8.8A1.6 1.6 0 0 0 6.6 21h10.8a1.6 1.6 0 0 0 1.6-1.6v-8.8A1.6 1.6 0 0 0 17.4 9h-1.2V6A4.2 4.2 0 0 0 12 1.8zm2.6 7.2H9.4V6a2.6 2.6 0 0 1 5.2 0z"
              />
            </svg>
          </span>
        )}
        <button
          class="transport__step"
          aria-label="BPM minus"
          disabled={clock.bpmLocked()}
          onClick={() => clock.setBpm(clock.bpm() - 1)}
        >
          −
        </button>

        {editing() && !clock.bpmLocked() ? (
          <input
            ref={refInput}
            class="transport__bpm-input"
            aria-label="BPM"
            type="number"
            min="40"
            max="300"
            value={editValue()}
            onInput={onBpmInput}
            onKeyDown={onBpmKeyDown}
            onBlur={commitEdit}
          />
        ) : (
          <button
            class="transport__bpm-num"
            aria-label={`${clock.bpm()} BPM${clock.bpmLocked() ? ', locked' : ''}`}
            disabled={clock.bpmLocked()}
            onClick={() => !clock.bpmLocked() && startEditFocused()}
            title={clock.bpmLocked() ? 'Tempo locked to the loop. Clear all to change' : 'Click to edit BPM'}
          >
            {clock.bpm()}
          </button>
        )}

        <button
          class="transport__step"
          aria-label="BPM plus"
          disabled={clock.bpmLocked()}
          onClick={() => clock.setBpm(clock.bpm() + 1)}
        >
          +
        </button>

        <div class="transport__bpm-meta">
          <div class="transport__bpm-line">
            <span class="transport__bpm-unit">BPM</span>
            {/* Controlled: the native select changes its own value before the engine has it, so it is
                set back to the meter shown; the feed's Transport brings the one the engine took. A pick
                with the mouse hands the transport keys back (`src/app/transport-keys.ts`). */}
            <select
              class="transport__meter"
              aria-label="Time signature"
              title={meterTitle()}
              disabled={clock.bpmLocked()}
              value={meterName(clock.meter())}
              onChange={(e) => {
                const picked = clock.meters.find((m) => meterName(m) === e.currentTarget.value);
                if (picked) clock.setMeter(picked);
                e.currentTarget.value = meterName(clock.meter());
              }}
            >
              <For each={clock.meters}>{(m) => <option value={meterName(m)}>{meterName(m)}</option>}</For>
            </select>
          </div>
          <div
            class="transport__beats"
            role="img"
            aria-label={clock.running() ? `Beat ${clock.beat() + 1} of ${clock.meter().beats}` : 'Transport idle'}
          >
            <For each={beatIndices(clock.meter().beats)}>
              {(b) => (
                <i
                  class="transport__beat"
                  classList={{ on: clock.running() && clock.beat() === b }}
                  aria-hidden="true"
                />
              )}
            </For>
          </div>
        </div>
      </div>

      {/* TAP beside the tempo it sets; dead once a loop fixes the tempo (clock.tap -> setBpm no-ops while
          locked). The `transport__pair`: row 1 while it fits (app/cmd-fit.ts). */}
      <button
        class="transport__tgl transport__pair"
        aria-label="Tap tempo"
        disabled={!tapGate().ok}
        onClick={() => clock.tap()}
        title={
          clock.bpmLocked() ? 'Tempo locked to the loop. Clear all to retap' : compound() ? 'Tap a tempo in dotted quarters' : 'Tap a tempo'
        }
      >
        TAP
      </button>

      {/* Record modes: how the next take records and how a stop lands. They take the wrapping row together,
          leaving global transport beside the loop dial. Each lights its lamp while on (`transport__tgl--led`). Every
          toggle here, TAP and master mute keep ONE aria-label and say on/off through aria-pressed alone; a
          label that flips as well reads "Click off, pressed". */}
      <div class="transport__modes">
        {/* CLICK and its volume read-out: one split pill, the read-out opens the volume. */}
        <div class="transport__split" role="group" aria-label="Metronome">
          <button
            class="transport__tgl transport__tgl--led"
            classList={{ 'is-on': clock.metronomeOn() }}
            aria-label="Metronome click"
            aria-pressed={clock.metronomeOn()}
            onClick={() => clock.toggleMetronome()}
            title="Metronome click"
          >
            CLICK
          </button>
          <button
            ref={clickPop.trigger}
            class="transport__tgl transport__readout bar-pop__trigger"
            classList={{ 'is-on': clock.metronomeOn() }}
            aria-label="Click volume"
            aria-haspopup="dialog"
            aria-expanded={clickPop.open()}
            aria-controls="lf-click-popover"
            disabled={clickPopDisabled()}
            onClick={clickPop.toggle}
            title="Click volume"
          >
            <span class="transport__readout-v">{clickVol()}</span>
          </button>
          <BarPopoverPanel pop={clickPop} id="lf-click-popover" label="Click volume" class="bar-pop--param">
            <label class="fxp-param">
              <span class="fxp-param__label">Volume</span>
              <input
                class="lf-range fxp-param__range"
                type="range"
                min="0"
                max="100"
                step="1"
                value={clickVol()}
                style={{ '--fill': `${clickVol()}%` }}
                disabled={clickPopDisabled()}
                aria-label="Click volume"
                aria-valuetext={`${clickVol()} percent`}
                onInput={(e) => clock.setClickVolume(Number(e.currentTarget.value) / 100)}
              />
              <span class="fxp-param__val">{clickVol()}</span>
            </label>
          </BarPopoverPanel>
        </div>

        {/* Fixed-length record — the length of the NEXT take in bars. Before a loop exists that is the
            first take (count-in, then auto-stop on the downbeat); after it, the next take — a shorter take
            repeats across the loop, a longer one grows the loop in whole loops. Read at arm,
            so the controls stay usable after the BPM lock and are locked only while a capture is live, or
            while RETAKE (whose passes are master-length) overrides them. The bars show only while FIXED
            is on: off, a take has no length to show. */}
        <div class="transport__fixed" role="group" aria-label="Take length in bars">
          <button
            class="transport__tgl transport__tgl--led"
            classList={{ 'is-on': looper.fixedLengthEnabled() }}
            aria-label="Fixed take length"
            aria-pressed={looper.fixedLengthEnabled()}
            disabled={fixedDisabled()}
            onClick={() => looper.toggleFixedLength()}
            title={fixedTitle()}
          >
            FIXED
          </button>
          <Show when={looper.fixedLengthEnabled()}>
            <div class="transport__bars" role="group" aria-label="Take length in bars">
              <button
                class="transport__step"
                aria-label="Fewer bars"
                disabled={fixedDisabled()}
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
                disabled={fixedDisabled() || effectiveFixedBars() >= maxTakeBars()}
                onClick={() => stepFixedBars(1)}
                title={hasMaster() ? (canMultiply() ? 'Past the loop: whole loops' : 'At most the loop length') : undefined}
              >
                +
              </button>
            </div>
          </Show>
        </div>

        {/* RETAKE — a take with a known length (FIXED first take, any later take) keeps rolling; the stop
            gesture keeps the last complete pass. Read at arm, so it is locked while a capture is live. */}
        <button
          class="transport__tgl transport__tgl--led"
          classList={{ 'is-on': looper.retakeEnabled() }}
          aria-label="Retake"
          aria-pressed={looper.retakeEnabled()}
          disabled={retakeDisabled()}
          onClick={() => looper.toggleRetake()}
          title="Keep recording round the loop until you stop. STOP, REC/DUB or REC on another track keeps the last complete pass. First track needs FIXED."
        >
          RETAKE
        </button>

        {/* AUTO REC replaces the first-track count-in with a level arm. The detector listens to the
            existing record tap and keeps a short onset look-back; later tracks still use the master
            boundary arm. Its SENS read-out opens the sensitivity, which stays editable while LISTENING so
            a noisy guitar chain can be tuned without cancelling the arm. */}
        <div class="transport__split" role="group" aria-label="Automatic record start">
          <button
            class="transport__tgl transport__tgl--led"
            classList={{ 'is-on': looper.autoRecordEnabled() }}
            aria-label="Auto record"
            aria-pressed={looper.autoRecordEnabled()}
            disabled={autoRecDisabled()}
            onClick={() => looper.toggleAutoRecord()}
            title="Start recording when you start playing, instead of counting in (first track). SENS is how quiet a sound may be and still start it: higher = more sensitive."
          >
            AUTO REC
          </button>
          <button
            ref={sensPop.trigger}
            class="transport__tgl transport__readout bar-pop__trigger"
            classList={{ 'is-on': looper.autoRecordEnabled() }}
            aria-label="Auto record sensitivity"
            aria-haspopup="dialog"
            aria-expanded={sensPop.open()}
            aria-controls="lf-sens-popover"
            disabled={sensPopDisabled()}
            onClick={sensPop.toggle}
            title="Auto record sensitivity"
          >
            <span class="transport__readout-k">SENS</span>
            <span class="transport__readout-v">{looper.autoRecordSensitivity()}</span>
          </button>
          <BarPopoverPanel pop={sensPop} id="lf-sens-popover" label="Auto record sensitivity" class="bar-pop--param">
            <label class="fxp-param">
              <span class="fxp-param__label">Sens</span>
              <input
                class="lf-range fxp-param__range"
                type="range"
                min="1"
                max="100"
                step="1"
                value={looper.autoRecordSensitivity()}
                style={{ '--fill': `${looper.autoRecordSensitivity()}%` }}
                disabled={sensPopDisabled()}
                aria-label="Auto record sensitivity"
                aria-valuetext={`${looper.autoRecordSensitivity()} of 100`}
                onInput={(e) => looper.setAutoRecordSensitivity(Number(e.currentTarget.value))}
              />
              <span class="fxp-param__val">{looper.autoRecordSensitivity()}</span>
            </label>
            <p class="bar-pop__hint">Higher starts recording from quieter input. The cyan tick on the record level is the trigger.</p>
          </BarPopoverPanel>
        </div>

        {/* END STOP — how every stop lands: the last mode, so all five lamps read as one row. */}
        <button
          class="transport__tgl transport__tgl--led"
          classList={{ 'is-on': looper.loopEndStopEnabled() }}
          aria-label="Stop playing loops at loop end"
          aria-pressed={looper.loopEndStopEnabled()}
          onClick={() => looper.toggleLoopEndStop()}
          title="Stop playback at loop end. Press STOP again for immediate stop. Recording and overdub still commit and stop immediately."
        >
          END STOP
        </button>
      </div>

      {/* Loop readout — the mini ring-dial (progress = loopPhase) + N BARS · S.S s. */}
      <div class="transport__loop" title="Master loop length">
        <svg class="transport__dial" viewBox="0 0 40 40" aria-hidden="true">
          <circle cx="20" cy="20" r="16" fill="none" stroke="rgba(148,168,215,.15)" stroke-width="3" />
          <Show when={hasMaster()}>
            <circle
              cx="20"
              cy="20"
              r="16"
              fill="none"
              stroke="var(--play)"
              stroke-width="3"
              stroke-linecap="round"
              stroke-dasharray={String(DIAL_C)}
              stroke-dashoffset={String(DIAL_C)}
              transform="rotate(-90 20 20)"
              ref={(el) => {
                registerPhaseDial(el, DIAL_C);
                onCleanup(unregisterPhaseDial);
              }}
            />
          </Show>
        </svg>
        <div class="transport__loop-meta">
          <span class="transport__loop-k">LOOP</span>
          {/* Unseen, under the read-out: the widest one it can show (150 bars of 6/8 at 300 BPM fill the
              60 s buffer), so the window keeps one width from the empty "—" on and the controls beside
              it never move when a take lands. */}
          <span class="transport__loop-fit" aria-hidden="true">
            000 BARS <small>· 00.0 s</small>
          </span>
          <span class="transport__loop-v">
            <Show when={hasMaster()} fallback="—">
              {loopBars()} {loopBars() === 1 ? 'BAR' : 'BARS'} <small>· {loopSecs().toFixed(1)} s</small>
            </Show>
          </span>
        </div>
      </div>

      {/* Global transport: ■/▶ ALL · FADE + its bars · ✕ ALL (two-step) · record level · IN FX (the
          input sends). ■/▶ ALL and FADE hold their widest word (`data-fit`), so nothing beside them
          moves when the word does; ✕ ALL does not (its ✕ SURE? would fold the brand at 1280 px). */}
      <div class="transport__global">
        <button
          class="transport__tgl transport__tgl--fit"
          data-fit="■ NOW"
          classList={{ 'is-on': anyLive() }}
          disabled={!hasMaster() && !anyLive()}
          onClick={() => (anyLive() ? looper.stopAll() : looper.playAll())}
          aria-label={anyStopping() ? 'Stop all tracks now' : anyLive() ? 'Stop all tracks' : 'Play all tracks'}
          title={anyStopping() ? 'Stop all now' : anyLive() ? 'Stop all' : 'Play all'}
        >
          {anyStopping() ? '■ NOW' : anyLive() ? '■ ALL' : '▶ ALL'}
        </button>
        <div class="transport__fade" role="group" aria-label="Fade out">
          <button
            class="transport__tgl transport__tgl--fit"
            data-fit="FADING"
            classList={{ 'is-armed': engineFade.fading() }}
            disabled={!fadeState().ok}
            onClick={() => engineFade.fadeAll()}
            aria-label={engineFade.fading() ? 'Stop the fade now' : 'Fade out all tracks'}
            title={fadeTitle()}
          >
            {engineFade.fading() ? 'FADING' : 'FADE'}
          </button>
          <div class="transport__bars" role="group" aria-label="Fade length in bars">
            <button
              class="transport__step"
              aria-label="Shorter fade"
              disabled={fadeBars() <= FADE_BARS[0]}
              onClick={() => stepFadeBars(-1)}
            >
              −
            </button>
            <span class="transport__bars-val">
              {fadeBars()}
              <span class="transport__bars-unit">{fadeBars() === 1 ? 'bar' : 'bars'}</span>
            </span>
            <button
              class="transport__step"
              aria-label="Longer fade"
              disabled={fadeBars() >= FADE_BARS[FADE_BARS.length - 1]}
              onClick={() => stepFadeBars(1)}
            >
              +
            </button>
          </div>
        </div>
        <button
          class="transport__tgl"
          classList={{ 'is-armed': clearAll.armed() }}
          disabled={!hasMaster() && !anyLive()}
          onClick={onClearAll}
          onKeyDown={clearAll.onKeyDown}
          aria-label={clearAll.armed() ? 'Clear all tracks, press again to confirm' : 'Clear all tracks'}
          title="Clear all tracks and reset loop length"
        >
          {clearAll.armed() ? '✕ SURE?' : '✕ ALL'}
        </button>
        {/* Record level — what the looper hears at recordTap (synths, plugins, a live input): "am I too
            quiet?" before the take. Peak-held in capture.ts, drawn by the waveform rAF loop (no signal).
            With AUTO on, a cyan tick marks the detector's RMS threshold on the same dB scale — the
            sensitivity number gets a scale, and "why hasn't it started" answers itself. */}
        <div
          class="transport__inmeter"
          role="meter"
          aria-label="Record level"
          aria-valuemin="0"
          aria-valuemax="1"
          aria-valuenow="0"
          title="Record level: what the looper hears (synths, plugins, a live input); with AUTO on, the cyan tick is the trigger level"
          ref={(el) => {
            registerInputMeter(el);
            onCleanup(unregisterInputMeter);
            createEffect(() => {
              el.classList.toggle('has-thr', looper.autoRecordEnabled());
              el.style.setProperty('--thr', meterFrac(autoRecordThreshold(looper.autoRecordSensitivity())).toFixed(3));
            });
          }}
        />
        <InputFx returnFocus={props.returnFocus} />
      </div>

      {/* flexible spacer — pushes master + the app.tsx tool icons to the far right */}
      <div class="transport__grow" />

      {/* Master output level — on engine.masterGain (post record-tap, pre limiter). Mute + slider +
          value; both go through the `master` module so the DEV single-clock gate stays consistent. */}
      <div class="transport__master" role="group" aria-label="Master volume">
        <button
          class="transport__master-mute"
          classList={{ 'is-muted': master.muted() }}
          aria-label="Mute master"
          aria-pressed={master.muted()}
          onClick={() => master.setMuted(!master.muted())}
          title="Mute master output"
        >
          <svg viewBox="0 0 24 24" width="15" height="15" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
            <path d="M4 9v6h3.5L12 19V5L7.5 9H4z" stroke-linejoin="round" />
            {master.muted() ? (
              <path d="M16 9l4 6M20 9l-4 6" stroke-linecap="round" />
            ) : (
              <path d="M16 8.5a5 5 0 0 1 0 7M18.5 6a8 8 0 0 1 0 12" stroke-linecap="round" />
            )}
          </svg>
        </button>
        <input
          class="lf-range transport__volume"
          type="range"
          min="0"
          max="100"
          step="1"
          value={Math.round(master.volume() * 100)}
          style={{ '--fill': `${Math.round(master.volume() * 100)}%` }}
          aria-label="Master volume"
          aria-valuetext={`${Math.round(master.volume() * 100)} percent`}
          onInput={(e) => master.setVolume(Number((e.target as HTMLInputElement).value) / 100)}
        />
        <span class="transport__volume-val">{Math.round(master.volume() * 100)}</span>
      </div>
    </>
  );
}
