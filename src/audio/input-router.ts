import { engine } from './engine';
import type { SynthEngine } from './synths/synth';
import { clampMidi, type NoteEvent } from './types';

/**
 * Live notes are scheduled at `ctx.currentTime + SCHEDULE_AHEAD` rather than letting Tone fall back
 * to `context.now()` (= currentTime + the 100 ms transport lookAhead), which added ~110 ms of audible
 * latency to every keypress. The small 5 ms guard keeps the time just in the future so the envelope
 * never lands in the past (which would click). The transport's own lookAhead is untouched, so
 * metronome + loop playback keep their safe scheduling buffer.
 */
const SCHEDULE_AHEAD = 0.005;

/**
 * A sink that routes notes to a native plugin slot. When set on the router it takes
 * precedence over the synth engine — notes go to the plugin instead. `velocity` is the
 * CLAP-normalised 0..1 form. The plugin path is fire-and-forget (the native RT thread applies the
 * note at its next block), so there is no `time` argument — unlike the synth's Web-Audio scheduling.
 *
 * This is an override layer: the slot-type model (a slot holds a plugin OR a synth, chosen in the
 * picker UI) supersedes it once that UI lands. For now it lets the keyboard drive a loaded plugin.
 */
export interface PluginNoteSink {
  noteOn(note: number, velocity: number): void;
  noteOff(note: number): void;
  /** The wheels, for a sink that plays them (the native engine's, `instrument.ts`); a plugin slot's
   * web sink omits both. Seeded when the sink takes over, like a synth engine. */
  setPitchBend?(semitones: number): void;
  setModulation?(depth: number): void;
}

/**
 * Central note router. All input sources (on-screen keyboard, computer keyboard, MIDI, and
 * later the VST slot) emit NoteEvents here; the router dispatches them to the active sink
 * (a built-in synth engine, or a native plugin via PluginNoteSink when one is set). Tracks
 * currently-held notes for diagnostics + panic.
 */
class InputRouter {
  private active: SynthEngine | null = null;
  /** When set, notes route here (a native plugin slot) instead of the synth engine — see PluginNoteSink. */
  private activePlugin: PluginNoteSink | null = null;
  private readonly heldBySource = new Map<number, Set<string>>();
  /** Pedals and deferred releases belong to the physical port/channel that produced them. */
  private readonly pedals = new Set<string>();
  private readonly sustained = new Map<number, Set<string>>();
  private readonly bends = new Map<string, number>();
  private readonly modulation = new Map<string, number>();
  private readonly heldListeners = new Set<(held: ReadonlySet<number>) => void>();

  /** Current MIDI performance controllers, retained so a synth swap inherits the live wheel state. */
  private pitchBend = 0;
  private modDepth = 0;

  setActiveEngine(engine: SynthEngine | null): void {
    if (this.active === engine) return; // idempotent — ensureActive calls this on every keypress
    if (this.active) this.allNotesOff();
    this.active = engine;
    // Seed the new engine with the live wheel state (no-op on voices that can't bend/vibrato).
    if (engine && !this.activePlugin) {
      engine.setPitchBend?.(this.pitchBend);
      engine.setModulation?.(this.modDepth);
    }
  }

  /**
   * Route notes to a native plugin slot instead of the synth engine. `null` reverts to the
   * synth. Flushes everything currently held (on whichever sink is active) before switching, so no
   * note hangs across the swap. While set, this overrides the synth even though `setActiveEngine`
   * keeps being called on every keypress (via `ensureActive`) — the override is the routing decision.
   */
  setActivePlugin(sink: PluginNoteSink | null): void {
    if (this.activePlugin === sink) return;
    this.allNotesOff();
    this.activePlugin = sink;
    sink?.setPitchBend?.(this.pitchBend);
    sink?.setModulation?.(this.modDepth);
  }

  get activeId(): string | null {
    return this.active?.id ?? null;
  }

  /** Note numbers currently held by any source (diagnostics / panic). */
  get held(): ReadonlySet<number> {
    return new Set(this.heldBySource.keys());
  }

  /**
   * Subscribe to changes in WHICH notes are physically held, from every source — the on-screen
   * keyboard draws its down keys from this, so a MIDI controller lights the same keys as a pointer.
   * Fires after the note has been dispatched to the sink, never before (the listener's work must not
   * sit in front of the sound). Returns the unsubscribe.
   */
  onHeldChange(listener: (held: ReadonlySet<number>) => void): () => void {
    this.heldListeners.add(listener);
    return () => this.heldListeners.delete(listener);
  }

  private emitHeld(): void {
    if (!this.heldListeners.size) return;
    const held = this.held;
    for (const listener of this.heldListeners) listener(held);
  }

  handle(ev: NoteEvent): void {
    if (!Number.isInteger(ev.note) || ev.note < 0 || ev.note > 127) return;
    const owner = ev.owner ?? ev.source;
    if (ev.type === 'on') {
      let sources = this.heldBySource.get(ev.note);
      const firstHold = !sources?.size;
      if (!sources) this.heldBySource.set(ev.note, sources = new Set());
      sources.add(owner);
      if (firstHold) {
        // Re-strike under the pedal: end the sustained voice before the new one (plugin sinks too).
        if (this.sustained.has(ev.note)) {
          if (this.activePlugin) this.activePlugin.noteOff(ev.note);
          else this.active?.noteOff(ev.note, engine.ctx.currentTime + SCHEDULE_AHEAD);
        }
        const velocity = clampMidi(ev.velocity) / 127;
        if (this.activePlugin) this.activePlugin.noteOn(ev.note, velocity);
        else this.active?.noteOn(ev.note, velocity, engine.ctx.currentTime + SCHEDULE_AHEAD);
        this.emitHeld();
      }
    } else {
      const sources = this.heldBySource.get(ev.note);
      if (!sources?.delete(owner)) return;
      if (!sources.size) this.heldBySource.delete(ev.note);
      // Host-side sustain: the deferral applies to plugin sinks too (bend/mod stay built-in only).
      if (this.pedals.has(owner) || this.pedals.has('global')) {
        let owners = this.sustained.get(ev.note);
        if (!owners) this.sustained.set(ev.note, owners = new Set());
        owners.add(this.pedals.has(owner) ? owner : 'global');
      }
      this.releaseIfUnheld(ev.note);
      if (!sources.size) this.emitHeld();
    }
  }

  private releaseIfUnheld(note: number): void {
    if (this.heldBySource.has(note) || this.sustained.has(note)) return;
    if (this.activePlugin) this.activePlugin.noteOff(note);
    else this.active?.noteOff(note, engine.ctx.currentTime + SCHEDULE_AHEAD);
  }

  /** MIDI CC64 is scoped to its port/channel; omitted owner retains the debug global pedal. */
  setSustain(on: boolean, owner = 'global'): void {
    if (on) { this.pedals.add(owner); return; }
    this.pedals.delete(owner);
    for (const [note, owners] of this.sustained) {
      if (!owners.delete(owner)) continue;
      if (!owners.size) this.sustained.delete(note);
      this.releaseIfUnheld(note);
    }
  }

  /** CC123 releases only this owner's physical keys and respects its pedal. Disconnect also resets it. */
  releaseSource(owner: string, disconnected = false): void {
    if (disconnected) this.setSustain(false, owner);
    for (const [note, sources] of this.heldBySource) {
      if (sources.has(owner)) this.handle({ type: 'off', note, velocity: 0, source: 'midi', owner });
    }
    if (disconnected) {
      this.bends.delete(owner);
      this.modulation.delete(owner);
      this.applyControllers();
    }
  }

  /** Last moved wheel wins across owners; unplugging it restores the surviving owner's setting. */
  setPitchBend(semitones: number, owner = 'global'): void {
    this.bends.delete(owner); this.bends.set(owner, semitones);
    this.applyControllers();
  }

  setModulation(depth: number, owner = 'global'): void {
    this.modulation.delete(owner); this.modulation.set(owner, depth);
    this.applyControllers();
  }

  /** This owner's wheel stops counting (MIDI learn took its CC1 over), as an unplug does: the surviving
   * owner's setting applies. */
  dropModulation(owner: string): void {
    if (this.modulation.delete(owner)) this.applyControllers();
  }

  private applyControllers(): void {
    this.pitchBend = [...this.bends.values()].at(-1) ?? 0;
    this.modDepth = [...this.modulation.values()].at(-1) ?? 0;
    const target = this.activePlugin ?? this.active;
    target?.setPitchBend?.(this.pitchBend);
    target?.setModulation?.(this.modDepth);
  }

  /** Sink swaps release notes but preserve connected physical controller state. */
  allNotesOff(): void {
    this.active?.allNotesOff();
    // Release every plugin-held note individually (CLAP has no standard all-notes-off event).
    // Sustained-but-released notes still sound on the plugin, so they are released too.
    if (this.activePlugin) {
      for (const note of this.heldBySource.keys()) this.activePlugin.noteOff(note);
      for (const note of this.sustained.keys()) {
        if (!this.heldBySource.has(note)) this.activePlugin.noteOff(note);
      }
    }
    const hadHeld = this.heldBySource.size > 0;
    this.heldBySource.clear();
    this.sustained.clear();
    if (hadHeld) this.emitHeld();
  }
}

export const inputRouter = new InputRouter();
