import { For, Show } from 'solid-js';
import { dismissToast, toasts } from '../../notify';
import './toasts.css';

/**
 * The single error-toast surface. Renders `notify.ts`'s toast signal as a fixed
 * bottom-right stack above all app chrome, so a failure that would otherwise reach only `console.error`
 * (invisible in a release WebView2 build) is actually seen. Mounted once in `App` next to the popovers.
 *
 * Non-modal + pointer-transparent container (the play path stays live behind it). Each toast is an
 * `aria-live` status; the whole body is a pointer dismiss shortcut, with a real close `<button>` for
 * keyboard users. Auto-dismiss + dedupe live in the store, not here.
 */
export function Toasts() {
  return (
    // ONE persistent live region (role="log": implicit polite live semantics + it legitimizes the
    // aria-label, which the generic role would drop). The toasts themselves stay role-less — a
    // per-toast role="status" would nest a SECOND, freshly-inserted live region, which NVDA/JAWS
    // can announce twice or miss entirely.
    <div class="toasts" role="log" aria-live="polite" aria-label="Notifications">
      <For each={toasts()}>
        {(t) => (
          <div
            class="toast"
            classList={{ 'toast--info': t.kind === 'info' }}
            data-toast-id={t.id}
            onClick={() => dismissToast(t.id)}
            title="Dismiss"
          >
            <span class="toast__accent" aria-hidden="true" />
            <div class="toast__body">
              <span class="lf-visually-hidden">{t.kind === 'info' ? 'Done' : 'Error'}</span>
              <span class="toast__msg">
                {t.message}
                <Show when={t.count > 1}>
                  <span class="toast__count">×{t.count}</span>
                </Show>
              </span>
              <Show when={t.detail}>
                <span class="toast__detail">{t.detail}</span>
              </Show>
            </div>
            <button
              type="button"
              class="toast__close"
              aria-label="Dismiss notification"
              onClick={(e) => {
                e.stopPropagation(); // the body click also dismisses; don't double-fire
                // Keyboard dismiss must not strand focus on <body> (the popovers return focus on
                // close — same bar): hand it to a neighbouring toast's close button when one remains.
                const closes = [...document.querySelectorAll<HTMLButtonElement>('.toast__close')];
                const i = closes.indexOf(e.currentTarget);
                const next =
                  closes[i + 1] ??
                  closes[i - 1] ??
                  document.querySelector<HTMLButtonElement>('.cmd button:not(:disabled)');
                const restoreKeyboardFocus = e.detail === 0;
                dismissToast(t.id);
                if (restoreKeyboardFocus) queueMicrotask(() => next?.focus());
              }}
            >
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" aria-hidden="true">
                <path d="M6 6l12 12M18 6L6 18" stroke-linecap="round" />
              </svg>
            </button>
          </div>
        )}
      </For>
    </div>
  );
}
