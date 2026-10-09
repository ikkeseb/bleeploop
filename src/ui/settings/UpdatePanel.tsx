import { For, Show } from 'solid-js';
import {
  installUpdate,
  updateInstalling,
  updateNoteLines,
  updateOffered,
  updateProgress,
  updateProgressFraction,
  updateProgressLabel,
} from '../../app/update';
import './update-panel.css';

/**
 * The update panel: what the updater found and the one press that installs it (`src/app/update.ts`;
 * the native half is `src-tauri/src/update.rs`). It hangs from the command bar's UPDATE pill, which
 * exists only while an update is offered — the pill and this panel are the whole affordance.
 *
 * It used to be a section at the top of Help. That buried the app's most important action under
 * thirteen sections of keyboard reference, in a panel called "Quick reference", behind a `?` cap: the
 * owner could not find it. An available update is a state the app is in, not a reference article.
 *
 * While the install runs the panel shows which of its three waits it is in (`updateProgressLabel`),
 * since the download is tens of megabytes over a network and the signature check runs over the whole
 * package. The bar is determinate only where there is a number: a server that declares no size leaves
 * it indeterminate rather than inventing a figure.
 */
export function UpdatePanel() {
  const update = () => updateOffered();
  return (
    <div class="update" role="group" aria-label="Update">
      <Show when={update()}>
        {(ready) => (
          <>
            <div class="update__title">
              Update ready <span class="update__ver">v{ready().version}</span>
            </div>
            <ul class="update__list">
              <For each={updateNoteLines(ready().notes)}>{(line) => <li>{line}</li>}</For>
            </ul>
            <p class="update__sub">
              BleepLoop closes, installs it and opens again. Your committed loops come back.
            </p>
            {/* The read-out replaces the copy above the button only once a stage has arrived, so a
                press that fails before the native side reports anything still reads as a press. */}
            <Show when={updateProgress()}>
              {(stage) => {
                const fraction = () => updateProgressFraction(stage());
                return (
                  <div
                    class="update__progress"
                    role="progressbar"
                    aria-label={updateProgressLabel(stage())}
                    aria-valuemin={0}
                    aria-valuemax={1}
                    aria-valuenow={fraction() ?? undefined}
                  >
                    <div class="update__bar">
                      <div
                        class="update__fill"
                        classList={{ 'update__fill--unknown': fraction() === null }}
                        style={fraction() === null ? undefined : { width: `${(fraction() ?? 0) * 100}%` }}
                      />
                    </div>
                    <span class="update__stage">{updateProgressLabel(stage())}</span>
                  </div>
                );
              }}
            </Show>
            <div class="update__actions">
              <button
                type="button"
                class="update__btn"
                disabled={updateInstalling()}
                onClick={() => void installUpdate()}
              >
                {updateInstalling() ? 'Updating…' : 'Update and restart'}
              </button>
            </div>
          </>
        )}
      </Show>
    </div>
  );
}
