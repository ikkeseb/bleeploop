import { For, Show, createSignal, onMount } from 'solid-js';
import { asioStatus } from '../state/audio-devices';
import asioLogo from '../../assets/third-party/ASIO-compatible-logo-Steinberg-R-white-transparent-RGB.svg';
import { DRUM_KIT } from '../state/drum-kit';
import { ACTION_LABELS, type ActionId } from '../../app/actions';
import { KEY_ACTIONS } from '../../app/transport-keys';
import { COMPUTER_MAP } from '../keyboard/Keyboard';
import { platform } from '../../platform';
import { BUILD_LABEL, copyDiagnostics, logFolder, openLogFolder } from './diagnostics';
import { SETTINGS_PAGE_LABELS, openSettings, settingsPath, type SettingsPage } from '../../settings-opener';
import './help.css';

/**
 * Help popover, two tabs (an offered update is NOT here: it has its own command-bar pill and panel,
 * `src/ui/settings/UpdatePanel.tsx`). `?` always opens on REFERENCE, what a player scans mid-jam and
 * sized to fit every window from the 960x600 minimum unscrolled (contact-sheet and layout-reachability
 * hold it): the looper keys and the pedals beside the play keys, the octave and the drum pads. MANUAL
 * is the rest, read once, ordered by the promise: guitar, MIDI pedals (HOLD among them), the looper,
 * the song-level controls (▶/■ ALL, END STOP, FADE, ✕ ALL), the transport, the other layers, the
 * layout move/hide/resize affordances (the stage view among them), the session, and About this build
 * last, whole (the ASIO lines below). Same popover pattern as AudioSettings (a command-bar `.tool` cap
 * → a `<Show>`-mounted panel). The drum-pad grid reads from DRUM_KIT, the play keys from COMPUTER_MAP
 * and the looper keys from KEY_ACTIONS, so none can drift from the real controls. The looper/transport copy mirrors the controls in Looper.tsx, Transport.tsx, InputFx.tsx
 * and FxPanel.tsx: a control's behaviour changes there first, then here.
 */

const PITCH_NAMES = ['C', 'C♯', 'D', 'D♯', 'E', 'F', 'F♯', 'G', 'G♯', 'A', 'A♯', 'B'] as const;
const COMPUTER_KEYS = Object.entries(COMPUTER_MAP).map(([key, offset]) => ({
  key,
  note: PITCH_NAMES[offset % PITCH_NAMES.length],
  sharp: [1, 3, 6, 8, 10].includes(offset % PITCH_NAMES.length),
}));
/** The play keys laid out as the keys they play: a natural spans two half-columns, a sharp straddles
 * the seam after the natural below it. */
const PLAY_KEYS = COMPUTER_KEYS.map((entry, i) => {
  const naturalsBefore = COMPUTER_KEYS.slice(0, i).filter((e) => !e.sharp).length;
  return { ...entry, column: entry.sharp ? 2 * naturalsBefore : 2 * naturalsBefore + 1 };
});
const PLAY_COLUMNS = 2 * COMPUTER_KEYS.filter((entry) => !entry.sharp).length + 1;

/** A KEY_ACTIONS key as its cap reads; any other key shows its own name. */
const KEY_CAPS: Readonly<Record<string, string>> = {
  ' ': 'Space',
  ArrowUp: '↑',
  ArrowDown: '↓',
  ArrowLeft: '←',
  ArrowRight: '→',
  PageUp: 'PgUp',
  PageDown: 'PgDn',
};
/** The looper keys grouped by the action they run, in the action table's order. */
const LOOPER_KEYS = (Object.keys(ACTION_LABELS) as ActionId[])
  .map((id) => ({
    label: ACTION_LABELS[id],
    keys: Object.keys(KEY_ACTIONS)
      .filter((key) => KEY_ACTIONS[key] === id)
      .map((key) => KEY_CAPS[key] ?? key),
  }))
  .filter((entry) => entry.keys.length > 0);

type HelpTab = 'reference' | 'manual';
const TABS: readonly { id: HelpTab; label: string }[] = [
  { id: 'reference', label: 'Reference' },
  { id: 'manual', label: 'Manual' },
];

export function Help() {
  // Ask for the log folder as Help opens, so Copy diagnostics has it before the click.
  onMount(() => void logFolder());
  // Every open starts on the reference: the popover remounts Help each time.
  const [tab, setTab] = createSignal<HelpTab>('reference');
  const tabEls: Partial<Record<HelpTab, HTMLButtonElement>> = {};
  // The tabs pattern: Tab reaches the selected tab only, the arrows (wrapping), Home and End pick and
  // focus a tab. A focused button keeps these keys from the transport (transport-keys.ts' yield rule).
  const onTabKey = (e: KeyboardEvent) => {
    const at = TABS.findIndex((t) => t.id === tab());
    const to = { ArrowLeft: at - 1, ArrowRight: at + 1, Home: 0, End: TABS.length - 1 }[e.key];
    if (to === undefined) return;
    e.preventDefault();
    const next = TABS[(to + TABS.length) % TABS.length].id;
    setTab(next);
    tabEls[next]?.focus();
  };
  return (
    <div class="help">
      <div class="help__tabs" role="tablist" aria-label="Help">
        <For each={TABS}>
          {(t) => (
            <button
              type="button"
              role="tab"
              ref={(el) => (tabEls[t.id] = el)}
              id={`lf-help-tab-${t.id}`}
              class="help__tab"
              aria-selected={tab() === t.id}
              aria-controls={`lf-help-${t.id}`}
              tabindex={tab() === t.id ? 0 : -1}
              onClick={() => setTab(t.id)}
              onKeyDown={onTabKey}
            >
              {t.label}
            </button>
          )}
        </For>
      </div>
      {/* Both panels stay mounted, the other one hidden but laid out (help.css), so Help keeps the taller
          one's height on a tab switch, each tab's aria-controls names a live panel and About's build line
          is in the DOM whichever tab shows (scripts/release-smoke.mjs reads it). Each panel is a Tab stop
          and its own scroller under the tabs (the reference holds no control, the manual's first is at its
          foot), so the keys read and scroll it there; a pointer click into one gives the keys back to the
          transport (transport-keys.ts). */}
      <Reference hidden={tab() !== 'reference'} />
      <Manual hidden={tab() !== 'manual'} />
    </div>
  );
}

/** What a player scans mid-jam: which key, which pad. */
function Reference(props: { hidden: boolean }) {
  return (
    <div
      class="help__panel help__panel--ref"
      id="lf-help-reference"
      role="tabpanel"
      aria-labelledby="lf-help-tab-reference"
      tabindex={0}
      hidden={props.hidden}
    >
      <div class="help__col">
        <section class="help__sec">
          <h3 class="help__h">Looper keys <span class="help__tag">work even with the keyboard hidden</span></h3>
          <p class="help__sub">Record, play, undo and clear act on the selected track; undo again redoes.</p>
          <div class="help__keys">
            <For each={LOOPER_KEYS}>
              {(entry) => (
                <>
                  <span class="help__caps">
                    <For each={entry.keys}>{(key) => <kbd class="help__kbd">{key}</kbd>}</For>
                  </span>
                  <span class="help__label">{entry.label}</span>
                </>
              )}
            </For>
            <span class="help__caps">
              <kbd class="help__kbd">1</kbd>–<kbd class="help__kbd">5</kbd>
            </span>
            <span class="help__label">Select a track</span>
          </div>
          <p class="help__sub">In drum mode the pads take <kbd class="help__kbd">1</kbd>–<kbd class="help__kbd">4</kbd>, so only <kbd class="help__kbd">5</kbd> selects a track there.</p>
        </section>

        <section class="help__sec">
          <h3 class="help__h">Pedals <span class="help__tag">hands stay on the guitar</span></h3>
          <ul class="help__list">
            <li>A keystroke footswitch or page turner sends keys: set each pedal to one of the looper keys above</li>
            {/* Only the page's name is the link here: a button is an inline block and never wraps, and the
                whole path would push this column a line past the 960x600 fit. */}
            <li>A MIDI pedal: learn its action in Audio Settings → <SettingsLink page="midi" short /> (Manual → Pedals)</li>
          </ul>
        </section>
      </div>

      <div class="help__col">
        <section class="help__sec">
          <h3 class="help__h">Play keys <span class="help__tag">computer-keyboard fallback</span></h3>
          <div class="help__piano" style={{ 'grid-template-columns': `repeat(${PLAY_COLUMNS}, 1fr)` }}>
            <For each={PLAY_KEYS}>
              {(k) => (
                <span
                  class="help__pkey"
                  classList={{ 'help__pkey--sharp': k.sharp }}
                  style={{ 'grid-column': `${k.column} / span 2`, 'grid-row': k.sharp ? 1 : 2 }}
                >
                  <kbd class="help__kbd">{k.key}</kbd>
                  <span class="help__note">{k.note}</span>
                </span>
              )}
            </For>
          </div>
          <p class="help__sub">Bottom row = naturals, top row = sharps. Plays the active slot's instrument.</p>
        </section>

        <section class="help__sec">
          <h3 class="help__h">Octave</h3>
          <div class="help__inline">
            <kbd class="help__kbd">z</kbd>
            <span class="help__sub">down</span>
            <kbd class="help__kbd">x</kbd>
            <span class="help__sub">up</span>
          </div>
        </section>

        <section class="help__sec">
          <h3 class="help__h">
            Drum pads <span class="help__tag">when the slot's synth is Drum</span>
          </h3>
          <div class="help__pads">
            <For each={DRUM_KIT}>
              {(v) => (
                <span class="help__pad">
                  <kbd class="help__kbd">{v.key}</kbd>
                  <span class="help__padname">{v.label}</span>
                </span>
              )}
            </For>
          </div>
        </section>
      </div>
    </div>
  );
}

/** What every control does, read once; About this build last. */
function Manual(props: { hidden: boolean }) {
  return (
    <div
      class="help__panel help__panel--manual"
      id="lf-help-manual"
      role="tabpanel"
      aria-labelledby="lf-help-tab-manual"
      tabindex={0}
      hidden={props.hidden}
    >
      <section class="help__sec">
        <h3 class="help__h">Guitar <span class="help__tag">the main way to play</span></h3>
        <ul class="help__list">
          <li>Pick an amp plugin as a slot's source, pick its input (<span class="help__note">In 1</span>, <span class="help__note">In 2</span>…) and use <span class="help__note">GO LIVE</span>. <span class="help__note">INPUT LIVE</span> means that slot hears its input and monitors it natively</li>
          <li>Raw input (a hardware synth, a mic): set a slot's source to <span class="help__note">Off</span>, pick its input and use GO LIVE. It is heard and recorded dry, and an Off slot plays no notes. Both slots can be live at once, each on its own input</li>
          <li>Each slot's volume sets its synth, plugin or input level. Synths and plugins feed the looper directly; the bar beside IN FX shows record level</li>
          <li><span class="help__note">IN FX</span> puts <span class="help__note">ECHO</span>, <span class="help__note">REVERB</span> and <span class="help__note">RING MOD</span> on the live input, before the looper: what they add is heard and recorded, the dry sound stays untouched. The pill lights while any of them is on</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Pedals <span class="help__tag">hands stay on the guitar</span></h3>
        <ul class="help__list">
          <li>A MIDI footswitch or controller: <SettingsLink page="midi" />. Pick an action beside <span class="help__note">MIDI LEARN</span>, press <span class="help__note">LEARN</span>, then tap the pedal once. Momentary and latching pedals both run it once per press; if a press runs twice or not at all, switch its kind in the list</li>
          <li>Every looper control can be learned. A track action (record, play, undo, clear, mute, reverse, copy, copy wide) acts on the selected track or on the track you pick; <span class="help__note">TAP</span>, <span class="help__note">CLICK</span>, <span class="help__note">END STOP</span>, <span class="help__note">FIXED</span> and IN FX's ECHO, REVERB and RING MOD work as their buttons do</li>
          <li><span class="help__note">HOLD</span>, beside a REC/DUB learned on a momentary pedal: hold the pedal to record or overdub, let go to stop</li>
          <li>A learned pedal or key only runs its action: it plays no note and holds no sustain. <span class="help__note">✕</span> in the list forgets it</li>
          <li><span class="help__note">Halve track</span> (MIDI learn only) keeps the selected track's first half, as <span class="help__note">✂ TRIM</span> does; undo gives the whole loop back</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Looper <span class="help__tag">5 tracks · the instrument</span></h3>
        <ul class="help__list">
          <li>The big ring records a track, then overdubs (layers) onto it once it has a take</li>
          <li><span class="help__note">▶ / ■</span> plays or stops a track &middot; <span class="help__note">CLR</span> clears it (press twice to confirm)</li>
          <li><span class="help__note">↶ UNDO</span> undoes the last overdub layer or trim (press again to redo)</li>
          <li><span class="help__note">FX</span> opens a track's effects, and its lamp lights while any of them is on &middot; <span class="help__note">MUTE</span> silences it &middot; the volume slider has a 0 dB detent at 1.0 and resets to it on a double-click &middot; the pan beside it (L / C / R) centres on a double-click, Alt-click or 0</li>
          <li><span class="help__note">DUB FEEDBACK</span>, last in the FX drawer, is what an overdub keeps of the layers under it: 100 % keeps them all, 0 % replaces them, and in between the old layers fade pass by pass</li>
          <li><span class="help__note">↺ REV</span> reverses a track in place. Overdub is blocked while reversed</li>
          <li><span class="help__note">EDIT</span> opens a track's edits: <span class="help__note">⧉ COPY</span> copies a take to the first empty track</li>
          <li><span class="help__note">↔ WIDE</span> copies it wide: the copy plays 20 ms behind, panned to the other side (a centred track goes half left, the copy half right), so one riff sounds like two guitars. Summed to mono the two cancel in places and sound hollow; a real double is a second take</li>
          <li><span class="help__note">✂ TRIM</span> keeps a track's first bars and repeats them across the loop. The loop keeps its length</li>
          <li>All tracks share one loop, so they stay locked together and can't drift</li>
          <li>A take shorter than the loop repeats across it: stop early, or set the bars with <span class="help__note">FIXED</span></li>
          <li>Keep playing past the end of the loop and the take grows it: the loop becomes as many whole loops as you played, rounded to the nearest, and the other tracks repeat</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Whole song <span class="help__tag">every track at once</span></h3>
        <ul class="help__list">
          <li><span class="help__note">▶ ALL</span> starts every stopped track together &middot; <span class="help__note">■ ALL</span> stops them all; a take being recorded or overdubbed ends as its own stop would end it</li>
          <li><span class="help__note">END STOP</span> on: a track's stop and ■ ALL wait for the end of the loop; it never delays a recording or overdub. While tracks wait, ■ ALL reads <span class="help__note">■ NOW</span>, and a second stop lands right away</li>
          <li><span class="help__note">FADE</span> fades every playing track out over its bars (1, 2, 4 or 8, from the stepper beside it) and stops them on the first bar line after; press it again while <span class="help__note">FADING</span> to stop now. Volumes never move, so ▶ ALL brings the tracks back</li>
          <li><span class="help__note">✕ ALL</span> clears every track and resets the loop length, which unlocks the tempo and the time signature (press twice to confirm)</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Transport &amp; tempo</h3>
        <ul class="help__list">
          <li>A 1-bar count-in (one bar of clicks, counted down to 1) leads the first recording. You come in on the downbeat after the count, not the button press. A later take recorded with the loops stopped gets the count-in too, and every track starts from the top on that downbeat</li>
          <li><span class="help__note">CLICK</span> toggles the metronome (never recorded, silent while nothing runs); the number beside it opens its volume. A mode that is on lights its lamp</li>
          <li><span class="help__note">FIXED</span> records exactly the bars set beside it and auto-stops on the downbeat. Off, every take runs until you stop it</li>
          <li><span class="help__note">RETAKE</span> keeps recording round the loop until you stop; STOP, REC/DUB or REC on another track keeps the last complete pass. The first track needs FIXED; on later takes FIXED is ignored while RETAKE is on</li>
          <li><span class="help__note">AUTO REC</span> replaces the first count-in: arm the track, then playing starts the take. The number beside it opens its sensitivity (<span class="help__note">SENS</span>): raise it for quieter input; the cyan tick on the record level is the trigger</li>
          <li>Click the <span class="help__note">BPM</span> to type it, or <span class="help__note">TAP</span> a tempo. Tempo locks to the first loop (clear all to change)</li>
          <li>The time signature beside BPM (4/4, 3/4, 6/8, 12/8, 5/4) sets the bar: the click accents its first beat, and FIXED, FADE and the grid count its bars. In 6/8 and 12/8, BPM and TAP count the dotted quarter (two and four beats a bar). It locks with the tempo</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Other layers <span class="help__tag">synths · MIDI</span></h3>
        <ul class="help__list">
          <li>A MIDI controller plays the synths and the other layers. Controller status shows in <SettingsLink page="diagnostics" /></li>
          <li>Click a slot to send MIDI and keyboard notes to its plugin or built-in synth</li>
          <li>With no controller connected, the computer keys play instead: Reference maps them</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Layout</h3>
        <ul class="help__list">
          <li>Drag any divider to resize &middot; double-click it to reset</li>
          <li>Keyboard bar: move it above / below the looper, or hide it</li>
          <li>The keyboard icon among the command bar's tools shows a hidden keyboard again</li>
          <li><span class="help__note">Stage view</span> (<kbd class="help__kbd">B</kbd>, the stage icon, or a learned pedal) fills the window with a visualizer of the loops, each track in its state's colour, with the selected track, the bar, the beat and the count-in still readable from across the room. The default look, SCOPE, draws each track's live signal after its FX, with the engine's whole output above them; ORBIT and STRATA draw each track's recorded loop and its volume. <kbd class="help__kbd">V</kbd> or its bottom-right button switches the look. The looper keys stay live; B, Esc or the ✕ button leaves it</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Session</h3>
        <ul class="help__list">
          <li><span class="help__note">⬇</span> (the export icon among the command bar's tools) exports every track + a master mix as WAV, with a session.json and each loaded plugin's settings, in one zip</li>
          <li><span class="help__note">⬆</span> imports such a zip, only while the looper is empty; the loops are restored locally on reopen anyway</li>
        </ul>
      </section>

      {/* The build a tester reports (`diagnostics.ts`): version + commit, Copy diagnostics, and Open log
          folder where there is a log file. Also the app's "About box equivalent" for Steinberg's ASIO
          Usage Guidelines (1e/1f: ASIO is on by default, so the unaltered logo must sit here, and only
          here). Section 14 allows the trademark line as plain text beside the logo. The ASIO lines are
          present in every build that links the SDK (the licence statement is about the binary), whether
          or not the driver was started or disabled at launch. */}
      <section class="help__sec">
        <h3 class="help__h">About this build</h3>
        <p class="help__about">{BUILD_LABEL}</p>
        <Show when={asioStatus().status !== 'not-compiled'}>
          <p class="help__about">This build links the Steinberg ASIO® SDK and is licensed GPLv3. The BleepLoop source is MIT.</p>
          <div class="help__asio">
            <img class="help__asio-logo" src={asioLogo} alt="ASIO Compatible" />
            <span>ASIO is a registered trademark of Steinberg Media Technologies GmbH</span>
          </div>
        </Show>
        <div class="help__actions">
          <button type="button" class="help__btn" onClick={() => void copyDiagnostics()}>
            Copy diagnostics
          </button>
          <Show when={platform.logs.available}>
            <button type="button" class="help__btn" onClick={openLogFolder}>
              Open log folder
            </button>
          </Show>
        </div>
      </section>
    </div>
  );
}

/** A page of Audio Settings named in the prose, as the rail names it (`settingsPath`): it closes Help and
 * opens the panel on that page. `short` shows the page's name alone, after prose that says the rest; the
 * link's accessible name still says the whole way. */
function SettingsLink(props: { page: SettingsPage; short?: boolean }) {
  return (
    <button
      type="button"
      class="lf-link help__link"
      aria-label={props.short ? settingsPath(props.page) : undefined}
      onClick={() => openSettings(props.page)}
    >
      <Show when={!props.short}>Audio Settings → </Show>
      <span class="help__note">{SETTINGS_PAGE_LABELS[props.page].toUpperCase()}</span>
    </button>
  );
}
