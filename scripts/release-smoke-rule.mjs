// scripts/release-smoke-rule.mjs — whether a commit carries a passed `pnpm release:smoke`, the Windows
// smoke that must pass on the very commit a tag publishes.
//
// The smoke runs on the PC, so no runner can rerun it: a full PASS records itself on the commit as a
// GitHub commit status (context `release-smoke`, state `success`; scripts/release-smoke.mjs), and
// build-exe.yml's `smoke-verdict` job reads the commit's statuses through this rule before `release`
// may publish. The newest status in the context decides: a later failure or pending status cancels an
// earlier pass. An owner-approved exception is the same status posted by hand on the owner's word, its
// description opening `owner waiver:` (the build-exe.yml header), so it too names the commit it covers.
//
// Pure: the verdict script hands in the API's list; anything it cannot read as a pass fails closed.
// Guard: verify/guards/release-smoke-gate.mjs.

export const SMOKE_CONTEXT = 'release-smoke';

/**
 * @param {unknown} statuses the commit's statuses as `GET repos/{repo}/commits/{sha}/statuses` lists them
 * @returns {{ ok: boolean, why: string }}
 */
export function smokeVerdict(statuses) {
  if (!Array.isArray(statuses)) return { ok: false, why: 'the commit statuses could not be read' };
  const ours = statuses.filter((s) => s && typeof s === 'object' && s.context === SMOKE_CONTEXT);
  if (ours.length === 0) {
    return { ok: false, why: `no ${SMOKE_CONTEXT} status on this commit: run pnpm release:smoke on it (docs/VERIFY.md) before the tag` };
  }
  // The API lists newest first; sort anyway, so the order of the list cannot decide.
  const newest = ours
    .map((s) => ({ s, t: Date.parse(s.created_at) }))
    .sort((a, b) => (Number.isFinite(b.t) ? b.t : -Infinity) - (Number.isFinite(a.t) ? a.t : -Infinity) || (Number(b.s.id) || 0) - (Number(a.s.id) || 0))[0];
  if (!Number.isFinite(newest.t)) return { ok: false, why: `the newest ${SMOKE_CONTEXT} status carries no readable time` };
  const { state, description } = newest.s;
  const said = typeof description === 'string' && description ? `: ${description}` : '';
  if (state !== 'success') return { ok: false, why: `the newest ${SMOKE_CONTEXT} status is ${JSON.stringify(state)}${said}` };
  return { ok: true, why: `${SMOKE_CONTEXT} passed on this commit at ${newest.s.created_at}${said}` };
}
