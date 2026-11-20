import { Show, type JSX } from 'solid-js';
import { createValueDrag } from './value-drag';

/**
 * A command-bar key that carries its value (the `.k-key--value` recipe): the legend over the value
 * line, the press its own (a mode's toggle, FADE's fade) and the value changed on the key itself
 * (`value-drag.ts`). While it changes, the value line is lit and the scale window under the key shows
 * the range. One accessible name, the state through aria-pressed on a latched key; the value rides in
 * the description, which says how to change it. A key whose press is refused while its value is live
 * stays enabled to assistive tech (its value is what it offers then) and its description says the press
 * is unavailable; it wears the disabled face.
 */
export interface ValueKeyProps {
  /** The accessible name: one name, never flipped with the state. */
  label: string;
  title: string;
  legend: JSX.Element;
  /** The value line (`.k-key__v`). */
  value: JSX.Element;
  /** The value in words, for the description. */
  valueText: string;
  /** A latched key: the LED over its legend, aria-pressed. */
  latched?: boolean;
  /** The mode is on (latched) or the key is engaged (FADE fading). */
  on: boolean;
  /** The press is refused (the mode cannot be switched now, FADE has nothing to fade). */
  pressDisabled: boolean;
  /** The value takes gestures now. A value live while its press is refused leaves the key focusable and
   * enabled, so the value can still be set before it is used. */
  adjustable: boolean;
  /** Why the press is refused, for the description while the value stays live (FADE: nothing plays). */
  pressRefusal?: string;
  step: (dir: 1 | -1, big: boolean) => void;
  pxPerStep: number;
  /** The scale window's content: the range, the value placed on it. */
  scale: () => JSX.Element;
  onPress: () => void;
  class?: string;
}

export function ValueKey(props: ValueKeyProps) {
  const drag = createValueDrag({ enabled: () => props.adjustable, step: (d, b) => props.step(d, b), pxPerStep: props.pxPerStep });
  // A press refused while the value stays live: focusable and enabled (the value is live), the disabled
  // face (transport.css), the refusal in the description.
  const softDisabled = () => props.pressDisabled && props.adjustable;
  const description = () =>
    [
      props.valueText,
      softDisabled() ? `${props.pressRefusal ?? 'The press is unavailable now'}, the value can still be changed` : '',
      props.adjustable ? 'Drag up or down, scroll, or use the arrow keys to change it' : '',
    ]
      .filter(Boolean)
      .join('. ');
  return (
    <button
      ref={drag.ref}
      class={`k-key k-key--value transport__key transport__vkey ${props.latched ? 'k-key--latch' : ''} ${props.class ?? ''}`}
      classList={{ 'is-on': props.on, 'is-adjusting': drag.adjusting(), 'is-soft-off': softDisabled() }}
      aria-label={props.label}
      aria-pressed={props.latched ? props.on : undefined}
      aria-description={description()}
      disabled={props.pressDisabled && !props.adjustable}
      title={props.title}
      {...drag.handlers}
      onClick={() => {
        if (drag.takeDragClick() || props.pressDisabled) return;
        props.onPress();
      }}
    >
      <Show when={props.latched}>
        <span class="k-led" aria-hidden="true" />
      </Show>
      <span class="k-key__l">{props.legend}</span>
      <span class="k-key__v">{props.value}</span>
      <Show when={drag.adjusting()}>
        <span class="transport__scale k-win" aria-hidden="true">
          {props.scale()}
        </span>
      </Show>
    </button>
  );
}

/** A scale window's content for a range: its ends printed, the value placed on a fine line between. */
export function RangeScale(props: { min: string; max: string; frac: number }) {
  return (
    <>
      <span class="transport__scale-end">{props.min}</span>
      <span class="transport__scale-line" style={{ '--f': Math.max(0, Math.min(1, props.frac)).toFixed(3) }} />
      <span class="transport__scale-end">{props.max}</span>
    </>
  );
}

/** A scale window's content for a few steps: each printed, the one set lit. */
export function StepScale(props: { steps: readonly number[]; value: number }) {
  return (
    <>
      {props.steps.map((s) => (
        <span class="transport__scale-step" classList={{ 'is-on': s === props.value }}>
          {s}
        </span>
      ))}
    </>
  );
}
