import { For, Show } from 'solid-js';
import { asioStatus } from '../../audio/audio-devices';
import asioLogo from '../../assets/third-party/ASIO-compatible-logo-Steinberg-R-white-transparent-RGB.svg';
import { DRUM_KIT } from '../../audio/synths/drum';
import { COMPUTER_MAP } from '../keyboard/Keyboard';
import './help.css';

/**
 * Help / quick-reference popover: how to play (guitar first, a MIDI controller for the other layers,
 * computer keys as a fallback), the looper + transport controls, the play map, and the layout
 * move/hide/resize affordances. Same popover pattern as AudioSettings (a command-bar `.tool` cap → a
 * `<Show>`-mounted panel). It LEADS with the instrument (the looper) — the keyboard is the fallback
 * play path, so its sections come last. The
 * drum-pad rows read from DRUM_KIT, and the piano legend reads from COMPUTER_MAP, so neither can drift
 * from the real controls. The looper/transport copy mirrors the controls in Looper.tsx + Transport.tsx.
 */

const PITCH_NAMES = ['C', 'C♯', 'D', 'D♯', 'E', 'F', 'F♯', 'G', 'G♯', 'A', 'A♯', 'B'] as const;
const COMPUTER_KEYS = Object.entries(COMPUTER_MAP).map(([key, offset]) => ({
  key,
  note: PITCH_NAMES[offset % PITCH_NAMES.length],
  sharp: [1, 3, 6, 8, 10].includes(offset % PITCH_NAMES.length),
}));
const NATURALS = COMPUTER_KEYS.filter((entry) => !entry.sharp);
const SHARPS = COMPUTER_KEYS.filter((entry) => entry.sharp);

export function Help() {
  return (
    <div class="help" role="group" aria-label="Quick reference">
      <div class="help__title">Quick reference</div>

      <section class="help__sec">
        <h3 class="help__h">Playing</h3>
        <ul class="help__list">
          <li>Guitar is the main way to play: load an amp plugin and use <span class="help__note">GO LIVE</span>. <span class="help__note">INPUT LIVE</span> means that slot receives the selected input and monitors it natively. Choose the input channel in Audio Settings</li>
          <li>A MIDI controller plays the synths and the other layers. Controller status shows in Audio Settings → diagnostics</li>
          <li>Click a slot to send MIDI and keyboard notes to its plugin or built-in synth</li>
          <li>ASIO supports one live slot. To use another amp slot, <span class="help__note">UNLOAD</span> the first plugin. Turning INPUT LIVE off keeps its driver reserved</li>
          <li><span class="help__note">MIC</span> is a separate mic / line path. Leave it off when playing guitar through a live plugin. Synths and plugins feed the looper directly; the bar beside MIC shows record level</li>
          <li>The computer keys below are a fallback when no controller is connected</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Looper <span class="help__tag">5 tracks · the instrument</span></h3>
        <ul class="help__list">
          <li>The big ring records a track, then overdubs (layers) onto it once it has a take</li>
          <li><span class="help__note">▶ / ■</span> plays or stops a track &middot; <span class="help__note">CLR</span> clears it (press twice to confirm)</li>
          <li><span class="help__note">↶ DUB</span> undoes the last overdub layer (press again to redo)</li>
          <li><span class="help__note">FX</span> opens a track's effects &middot; <span class="help__note">MUTE</span> silences it &middot; the volume slider has a 0 dB detent at 1.0</li>
          <li><span class="help__note">↺ REV</span> reverses a track in place. Overdub is blocked while reversed</li>
          <li><span class="help__note">⧉ COPY</span> copies a take to the first empty track</li>
          <li>All tracks share one loop, so they stay locked together and can't drift</li>
          <li>A take shorter than the loop repeats across it — stop early, or set the bars with <span class="help__note">FIXED</span></li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Transport &amp; tempo</h3>
        <ul class="help__list">
          <li>A 1-bar count-in (four clicks) leads the first recording. You come in on the counted "1", not the button press</li>
          <li><span class="help__note">CLICK</span> toggles the metronome (its own volume, never recorded, silent while nothing runs)</li>
          <li><span class="help__note">FIXED N</span> records exactly N bars and auto-stops on the downbeat</li>
          <li><span class="help__note">AUTO REC · SENS</span> replaces the first count-in: arm the track, then playing starts the take. Raise sensitivity for quieter input; the cyan tick on the record level is the trigger</li>
          <li>Click the <span class="help__note">BPM</span> to type it, or <span class="help__note">TAP</span> a tempo. Tempo locks to the first loop (clear all to change)</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Looper keys <span class="help__tag">work even with the keyboard hidden</span></h3>
        <ul class="help__list">
          <li><kbd class="help__kbd">1</kbd>–<kbd class="help__kbd">5</kbd> select a track &middot; <kbd class="help__kbd">↑</kbd> <kbd class="help__kbd">↓</kbd> (or <kbd class="help__kbd">PgUp</kbd> <kbd class="help__kbd">PgDn</kbd>, <kbd class="help__kbd">←</kbd> <kbd class="help__kbd">→</kbd>) step to the previous / next one</li>
          <li><kbd class="help__kbd">Space</kbd> records / overdubs the selected track &middot; <kbd class="help__kbd">Enter</kbd> plays / stops it</li>
          <li><kbd class="help__kbd">Backspace</kbd> undoes its last overdub (again to redo) &middot; <kbd class="help__kbd">Delete</kbd> twice clears it</li>
          <li>In drum mode the pads take <kbd class="help__kbd">1</kbd>–<kbd class="help__kbd">4</kbd>, so only <kbd class="help__kbd">5</kbd> selects a track there</li>
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
          <li>Restore a hidden keyboard from the keyboard icon at the far right of the command bar</li>
        </ul>
      </section>

      <section class="help__sec">
        <h3 class="help__h">Session</h3>
        <ul class="help__list">
          <li><span class="help__note">⬇</span> (far right) exports every track + a master mix as WAV, with a session.json, in one zip</li>
          <li><span class="help__note">⬆</span> imports such a zip, only while the looper is empty; the loops are restored locally on reopen anyway</li>
        </ul>
      </section>

      {/* The app's "About box equivalent" for Steinberg's ASIO Usage Guidelines (1e/1f: ASIO is on by
          default, so the unaltered logo must sit here, and only here). Section 14 allows the trademark
          line as plain text beside the logo. Present in every build that links the SDK (the licence
          statement is about the binary), whether or not the driver was started or disabled at launch. */}
      <Show when={asioStatus().status !== 'not-compiled'}>
        <section class="help__sec">
          <h3 class="help__h">About this build</h3>
          <p class="help__about">This build links the Steinberg ASIO® SDK and is licensed GPLv3. The BleepLoop source is MIT.</p>
          <div class="help__asio">
            <img class="help__asio-logo" src={asioLogo} alt="ASIO Compatible" />
            <span>ASIO is a registered trademark of Steinberg Media Technologies GmbH</span>
          </div>
        </section>
      </Show>
    </div>
  );
}
