import { Show, createEffect, createSignal, onCleanup, onMount } from 'solid-js';
import { activeIsDrum, availablePlugins, nativeHostReady, scanForPlugins, scanning } from './audio/instrument';
import * as midi from './audio/midi';
import { autosave } from './audio/autosave';
import { master } from './audio/master';
import { Keyboard } from './ui/keyboard/Keyboard';
import { Looper } from './ui/looper/Looper';
import { Transport } from './ui/transport/Transport';
import { SessionTools } from './ui/transport/SessionTools';
import { InstrumentSlot } from './ui/instrument/InstrumentSlot';
import { AudioSettings } from './ui/settings/AudioSettings';
import { Help } from './ui/settings/Help';
import { SplitStack, type StackPanel } from './ui/layout/SplitStack';
import { Toasts } from './ui/toast/Toasts';
import * as layoutStore from './ui/layout/layout-store';
import { installFrontendLogPipe, platform } from './platform';
import { bootPluginHost } from './app/boot';
import { installCloseGuard } from './app/close-guard';
import { installCmdFit } from './app/cmd-fit';
import { installTransportKeys, type TransportKeys } from './app/transport-keys';
import { installLfDebug } from './debug/lf';

export function App() {
  const [crossOriginIsolated, setCrossOriginIsolated] = createSignal(false);
  const [settingsOpen, setSettingsOpen] = createSignal(false);
  const [helpOpen, setHelpOpen] = createSignal(false);
  // The two command-bar popovers share one anchor (top-right), so opening one closes the other — they can
  // never overlap, and the open caps stay mutually exclusive.
  const openSettings = () => {
    setHelpOpen(false);
    setSettingsOpen((v) => !v);
  };
  const openHelp = () => {
    setSettingsOpen(false);
    setHelpOpen((v) => !v);
  };

  // A11y: manage focus for the two command-bar popovers. On open, focus moves into the panel (so a
  // keyboard/SR user lands on the content, not stranded on the trigger); on a KEYBOARD close, focus
  // returns to the trigger that opened it (no lost focus after Escape). These are non-modal popovers
  // — the play path stays live behind them — so role="dialog" + focus return, not a trap. The return
  // goes through transportKeys.returnFocus so Space/Enter on the returned trigger still drive the
  // looper (the transport yield rule lives at the handler in transport-keys.ts).
  let helpBtn: HTMLButtonElement | undefined;
  let gearBtn: HTMLButtonElement | undefined;
  // Pointer-vs-keyboard provenance of the last input lives in the transport-keys handler (installed
  // in onMount): a pointer-driven close must not return focus to the trigger (see TransportKeys).
  // Read (never written) here, so no signal churn.
  let transportKeys: TransportKeys | undefined;
  const lastInputWasPointer = () => transportKeys?.lastInputWasPointer() ?? false;
  const focusPanel = (el: HTMLDivElement) => queueMicrotask(() => el.focus());
  createEffect<boolean>((prevOpen) => {
    const open = helpOpen();
    if (prevOpen && !open && !lastInputWasPointer()) transportKeys?.returnFocus(helpBtn);
    return open;
  }, false);
  createEffect<boolean>((prevOpen) => {
    const open = settingsOpen();
    if (prevOpen && !open && !lastInputWasPointer()) transportKeys?.returnFocus(gearBtn);
    return open;
  }, false);

  onMount(() => {
    // First: pipe console.error + uncaught errors into the native log, so even a plugin-host init
    // failure below is captured in a release build (no visible WebView2 console otherwise).
    installFrontendLogPipe();
    const stopAutosave = autosave.start();
    onCleanup(stopAutosave);
    setCrossOriginIsolated(self.crossOriginIsolated === true);
    // Keyboard transport (the named actions of `src/app/actions.ts`, plus 1–5) + the Escape popover
    // close + the pointer-blur discipline: `src/app/transport-keys.ts`. Window-level, so it never
    // depends on what is focused or mounted.
    transportKeys = installTransportKeys({
      onEscape: () => {
        if (settingsOpen()) setSettingsOpen(false);
        if (helpOpen()) setHelpOpen(false);
      },
    });
    onCleanup(transportKeys.dispose);
    // Close guard + local recovery (native confirm / web beforeunload): `src/app/close-guard.ts`.
    onCleanup(installCloseGuard());
    // Sync masterGain to the persisted master volume (no-op at unity default; restores a saved level
    // on reload). Creates the AudioContext suspended — matches the engine's lazy pattern.
    master.init();
    // Attempt MIDI on mount — graceful if unavailable.
    void midi.start();
    // Native plugin host boot chain (no-op in the browser build): `src/app/boot.ts`.
    onCleanup(bootPluginHost());

    if (import.meta.env.DEV) {
      // Debug surface for automated (Playwright) verification + by-ear/by-eye gates: `src/debug/lf.ts`.
      // The popover drivers are supplied here because their signals are component-local.
      installLfDebug({
        openSettings: () => setSettingsOpen(true),
        closeSettings: () => setSettingsOpen(false),
        openHelp: () => setHelpOpen(true),
        closeHelp: () => setHelpOpen(false),
      });
      // Env-triggered native probes (DEV, PC only): `src/debug/restart-survey.ts`,
      // `src/debug/editor-smoke.ts`, `src/debug/swap-stress.ts`.
      if (import.meta.env.VITE_LF_PROBE === 'restart-survey') {
        void import('./debug/restart-survey').then((m) => m.runRestartSurvey());
      } else if (import.meta.env.VITE_LF_PROBE === 'editor-smoke') {
        void import('./debug/editor-smoke').then((m) => m.runEditorSmoke());
      } else if (import.meta.env.VITE_LF_PROBE === 'swap-stress') {
        void import('./debug/swap-stress').then((m) => m.runSwapStress());
      }
    }
  });

  // Drum-aware chrome: when the active slot's synth is the GM drum kit (and not overridden by a
  // loaded plugin, which always plays chromatically), the keyboard IS a pad grid — so the command-bar
  // toggle + the keyboard pane label say "drums" instead of "keyboard". `activeIsDrum` is the ONE shared
  // predicate (audio/instrument.ts); Keyboard.tsx's drumActive memo reads the same accessor.
  const keyboardNoun = () => (activeIsDrum() ? 'drums' : 'keyboard');

  // ----- stage regions -----
  // Stable descriptors created once: reordering the keyboard around the looper MOVES the DOM nodes
  // (SplitStack's <For> keys by descriptor reference) rather than remounting them, so the looper's RAF
  // canvas loop + capture state survive every layout change. `render` is a function so a region that's
  // removed (keyboard hidden) tears down cleanly (releases its key listeners), and re-mounts fresh.

  // The two instrument slots are compact cards side by side; compact rows don't need width resize, so
  // params expand DOWN as an accordion instead. A drawer that outgrows the instrument region scrolls
  // INSIDE its card (under the pinned header row), so the card's decorative top edge stays anchored and
  // the looper never moves (see .src in app.css). Each InstrumentSlot owns its per-slot `paramsOpen`
  // disclosure (state local to the instance).
  const renderInstrument = () => (
    <section class="zone zone--instrument" aria-label="Instrument">
      <div class="src">
        <InstrumentSlot slot={0} />
        <InstrumentSlot slot={1} />
      </div>
    </section>
  );

  const renderKeyboard = () => (
    <section class="zone zone--keyboard" aria-label={activeIsDrum() ? 'Drums' : 'Keyboard'}>
      <Keyboard
        placement={layoutStore.keyboardPlacement()}
        onMove={layoutStore.moveKeyboard}
        onHide={() => layoutStore.setKeyboardPlacement('hidden')}
      />
    </section>
  );

  const renderLooper = () => (
    // The looper zone has no header (title + Transport) — the command bar owns transport, and the
    // lanes ARE the zone. The section keeps its accessible name via aria-label (no dangling
    // labelledby). Only header chrome is absent; the pane/SplitStack structure is untouched, so the
    // looper's RAF canvases + capture state do NOT remount.
    <section class="zone zone--looper" aria-label="Looper">
      <div class="zone__body">
        <Looper />
      </div>
    </section>
  );

  // The instrument source row is a compact ~56px band, so it hugs its content instead of holding an
  // fr weight — `autoSize` makes its stage track `auto` (content height) and drops it out of the weight
  // math entirely, killing the dead band that a fixed fr weight left between the source row and lane 1. It
  // has no draggable divider (nothing to resize); the keyboard↔looper pair below owns the resizable area.
  // defaultWeight (the looper-hero baseline) still drives the keyboard/looper first-run seed + divider reset.
  const instrumentPanel: StackPanel = { id: 'instrument', label: 'instrument', render: renderInstrument, autoSize: true };
  // The keyboard is one horizontal ribbon. Five compact lanes need 5 × 57px + four 6px gaps + the
  // looper's 1px top padding.
  const keyboardPanel: StackPanel = { id: 'keyboard', label: 'keyboard', render: renderKeyboard, min: 0.1, minPx: 56, defaultWeight: layoutStore.DEFAULT_STAGE_WEIGHTS.keyboard };
  const looperPanel: StackPanel = { id: 'looper', label: 'looper', render: renderLooper, min: 0.18, minPx: 310, defaultWeight: layoutStore.DEFAULT_STAGE_WEIGHTS.looper };

  // The vertical stage stack. The keyboard slots in above the looper, below it, or not at all.
  const stagePanels = (): StackPanel[] => {
    switch (layoutStore.keyboardPlacement()) {
      case 'top':
        return [instrumentPanel, keyboardPanel, looperPanel];
      case 'bottom':
        return [instrumentPanel, looperPanel, keyboardPanel];
      default:
        return [instrumentPanel, looperPanel];
    }
  };

  // System-status aggregate for the command-bar lamp. Per-item detail (host/isolated/plugin/midi) lives
  // in the Audio Settings diagnostics block; this lamp is the at-a-glance rollup. Amber when
  // crossOriginIsolated is false — the one condition that actually breaks the looper (no SharedArrayBuffer
  // capture ring); the title lists all four states so the detail is a hover away in every build.
  const systemWarn = () => !crossOriginIsolated();
  const systemStatusTitle = () => {
    const pluginState = platform.pluginHost.available
      ? scanning()
        ? 'scanning…'
        : `${availablePlugins().length} found`
      : 'unavailable';
    const midiStatus = midi.midiStatus() === 'connected' ? midi.midiDevices().join(', ') : midi.midiStatus();
    return [
      `host: ${platform.kind}`,
      `isolated: ${crossOriginIsolated() ? 'yes' : 'no'}`,
      `plugin: ${pluginState}`,
      `midi: ${midiStatus}`,
    ].join('\n');
  };

  return (
    <div class="app">
      {/* Command bar — ONE card combining brand, system lamp, transport, and tool icons. Left→right:
          brand · system lamp · (Transport fragment: BPM · CLICK/FIXED/TAP · loop ring-dial · ■/✕ ALL ·
          MIC · spacer · master) · tool icons. Host/isolated/plugin/midi detail lives in the Audio
          Settings diagnostics block; the lamp is their at-a-glance aggregate and its title lists all
          four. */}
      <header class="cmd" ref={(el) => onCleanup(installCmdFit(el))}>
        <span class="brand">
          <i class="brand__dot" aria-hidden="true" />
          <span>
            BLEEP<b>LOOP</b>
          </span>
        </span>
        <span
          class="cmd__lamp"
          classList={{ 'cmd__lamp--warn': systemWarn() }}
          role="img"
          title={systemStatusTitle()}
          aria-label={`System status, ${systemStatusTitle().replace(/\n/g, ', ')}`}
        />
        {/* Preserve the plugin-scan live region. Kept always mounted in the native build so a rescan
            completion is still announced with the settings popover closed; the visible read-out lives
            in the Audio Settings diagnostics block. */}
        <Show when={platform.pluginHost.available}>
          <span class="cmd__sr" role="status" aria-live="polite">
            {scanning() ? 'Scanning plugins' : `${availablePlugins().length} plugins found`}
          </span>
        </Show>

        <Transport />

        <div class="tools">
          <SessionTools />
          {/* Keyboard show/hide — always available (the keyboard exists in every build), so this is the
              restore affordance when the on-screen keyboard is hidden. Engaged = visible. */}
          <button
            type="button"
            class="tool tool--kbd"
            classList={{ 'tool--on': layoutStore.keyboardVisible() }}
            aria-label={layoutStore.keyboardVisible() ? `Hide ${keyboardNoun()}` : `Show ${keyboardNoun()}`}
            aria-pressed={layoutStore.keyboardVisible()}
            title={layoutStore.keyboardVisible() ? `Hide ${keyboardNoun()}` : `Show ${keyboardNoun()}`}
            onClick={layoutStore.toggleKeyboardHidden}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
              <rect x="3" y="6" width="18" height="12" rx="1.5" />
              <path d="M8 6v8M12 6v8M16 6v8" stroke-linecap="round" />
            </svg>
          </button>
          {/* Rescan — re-runs the native scan so a plugin installed after startup shows up without a
              restart. Hidden in the web build (no host); disabled + spinning while a scan runs
              (aria-busy reflects that; the plugin chip's live-region announces completion). */}
          <Show when={platform.pluginHost.available}>
            <button
              type="button"
              class="tool tool--rescan"
              classList={{ scanning: scanning() }}
              aria-label="Rescan plugins"
              title="Rescan plugins"
              aria-busy={scanning()}
              disabled={scanning() || !nativeHostReady()}
              onClick={() => void scanForPlugins({ force: true })}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
                <path d="M21 12a9 9 0 1 1-2.64-6.36" stroke-linecap="round" />
                <path d="M21 4v5h-5" stroke-linecap="round" stroke-linejoin="round" />
              </svg>
            </button>
          </Show>
          {/* Help / shortcuts — universal (the keyboard play map applies in every build), so unlike the
              native-only rescan/gear it isn't gated on the plugin host. Engaged = panel open. */}
          <button
            type="button"
            class="tool tool--help"
            classList={{ 'tool--on': helpOpen() }}
            aria-label="Keyboard & layout help"
            aria-expanded={helpOpen()}
            aria-controls="lf-help-popover"
            ref={helpBtn}
            title="Keyboard & layout help"
            onClick={openHelp}
          >
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
              <circle cx="12" cy="12" r="9" />
              <path d="M9.2 9.3a2.8 2.8 0 0 1 5.4 1c0 1.9-2.8 2.5-2.8 4" stroke-linecap="round" />
              <circle cx="12" cy="17.4" r="0.9" fill="currentColor" stroke="none" />
            </svg>
          </button>
          {/* Audio settings gear: opens the global device/buffer-size popover. Native-only. */}
          <Show when={platform.pluginHost.available}>
            <button
              type="button"
              class="tool tool--gear"
              classList={{ 'tool--on': settingsOpen() }}
              aria-label="Audio settings"
              aria-expanded={settingsOpen()}
              aria-controls="lf-audio-popover"
              ref={gearBtn}
              title="Audio settings"
              onClick={openSettings}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" aria-hidden="true">
                <circle cx="12" cy="12" r="3.2" />
                <path d="M19.4 13a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09a1.65 1.65 0 0 0-1-1.51 1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09a1.65 1.65 0 0 0 1.51-1 1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z" />
              </svg>
            </button>
          </Show>
        </div>
      </header>

      {/* Audio settings popover: a full-screen transparent backdrop click-catcher (closes on
          outside click; Escape handled at the window level above) anchoring the panel top-right under
          the command-bar gear. The keyboard/MIDI play path still works behind it (keydown isn't blocked). */}
      <Show when={settingsOpen()}>
        <div class="settings-popover__backdrop" onClick={() => setSettingsOpen(false)}>
          <div
            class="settings-popover"
            id="lf-audio-popover"
            role="dialog"
            aria-label="Audio settings"
            tabindex={-1}
            ref={focusPanel}
            onClick={(e) => e.stopPropagation()}
          >
            <AudioSettings />
          </div>
        </div>
      </Show>

      {/* Help / shortcuts popover — same shell + anchor as the settings popover (one is open at a time). */}
      <Show when={helpOpen()}>
        <div class="settings-popover__backdrop" onClick={() => setHelpOpen(false)}>
          <div
            class="settings-popover"
            id="lf-help-popover"
            role="dialog"
            aria-label="Keyboard & layout help"
            tabindex={-1}
            ref={focusPanel}
            onClick={(e) => e.stopPropagation()}
          >
            <Help />
          </div>
        </div>
      </Show>

      {/* Error-toast surface: the ONE user-visible channel for failures that otherwise
          reach only console.error (invisible in a release WebView2 build). Rendered once, above the
          popovers; pointer-transparent so the play path stays live behind it. */}
      <Toasts />

      {/* The stage is a vertical SplitStack of resizable regions; the keyboard slots in above the looper,
          below it, or is hidden (restore from the command-bar piano cap). Each region resizes by dragging the
          divider between it and its neighbour. */}
      <main class="stage">
        <SplitStack
          orientation="vertical"
          panels={stagePanels()}
          sizes={layoutStore.stageSizes}
          onSizes={layoutStore.setStageSizes}
        />
      </main>
    </div>
  );
}
