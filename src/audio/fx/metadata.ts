// PURE FX metadata + serialized-state validation. No Tone, engine, looper, DOM, or Tauri imports:
// session-schema.ts and Node verification import this module at runtime.

export type FxKind = 'filter' | 'pitch' | 'stutter' | 'delay' | 'reverb';

export interface FxParamDef {
  key: string;
  label: string;
  min: number;
  max: number;
  step: number;
  default: number;
  unit?: string;
  integer?: boolean;
  /** For index-style params: human labels (value is the index into this list). */
  choices?: readonly string[];
}

export interface FxState {
  bypassed: boolean;
  params: Record<string, number>;
}

export const FX_DIVISIONS = ['4n', '8n', '8n.', '16n'] as const;
/** Shared reverb timing — live FxChain and offline export tail planning must stay identical. */
export const REVERB_DECAY_SECONDS = 2.6;
export const REVERB_PRE_DELAY_SECONDS = 0.02;
const DIVISION_LABELS = ['1/4', '1/8', '1/8.', '1/16'] as const;

const FILTER_PARAMS: readonly FxParamDef[] = [
  { key: 'cutoff', label: 'Cutoff', min: 120, max: 14000, step: 1, default: 1200, unit: 'Hz' },
  { key: 'q', label: 'Reso', min: 0.1, max: 14, step: 0.1, default: 2 },
];
const PITCH_PARAMS: readonly FxParamDef[] = [
  { key: 'semitones', label: 'Pitch', min: -12, max: 12, step: 1, default: 0, unit: 'st', integer: true },
];
const STUTTER_PARAMS: readonly FxParamDef[] = [
  { key: 'rate', label: 'Rate', min: 0, max: 3, step: 1, default: 1, integer: true, choices: DIVISION_LABELS },
];
const DELAY_PARAMS: readonly FxParamDef[] = [
  { key: 'time', label: 'Time', min: 0, max: 3, step: 1, default: 1, integer: true, choices: DIVISION_LABELS },
  { key: 'feedback', label: 'Fbk', min: 0, max: 0.95, step: 0.01, default: 0.4 },
  { key: 'mix', label: 'Mix', min: 0, max: 1, step: 0.01, default: 0.3 },
];
const REVERB_PARAMS: readonly FxParamDef[] = [
  { key: 'amount', label: 'Send', min: 0, max: 1, step: 0.01, default: 0.3 },
];

/** Param definitions by kind: the single range/key/choice contract for UI, audio, and import. */
export const FX_PARAM_DEFS: Record<FxKind, readonly FxParamDef[]> = {
  filter: FILTER_PARAMS,
  pitch: PITCH_PARAMS,
  stutter: STUTTER_PARAMS,
  delay: DELAY_PARAMS,
  reverb: REVERB_PARAMS,
};

/** The five FX in fixed serialized/chain order. */
export const FX_META: readonly { kind: FxKind; label: string }[] = [
  { kind: 'filter', label: 'Filter' },
  { kind: 'pitch', label: 'Pitch' },
  { kind: 'stutter', label: 'Stutter' },
  { kind: 'delay', label: 'Delay' },
  { kind: 'reverb', label: 'Reverb' },
];

/** Validate one serialized five-effect chain and return a deep copy with exact parameter keys. */
export function validateFxStates(rawFx: unknown, context: string): FxState[] {
  if (!Array.isArray(rawFx) || rawFx.length !== FX_META.length) {
    throw new Error(
      `${context} fx must be exactly ${FX_META.length} entries (chain order filter,pitch,stutter,delay,reverb), got ${Array.isArray(rawFx) ? rawFx.length : JSON.stringify(rawFx)}`,
    );
  }

  return rawFx.map((raw: unknown, k: number) => {
    if (typeof raw !== 'object' || raw === null || Array.isArray(raw)) {
      throw new Error(`${context} fx[${k}] is not an object`);
    }
    const entry = raw as Record<string, unknown>;
    if (typeof entry.bypassed !== 'boolean') {
      throw new Error(`${context} fx[${k}].bypassed must be a boolean, got ${JSON.stringify(entry.bypassed)}`);
    }
    if (typeof entry.params !== 'object' || entry.params === null || Array.isArray(entry.params)) {
      throw new Error(`${context} fx[${k}].params must be an object, got ${JSON.stringify(entry.params)}`);
    }

    const paramsIn = entry.params as Record<string, unknown>;
    const defs = FX_PARAM_DEFS[FX_META[k].kind];
    const expectedKeys = new Set(defs.map((def) => def.key));
    for (const def of defs) {
      if (!Object.prototype.hasOwnProperty.call(paramsIn, def.key)) {
        throw new Error(`${context} fx[${k}].params missing "${def.key}"`);
      }
    }
    for (const key of Object.keys(paramsIn)) {
      if (!expectedKeys.has(key)) {
        throw new Error(`${context} fx[${k}].params has unknown key "${key}"`);
      }
    }

    const params: Record<string, number> = {};
    for (const def of defs) {
      const value = paramsIn[def.key];
      if (typeof value !== 'number' || !Number.isFinite(value)) {
        throw new Error(`${context} fx[${k}].params.${def.key} must be a finite number, got ${JSON.stringify(value)}`);
      }
      if (value < def.min || value > def.max) {
        throw new Error(`${context} fx[${k}].params.${def.key} outside ${def.min}..${def.max} (got ${value})`);
      }
      if (def.integer && !Number.isInteger(value)) {
        throw new Error(`${context} fx[${k}].params.${def.key} must be an integer, got ${value}`);
      }
      params[def.key] = value;
    }
    return { bypassed: entry.bypassed, params };
  });
}
