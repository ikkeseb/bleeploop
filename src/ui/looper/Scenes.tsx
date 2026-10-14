import { For, Show, createEffect, createMemo, on } from 'solid-js';
import { looper, scenes } from '../state/audio';
import { runScene } from '../../app/actions';
import { announceLooper } from './gates';
import './scenes.css';

/**
 * PROTOTYPE (scenes): the scene strip over the lanes. A scene is which lanes play (their MUTE) and their
 * volumes; the table and the bar-line switch are the engine's (`{ Scene }`, `Scenes` in
 * `engine-wire.ts`; in the prototype the web engine fake runs them). A chip press sends the engine's
 * scene press and the chips follow the feed: the current scene lit warm-white, an armed one in the
 * ARMED lanes' amber dashed pulse until the bar line, each chip's five pips the lanes it holds (filled:
 * plays, hollow: muted, none: not in the scene). The status line says what the next press does.
 *
 * It sits above the lanes, not in the command bar: it is about the lanes (a scene is a lane pattern),
 * it reads on the eye's path from the bar to lane 1, and the command bar is full below 1100 px
 * (`docs/backlog-taste.md`).
 */

export const SCENE_LETTERS = ['A', 'B', 'C', 'D'] as const;
const KEYS = ['F1', 'F2', 'F3', 'F4'] as const;

export function Scenes() {
  const view = scenes.view;
  const letter = (i: number) => SCENE_LETTERS[i] ?? String(i + 1);

  /** The current scene no longer matches the lanes (a MUTE or a volume moved since it was saved or
   * applied): pressing it saves the change. */
  const edited = createMemo(() => {
    const v = view();
    if (v.current === null) return false;
    const lanes = v.table[v.current];
    if (!lanes) return false;
    return lanes.some(
      (l, i) => l !== null && (l.muted !== looper.trackMuted(i) || Math.abs(l.volume - looper.trackVolume(i)) > 0.005),
    );
  });

  const status = (): { text: string; tone?: 'armed' | 'saved' } => {
    const v = view();
    const saved = scenes.saved();
    if (v.armed !== null) return { text: `${letter(v.armed)} on the next bar`, tone: 'armed' };
    if (v.current === null) return { text: 'press a scene to save which tracks play' };
    if (edited()) return { text: `${letter(v.current)} changed · press ${letter(v.current)} to save` };
    if (saved && saved.scene === v.current) return { text: `${letter(v.current)} saved`, tone: 'saved' };
    return { text: `press ${letter(v.current)} again to save it` };
  };

  // The switch is heard, not seen: say it to a screen reader too.
  createEffect(
    on(
      () => [view().current, view().armed] as const,
      ([current, armed], prev) => {
        if (armed !== null && armed !== prev?.[1]) announceLooper(`Scene ${letter(armed)} armed, switching on the next bar`);
        else if (current !== null && current !== prev?.[0]) announceLooper(`Scene ${letter(current)}`);
      },
      { defer: true },
    ),
  );

  const chipState = (i: number): 'armed' | 'current' | 'stored' | 'empty' => {
    const v = view();
    if (v.armed === i) return 'armed';
    if (v.current === i) return 'current';
    return v.table[i] ? 'stored' : 'empty';
  };

  const describe = (i: number): string => {
    const s = chipState(i);
    const what = s === 'armed' ? 'armed for the next bar' : s === 'current' ? 'playing' : s === 'stored' ? 'stored' : 'empty';
    return `Scene ${letter(i)}, ${what}`;
  };

  return (
    <div class="lp-scenes" role="group" aria-label="Scenes">
      <span class="lp-scenes__label" aria-hidden="true">
        SCENES
      </span>
      <div class="lp-scenes__chips">
        <For each={Array.from({ length: scenes.count }, (_, i) => i)}>
          {(i) => (
            <button
              type="button"
              class="lp-scene"
              data-state={chipState(i)}
              classList={{ 'is-edited': view().current === i && edited() }}
              aria-pressed={view().current === i}
              aria-label={describe(i)}
              title={`Scene ${letter(i)} (${KEYS[i]})`}
              onClick={() => runScene(i)}
            >
              <span class="lp-scene__letter">{letter(i)}</span>
              <Show when={view().table[i]}>
                {(lanes) => (
                  <span class="lp-scene__pips" aria-hidden="true">
                    <For each={lanes()}>
                      {(l) => <i class="lp-scene__pip" data-pip={l === null ? 'none' : l.muted ? 'muted' : 'plays'} />}
                    </For>
                  </span>
                )}
              </Show>
              <Show when={scenes.saved()?.scene === i ? scenes.saved() : null} keyed>
                {(saved) => <i class="lp-scene__flash" data-save={saved.n} aria-hidden="true" />}
              </Show>
            </button>
          )}
        </For>
      </div>
      <span class="lp-scenes__status" data-tone={status().tone}>
        {status().text}
      </span>
    </div>
  );
}
