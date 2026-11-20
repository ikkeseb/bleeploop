/* BLWave: deterministic, believable loop waveforms for the BleepLoop mockups.
   Shared by every direction so the lanes show the same takes; each direction styles them its own way.

   BLWave.make(style, { bars = 2, beats = 4, cols = 1600, seed = 1 })
     -> { cols, peak: Float32Array (0..1, |amplitude| envelope per column),
          rms: Float32Array (0..1, always <= peak), lo: Float32Array (-1..0), hi: Float32Array (0..1) }
     style: 'strum' (rhythm guitar, 8th strums with accents)
            'chug'  (palm-muted 16ths, tight)
            'pick'  (arpeggio, ringing 8ths/16ths)
            'lead'  (sustained notes, bends, a run at the end)
            'bass'  (round quarter and 8th notes)
   BLWave.path(data, w, h, { which = 'peak', mid = h / 2, gain = 0.92 })
     -> an SVG path 'd' for a closed, mirrored (asymmetric lo/hi) body, w x h px, centred on mid.
   BLWave.columns(data, n) -> [{ lo, hi, rms }] n evenly spaced columns, for bar/line drawings on canvas or as rects.
   BLWave.draw(canvas, data, { color, rmsColor, gain }) -> paints peak body and an inner rms body (device-pixel crisp). */
(function (root) {
  function rng(seed) {
    let s = (seed * 2654435761) >>> 0 || 1;
    return function () {
      s ^= s << 13; s >>>= 0; s ^= s >>> 17; s ^= s << 5; s >>>= 0;
      return s / 4294967296;
    };
  }
  // one note: time in beats, level 0..1, decay in beats, attack in beats, sustain floor
  function patterns(style, bars, beats, r) {
    const notes = [];
    const total = bars * beats;
    if (style === 'strum') {
      // D . D U . U D U per bar, accents on 1 and the "and" of 2
      const pat = [[0, 1.0], [1, 0.72], [1.5, 0.55], [2.5, 0.8], [3, 0.7], [3.5, 0.5]];
      for (let b = 0; b < bars; b++) for (const [t, l] of pat) {
        if (t >= beats) continue;
        notes.push({ t: b * beats + t + (r() - 0.5) * 0.02, l: l * (0.88 + r() * 0.2), d: 0.55 + r() * 0.25, a: 0.012, s: 0.16 });
      }
      // chord change rings longer on the last bar
      notes.push({ t: total - 0.5, l: 0.62, d: 0.9, a: 0.012, s: 0.2 });
    } else if (style === 'chug') {
      for (let i = 0; i < total * 4; i++) {
        const onBeat = i % 4 === 0, rest = (i % 16 === 7) || (i % 16 === 15 && r() < 0.5);
        if (rest) continue;
        const open = i % 16 === 12 || i % 16 === 14; // open chord stabs
        notes.push({ t: i / 4 + (r() - 0.5) * 0.01, l: (open ? 0.98 : onBeat ? 0.8 : 0.56) * (0.9 + r() * 0.15), d: open ? 0.5 : 0.11, a: 0.006, s: open ? 0.2 : 0.03 });
      }
    } else if (style === 'pick') {
      const seq = [0.9, 0.5, 0.62, 0.5, 0.74, 0.5, 0.62, 0.55];
      for (let i = 0; i < total * 2; i++) {
        notes.push({ t: i / 2 + (r() - 0.5) * 0.015, l: seq[i % 8] * (0.85 + r() * 0.2), d: 0.42 + r() * 0.2, a: 0.01, s: 0.05 });
      }
    } else if (style === 'lead') {
      let t = 0;
      const lens = [1.5, 0.5, 1, 1, 2, 0.5, 0.5, 0.25, 0.25, 0.25, 0.25];
      let i = 0;
      while (t < total - 0.05) {
        const len = Math.min(lens[i % lens.length], total - t);
        notes.push({ t: t + (r() - 0.5) * 0.015, l: 0.55 + r() * 0.42, d: 0.5 + len * 0.9, a: 0.02, s: 0.24, len: len * (0.82 + r() * 0.16), vib: len >= 1 });
        t += len; i++;
      }
    } else { // bass
      const pat = [[0, 1.0, 0.9], [1.5, 0.7, 0.4], [2, 0.85, 0.9], [3, 0.7, 0.45], [3.5, 0.62, 0.4]];
      for (let b = 0; b < bars; b++) for (const [t, l, len] of pat) {
        if (t >= beats) continue;
        notes.push({ t: b * beats + t + (r() - 0.5) * 0.01, l: l * (0.9 + r() * 0.12), d: len * 1.5, a: 0.018, s: 0.5, len: len });
      }
    }
    return notes;
  }
  function make(style, opt) {
    opt = opt || {};
    const bars = opt.bars || 2, beats = opt.beats || 4, cols = opt.cols || 1600, seed = opt.seed || 1;
    const r = rng(seed + style.length * 97);
    const total = bars * beats;
    const notes = patterns(style, bars, beats, r);
    const env = new Float32Array(cols);
    for (const n of notes) {
      const c0 = Math.floor((n.t / total) * cols);
      const end = n.len ? n.t + n.len : n.t + n.d * 4;
      for (let c = Math.max(0, c0); c < cols; c++) {
        const tb = (c / cols) * total - n.t; // beats since onset
        if (tb < 0) continue;
        if ((c / cols) * total > end + 0.25) break;
        let v = tb < n.a ? tb / n.a : Math.exp(-(tb - n.a) / n.d);
        v = n.s + (1 - n.s) * v;                       // sustain floor
        if (n.len && tb > n.len) v *= Math.exp(-(tb - n.len) / 0.06); // released
        else if (!n.len) v *= Math.exp(-Math.max(0, tb - n.d * 1.4) / (n.d * 0.9));
        if (n.vib) v *= 1 + 0.05 * Math.sin(tb * 34) * Math.min(1, tb * 1.5);
        v *= n.l;
        if (v > env[c]) env[c] = v;                     // louder note masks, as an amp compresses
        else env[c] = Math.min(1, env[c] + v * 0.18);
      }
    }
    // texture: per-column jitter with slower drift, so the body has grain like real audio
    const peak = new Float32Array(cols), rms = new Float32Array(cols), lo = new Float32Array(cols), hi = new Float32Array(cols);
    let drift = 0;
    for (let c = 0; c < cols; c++) {
      drift = drift * 0.86 + (r() - 0.5) * 0.14;
      const e = env[c];
      const j = 0.78 + r() * 0.22 + drift * 0.5;
      const p = Math.max(0, Math.min(1, e * j));
      peak[c] = p;
      rms[c] = p * (0.42 + 0.2 * Math.min(1, e * 1.4) + r() * 0.05);
      const asym = 0.84 + r() * 0.16;                  // real takes are not mirror-symmetric
      const flip = r() < 0.5;
      hi[c] = flip ? p : p * asym;
      lo[c] = -(flip ? p * asym : p);
    }
    // a soft floor of amp hiss between notes
    for (let c = 0; c < cols; c++) {
      const n = 0.012 + r() * 0.012;
      if (peak[c] < n) { peak[c] = n; hi[c] = n; lo[c] = -n; rms[c] = n * 0.6; }
    }
    return { cols: cols, peak: peak, rms: rms, lo: lo, hi: hi, bars: bars, beats: beats, style: style };
  }
  function columns(data, n) {
    const out = [];
    const step = data.cols / n;
    for (let i = 0; i < n; i++) {
      let lo = 0, hi = 0, rm = 0, k = 0;
      for (let c = Math.floor(i * step); c < Math.floor((i + 1) * step) || k === 0; c++) {
        const cc = Math.min(data.cols - 1, c);
        if (data.lo[cc] < lo) lo = data.lo[cc];
        if (data.hi[cc] > hi) hi = data.hi[cc];
        rm += data.rms[cc]; k++;
      }
      out.push({ lo: lo, hi: hi, rms: rm / k });
    }
    return out;
  }
  function path(data, w, h, opt) {
    opt = opt || {};
    const which = opt.which || 'peak', mid = opt.mid == null ? h / 2 : opt.mid, gain = opt.gain == null ? 0.92 : opt.gain;
    const n = Math.max(2, Math.round(w)), cs = columns(data, n), half = (h / 2) * gain;
    let top = '', bot = '';
    for (let i = 0; i < n; i++) {
      const x = (i / (n - 1)) * w;
      const up = which === 'rms' ? cs[i].rms : cs[i].hi;
      const dn = which === 'rms' ? cs[i].rms : -cs[i].lo;
      top += (i ? 'L' : 'M') + x.toFixed(1) + ' ' + (mid - up * half).toFixed(2);
      bot = 'L' + x.toFixed(1) + ' ' + (mid + dn * half).toFixed(2) + bot;
    }
    return top + bot + 'Z';
  }
  function draw(canvas, data, opt) {
    opt = opt || {};
    const dpr = root.devicePixelRatio || 1, w = canvas.clientWidth, h = canvas.clientHeight;
    canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr);
    const g = canvas.getContext('2d'); g.scale(dpr, dpr);
    const cs = columns(data, Math.round(w)), half = (h / 2) * (opt.gain == null ? 0.92 : opt.gain), mid = h / 2;
    g.fillStyle = opt.color || '#45d295';
    for (let i = 0; i < cs.length; i++) g.fillRect(i, mid - cs[i].hi * half, 1, Math.max(1, (cs[i].hi - cs[i].lo) * half));
    if (opt.rmsColor) {
      g.fillStyle = opt.rmsColor;
      for (let i = 0; i < cs.length; i++) g.fillRect(i, mid - cs[i].rms * half, 1, Math.max(1, cs[i].rms * 2 * half));
    }
  }
  root.BLWave = { make: make, columns: columns, path: path, draw: draw };
})(typeof window !== 'undefined' ? window : globalThis);
