/** A normalized note event from any input source (on-screen keys, computer kbd, MIDI). */
export interface NoteEvent {
  type: 'on' | 'off';
  /** MIDI note number (0..127). */
  note: number;
  /** MIDI velocity (0..127). Ignored for 'off'. */
  velocity: number;
  source: 'pointer' | 'computer' | 'midi';
  /** Stable physical owner, such as a MIDI port/channel. Defaults to source for legacy callers. */
  owner?: string;
}

function clampMidi(v: number): number {
  return v < 0 ? 0 : v > 127 ? 127 : v;
}

/**
 * Where the router's notes and wheels go: the engine's note target (`instrument.ts`). `velocity` is the
 * normalised 0..1 form. Fire-and-forget: the engine applies each note at its next block.
 */
export interface NoteSink {
  noteOn(note: number, velocity: number): void;
  noteOff(note: number): void;
  /** Seeded with the live wheel state when the sink takes over. */
  setPitchBend?(semitones: number): void;
  setModulation?(depth: number): void;
}

/**
 * OWNS: which notes are held and by whom. All input sources (on-screen keyboard, computer keyboard,
 * MIDI) emit NoteEvents here; the router dispatches them to the sink. It keeps each held note's
 * owners, the sustain pedals and the wheels per physical owner, as the engine expects (sustain and a
 * held note's owner stay here, `lf_engine::Command::NoteOn`).
 */
class InputRouter {
  private sink: NoteSink | null = null;
  private readonly heldBySource = new Map<number, Set<string>>();
  /** Pedals and deferred releases belong to the physical port/channel that produced them. */
  private readonly pedals = new Set<string>();
  private readonly sustained = new Map<number, Set<string>>();
  private readonly bends = new Map<string, number>();
  private readonly modulation = new Map<string, number>();
  private readonly heldListeners = new Set<(held: ReadonlySet<number>) => void>();

  /** Current MIDI performance controllers, retained so a new sink inherits the live wheel state. */
  private pitchBend = 0;
  private modDepth = 0;

  /**
   * Route notes to `sink` (`null` = nowhere). Flushes everything currently held on the old sink before
   * switching, so no note hangs across the swap. Idempotent: the same sink is a no-op, so calling it on
   * every keypress flushes nothing.
   */
  setSink(sink: NoteSink | null): void {
    if (this.sink === sink) return;
    this.allNotesOff();
    this.sink = sink;
    sink?.setPitchBend?.(this.pitchBend);
    sink?.setModulation?.(this.modDepth);
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
        // Re-strike under the pedal: end the sustained voice before the new one.
        if (this.sustained.has(ev.note)) this.sink?.noteOff(ev.note);
        this.sink?.noteOn(ev.note, clampMidi(ev.velocity) / 127);
        this.emitHeld();
      }
    } else {
      const sources = this.heldBySource.get(ev.note);
      if (!sources?.delete(owner)) return;
      if (!sources.size) this.heldBySource.delete(ev.note);
      // Host-side sustain: a release under a held pedal is deferred until the pedal lifts.
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
    this.sink?.noteOff(note);
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
    this.sink?.setPitchBend?.(this.pitchBend);
    this.sink?.setModulation?.(this.modDepth);
  }

  /** Sink swaps release notes but preserve connected physical controller state. */
  allNotesOff(): void {
    // Release every held note individually (there is no all-notes-off command). Sustained-but-released
    // notes still sound, so they are released too.
    if (this.sink) {
      for (const note of this.heldBySource.keys()) this.sink.noteOff(note);
      for (const note of this.sustained.keys()) {
        if (!this.heldBySource.has(note)) this.sink.noteOff(note);
      }
    }
    const hadHeld = this.heldBySource.size > 0;
    this.heldBySource.clear();
    this.sustained.clear();
    if (hadHeld) this.emitHeld();
  }
}

export const inputRouter = new InputRouter();
