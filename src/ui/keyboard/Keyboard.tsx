import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount } from 'solid-js';
import { engine } from '../../audio/engine';
import { inputRouter } from '../../audio/input-router';
import { activeIsDrum, activeSlot, ensureActive, slotIds, slotPlugins } from '../../audio/instrument';
import { DRUM_KIT } from '../../audio/synths/drum';
import type { KeyboardPlacement } from '../layout/layout-store';
import { isBlackKey, noteName, octaveBase } from './notes';
import './keyboard.css';

/** Where this keyboard currently sits + the move/hide callbacks (wired to the layout store in app.tsx).
 * Optional so the component still renders standalone. */
interface KeyboardProps {
  placement?: KeyboardPlacement;
  onMove?: () => void;
  onHide?: () => void;
}

const KEY_COUNT = 37; // three octaves + the top C

// Computer-keyboard layout (one+ octave of offsets from the base C). z/x shift octave.
export const COMPUTER_MAP: Readonly<Record<string, number>> = {
  a: 0, w: 1, s: 2, e: 3, d: 4, f: 5, t: 6, g: 7, y: 8, h: 9, u: 10, j: 11,
  k: 12, o: 13, l: 14, p: 15,
};

/**
 * Drum-pad layout. When the active slot's synth is the Drum machine, the chromatic piano is
 * useless — the drum voices only the GM percussion notes, which the default keyboard octave
 * (MIDI 60+) never reaches. So we swap in a 4×4 grid of labelled pads instead. The kit is defined
 * once in DRUM_KIT (audio/synths/drum.ts) — note, label, and pad key — and both this UI and the
 * synth's note dispatch read it, so they can't drift. The pads emit the exact same GM NoteEvents
 * an external MIDI controller would, so the MIDI path is unchanged — pads are a second producer.
 */
const PAD_VELOCITY = 110;

interface KeyLayout {
  note: number;
  black: boolean;
  leftPct: number;
  widthPct: number;
}

type KeyboardSource = 'pointer' | 'computer';

interface Hold {
  note: number;
  source: KeyboardSource;
  owner?: string;
  active: boolean;
  sounding: boolean;
}

export function Keyboard(props: KeyboardProps = {}) {
  const [baseOctave, setBaseOctave] = createSignal(4); // C4 = MIDI 60
  const [downNotes, setDownNotes] = createSignal<ReadonlySet<number>>(inputRouter.held);

  // The active slot's synth — drum gets the pad layout, everything else the piano. A slot in plugin
  // mode is always played chromatically (piano), never the GM pad grid, even if its underlying synth
  // id is still 'drum'. Memoized over the ONE shared `activeIsDrum` predicate (audio/instrument.ts),
  // which app.tsx's chrome + the digit-select yield in src/app/transport-keys.ts also read — so the
  // pad-vs-piano decision can't diverge.
  const drumActive = createMemo(() => activeIsDrum());

  const layout = createMemo<KeyLayout[]>(() => {
    const base = octaveBase(baseOctave());
    const notes = Array.from({ length: KEY_COUNT }, (_, i) => base + i);
    const whiteCount = notes.filter((n) => !isBlackKey(n)).length;
    const whiteW = 100 / whiteCount;
    const blackW = whiteW * 0.62;
    let placed = 0;
    return notes.map((note) => {
      if (!isBlackKey(note)) {
        const leftPct = placed * whiteW;
        placed += 1;
        return { note, black: false, leftPct, widthPct: whiteW };
      }
      return { note, black: true, leftPct: placed * whiteW - blackW / 2, widthPct: blackW };
    });
  });

  // The down keys mirror the ROUTER's held set, not this component's own presses, so a MIDI controller
  // lights the same keys (and GM pads) as the pointer and computer keyboard. A note outside the visible
  // octaves simply has no key to light.
  onCleanup(inputRouter.onHeldChange(setDownNotes));

  // --- shared press/release ---
  async function startHold(hold: Hold, velocity: number) {
    try {
      await engine.start();
    } catch (err) {
      hold.active = false;
      console.error('[Keyboard] audio start failed', err);
      return;
    }
    if (!hold.active) return;
    ensureActive();
    hold.sounding = true;
    inputRouter.handle({ type: 'on', note: hold.note, velocity, source: hold.source, owner: hold.owner });
  }
  function endHold(hold: Hold) {
    hold.active = false;
    if (!hold.sounding) return;
    hold.sounding = false;
    inputRouter.handle({ type: 'off', note: hold.note, velocity: 0, source: hold.source, owner: hold.owner });
  }

  // --- pointer (mouse / touch / pen) ---
  const pointerNotes = new Map<number, Hold>();
  function velocityFromPointer(e: PointerEvent, el: HTMLElement): number {
    const r = el.getBoundingClientRect();
    const rel = Math.min(1, Math.max(0, (e.clientY - r.top) / r.height));
    return Math.round(72 + rel * 55); // top of key = softer, bottom = harder
  }
  function onPointerDown(e: PointerEvent, note: number) {
    const el = e.currentTarget as HTMLElement;
    el.setPointerCapture(e.pointerId);
    const hold: Hold = { note, source: 'pointer', owner: `pointer:${e.pointerId}`, active: true, sounding: false };
    pointerNotes.set(e.pointerId, hold);
    void startHold(hold, velocityFromPointer(e, el));
  }
  // Drum pads fire at a fixed velocity (they're one-shots, not a velocity-sensitive key surface).
  function onPadPointerDown(e: PointerEvent, note: number) {
    const el = e.currentTarget as HTMLElement;
    el.setPointerCapture(e.pointerId);
    const hold: Hold = { note, source: 'pointer', owner: `pointer:${e.pointerId}`, active: true, sounding: false };
    pointerNotes.set(e.pointerId, hold);
    void startHold(hold, PAD_VELOCITY);
  }
  function onPointerUp(e: PointerEvent) {
    const hold = pointerNotes.get(e.pointerId);
    if (hold) {
      pointerNotes.delete(e.pointerId);
      endHold(hold);
    }
  }

  // --- computer keyboard ---
  const heldKeyNote = new Map<string, Hold>();
  function onKeyDown(e: KeyboardEvent) {
    if (e.repeat || e.metaKey || e.ctrlKey || e.altKey) return;
    // Yield to text entry only: typing in a settings field (device <select>, the rec-align number box)
    // must not play — and with a track armed, record — notes. Unlike the transport yield in
    // src/app/transport-keys.ts this
    // does NOT include BUTTON or tabindex widgets: a focused cap never competes for a–l/w–p/z/x.
    const el = document.activeElement as HTMLElement | null;
    if (el && el !== document.body) {
      const tag = el.tagName;
      if (tag === 'INPUT' || tag === 'SELECT' || tag === 'TEXTAREA' || el.isContentEditable) return;
    }
    const k = e.key.toLowerCase();
    // Drum mode: pad keys map straight to the GM drum voices; octave shift is meaningless.
    if (drumActive()) {
      const pad = DRUM_KIT.find((p) => p.key === k);
      if (pad === undefined || heldKeyNote.has(k)) return;
      e.preventDefault();
      const hold: Hold = { note: pad.note, source: 'computer', owner: `key:${k}`, active: true, sounding: false };
      heldKeyNote.set(k, hold);
      void startHold(hold, PAD_VELOCITY);
      return;
    }
    if (k === 'z') {
      setBaseOctave((o) => Math.max(0, o - 1));
      return;
    }
    if (k === 'x') {
      setBaseOctave((o) => Math.min(8, o + 1));
      return;
    }
    const offset = COMPUTER_MAP[k];
    if (offset === undefined || heldKeyNote.has(k)) return;
    e.preventDefault();
    const note = octaveBase(baseOctave()) + offset;
    // Owner per physical key: after an octave shift two held keys can land on the SAME note, and the
    // router must keep it sounding (and lit) until the last of them lifts.
    const hold: Hold = { note, source: 'computer', owner: `key:${k}`, active: true, sounding: false };
    heldKeyNote.set(k, hold);
    void startHold(hold, 100);
  }
  function onKeyUp(e: KeyboardEvent) {
    // No focus guard here (deliberate): a key held while focus moves into a field must still release
    // its note — the release is keyed off heldKeyNote, so it only fires for notes this handler started.
    const k = e.key.toLowerCase();
    const hold = heldKeyNote.get(k);
    if (hold) {
      heldKeyNote.delete(k);
      endHold(hold);
    }
  }
  /** Drop this keyboard's own held-note bookkeeping. No router traffic (the router owns the down-set). */
  function clearLocalHeld() {
    for (const hold of heldKeyNote.values()) hold.active = false;
    for (const hold of pointerNotes.values()) hold.active = false;
    pointerNotes.clear();
    heldKeyNote.clear();
  }
  function panic() {
    // Release with each note's ACTUAL source so the (source-aware) router only drops notes this
    // keyboard owns — a MIDI-held note must survive a window-blur panic from the on-screen keys.
    for (const hold of heldKeyNote.values()) endHold(hold);
    for (const hold of pointerNotes.values()) endHold(hold);
    clearLocalHeld();
  }

  // Flush local held state whenever the active instrument changes — a slot switch or the active
  // slot's synth changing (e.g. pad <-> piano). The swap already flushes the audio voices
  // (instrument.ts -> inputRouter.setActiveEngine -> allNotesOff), so we only need to drop this
  // keyboard's stale bookkeeping: otherwise heldKeyNote keeps the old mapping, leaving the still-held
  // computer key dead (guarded by `heldKeyNote.has(k)`) and the on-screen key visually stuck until
  // it's physically released. No router traffic here — the audio side is already clean.
  const activeInstrumentKey = createMemo(
    () => `${activeSlot()}:${slotPlugins()[activeSlot()]?.id ?? slotIds()[activeSlot()]}`,
  );
  let prevInstrumentKey = activeInstrumentKey();
  createEffect(() => {
    const key = activeInstrumentKey();
    if (key !== prevInstrumentKey) {
      prevInstrumentKey = key;
      clearLocalHeld();
    }
  });

  onMount(() => {
    window.addEventListener('keydown', onKeyDown);
    window.addEventListener('keyup', onKeyUp);
    window.addEventListener('blur', panic);
  });
  onCleanup(() => {
    window.removeEventListener('keydown', onKeyDown);
    window.removeEventListener('keyup', onKeyUp);
    window.removeEventListener('blur', panic);
    panic();
  });

  return (
    // Orbit V2 key ribbon — ONE horizontal glass bar: left meta (drum-aware label + octave −/+ + the
    // current octave) · the keys/pads tray (flex-fills, stretches vertically when the pane is dragged
    // taller) · right meta (accurate key-row hint + move/hide). Keys/pads stay POINTER-ONLY (no tab
    // stops — the tab-trap decision); the octave/move/hide buttons stay focusable.
    <div class="kb" classList={{ 'kb--drum': drumActive() }}>
      <div class="kb__meta kb__meta--left">
        {/* Pane label follows the active instrument: DRUMS when the active slot's synth is the GM
            kit, KEYBOARD otherwise. */}
        <span class="kb__title">{drumActive() ? 'Drums' : 'Keyboard'}</span>
        {/* Octave controls show only in piano mode (drums don't shift octave). */}
        <Show when={!drumActive()}>
          <div class="kb__oct">
            <button
              type="button"
              class="kb__cap"
              aria-label="Octave down"
              onClick={() => setBaseOctave((o) => Math.max(0, o - 1))}
            >
              −
            </button>
            <span class="kb__base">C{baseOctave()}</span>
            <button
              type="button"
              class="kb__cap"
              aria-label="Octave up"
              onClick={() => setBaseOctave((o) => Math.min(8, o + 1))}
            >
              +
            </button>
          </div>
        </Show>
      </div>

      <Show
        when={drumActive()}
        fallback={
          <div class="kb__keys">
            <For each={layout()}>
              {(k) => (
                <div
                  classList={{
                    kb__key: true,
                    'kb__key--white': !k.black,
                    'kb__key--black': k.black,
                    'kb__key--down': downNotes().has(k.note),
                  }}
                  style={{ left: `${k.leftPct}%`, width: `${k.widthPct}%` }}
                  data-note={k.note}
                  onPointerDown={(e) => onPointerDown(e, k.note)}
                  onPointerUp={onPointerUp}
                  onPointerCancel={onPointerUp}
                >
                  {/* Octave markers only (C keys) — a slim ribbon can't carry a legible label on every
                      key, and per-key names just add grey-on-grey clutter. */}
                  <Show when={k.note % 12 === 0}>
                    <span class="kb__label">{noteName(k.note)}</span>
                  </Show>
                </div>
              )}
            </For>
          </div>
        }
      >
        {/* Drum variant: the 4×4 GM kit as ONE 16-wide row of the same glass cells, so the ribbon height
            never jumps on a synth swap and each pad stays comfortably clickable (grows with the pane). */}
        <div class="kb__pads">
          <For each={DRUM_KIT}>
            {(pad) => (
              <div
                classList={{ kb__pad: true, 'kb__pad--down': downNotes().has(pad.note) }}
                data-note={pad.note}
                onPointerDown={(e) => onPadPointerDown(e, pad.note)}
                onPointerUp={onPointerUp}
                onPointerCancel={onPointerUp}
              >
                <span class="kb__pad-lamp" aria-hidden="true" />
                <span class="kb__pad-label">{pad.label}</span>
                <span class="kb__pad-key">{pad.key.toUpperCase()}</span>
              </div>
            )}
          </For>
        </div>
      </Show>

      <div class="kb__meta kb__meta--right">
        {/* Compact key-row hint — the ACTUAL BleepLoop map (home row = naturals, top row = sharps), not
            the mockup's placeholder "Z–M · Q–P" (which would mislead: z/x shift octave here). Drum mode
            hides it — every pad self-labels its own key. */}
        <Show when={!drumActive()}>
          <span class="kb__hint">A–L · W–P</span>
        </Show>
        {/* move (top<->bottom) + hide controls — wired to the layout store. Restore a hidden keyboard
            from the command-bar keyboard toggle. */}
        <Show when={props.onMove || props.onHide}>
          <div class="kb__actions">
            <Show when={props.onMove}>
              <button
                type="button"
                class="kb__cap kb__cap--act"
                aria-label={drumActive() ? 'Move drums' : 'Move keyboard'}
                title={props.placement === 'bottom' ? 'Move above the looper' : 'Move below the looper'}
                onClick={() => props.onMove?.()}
              >
                ⇅
              </button>
            </Show>
            <Show when={props.onHide}>
              <button
                type="button"
                class="kb__cap kb__cap--act"
                aria-label={drumActive() ? 'Hide drums' : 'Hide keyboard'}
                title={drumActive() ? 'Hide drums' : 'Hide keyboard'}
                onClick={() => props.onHide?.()}
              >
                ×
              </button>
            </Show>
          </div>
        </Show>
      </div>
    </div>
  );
}
