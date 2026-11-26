# Ring of Takes

A 19.2-second BleepLoop film. A picked-bass phrase, a drum-machine take and a glass-key phrase become concentric pressings on a graphite platen. A carriage reads all three at the playback position. The amber drum and green key pressings lift off, turn upright and dock as the O characters in the closing wordmark. The bass stays on the platen.

Full builds write the two H.264/AAC cuts and the 1920 × 1080 hero to `deliverables/` beside this README. The approved masters belong to the on-disk jobs archive under `bleeploop-ring-of-takes/`.

## Score and geometry

100 BPM, 4/4, four bars per revolution, 25 fps. The film plays the loop twice. Picked bass is present at frame 0. Drums lock at frame 120 (4.80 s), glass keys at frame 240 (9.60 s). Their first appearance in the shot leads their landing by seven frames.

- **Bass** uses the opening four bars of Rack’s *Leave me a part* player take, from `prototype/leave-me-a-part` at `54a4aef`. The score plays Karoryfer’s Black And Blue Basses picked-bass samples (CC0). The included FLAC is the mono, lossless excerpt.
- **Drums** are an original part played with four Rack synthwave one-shots. Rack’s `public/samples/SAMPLES.md` identifies them as public-domain, project-generated sounds.
- **Keys** are an original stereo FM phrase. Its E minor and C voicings follow the bass. A tempo-synced alternating delay wraps around the loop.

Each take contains 423,360 stereo PCM samples per channel. Each of the 960 sections spans 441 samples. For each section, the light-facing radius follows the normalized peak of the stereo magnitude, and the height follows both that peak and the normalized RMS. Frame zero is at twelve o’clock. The read arm moves clockwise and returns to the first sample on frame 240. A short peak/RMS envelope flexes each playing ring and sets the brightness and trail length of its pickup light. The compositor addresses the 960 sections by triangle so the light travels on every output frame.

## Rebuild

From the repository root on Linux:

```sh
craft/ring-of-takes/render.sh --draft  # Score, mesh audit and a quarter-size plate from each camera
craft/ring-of-takes/render.sh          # Both films and the hero
```

The script uses Blender 5.2 Cycles on CPU, Python with NumPy, FFmpeg with libass and x264, a C++17 compiler with OpenMP and libpng, and the installed JetBrains Mono Nerd Font. Instrument Sans and its OFL licence travel with the piece. `PROMO_PYTHON`, `PROMO_BLENDER` and `PROMO_THREADS` select the Python interpreter (default `python3`), Blender executable (default `blender`) and CPU thread count (default 12). The scene loads `/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf`.

Blender renders native-resolution beauty plates and exports each frame’s posed mesh projection. The compositor reprojects adjacent plates through those triangles and rejects surfaces hidden in either view. The phone cut uses its own portrait plates for the extra field of view and shares the matching portion of the wide camera at the same instant. Both receive separately placed take labels and the same stereo master. The closing letters stand upstage throughout the shot. From frame 336 through the final hold, each format uses its own native Cycles exposures throughout the lift and letter reveal. The hero has a dedicated camera and a 48-sample exposure.

Interrupted plate renders resume inside a content-keyed directory under `output/`. The clean camera cache and PCM stems stay there. The mesh audit compares every ring section against the WAV. The delivery check fully decodes both video streams, checks frame count, timing and sample correlation, compares their decoded stereo audio, and checks the hero dimensions.
