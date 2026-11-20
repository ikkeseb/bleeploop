import { For, Show, createEffect, createMemo, createSignal, onCleanup, untrack } from 'solid-js';
import { clock, looper, master, sampleRate } from '../state/audio';
import { FADE_BARS, engineFade, laterTakeBars } from '../state/engine-store';
import { anyTrackIn, beatIndices, createTwoStepConfirm, masterBars } from '../looper/shared';
import { autoRecGate, fadeGate, fixedGate, retakeGate, tapGate } from '../looper/gates';
import { meterFrac, registerInputMeter, registerPhaseDial, unregisterInputMeter, unregisterPhaseDial } from '../looper/waveform';
import { autoRecordThreshold } from '../state/auto-record';
import { RangeScale, StepScale, ValueKey } from './ValueKey';
import { createValueDrag } from './value-drag';
import './transport.css';

/**
 * The command bar's groups (the app.css header's three bands: this one is TIME and the whole machine).
 * Renders as a Fragment, so its children are direct items of app.tsx's `.cmd`, between the brand and
 * the tools; `app/cmd-fit.ts` measures them. Left to right, parted by panel (`.transport__gap`):
 * 1. THE ONE DISPLAY (`.k-bezel > .k-glass`): the tempo (typed with a click before the first take,
 *    stepped by a drag, the wheel or the arrows), BPM and the meter picker beside it, the padlock once a
 *    loop fixes them; the beat lamps; the loop's bars and the position; the loop's progress along the
 *    display's lower edge. TAP beside it.
 * 2. Transport: ■/▶ ALL, FADE (its bars on the key), ✕ ALL (two-step: CONFIRM).
 * 3. The record modes, five latched keys: CLICK (its volume on the key), FIXED (the next take's bars),
 *    RETAKE, AUTO REC (its sensitivity), END STOP.
 * 4. Master: the record level on the LED ladder (AUTO's trigger level ticked on it), mute, the volume
 *    knob and its value.
 * The ALL / loop read-out / master controls live here alone (the looper zone has no head row).
 */
export function Transport() {
  const anyStopping = createMemo(() =>
    Array.from({ length: looper.trackCount }, (_, i) => looper.track(i)().stopAt !== null).some(Boolean),
  );

  // ----- the tempo: typed in place (a click), stepped by the value gesture; read-only once a loop fixes
  // the tempo and the meter (the padlock and a one-shot pulse make the freeze legible) -----
  const [editing, setEditing] = createSignal(false);
  const [editValue, setEditValue] = createSignal('');
  let inputRef: HTMLInputElement | undefined;
  function startEdit() {
    setEditValue(String(clock.bpm()));
    setEditing(true);
    // Focus after Solid re-renders.
    setTimeout(() => inputRef?.select(), 0);
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
  const tempoDrag = createValueDrag({
    enabled: () => !clock.bpmLocked(),
    step: (dir, big) => clock.setBpm(clock.bpm() + dir * (big ? 10 : 1)),
    pxPerStep: 4,
  });

  // One-shot "tempo just locked" pulse, on the false->true edge only (not on mount, not on unlock): the
  // engine freezes the tempo when the first take's count-in starts (AUTO REC: at its onset).
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
  onCleanup(() => clearTimeout(pulseTimer));

  // ----- the meter: picked before a loop, locked with the tempo (the engine drops a SetMeter while a
  // loop exists). A compound meter (x/8) counts its dotted quarters: BPM, TAP, the count-in and the lamps.
  const meterName = (m: { num: number; den: number }) => `${m.num}/${m.den}`;
  const compound = () => clock.meter().den === 8;
  const meterTitle = () =>
    clock.bpmLocked()
      ? 'Meter locked to the loop. Clear all to change'
      : compound()
        ? `Time signature. BPM and TAP count dotted quarters, ${clock.meter().beats} a bar`
        : 'Time signature';

  // ----- the loop: its bars, the position, the progress line -----
  const masterFrames = () => looper.masterLengthFrames();
  const hasMaster = () => masterFrames() > 0;
  const loopBars = () => masterBars(masterFrames(), clock.bpm(), sampleRate(), clock.meter().beats);
  const loopSecs = () => masterFrames() / sampleRate();
  // The position: the beat from the feed's heard beat, its bar read off the loop's phase at that beat
  // (the nearest beat, so a downbeat heard a hair early still lands in its own bar). A plain read of the
  // phase in an effect a beat drives, never in the draw loop (invariant 6).
  const position = createMemo(() => {
    const beat = clock.beat();
    if (!hasMaster()) return '—';
    const beats = clock.meter().beats;
    const total = loopBars() * beats;
    const at = Math.round(untrack(() => looper.phaseValue()) * total) % total;
    return `${Math.floor(at / beats) + 1}.${beat + 1}`;
  });
  // The progress line is drawn by the waveform rAF loop through the dial's dash (registerPhaseDial), as
  // a line PROGRESS units long, never through a signal (invariant 6).
  const PROGRESS = 1000;
  const barTicks = () => {
    const n = loopBars();
    return n > 1 && n <= 32 ? Array.from({ length: n - 1 }, (_, k) => ((k + 1) / n) * 100) : [];
  };

  // ----- global transport (■/▶ ALL, FADE, ✕ ALL two-step) wired to the shared looper store -----
  // Memoized so each five-lane scan runs once per state change, not once per consuming control per render.
  const anyLive = createMemo(() => anyTrackIn('PLAYING', 'OVERDUBBING', 'RECORDING'));

  // ----- FIXED length, shown as what the next take will actually record -----
  // Up to the loop's bar count a later take is that many bars and repeats across the loop; past it the
  // loop grows to it in whole loops (multiply), so a step is a bar up to the loop and a whole loop above
  // it. FIXED off, a take runs until the press and may grow the loop too (E10: the stop picks the nearest
  // whole number of loops). `nextTakeMaxBars()` bounds it: 32 before a loop, then the longest take the
  // looper records. The key shows the EFFECTIVE value so the UI never promises a take the looper will not
  // record; the signal itself is left alone, so a longer loop restores the player's choice.
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
    const next = Math.max(1, Math.min(maxTakeBars(), bars + dir * (byLoop ? loop : 1)));
    if (next !== bars) looper.setFixedLengthBars(next);
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
        ? 'Length of the next take in bars (drag or scroll on the key to change it). Shorter than the loop: it repeats across the loop. Longer: the loop grows to it in whole loops, and the other tracks repeat. Off: the take runs until you stop it, and a take stopped past the loop grows it to the nearest whole number of loops'
        : hasMaster()
          ? 'Length of the next take, in bars (at most the loop; drag or scroll on the key to change it). A shorter take repeats across the loop'
          : 'Length of the first take, in bars (count-in + auto-stop on the downbeat; drag or scroll on the key to change it)';

  // ----- FADE: every playing lane fades out over FADE's bars and stops on the bar line; a second press
  // while they fade stops them at once. Its bars step through FADE_BARS, read at the press, and stay
  // settable while nothing plays.
  const fadeState = createMemo(() => fadeGate());
  const fadeBars = () => engineFade.bars();
  const stepFadeBars = (dir: 1 | -1) => {
    const k = FADE_BARS.indexOf(fadeBars() as (typeof FADE_BARS)[number]);
    const next = FADE_BARS[Math.max(0, Math.min(FADE_BARS.length - 1, k + dir))];
    if (next !== fadeBars()) engineFade.setBars(next);
  };
  const fadeTitle = () => {
    const g = fadeState();
    if (!g.ok) return `Fade out: ${g.reason}. Drag or scroll on the key to set its bars`;
    if (engineFade.fading()) return 'Fading out. Press again to stop now';
    return `Fade every playing track out over ${fadeBars()} ${fadeBars() === 1 ? 'bar' : 'bars'}, then stop on the bar line. Drag or scroll on the key to set its bars`;
  };

  // Two-step clear-all — same latch as the per-track CLR; the guard keeps it from arming with nothing to clear.
  const clearAll = createTwoStepConfirm(() => looper.clearAll());
  const onClearAll = () => {
    if (!hasMaster() && !anyLive()) return;
    clearAll.trigger();
  };

  // ----- CLICK's volume and AUTO REC's sensitivity, on their keys: live while the setting is (CLICK on;
  // AUTO on before a loop), as their popovers were.
  const clickVol = () => Math.round(clock.clickVolume() * 100);
  const sens = () => looper.autoRecordSensitivity();
  const sensLive = () => looper.autoRecordEnabled() && !clock.bpmLocked();
  const bigStep = (big: boolean) => (big ? 10 : 1);

  // ----- master -----
  const vol = () => Math.round(master.volume() * 100);
  const volDrag = createValueDrag({
    enabled: () => true,
    step: (dir, big) => master.setVolume(Math.max(0, Math.min(100, vol() + dir * bigStep(big))) / 100),
    pxPerStep: 2,
    focusOnPointer: false,
  });
  const KNOB_TICKS = Array.from({ length: 11 }, (_, k) => -135 + k * 27);

  const unit = (n: number) => (n === 1 ? 'BAR' : 'BARS');

  return (
    <>
      {/* 1. THE ONE DISPLAY and TAP */}
      <div class="transport__time">
        <div
          class="k-bezel transport__disp"
          classList={{ 'transport__bpm-group--locked': clock.bpmLocked(), 'transport__bpm-group--just-locked': justLocked() }}
        >
          <div class="k-glass transport__glass">
            <div class="transport__tempo-cell">
              <Show
                when={editing() && !clock.bpmLocked()}
                fallback={
                  <button
                    ref={tempoDrag.ref}
                    class="transport__bpm-num"
                    classList={{ 'is-adjusting': tempoDrag.adjusting() }}
                    aria-label={`${clock.bpm()} BPM${clock.bpmLocked() ? ', locked' : ''}`}
                    aria-description={clock.bpmLocked() ? undefined : 'Click to type a tempo; drag up or down, scroll, or use the arrow keys to step it'}
                    disabled={clock.bpmLocked()}
                    title={clock.bpmLocked() ? 'Tempo locked to the loop. Clear all to change' : 'Click to type a tempo. Drag or scroll to step it'}
                    {...tempoDrag.handlers}
                    onClick={() => {
                      if (tempoDrag.takeDragClick() || clock.bpmLocked()) return;
                      startEdit();
                    }}
                  >
                    {clock.bpm()}
                  </button>
                }
              >
                <input
                  ref={inputRef}
                  class="transport__bpm-input"
                  aria-label="BPM"
                  type="number"
                  min="40"
                  max="300"
                  value={editValue()}
                  onInput={(e) => setEditValue(e.currentTarget.value)}
                  onKeyDown={onBpmKeyDown}
                  onBlur={commitEdit}
                />
              </Show>
            </div>
            <div class="transport__bpm-meta">
              <span class="transport__bpm-unit">
                BPM
                <Show when={clock.bpmLocked()}>
                  <svg class="transport__bpm-lock" viewBox="5 1.8 14 19.2" aria-hidden="true">
                    <title>Tempo locked to the loop. Clear all to change</title>
                    <path
                      fill="currentColor"
                      d="M12 1.8a4.2 4.2 0 0 0-4.2 4.2V9H6.6A1.6 1.6 0 0 0 5 10.6v8.8A1.6 1.6 0 0 0 6.6 21h10.8a1.6 1.6 0 0 0 1.6-1.6v-8.8A1.6 1.6 0 0 0 17.4 9h-1.2V6A4.2 4.2 0 0 0 12 1.8zm2.6 7.2H9.4V6a2.6 2.6 0 0 1 5.2 0z"
                    />
                  </svg>
                </Show>
              </span>
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
                {(b) => <i class="transport__beat" classList={{ on: clock.running() && clock.beat() === b }} aria-hidden="true" />}
              </For>
            </div>
            <div
              class="transport__loop transport__ro"
              title={
                hasMaster()
                  ? `Master loop length: ${loopBars()} ${loopBars() === 1 ? 'bar' : 'bars'}, ${loopSecs().toFixed(1)} s`
                  : 'Master loop length'
              }
            >
              <span class="transport__cap">LOOP</span>
              <span class="transport__fig transport__loop-v">
                <Show when={hasMaster()} fallback="—">
                  {loopBars()} <span class="transport__fig-u">{unit(loopBars())}</span>
                </Show>
              </span>
            </div>
            <div class="transport__ro transport__pos" title="Position in the loop: bar.beat">
              <span class="transport__cap">BAR.BEAT</span>
              <span class="transport__fig transport__pos-v">{position()}</span>
            </div>
            {/* The loop's progress: one fine line along the lower edge, a tick per bar; lit while a lane
                sounds or records (`anyLive`), tone over a stopped loop. */}
            <div class="transport__progress" classList={{ 'is-live': anyLive() }} aria-hidden="true">
              <For each={barTicks()}>{(at) => <b style={{ left: `${at}%` }} />}</For>
              <Show when={hasMaster()}>
                <svg class="transport__progress-line" viewBox={`0 0 ${PROGRESS} 3`} preserveAspectRatio="none">
                  <line
                    x1="0"
                    y1="1.5"
                    x2={PROGRESS}
                    y2="1.5"
                    stroke-width="3"
                    stroke-dasharray={String(PROGRESS)}
                    stroke-dashoffset={String(PROGRESS)}
                    ref={(el) => {
                      registerPhaseDial(el as unknown as SVGCircleElement, PROGRESS);
                      onCleanup(unregisterPhaseDial);
                    }}
                  />
                </svg>
              </Show>
            </div>
          </div>
        </div>
        {/* TAP beside the tempo it sets; dead once a loop fixes the tempo (clock.tap -> setBpm no-ops while
            locked). */}
        <button
          class="k-key transport__key transport__tap"
          aria-label="Tap tempo"
          disabled={!tapGate().ok}
          onClick={() => clock.tap()}
          title={clock.bpmLocked() ? 'Tempo locked to the loop. Clear all to retap' : compound() ? 'Tap a tempo in dotted quarters' : 'Tap a tempo'}
        >
          TAP
        </button>
      </div>

      <i class="transport__gap" aria-hidden="true" />

      {/* 2. Transport: ■/▶ ALL · FADE and its bars · ✕ ALL (two-step) */}
      <div class="transport__global" role="group" aria-label="Transport">
        <button
          class="k-key transport__key"
          disabled={!hasMaster() && !anyLive()}
          onClick={() => (anyLive() ? looper.stopAll() : looper.playAll())}
          aria-label={anyStopping() ? 'Stop all tracks now' : anyLive() ? 'Stop all tracks' : 'Play all tracks'}
          title={anyStopping() ? 'Stop all now' : anyLive() ? 'Stop all' : 'Play all'}
        >
          {anyStopping() ? '■ NOW' : anyLive() ? '■ ALL' : '▶ ALL'}
        </button>
        <ValueKey
          class="transport__fade"
          label={engineFade.fading() ? 'Stop the fade now' : 'Fade out all tracks'}
          title={fadeTitle()}
          legend={engineFade.fading() ? 'FADING' : 'FADE'}
          value={
            <>
              {fadeBars()} <span class="k-key__u">{unit(fadeBars())}</span>
            </>
          }
          valueText={`Fade length ${fadeBars()} ${fadeBars() === 1 ? 'bar' : 'bars'}`}
          on={engineFade.fading()}
          pressDisabled={!fadeState().ok}
          adjustable
          step={stepFadeBars}
          pxPerStep={14}
          scale={() => <StepScale steps={FADE_BARS} value={fadeBars()} />}
          onPress={() => engineFade.fadeAll()}
        />
        <button
          class="k-key k-key--clr transport__key"
          classList={{ 'k-key--armed': clearAll.armed() }}
          disabled={!hasMaster() && !anyLive()}
          onClick={onClearAll}
          onKeyDown={clearAll.onKeyDown}
          aria-label={clearAll.armed() ? 'Clear all tracks, press again to confirm' : 'Clear all tracks'}
          title="Clear all tracks and reset loop length"
        >
          {clearAll.armed() ? 'CONFIRM' : '✕ ALL'}
        </button>
      </div>

      <i class="transport__gap" aria-hidden="true" />

      {/* 3. The record modes: how the next take records and how a stop lands. Each lights its LED while on;
          one aria-label each, on/off through aria-pressed alone (a label that flips as well reads "Click
          off, pressed"). A mode that is on but cannot act now keeps its LED lit over the disabled legend,
          with a padlock (transport.css). */}
      <div class="transport__modes" role="group" aria-label="Record modes">
        <ValueKey
          latched
          class="transport__mode"
          label="Metronome click"
          title="Metronome click. Drag or scroll on the key to set its volume"
          legend="CLICK"
          value={clickVol()}
          valueText={`Click volume ${clickVol()} percent`}
          on={clock.metronomeOn()}
          pressDisabled={false}
          adjustable={clock.metronomeOn()}
          step={(dir, big) => clock.setClickVolume(Math.max(0, Math.min(100, clickVol() + dir * bigStep(big))) / 100)}
          pxPerStep={2}
          scale={() => <RangeScale min="0" max="100" frac={clickVol() / 100} />}
          onPress={() => clock.toggleMetronome()}
        />
        {/* FIXED: the length of the NEXT take in bars. Before a loop exists that is the first take (count-in,
            then auto-stop on the downbeat); after it, a shorter take repeats across the loop, a longer one
            grows the loop in whole loops. Read at arm, so it stays settable after the tempo locks and is
            locked only while a capture is live, or while RETAKE (whose passes are master-length) overrides
            it. */}
        <ValueKey
          latched
          class="transport__mode transport__fixed"
          label="Fixed take length"
          title={fixedTitle()}
          legend="FIXED"
          value={
            <>
              {effectiveFixedBars()} <span class="k-key__u">{unit(effectiveFixedBars())}</span>
            </>
          }
          valueText={`Take length ${effectiveFixedBars()} ${effectiveFixedBars() === 1 ? 'bar' : 'bars'}`}
          on={looper.fixedLengthEnabled()}
          pressDisabled={fixedDisabled()}
          adjustable={looper.fixedLengthEnabled() && !fixedDisabled()}
          step={stepFixedBars}
          pxPerStep={10}
          scale={() => <RangeScale min="1" max={String(maxTakeBars())} frac={(effectiveFixedBars() - 1) / Math.max(1, maxTakeBars() - 1)} />}
          onPress={() => looper.toggleFixedLength()}
        />
        {/* RETAKE — a take with a known length (FIXED first take, any later take) keeps rolling; the stop
            gesture keeps the last complete pass. Read at arm, so it is locked while a capture is live. */}
        <button
          class="k-key k-key--latch transport__key transport__mode"
          classList={{ 'is-on': looper.retakeEnabled() }}
          aria-label="Retake"
          aria-pressed={looper.retakeEnabled()}
          disabled={retakeDisabled()}
          onClick={() => looper.toggleRetake()}
          title="Keep recording round the loop until you stop. STOP, REC/DUB or REC on another track keeps the last complete pass. First track needs FIXED."
        >
          <span class="k-led" aria-hidden="true" />
          <span class="k-key__l">RETAKE</span>
        </button>
        {/* AUTO REC replaces the first-track count-in with a level arm. The detector listens to the existing
            record tap and keeps a short onset look-back; later tracks still use the master boundary arm. Its
            sensitivity stays settable while LISTENING, so a noisy chain can be tuned without cancelling the
            arm. */}
        <ValueKey
          latched
          class="transport__mode transport__auto"
          label="Auto record"
          title="Start recording when you start playing, instead of counting in (first track). The value is how quiet a sound may be and still start it: higher = more sensitive. Drag or scroll on the key to change it; the cyan tick on the level meter is the trigger."
          legend="AUTO REC"
          value={sens()}
          valueText={`Sensitivity ${sens()} of 100`}
          on={looper.autoRecordEnabled()}
          pressDisabled={autoRecDisabled()}
          adjustable={sensLive()}
          step={(dir, big) => looper.setAutoRecordSensitivity(sens() + dir * bigStep(big))}
          pxPerStep={2}
          scale={() => <RangeScale min="1" max="100" frac={(sens() - 1) / 99} />}
          onPress={() => looper.toggleAutoRecord()}
        />
        {/* END STOP — how every stop lands: the last mode, so all five LEDs read as one row. */}
        <button
          class="k-key k-key--latch transport__key transport__mode"
          classList={{ 'is-on': looper.loopEndStopEnabled() }}
          aria-label="Stop playing loops at loop end"
          aria-pressed={looper.loopEndStopEnabled()}
          onClick={() => looper.toggleLoopEndStop()}
          title="Stop playback at loop end. Press STOP again for immediate stop. Recording and overdub still commit and stop immediately."
        >
          <span class="k-led" aria-hidden="true" />
          <span class="k-key__l">END STOP</span>
        </button>
      </div>

      <i class="transport__gap" aria-hidden="true" />

      {/* 4. Master: the level meter, mute, the volume knob and its value (engine.masterGain, post record
          tap, pre limiter), through the `master` module: UI-owned settings the engine keeps and replays. */}
      <div class="transport__master" role="group" aria-label="Master output">
        <button
          class="transport__master-mute"
          classList={{ 'is-muted': master.muted() }}
          aria-label="Mute master"
          aria-pressed={master.muted()}
          onClick={() => master.setMuted(!master.muted())}
          title="Mute master output"
        >
          <svg viewBox="0 0 18 18" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M2.5 7v4h2.8L9 14.2V3.8L5.3 7H2.5z" />
            {master.muted() ? <path d="M12 7l4 4M16 7l-4 4" /> : <path d="M12 6.5a3.6 3.6 0 0 1 0 5M14.2 4.4a6.6 6.6 0 0 1 0 9.2" />}
          </svg>
        </button>
        {/* The level the looper hears at its record tap (synths, plugins, a live input): "am I too quiet?"
            before the take, on the LED ladder (−60..0 dBFS). Peak-held in capture.ts and written by the
            waveform rAF loop as `--lvl` (no signal). With AUTO on, a cyan tick marks the detector's
            threshold on the same scale. */}
        <div class="transport__meter-box">
          <div
            class="k-meter transport__inmeter"
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
          >
            <i class="transport__thr" aria-hidden="true" />
          </div>
          <div class="transport__meter-scale" aria-hidden="true">
            <For each={[-48, -24, -12, 0]}>
              {(db) => (
                <span style={{ left: `${((db + 60) / 60) * 100}%` }}>
                  <b />
                  {db}
                </span>
              )}
            </For>
          </div>
        </div>
        <div
          ref={volDrag.ref}
          class="k-knob transport__knob"
          classList={{ 'is-adjusting': volDrag.adjusting() }}
          style={{ '--a': `${-135 + vol() * 2.7}deg` }}
          role="slider"
          tabindex="0"
          aria-label="Master volume"
          aria-valuemin="0"
          aria-valuemax="100"
          aria-valuenow={vol()}
          aria-valuetext={`${vol()} percent${master.muted() ? ', muted' : ''}`}
          title="Master volume: drag up or down, or scroll"
          {...volDrag.handlers}
        >
          <svg class="transport__knob-ticks" viewBox="-30 -30 60 60" aria-hidden="true">
            <For each={KNOB_TICKS}>{(a) => <line x1="0" y1="-26" x2="0" y2="-23" transform={`rotate(${a})`} />}</For>
          </svg>
        </div>
        <span class="k-win transport__volume-val" classList={{ 'is-off': master.muted() }}>
          {vol()}
        </span>
      </div>
    </>
  );
}
