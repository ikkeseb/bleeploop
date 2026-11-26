"""Make the three source takes, the sample map and the stereo film master."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import wave

import numpy as np

HERE = Path(__file__).resolve().parent
OUT = Path(os.getenv("PROMO_WORK", HERE / "output"))
SR, FPS, FRAMES, BINS = 44100, 25, 480, 960
BEAT = SR * 3 // 5
LENGTH = BEAT * 16
CUES = [0, 120, 240]
NAMES = ["Picked bass", "Drum machine", "Glass keys"]


def read(name):
    data = subprocess.check_output(["ffmpeg", "-v", "error", "-i", HERE / "inputs" / name,
                                    "-ar", str(SR), "-ac", "1", "-f", "f32le", "-"])
    return np.frombuffer(data, "<f4").astype(float)


def save(name, samples):
    pcm = np.rint(samples * 32767).astype("<i2")
    with wave.open(str(OUT / name), "wb") as f:
        f.setparams((samples.shape[1], 2, SR, 0, "NONE", "uncompressed"))
        f.writeframes(pcm.tobytes())
    return pcm.astype(float) / 32767


def hit(track, signal, start, level=1, pan=0):
    pos = np.arange(len(signal)) + round(start)
    track[pos % LENGTH] += level * signal[:, None] * np.sqrt([1 - pan, 1 + pan])


def build():
    OUT.mkdir(exist_ok=True)
    sample = read("rack-bass.flac")
    assert len(sample) == LENGTH
    sample[-440:] *= np.linspace(1, 0, 440)
    bass = np.column_stack([sample, sample])
    drums = np.zeros((LENGTH, 2))
    keys = drums.copy()
    kit = {k: read(n + ".ogg") for k, n in zip(("kick", "snare", "hat", "open"),
           ("kick-hard-1", "snare-hard-1", "hat-soft-1", "openhat-1"))}
    for bar in range(4):
        base = bar * 4 * BEAT
        for step in ([0, 6, 10] if bar % 2 == 0 else [0, 7, 10, 14]):
            hit(drums, kit["kick"], base + step * BEAT / 4, .85 if step == 0 else .60)
        for step in (4, 12):
            hit(drums, kit["snare"], base + step * BEAT / 4, .64, .08)
        for step in (0, 2, 4, 6, 8, 10, 12, 14, 15):
            open_hat = step == 14 and bar % 2
            hit(drums, kit["open" if open_hat else "hat"], base + step * BEAT / 4,
                .23 if step % 4 == 2 else .12, -.23 if step % 4 else .18)
    # An answering phrase with air between the notes. E minor moves into C.
    t = np.arange(round(1.8 * SR)) / SR
    for bar, pitches in enumerate(((71, 67, 74, 71), (67, 66, 64), (67, 72, 76, 74), (72, 67, 64))):
        for j, midi in enumerate(pitches):
            angle = 2 * np.pi * 440 * 2 ** ((midi - 69) / 12) * t
            voice = np.sin(angle + 2.1 * np.exp(-12 * t) * np.sin(2 * angle))
            voice += .19 * np.sin(2.003 * angle) + .07 * np.sin(4.01 * angle)
            voice *= (1 - np.exp(-180 * t)) * np.exp(-4 * t) * np.minimum(1, (t[-1] - t) * 20)
            pos = (bar * 4 + (0, 1.5, 2.75, 3.5)[j]) * BEAT
            for delay, gain, pan in ((0, .8, -.16), (.75, .23, .55), (1.5, .105, -.60)):
                hit(keys, voice, pos + delay * BEAT, gain, pan)
    mix = np.zeros((LENGTH * 2, 2))
    meta = []
    for i, (track, level) in enumerate(zip((bass, drums, keys), (.54, .51, .32))):
        name = f"take-{i + 1}.wav"
        track = save(name, track * level / np.max(np.abs(track)))
        gain = np.sqrt(np.mean(track**2, axis=1))
        windows = np.array_split(gain, BINS)
        peak = np.array([np.max(w) for w in windows])
        rms = np.array([np.sqrt(np.mean(w**2)) for w in windows])
        meta.append(dict(name=NAMES[i], file=name, sha256=hashlib.sha256((OUT/name).read_bytes()).hexdigest(),
                         entry_frame=CUES[i], peak=(peak/peak.max()).round(7).tolist(),
                         rms=(rms/rms.max()).round(7).tolist()))
        start = CUES[i] * SR // FPS
        mix[start:] += np.tile(track, (2, 1))[start:]
    mix[-int(.34*SR):] *= np.linspace(1, 0, int(.34*SR))[:, None] ** 1.4
    save("master.wav", mix * .93 / np.max(np.abs(mix)))
    (OUT / "map.json").write_text(json.dumps(dict(sr=SR, fps=FPS, frames=FRAMES,
        samples_per_loop=LENGTH, bins=BINS, tracks=meta)))
    print(f"{FRAMES} frames, {len(mix)} stereo samples, take entries {CUES}")


if __name__ == "__main__":
    build()
