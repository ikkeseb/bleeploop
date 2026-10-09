/**
 * The readers the wire decoders share (`engine-wire.ts`, `midi-wire.ts`): each checks one JSON value
 * and throws, naming the field, on anything else. Pure: a Node guard imports it.
 */

export type Obj = Record<string, unknown>;

export function fail(what: string, got: unknown): never {
  throw new Error(`wire: ${what}, got ${JSON.stringify(got)}`);
}

export function obj(v: unknown, what: string): Obj {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) fail(`${what} must be an object`, v);
  return v as Obj;
}

export function num(v: unknown, what: string): number {
  if (typeof v !== 'number' || !Number.isFinite(v)) fail(`${what} must be a finite number`, v);
  return v;
}

export function int(v: unknown, what: string, min = 0, max = Number.MAX_SAFE_INTEGER): number {
  if (!Number.isSafeInteger(v) || (v as number) < min || (v as number) > max) {
    fail(`${what} must be an integer in ${min}..${max}`, v);
  }
  return v as number;
}

export function bool(v: unknown, what: string): boolean {
  if (typeof v !== 'boolean') fail(`${what} must be a boolean`, v);
  return v;
}

export function str(v: unknown, what: string): string {
  if (typeof v !== 'string') fail(`${what} must be a string`, v);
  return v;
}

export function oneOf<T extends string>(v: unknown, set: readonly T[], what: string): T {
  if (!set.includes(v as T)) fail(`${what} must be one of ${set.join('|')}`, v);
  return v as T;
}

export function array(v: unknown, what: string, length?: number): unknown[] {
  if (!Array.isArray(v) || (length !== undefined && v.length !== length)) {
    fail(`${what} must be an array${length === undefined ? '' : ` of ${length}`}`, v);
  }
  return v;
}

/** An externally tagged enum value: a unit variant's name, or a one-key object. */
export function tagged(v: unknown, what: string): [string, unknown] {
  if (typeof v === 'string') return [v, undefined];
  const o = obj(v, what);
  const keys = Object.keys(o);
  if (keys.length !== 1) fail(`${what} must carry exactly one variant`, v);
  return [keys[0], o[keys[0]]];
}

export const nullable = <T>(v: unknown, read: (v: unknown) => T): T | null => (v === null || v === undefined ? null : read(v));
