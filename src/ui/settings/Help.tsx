import { For, Show, onMount } from 'solid-js';
import { asioStatus } from '../state/audio-devices';
import asioLogo from '../../assets/third-party/ASIO-compatible-logo-Steinberg-R-white-transparent-RGB.svg';
import { DRUM_KIT } from '../state/drum-kit';
import { ACTION_LABELS, type ActionId } from '../../app/actions';
import { KEY_ACTIONS } from '../../app/transport-keys';
import { COMPUTER_MAP } from '../keyboard/Keyboard';
import { platform } from '../../platform';
import { BUILD_LABEL, copyDiagnostics, logFolder, openLogFolder } from './diagnostics';
import { installUpdate, updateInstalling, updateNoteLines, updateOffered } from '../../app/update';
import './help.css';

/**
 * Help / quick-reference popover, ordered by the promise (an update the updater offers sits above it):
 * the first screen is guitar, the looper keys and the pedals (every looper action by foot); then the
 * looper, the song-level controls (▶/■ ALL, END STOP, FADE, ✕ ALL), the transport, the other layers
 * (a MIDI controller, computer keys as a fallback), the play map, and the layout move/hide/resize
 * affordances (the stage view among them). The keyboard is the fallback play path, so its sections come last. Same popover
 * pattern as AudioSettings (a command-bar `.tool` cap → a `<Show>`-mounted panel). The drum-pad rows
 * read from DRUM_KIT, the piano legend from COMPUTER_MAP and the looper keys from KEY_ACTIONS, so none
 * can drift from the real controls. The looper/transport copy mirrors the controls in Looper.tsx,
 * Transport.tsx, InputFx.tsx and FxPanel.tsx: a control's behaviour changes there first, then here.
 */

const PITCH_NAMES = ['C', 'C♯', 'D', 'D♯', 'E', 'F', 'F♯', 'G', 'G♯', 'A', 'A♯', 'B'] as const;
const COMPUTER_KEYS = Object.entries(COMPUTER_MAP).map(([key, offset]) => ({
  key,
  note: PITCH_NAMES[offset % PITCH_NAMES.length],
  sharp: [1, 3, 6, 8, 10].includes(offset % PITCH_NAMES.length),
}));
const NATURALS = COMPUTER_KEYS.filter((entry) => !entry.sharp);
const SHARPS = COMPUTER_KEYS.filter((entry) => entry.sharp);

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

export function Help() {
  // Ask for the log folder as Help opens, so Copy diagnostics has it before the click.
  onMount(() => void logFolder());
  return (
    <div class="help" role="group" aria-label="Quick reference">
      <div class="help__title">Quick reference</div>

      {/* A newer release the updater found at launch (`src/app/update.ts`); the Help cap wears a dot. */}
      <Show when={updateOffered()}>
        {(update) => (
          <section class="help__sec help__update">
            <h3 class="help__h">Update ready <span class="help__tag">v{update().version}</span></h3>
            <ul class="help__list">
              <For each={updateNoteLines(update().notes)}>{(line) => <li>{line}</li>}</For>
            </ul>
            <p class="help__sub">BleepLoop closes, installs it and opens again. Your committed loops come back.</p>
            <div class="help__actions">
              <button type="button" class="help__btn" disabled={updateInstalling()} onClick={() => void installUpdate()}>
                {updateInstalling() ? 'Updating…' : 'Update and restart'}
              </button>
            </div>
          </section>
        )}
      </Show>

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
        <h3 class="help__h">Looper keys <span class="help__tag">work even with the keyboard hidden</span></h3>
        <p class="help__sub">Record, play, undo and clear act on the selected track; undo again redoes.</p>
        <div class="help__keyrow">
          <For each={LOOPER_KEYS}>
            {(entry) => (
              <span class="help__chip">
                <For each={entry.keys}>{(key) => <kbd class="help__kbd">{key}</kbd>}</For>
                <span class="help__note">{entry.label}</span>
              </span>
            )}
          </For>
        </div>
        <ul class="help__list">
          <li><kbd class="help__kbd">1</kbd>–<kbd class="help__kbd">5</kbd> select a track</li>
          <li>In drum mode the pads take <kbd class="help__kbd">1</kbd>–<kbd class="help__kbd">4</kbd>, so only <kbd class="help__kbd">5</kbd> selects a track there</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Pedals <span class="help__tag">hands stay on the guitar</span></h3>
        <ul class="help__list">
          <li>A keystroke footswitch or page turner sends keys: set each pedal to one of the looper keys above</li>
          <li>A MIDI footswitch or controller: Audio Settings → <span class="help__note">MIDI LEARN</span>. Pick an action, press <span class="help__note">LEARN</span>, then tap the pedal once. Momentary and latching pedals both run it once per press; if a press runs twice or not at all, switch its kind in the list</li>
          <li>Every looper control can be learned. A track action (record, play, undo, clear, mute, reverse, copy) acts on the selected track or on the track you pick; tap tempo, click, end stop, fixed and the input echo, reverb and ring mod switch as their buttons do</li>
          <li><span class="help__note">HOLD</span> on a momentary record pedal: hold it to record or overdub, let go to stop</li>
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
          <li><span class="help__note">FX</span> opens a track's effects &middot; <span class="help__note">MUTE</span> silences it &middot; the volume slider has a 0 dB detent at 1.0</li>
          <li><span class="help__note">DUB FEEDBACK</span>, last in the FX drawer, is what an overdub keeps of the layers under it: 100 % keeps them all, 0 % replaces them, and in between the old layers fade pass by pass</li>
          <li><span class="help__note">↺ REV</span> reverses a track in place. Overdub is blocked while reversed</li>
          <li><span class="help__note">⧉ COPY</span> copies a take to the first empty track</li>
          <li><span class="help__note">✂ TRIM</span> keeps a track's first bars and repeats them across the loop. The loop keeps its length</li>
          <li>All tracks share one loop, so they stay locked together and can't drift</li>
          <li>A take shorter than the loop repeats across it — stop early, or set the bars with <span class="help__note">FIXED</span></li>
          <li>Keep playing past the end of the loop and the take grows it: the loop becomes as many whole loops as you played, rounded to the nearest, and the other tracks repeat</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Whole song <span class="help__tag">every track at once</span></h3>
        <ul class="help__list">
          <li><span class="help__note">▶ ALL</span> starts every stopped track together &middot; <span class="help__note">■ ALL</span> stops them all; a take being recorded or overdubbed ends as its own stop would end it</li>
          <li><span class="help__note">END STOP</span> on: a track's stop and ■ ALL wait for the end of the loop; it never delays a recording or overdub. While tracks wait, ■ ALL reads <span class="help__note">■ NOW</span>, and a second stop lands right away</li>
          <li><span class="help__note">FADE</span> fades every playing track out over its bars (1, 2, 4 or 8, from the stepper beside it) and stops them on the first bar line after; press it again while <span class="help__note">FADING</span> to stop now. Volumes never move, so ▶ ALL brings the tracks back</li>
          <li><span class="help__note">✕ ALL</span> clears every track and resets the loop length, which unlocks the tempo (press twice to confirm)</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Transport &amp; tempo</h3>
        <ul class="help__list">
          <li>A 1-bar count-in (four clicks) leads the first recording. You come in on the counted "1", not the button press. A later take recorded with the loops stopped gets the count-in too, and every track starts from the top on its "1"</li>
          <li><span class="help__note">CLICK</span> toggles the metronome (its own volume, never recorded, silent while nothing runs)</li>
          <li><span class="help__note">FIXED N</span> records exactly N bars and auto-stops on the downbeat. Off, every take runs until you stop it</li>
          <li><span class="help__note">RETAKE</span> keeps recording round the loop until you stop; STOP, REC/DUB or REC on another track keeps the last complete pass. The first track needs FIXED; on later takes FIXED is ignored while RETAKE is on</li>
          <li><span class="help__note">AUTO REC · SENS</span> replaces the first count-in: arm the track, then playing starts the take. Raise sensitivity for quieter input; the cyan tick on the record level is the trigger</li>
          <li>Click the <span class="help__note">BPM</span> to type it, or <span class="help__note">TAP</span> a tempo. Tempo locks to the first loop (clear all to change)</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Other layers <span class="help__tag">synths · MIDI</span></h3>
        <ul class="help__list">
          <li>A MIDI controller plays the synths and the other layers. Controller status shows in Audio Settings → diagnostics</li>
          <li>Click a slot to send MIDI and keyboard notes to its plugin or built-in synth</li>
          <li>The computer keys below are a fallback when no controller is connected</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Play keys <span class="help__tag">computer-keyboard fallback</span></h3>
        <div class="help__keyrow">
          <For each={SHARPS}>
            {(s) => (
              <span class="help__chip help__chip--sharp">
                <kbd class="help__kbd">{s.key}</kbd>
                <span class="help__note">{s.note}</span>
              </span>
            )}
          </For>
        </div>
        <div class="help__keyrow">
          <For each={NATURALS}>
            {(s) => (
              <span class="help__chip">
                <kbd class="help__kbd">{s.key}</kbd>
                <span class="help__note">{s.note}</span>
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

      <section class="help__sec">
        <h3 class="help__h">Layout</h3>
        <ul class="help__list">
          <li>Drag any divider to resize &middot; double-click it to reset</li>
          <li>Keyboard bar: move it above / below the looper, or hide it</li>
          <li>The keyboard icon among the command bar's tools shows a hidden keyboard again</li>
          <li><span class="help__note">Stage view</span> (<kbd class="help__kbd">B</kbd>, the stage icon, or a learned pedal) fills the window with each track's state, the bar and the beat, readable from across the room. The looper keys stay live; B, Esc or <span class="help__note">EXIT</span> leaves it</li>
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
