//! DEV: the loopback chirp and its offline analysis, shared by the Stage 1 spike
//! (`host/engine_spike.rs`) and the engine probe's lag phase (`engine_io/probe.rs`): a chirp leaves on
//! one output channel, comes back through a cable on an input, and each arrival is found by normalised
//! cross-correlation, refined to a sub-frame position (`docs/plans/native-engine.md` § Stage 1).

pub(crate) const CHIRP_LEN: usize = 64;
pub(crate) const CHIRP_AMP: f32 = 0.5;

/// A 64-frame Hann-windowed linear sweep, 1 → 16 kHz at `rate`: a sharp, unambiguous correlation peak.
pub(crate) fn chirp(rate: u32) -> [f32; CHIRP_LEN] {
    let (f0, f1) = (1_000.0f64, 16_000.0f64);
    let t_len = CHIRP_LEN as f64 / rate as f64;
    let mut c = [0.0f32; CHIRP_LEN];
    for (n, s) in c.iter_mut().enumerate() {
        let t = n as f64 / rate as f64;
        let phase = 2.0 * std::f64::consts::PI * (f0 * t + (f1 - f0) * t * t / (2.0 * t_len));
        let hann = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * n as f64 / (CHIRP_LEN - 1) as f64).cos();
        *s = (CHIRP_AMP as f64 * hann * phase.sin()) as f32;
    }
    c
}

pub(crate) struct Hit {
    pub(crate) pos: f64,
    pub(crate) ncc: f64,
}

/// Normalised cross-correlation of `tmpl` against `sig` at integer offsets `from..to`; the best
/// |ncc| refined to a sub-frame position by a parabola through its neighbours.
pub(crate) fn xcorr_peak(sig: &[f32], tmpl: &[f32], from: i64, to: i64) -> Option<Hit> {
    let t_energy: f64 = tmpl.iter().map(|&v| (v as f64) * (v as f64)).sum();
    let corr = |off: i64| -> Option<(f64, f64)> {
        if off < 0 || off as usize + tmpl.len() > sig.len() {
            return None;
        }
        let seg = &sig[off as usize..off as usize + tmpl.len()];
        let dot: f64 = seg.iter().zip(tmpl).map(|(&a, &b)| a as f64 * b as f64).sum();
        let e: f64 = seg.iter().map(|&a| a as f64 * a as f64).sum();
        Some((dot, if e > 0.0 { dot / (e * t_energy).sqrt() } else { 0.0 }))
    };
    let mut best: Option<(i64, f64, f64)> = None;
    for off in from..to {
        if let Some((dot, ncc)) = corr(off) {
            if best.is_none_or(|b| dot.abs() > b.1.abs()) {
                best = Some((off, dot, ncc));
            }
        }
    }
    let (off, dot, ncc) = best?;
    let (l, r) = (corr(off - 1).map_or(dot, |c| c.0), corr(off + 1).map_or(dot, |c| c.0));
    let (l, c, r) = (l * dot.signum(), dot.abs(), r * dot.signum());
    let denom = l - 2.0 * c + r;
    let delta = if denom.abs() > 1e-12 { (0.5 * (l - r) / denom).clamp(-0.5, 0.5) } else { 0.0 };
    Some(Hit { pos: off as f64 + delta, ncc: ncc.abs() })
}

/// The first strong arrival in `sig[from..to]` (direct), then — for echo runs — the strongest one
/// after it has rung out (the echo). Positions are absolute indices into `sig`.
pub(crate) fn find_arrivals(sig: &[f32], tmpl: &[f32], from: usize, to: usize, echo: bool) -> (Option<Hit>, Option<Hit>) {
    let to = to.min(sig.len());
    if from + CHIRP_LEN >= to {
        return (None, None);
    }
    let win = &sig[from..to];
    let max = win.iter().fold(0.0f32, |m, v| m.max(v.abs()));
    if max <= 0.0 {
        return (None, None);
    }
    let first = win.iter().position(|v| v.abs() >= 0.5 * max).unwrap() + from;
    let direct = xcorr_peak(sig, tmpl, first as i64 - CHIRP_LEN as i64, first as i64 + 8);
    if !echo {
        return (direct, None);
    }
    let Some(d) = direct.as_ref() else { return (None, None) };
    // The direct chirp has rung out 64 frames + 3 ms after its start.
    let after = (d.pos as usize + CHIRP_LEN + 144).min(to);
    let tail = &sig[after..to];
    let Some((i, _)) = tail.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())) else {
        return (direct, None);
    };
    let at = (after + i) as i64;
    let echo_hit = xcorr_peak(sig, tmpl, at - CHIRP_LEN as i64, at + 8);
    (direct, echo_hit)
}

pub(crate) fn median(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return f64::NAN;
    }
    let mut s = xs.to_vec();
    s.sort_by(f64::total_cmp);
    let m = s.len() / 2;
    if s.len() % 2 == 1 { s[m] } else { (s[m - 1] + s[m]) / 2.0 }
}

pub(crate) fn spread(xs: &[f64]) -> f64 {
    let (lo, hi) = xs.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    if xs.is_empty() { f64::NAN } else { hi - lo }
}

/// Least-squares slope of `ys` over `xs`.
pub(crate) fn slope(xs: &[f64], ys: &[f64]) -> f64 {
    let n = xs.len() as f64;
    if xs.len() < 2 {
        return f64::NAN;
    }
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let num: f64 = xs.iter().zip(ys).map(|(x, y)| (x - mx) * (y - my)).sum();
    let den: f64 = xs.iter().map(|x| (x - mx) * (x - mx)).sum();
    if den > 0.0 { num / den } else { f64::NAN }
}
