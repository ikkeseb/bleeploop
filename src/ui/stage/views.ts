import { orbit } from './orbit';
import { scope } from './scope';
import { strata } from './strata';
import type { StageViewDef } from './visual';

/**
 * The stage's looks, in the order the view switch cycles them; the first is the default. Adding or
 * dropping a look is one entry here (and its module).
 */
export const STAGE_VIEWS: readonly StageViewDef[] = [scope, orbit, strata];
