import { activeIsDrum } from '../audio/instrument';
import { looper } from '../audio/looper/looper';
import { DRUM_KIT } from '../audio/synths/drum';
import * as layoutStore from '../ui/layout/layout-store';

export interface TransportKeysOptions {
  /** Escape was pressed — the app closes whichever popover is open. Never a play key. */
  onEscape: () => void;
}

export interface TransportKeys {
  dispose: () => void;
  /**
   * Was the last user input a POINTER? A pointer-driven popover close (trigger click or click-away)
   * must not leave the trigger focused (a focused button reads as "operated" to nothing, and a pointer
   * user expects focus to stay where they clicked). Keyboard closes KEEP the return, through
   * `returnFocus` so the transport still owns Space/Enter there. Read by app.tsx's popover focus effects.
   */
  lastInputWasPointer: () => boolean;
  /**
   * Focus `el` on the APP's behalf (the popover trigger after a keyboard close). The transport keys
   * do not yield to it: the user did not reach it to operate it, so Space after Escape arms REC
   * instead of re-opening the popover. Any later focus move (Tab, click) ends the exemption.
   */
  returnFocus: (el: HTMLElement | undefined) => void;
}

/**
 * Keyboard looper transport: an always-on WINDOW handler so transport survives with the keyboard pane
 * hidden (the footswitch/numpad-as-looper-pedal case). Disjoint from the note-play keys (a–l, w–p,
 * z/x) and from drum pads (see the drumActive gate) — the note path is untouched. Also owns the
 * capture-phase click handler that blurs pointer-activated controls so the transport keys never get
 * hijacked by a focused button.
 */
export function installTransportKeys(opts: TransportKeysOptions): TransportKeys {
  let lastInputWasPointer = false;
  // The element the app itself last focused via returnFocus(); cleared by any other focus move.
  let appFocused: Element | null = null;

  const onKeyDown = (e: KeyboardEvent) => {
    lastInputWasPointer = false;
    if (e.key === 'Escape') {
      opts.onEscape();
      return;
    }

    if (e.repeat || e.metaKey || e.ctrlKey || e.altKey) return;

    // Yield rule — the transport gives the key up only to a control the USER is operating:
    // - editable fields (input/select/textarea/contenteditable) always keep their keys;
    // - buttons and tabindex>=0 widgets (SplitStack divider = role=separator, which binds
    //   Enter/Arrows/Home) keep them, unless the app put focus there itself (returnFocus: the
    //   popover trigger after Escape), so Space after Escape arms REC instead of re-opening;
    // - tabindex=-1 elements (the Help/Settings panels, focused on open for screen readers) are
    //   focus targets, not controls, so the play path stays live behind an open popover.
    const el = document.activeElement as HTMLElement | null;
    if (el && el !== document.body) {
      const tag = el.tagName;
      if (tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA' || el.isContentEditable) return;
      if (el !== appFocused && (tag === 'BUTTON' || (el.hasAttribute('tabindex') && el.tabIndex >= 0))) {
        return;
      }
    }

    // Space = REC/DUB, Enter = PLAY/STOP on the selected track. preventDefault stops Space page-scroll.
    if (e.key === ' ' || e.code === 'Space') {
      e.preventDefault();
      looper.recDubSelected();
      return;
    }
    if (e.key === 'Enter') {
      e.preventDefault();
      looper.playStopSelected();
      return;
    }

    // Digits 1–5 select a track. Collision precedence: in drum mode the pad grid owns 1–4
    // (Crash/Ride/Cowbell/Tamb, DRUM_KIT) — note-play wins — so digit-select yields those; '5' is
    // never a pad key and always selects. In piano mode all of 1–5 select. (activeIsDrum() is the
    // shared predicate from audio/instrument.ts, the same one Keyboard.tsx's drumActive reads.) The
    // yield only applies while the keyboard pane is VISIBLE: the pad key handler lives in Keyboard.tsx
    // and unmounts with the pane, so with it hidden the digits would otherwise go entirely dead in
    // drum mode.
    if (e.key >= '1' && e.key <= '5') {
      const idx = Number(e.key) - 1;
      if (activeIsDrum() && layoutStore.keyboardVisible() && DRUM_KIT.some((pad) => pad.key === e.key)) return;
      e.preventDefault();
      looper.selectTrack(idx);
    }
  };
  window.addEventListener('keydown', onKeyDown);

  // A pointer click leaves the clicked <button> focused, so the keydown yield above (correctly)
  // hands Space/Enter/digits back to it — hijacking transport until focus moves. Fix the CAUSE, not
  // the yield: after a POINTER activation only (e.detail > 0; keyboard-activated clicks report 0, so
  // Tab-focus is untouched and the yield keeps serving keyboard users), blur the activated control so
  // the window transport handler owns the keys again. Delegated at the window so it covers every
  // button (lanes, transport, tools) including ones mounted later. CAPTURE phase is load-bearing:
  // Solid delegates 'click' at document (below window in the bubble chain) and several buttons'
  // onClick call e.stopPropagation() (slot A/B, synth pills, plugin controls) — a bubble-phase window
  // listener would never see those. Capture runs top-down before any bubble handler, and e.target is
  // still the deepest clicked node, so closest() resolves. Safe for the popover triggers: the panels
  // manage their own focus (focusPanel moves INTO the panel via microtask, which runs after this
  // synchronous blur; a keyboard close re-focuses the trigger), so none rely on a button staying
  // focused after a click.
  // input[type=range] is in the selector for the same reason: a dragged slider (looper/FX/master
  // volume, plugin params) keeps focus with no visible ring (:focus-visible is pointer-suppressed),
  // so transport keys die silently. A drag ending off the thumb still lands here — a range takes
  // implicit pointer capture, so mouseup retargets to it and the click fires on the slider. Sliders
  // reached by Tab are untouched — no click, so their arrow-key control survives. The capture phase
  // also makes this the one place that records whether the last input was a pointer, for the popover
  // focus return.
  const onClick = (e: MouseEvent) => {
    lastInputWasPointer = e.detail > 0;
    // Explicit type arg: the compound selector misses TS's tag-name overload, which would infer a
    // bare Element (no .blur()).
    if (e.detail > 0) {
      (e.target as HTMLElement | null)?.closest<HTMLElement>('button, input[type="range"]')?.blur();
    }
  };
  window.addEventListener('click', onClick, { capture: true });

  const onFocusIn = (e: FocusEvent) => {
    if (e.target !== appFocused) appFocused = null;
  };
  window.addEventListener('focusin', onFocusIn, { capture: true });

  return {
    dispose: () => {
      window.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('click', onClick, { capture: true });
      window.removeEventListener('focusin', onFocusIn, { capture: true });
    },
    lastInputWasPointer: () => lastInputWasPointer,
    returnFocus: (el) => {
      if (!el) return;
      appFocused = el; // before focus(): focusin fires synchronously and must see it
      el.focus();
    },
  };
}
