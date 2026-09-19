import { For, Show, createEffect, createMemo, createSignal, onCleanup } from 'solid-js';
import { clock } from '../../audio/clock';
import { engine } from '../../audio/engine';
import { notifyError } from '../../notify';
import { looper } from '../../audio/looper/looper';
import { master } from '../../audio/master';
import { anyTrackIn, createTwoStepConfirm, masterBars } from '../looper/Looper';
import { meterFrac, registerInputMeter, registerPhaseDial, unregisterInputMeter, unregisterPhaseDial } from '../looper/waveform';
import { autoRecordThreshold } from '../../audio/looper/auto-record';
import './transport.css';

/**
 * Command-bar transport cluster (Orbit V2). Renders as a Fragment so its children become direct
 * flex items of the `.cmd` header in app.tsx — the internal `.grow` spacer then pushes master + the
 * app.tsx tool icons to the far right. Left→right: BPM group (34px numeral + steppers + beat dots) ·
 * CLICK / FIXED-N / AUTO / TAP / END STOP toggles · loop ring-dial readout · ■/▶ ALL + ✕ ALL (two-step) + MIC LIVE ·
 * spacer · master mute + slider + value. EXPORT / IMPORT are icon tools in app.tsx's `.tools`
 * cluster (`SessionTools.tsx`).
 *
 * The ALL / MIC / loop-readout / master controls live here alone — this command-bar cluster is their
 * single home (the looper zone has no head row).
 */
export function Transport() {
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
    const n = parseFloat(editValue());
    if (!Number.isNaN(n)) clock.setBpm(n);
    setEditing(false);
  }

  function onBpmKeyDown(e: KeyboardEvent) {
    if (e.key === 'Enter') commitEdit();
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

  // One-shot "tempo just locked" pulse. The looper freezes BPM at the first master-loop commit
  // (clock.setBpmLocked(true)); without a visible cue that freeze is silent/surprising. Fire a brief
  // pulse only on the false->true edge (not on mount, not on unlock).
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
  const loopBars = () => masterBars(masterFrames(), clock.bpm(), engine.ctx.sampleRate);
  const loopSecs = () => masterFrames() / engine.ctx.sampleRate;
  // r=16 ⇒ circumference 2π·16 ≈ 100.53; the progress arc fills as the loop phase goes 0→1. The arc is
  // driven by the waveform rAF loop (registerPhaseDial), not a signal — invariant 6.
  const DIAL_C = 2 * Math.PI * 16;

  // ----- global transport (■/▶ ALL, ✕ ALL two-step, MIC) wired to the shared looper store -----
  // Memoized so each five-lane scan runs once per state change, not once per consuming control per render.
  const anyLive = createMemo(() => anyTrackIn('PLAYING', 'OVERDUBBING', 'RECORDING'));
  const anyCapturing = createMemo(() => anyTrackIn('RECORDING', 'OVERDUBBING'));

  // Two-step clear-all — same latch as the per-track CLR; the guard keeps it from arming with nothing to clear.
  const clearAll = createTwoStepConfirm(() => looper.clearAll());
  const onClearAll = () => {
    if (!hasMaster() && !anyLive()) return;
    clearAll.trigger();
  };

  // Toggle mic/line input. toggleInput() resolves false in two cases — a successful DISARM, and a
  // failed ARM (no device: armInput returns false with only a console.warn) — so we branch on the armed
  // state captured BEFORE the toggle: a false result while arming means "no input available". A rejected
  // promise is getUserMedia denying/failing. Both were previously swallowed (console-only, invisible in a
  // release build); surface each as a toast. The underlying console.warn/error still feed the release log.
  const onToggleMic = () => {
    const wasArmed = looper.inputArmed();
    looper.toggleInput().then(
      (armed) => {
        if (!wasArmed && !armed) notifyError('No audio input available to arm');
      },
      (err) => {
        notifyError("Couldn't arm the mic/line input", err);
      },
    );
  };

  onCleanup(() => clearTimeout(pulseTimer));

  return (
    <>
      {/* BPM group — big mono numeral, ± steppers, unit + beat dots. Read-only once a loop fixes the
          tempo (lock glyph + disabled steppers + one-shot pulse make the freeze legible). */}
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
            aria-label="BPM"
            disabled={clock.bpmLocked()}
            onClick={() => !clock.bpmLocked() && startEditFocused()}
            title={clock.bpmLocked() ? 'Clear all to change loop length' : 'Click to edit BPM'}
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
          <span class="transport__bpm-unit">BPM</span>
          <div
            class="transport__beats"
            aria-label={clock.running() ? `Beat ${clock.beat() + 1}` : 'Transport idle'}
          >
            <For each={[0, 1, 2, 3]}>
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

      {/* Playback/record modes share the wrapping row, leaving global transport beside the loop dial. */}
      <div class="transport__modes">
        <div class="transport__click" role="group" aria-label="Metronome">
          <button
            class="transport__tgl"
            classList={{ 'is-on': clock.metronomeOn() }}
            aria-label={clock.metronomeOn() ? 'Click off' : 'Click on'}
            aria-pressed={clock.metronomeOn()}
            onClick={() => clock.setMetronome(!clock.metronomeOn())}
            title="Metronome click"
          >
            CLICK
          </button>
          <Show when={clock.metronomeOn()}>
            <input
              class="lf-range transport__volume transport__volume--click"
              type="range"
              min="0"
              max="100"
              step="1"
              value={Math.round(clock.clickVolume() * 100)}
              style={{ '--fill': `${Math.round(clock.clickVolume() * 100)}%` }}
              aria-label="Click volume"
              aria-valuetext={`${Math.round(clock.clickVolume() * 100)} percent`}
              title="Click volume"
              onInput={(e) => clock.setClickVolume(Number((e.target as HTMLInputElement).value) / 100)}
            />
          </Show>
        </div>

        {/* Fixed-length record — first-track record runs the count-in then captures exactly N bars and
            auto-stops on the downbeat. The bar stepper slides in while it's on; disabled once a loop
            fixes the tempo. */}
        <div class="transport__fixed" role="group" aria-label="Fixed-length record">
          <button
            class="transport__tgl"
            classList={{ 'is-on': looper.fixedLengthEnabled() }}
            aria-label={looper.fixedLengthEnabled() ? 'Fixed-length record on' : 'Fixed-length record off'}
            aria-pressed={looper.fixedLengthEnabled()}
            disabled={clock.bpmLocked()}
            onClick={() => looper.setFixedLengthEnabled(!looper.fixedLengthEnabled())}
            title="Record a fixed number of bars (count-in + auto-stop on the downbeat)"
          >
            FIXED {looper.fixedLengthBars()}
          </button>
          <Show when={looper.fixedLengthEnabled()}>
            <div class="transport__bars" role="group" aria-label="Loop length in bars">
              <button
                class="transport__step"
                aria-label="Fewer bars"
                disabled={clock.bpmLocked()}
                onClick={() => looper.setFixedLengthBars(looper.fixedLengthBars() - 1)}
              >
                −
              </button>
              <span class="transport__bars-val" aria-live="polite">
                {looper.fixedLengthBars()}
                <span class="transport__bars-unit">{looper.fixedLengthBars() === 1 ? 'bar' : 'bars'}</span>
              </span>
              <button
                class="transport__step"
                aria-label="More bars"
                disabled={clock.bpmLocked()}
                onClick={() => looper.setFixedLengthBars(looper.fixedLengthBars() + 1)}
              >
                +
              </button>
            </div>
          </Show>
        </div>

        {/* RETAKE — a take with a known length (FIXED first take, any later take) keeps rolling; the stop
            gesture keeps the last complete pass. Read at arm, so it is locked while a capture is live. */}
        <button
          class="transport__tgl"
          classList={{ 'is-on': looper.retakeEnabled() }}
          aria-label={looper.retakeEnabled() ? 'Retake on' : 'Retake off'}
          aria-pressed={looper.retakeEnabled()}
          disabled={anyCapturing()}
          onClick={() => looper.setRetakeEnabled(!looper.retakeEnabled())}
          title="Keep recording round the loop until you stop. STOP, REC/DUB or REC on another track keeps the last complete pass. First track needs FIXED."
        >
          RETAKE
        </button>

        {/* AUTO REC replaces the first-track count-in with a level arm. The detector listens to the
            existing record tap and keeps a short onset look-back; later tracks still use the master
            boundary arm. Sensitivity remains editable while LISTENING so a noisy guitar chain can be
            tuned without cancelling the arm. */}
        <div class="transport__auto" role="group" aria-label="Automatic record start">
          <button
            class="transport__tgl"
            classList={{ 'is-on': looper.autoRecordEnabled() }}
            aria-label={looper.autoRecordEnabled() ? 'Auto record on' : 'Auto record off'}
            aria-pressed={looper.autoRecordEnabled()}
            disabled={clock.bpmLocked() || anyCapturing()}
            onClick={() => looper.setAutoRecordEnabled(!looper.autoRecordEnabled())}
            title="Wait for input instead of counting in on the first track"
          >
            AUTO <span class="transport__auto-value">{looper.autoRecordSensitivity()}</span>
          </button>
          <input
            class="lf-range transport__volume transport__volume--auto"
            classList={{ 'is-inactive': !looper.autoRecordEnabled() }}
            type="range"
            min="1"
            max="100"
            step="1"
            value={looper.autoRecordSensitivity()}
            style={{ '--fill': `${looper.autoRecordSensitivity()}%` }}
            disabled={!looper.autoRecordEnabled() || clock.bpmLocked()}
            aria-hidden={!looper.autoRecordEnabled()}
            aria-label="Auto record sensitivity"
            aria-valuetext={`${looper.autoRecordSensitivity()} of 100`}
            title="Higher starts recording from quieter input"
            onInput={(e) => looper.setAutoRecordSensitivity(Number((e.target as HTMLInputElement).value))}
          />
        </div>

        {/* TAP — dead once a loop fixes the tempo (clock.tap -> setBpm no-ops while locked). */}
        <button
          class="transport__tgl"
          aria-label="Tap tempo"
          disabled={clock.bpmLocked()}
          onClick={() => clock.tap()}
          title={clock.bpmLocked() ? 'Tempo locked to the loop. Clear all to retap' : 'Tap a tempo'}
        >
          TAP
        </button>
        <button
          class="transport__tgl"
          classList={{ 'is-on': looper.loopEndStopEnabled() }}
          aria-label="Stop playing loops at loop end"
          aria-pressed={looper.loopEndStopEnabled()}
          onClick={() => looper.setLoopEndStopEnabled(!looper.loopEndStopEnabled())}
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
          <span class="transport__loop-v">
            <Show when={hasMaster()} fallback="—">
              {loopBars()} {loopBars() === 1 ? 'BAR' : 'BARS'} <small>· {loopSecs().toFixed(1)} s</small>
            </Show>
          </span>
        </div>
      </div>

      {/* Global transport: ■/▶ ALL · ✕ ALL (two-step) · MIC LIVE. */}
      <div class="transport__global">
        <button
          class="transport__tgl"
          classList={{ 'is-on': anyLive() }}
          disabled={!hasMaster()}
          onClick={() => (anyLive() ? looper.stopAll() : looper.playAll())}
          aria-label={anyStopping() ? 'Stop all tracks now' : anyLive() ? 'Stop all tracks' : 'Play all tracks'}
          title={anyStopping() ? 'Stop all now' : anyLive() ? 'Stop all' : 'Play all'}
        >
          {anyStopping() ? '■ NOW' : anyLive() ? '■ ALL' : '▶ ALL'}
        </button>
        <button
          class="transport__tgl"
          classList={{ 'is-armed': clearAll.armed() }}
          disabled={!hasMaster() && !anyLive()}
          onClick={onClearAll}
          aria-label={clearAll.armed() ? 'Clear all tracks, press again to confirm' : 'Clear all tracks'}
          title="Clear all tracks and reset loop length"
        >
          {clearAll.armed() ? '✕ SURE?' : '✕ ALL'}
        </button>
        {/* Record level — what the looper hears at recordTap (synths, plugins, an armed mic): "am I too
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
          title="Record level: what the looper hears (synths, plugins, an armed mic); with AUTO on, the cyan tick is the trigger level"
          ref={(el) => {
            registerInputMeter(el);
            onCleanup(unregisterInputMeter);
            createEffect(() => {
              el.classList.toggle('has-thr', looper.autoRecordEnabled());
              el.style.setProperty('--thr', meterFrac(autoRecordThreshold(looper.autoRecordSensitivity())).toFixed(3));
            });
          }}
        />
        <button
          class="transport__tgl"
          classList={{ 'is-on-green': looper.inputArmed() }}
          onClick={onToggleMic}
          aria-label={looper.inputArmed() ? 'Mic live, disarm input' : 'Arm mic / line input'}
          aria-pressed={looper.inputArmed()}
          title="Synths are always recorded; this arms mic / line input"
        >
          {looper.inputArmed() ? '● MIC LIVE' : 'MIC'}
        </button>
      </div>

      {/* flexible spacer — pushes master + the app.tsx tool icons to the far right */}
      <div class="transport__grow" />

      {/* Master output level — on engine.masterGain (post record-tap, pre limiter). Mute + slider +
          value; both go through the `master` module so the DEV single-clock gate stays consistent. */}
      <div class="transport__master" role="group" aria-label="Master volume">
        <button
          class="transport__master-mute"
          classList={{ 'is-muted': master.muted() }}
          aria-label={master.muted() ? 'Unmute master' : 'Mute master'}
          aria-pressed={master.muted()}
          onClick={() => master.setMuted(!master.muted())}
          title={master.muted() ? 'Unmute' : 'Mute master output'}
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
