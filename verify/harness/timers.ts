/**
 * Virtual `setTimeout`/`setInterval` on the rig's clock. Time moves only when the rig advances the
 * audio context, so a drain tick, a pulse waker or an overdub swap fires at a known audio time, and a
 * stall (the main thread blocked while audio keeps rendering) is one call. A late interval fires once
 * and resumes its cadence from the late fire, like a browser timer after a blocked main thread.
 */

interface VirtualTimer {
  id: number;
  due: number;
  interval: number | null;
  fn: () => void;
  seq: number;
  /** The registering call site, e.g. `scheduleOverdubSwap (…/playback.ts:170:17)`. */
  origin: string;
}

export const realSetImmediate = globalThis.setImmediate;

/** Resolve after every queued microtask (and their follow-ups) has run. */
export function flushMicrotasks(): Promise<void> {
  return new Promise((resolve) => realSetImmediate(resolve));
}

export class VirtualTimers {
  nowMs = 0;
  /** Every registration since the last reset, for churn assertions. */
  created = 0;
  cleared = 0;
  private readonly live = new Map<number, VirtualTimer>();
  private nextId = 1;
  private seq = 0;

  reset(nowMs: number): void {
    this.live.clear();
    this.nowMs = nowMs;
    this.created = 0;
    this.cleared = 0;
  }

  /** Pending timers whose registering call site matches `origin` (e.g. /scheduleOverdubSwap/). */
  pending(origin?: RegExp): number {
    let n = 0;
    for (const t of this.live.values()) if (!origin || origin.test(t.origin)) n++;
    return n;
  }

  add(fn: () => void, ms: number | undefined, interval: boolean): number {
    const delay = Math.max(0, Number(ms) || 0);
    const id = this.nextId++;
    const origin = (new Error().stack ?? '').split('\n').slice(1).find((line) => !line.includes('/harness/timers.ts')) ?? '';
    this.live.set(id, {
      id,
      due: this.nowMs + delay,
      interval: interval ? Math.max(1, delay) : null,
      fn,
      seq: this.seq++,
      origin: origin.trim(),
    });
    this.created++;
    return id;
  }

  clear(id: unknown): void {
    if (typeof id === 'number' && this.live.delete(id)) this.cleared++;
  }

  /** Fire every timer due at or before `untilMs`, earliest first. Returns how many fired. */
  fire(untilMs: number): number {
    let fired = 0;
    for (;;) {
      let next: VirtualTimer | undefined;
      for (const t of this.live.values()) {
        if (t.due > untilMs) continue;
        if (!next || t.due < next.due || (t.due === next.due && t.seq < next.seq)) next = t;
      }
      if (!next) break;
      this.nowMs = Math.max(this.nowMs, next.due);
      if (next.interval === null) this.live.delete(next.id);
      else {
        const cadence = next.due + next.interval;
        next.due = cadence > untilMs ? cadence : untilMs + next.interval;
        next.seq = this.seq++;
      }
      next.fn();
      fired++;
    }
    this.nowMs = Math.max(this.nowMs, untilMs);
    return fired;
  }

  install(): void {
    const g = globalThis as unknown as Record<string, unknown>;
    g.setTimeout = (fn: () => void, ms?: number) => this.add(fn, ms, false);
    g.setInterval = (fn: () => void, ms?: number) => this.add(fn, ms, true);
    g.clearTimeout = (id: unknown) => this.clear(id);
    g.clearInterval = (id: unknown) => this.clear(id);
  }
}
