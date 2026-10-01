//! Self-contained HTML report: summary, findings and SVG charts, no scripts and no network.

use crate::model::Session;
use crate::stats::{self, Stats};
use std::fmt::Write;

const W: f64 = 640.0;
const H: f64 = 250.0;
const ML: f64 = 54.0;
const MR: f64 = 12.0;
const MT: f64 = 10.0;
const MB: f64 = 38.0;
const MAX_POINTS: usize = 6000;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// Round numbers to put axis ticks on.
pub fn nice_ticks(lo: f64, hi: f64, target: usize) -> Vec<f64> {
    if !lo.is_finite() || !hi.is_finite() || hi <= lo {
        return vec![lo];
    }
    let raw = (hi - lo) / target.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0]
        .iter()
        .map(|m| m * mag)
        .find(|s| *s >= raw)
        .unwrap_or(10.0 * mag);
    let mut v = (lo / step).ceil() * step;
    let mut out = Vec::new();
    while v <= hi + step * 1e-9 {
        out.push(if v.abs() < step * 1e-9 { 0.0 } else { v });
        v += step;
    }
    out
}

fn fmt_num(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-" { "0".into() } else { s.to_string() }
}

struct Axes {
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
}

impl Axes {
    fn px(&self, x: f64) -> f64 {
        ML + (x - self.x0) / (self.x1 - self.x0) * (W - ML - MR)
    }
    fn py(&self, y: f64) -> f64 {
        H - MB - (y - self.y0) / (self.y1 - self.y0) * (H - MB - MT)
    }

    fn frame(&self, xlabel: &str, ylabel: &str, yticks: &[(f64, String)]) -> String {
        let mut s = String::new();
        for t in nice_ticks(self.x0, self.x1, 6) {
            let x = self.px(t);
            let _ = write!(
                s,
                r#"<line class="grid" x1="{x:.1}" y1="{MT}" x2="{x:.1}" y2="{b}"/><text class="tick" x="{x:.1}" y="{ty}" text-anchor="middle">{}</text>"#,
                fmt_num(t),
                b = H - MB,
                ty = H - MB + 14.0
            );
        }
        for (v, label) in yticks {
            let y = self.py(*v);
            let _ = write!(
                s,
                r#"<line class="grid" x1="{ML}" y1="{y:.1}" x2="{r}" y2="{y:.1}"/><text class="tick" x="{lx}" y="{ty:.1}" text-anchor="end">{label}</text>"#,
                r = W - MR,
                lx = ML - 6.0,
                ty = y + 3.5
            );
        }
        let _ = write!(
            s,
            r#"<text class="label" x="{cx}" y="{by}" text-anchor="middle">{}</text><text class="label" transform="translate(12 {cy}) rotate(-90)" text-anchor="middle">{}</text>"#,
            esc(xlabel),
            esc(ylabel),
            cx = ML + (W - ML - MR) / 2.0,
            by = H - 6.0,
            cy = MT + (H - MB - MT) / 2.0
        );
        s
    }
}

fn linear_yticks(y0: f64, y1: f64) -> Vec<(f64, String)> {
    nice_ticks(y0, y1, 5).into_iter().map(|v| (v, fmt_num(v))).collect()
}

fn svg(inner: String, title: &str) -> String {
    format!(
        r#"<figure><svg viewBox="0 0 {W} {H}" role="img" aria-label="{t}"><title>{t}</title>{inner}</svg><figcaption>{t}</figcaption></figure>"#,
        t = esc(title)
    )
}

fn decimate<T: Copy>(pts: &[T], keep: impl Fn(&T) -> bool) -> Vec<T> {
    let stride = pts.len().div_ceil(MAX_POINTS).max(1);
    pts.iter()
        .enumerate()
        .filter(|(i, p)| i % stride == 0 || keep(p))
        .map(|(_, p)| *p)
        .collect()
}

fn scatter(points: &[(f64, f64)], ymax: f64, xlabel: &str, ylabel: &str, title: &str) -> String {
    let x1 = points.last().map_or(1.0, |p| p.0).max(1e-9);
    let a = Axes { x0: 0.0, x1, y0: 0.0, y1: ymax };
    let mut s = a.frame(xlabel, ylabel, &linear_yticks(0.0, ymax));
    for (x, y) in points {
        let clipped = *y > ymax;
        let y = y.min(ymax);
        let _ = write!(
            s,
            r#"<circle class="{}" cx="{:.1}" cy="{:.1}" r="1.3"/>"#,
            if clipped { "dot clip" } else { "dot" },
            a.px(*x),
            a.py(y)
        );
    }
    svg(s, title)
}

fn polyline(points: &[(f64, f64)], ymax: f64, xlabel: &str, ylabel: &str, title: &str) -> String {
    let x1 = points.last().map_or(1.0, |p| p.0).max(1e-9);
    let a = Axes { x0: 0.0, x1, y0: 0.0, y1: ymax.max(1e-9) };
    let mut s = a.frame(xlabel, ylabel, &linear_yticks(0.0, a.y1));
    let mut d = String::new();
    for (i, (x, y)) in points.iter().enumerate() {
        let _ = write!(d, "{}{:.1},{:.1}", if i == 0 { "M" } else { "L" }, a.px(*x), a.py(y.min(a.y1)));
    }
    let _ = write!(s, r#"<path class="line" d="{d}"/>"#);
    svg(s, title)
}

/// Histogram with a log count axis so the tail of rare slow intervals stays visible.
fn histogram(values: &[f64], hi: f64, title: &str) -> String {
    const BINS: usize = 60;
    let mut counts = [0usize; BINS];
    for v in values {
        let b = ((v / hi) * BINS as f64) as usize;
        counts[b.min(BINS - 1)] += 1;
    }
    let max_log = counts.iter().map(|c| ((*c + 1) as f64).log10()).fold(1.0, f64::max);
    let a = Axes { x0: 0.0, x1: hi, y0: 0.0, y1: max_log };
    // Ticks at 0, 1, 10, 100, ... reports, placed where the bars put those counts.
    let mut yticks = vec![(0.0, "0".to_string())];
    for e in 0..=max_log.ceil() as u32 {
        let count = 10u64.pow(e);
        let pos = ((count + 1) as f64).log10();
        if pos <= max_log + 1e-9 {
            yticks.push((pos, count.to_string()));
        }
    }
    let mut s = a.frame("interval (ms)", "reports (log)", &yticks);
    let bw = (W - ML - MR) / BINS as f64;
    for (i, c) in counts.iter().enumerate() {
        if *c == 0 {
            continue;
        }
        let y = a.py(((*c + 1) as f64).log10());
        let _ = write!(
            s,
            r#"<rect class="bar" x="{:.1}" y="{y:.1}" width="{:.1}" height="{:.1}"/>"#,
            ML + i as f64 * bw + 0.5,
            (bw - 1.0).max(0.5),
            (H - MB - y).max(0.0)
        );
    }
    svg(s, title)
}

fn xy_path(session: &Session, title: &str) -> String {
    let (mut x, mut y) = (0.0f64, 0.0f64);
    let mut pts = Vec::with_capacity(session.samples.len());
    for s in &session.samples {
        x += s.dx as f64;
        y += s.dy as f64;
        pts.push((x / session.cpi * 2.54, y / session.cpi * 2.54));
    }
    let pts = decimate(&pts, |_| false);
    let (minx, maxx) = pts.iter().fold((0.0f64, 0.0f64), |a, p| (a.0.min(p.0), a.1.max(p.0)));
    let (miny, maxy) = pts.iter().fold((0.0f64, 0.0f64), |a, p| (a.0.min(p.1), a.1.max(p.1)));
    // Equal scale on both axes so the shape is not distorted.
    let span = (maxx - minx).max(maxy - miny).max(1e-6);
    let (pw, ph) = (W - ML - MR, H - MB - MT);
    let scale = (pw / span).min(ph / span);
    let ox = ML + (pw - (maxx - minx) * scale) / 2.0;
    let oy = MT + (ph - (maxy - miny) * scale) / 2.0;
    let mut d = String::new();
    for (i, (px, py)) in pts.iter().enumerate() {
        let _ = write!(
            d,
            "{}{:.1},{:.1}",
            if i == 0 { "M" } else { "L" },
            ox + (px - minx) * scale,
            oy + (py - miny) * scale
        );
    }
    let mut s = format!(r#"<path class="line" d="{d}"/>"#);
    let _ = write!(
        s,
        r#"<text class="label" x="{}" y="{}" text-anchor="middle">{:.1} cm across, equal axes (y points down)</text>"#,
        ML + pw / 2.0,
        H - 6.0,
        span
    );
    svg(s, title)
}

pub fn html(session: &Session, st: &Stats) -> String {
    let samples = &session.samples;
    let all = stats::intervals(samples);
    let times: Vec<f64> = samples.iter().skip(1).map(|s| s.t_ms / 1000.0).collect();
    let active: Vec<(f64, f64)> = times
        .iter()
        .copied()
        .zip(all.iter().copied())
        .filter(|(_, d)| *d <= st.pause_ms)
        .collect();

    let ymax = (st.interval.p999 * 1.5).max(st.period_ms * 2.0).min(st.pause_ms);
    let pts = decimate(&active, |p| p.1 > st.interval.p99);
    let scatter_svg = scatter(
        &pts,
        ymax,
        "time (s)",
        "interval (ms)",
        "Interval between reports over time (points above the top edge are drawn on it)",
    );

    let hist_vals: Vec<f64> = active.iter().map(|p| p.1).collect();
    let hist_svg = histogram(&hist_vals, ymax, "Interval distribution");

    // Reports per second in 100 ms windows.
    let win = 100.0;
    // The last window is partial, so it is left out rather than drawn as a false drop.
    let nwin = (st.duration_ms / win).floor().max(1.0) as usize;
    let mut counts = vec![0usize; nwin];
    for s in samples {
        if let Some(c) = counts.get_mut((s.t_ms / win) as usize) {
            *c += 1;
        }
    }
    let rate: Vec<(f64, f64)> = counts
        .iter()
        .enumerate()
        .filter(|(_, c)| **c > 0)
        .map(|(i, c)| (i as f64 * win / 1000.0, *c as f64 * 1000.0 / win))
        .collect();
    let rate_max = rate.iter().map(|p| p.1).fold(0.0, f64::max) * 1.1;
    let rate_svg = polyline(&rate, rate_max, "time (s)", "reports per second", "Report rate in 100 ms windows (windows with no reports are skipped)");

    let speed: Vec<(f64, f64)> = stats::speed_bins(samples)
        .into_iter()
        .map(|(t, v)| (t / 1000.0, v * 1000.0 / session.cpi * 2.54))
        .collect();
    let speed_max = speed.iter().map(|p| p.1).fold(0.0, f64::max) * 1.1;
    let speed_svg = polyline(&speed, speed_max, "time (s)", "speed (cm/s)", "Speed in 10 ms bins");
    let path_svg = xy_path(session, "Path of the capture");

    let m = &st.motion;
    let pause_label = format!("Pauses excluded (over {:.0} ms)", st.pause_ms);
    let rows = [
        ("Reports", format!("{}", st.reports)),
        ("Duration", format!("{:.2} s", st.duration_ms / 1000.0)),
        ("Overall rate", format!("{:.1} Hz", st.overall_hz)),
        ("Median rate", format!("{:.1} Hz", st.median_hz)),
        ("Interval mean", format!("{:.4} ms", st.interval.mean)),
        ("Interval median", format!("{:.4} ms", st.interval.median)),
        ("Interval stdev", format!("{:.4} ms", st.interval.stdev)),
        ("Interval min / max", format!("{:.4} / {:.4} ms", st.interval.min, st.interval.max)),
        ("Interval 99th / 99.9th percentile", format!("{:.4} / {:.4} ms", st.interval.p99, st.interval.p999)),
        ("Low 1% / 0.1% rate", format!("{:.0} / {:.0} Hz", 1000.0 / st.interval.p99, 1000.0 / st.interval.p999)),
        ("Jitter (mean consecutive difference)", format!("{:.4} ms", st.jitter_ms)),
        (pause_label.as_str(), format!("{}", st.pauses)),
        ("Gaps", format!("{} ({} delayed, {} lost)", st.gaps.count, st.gaps.delayed, st.gaps.lost)),
        ("Bunched reports", format!("{}", st.bunched)),
        ("Zero-motion reports", format!("{}", m.zero_reports)),
        ("Path length", format!("{:.0} counts, {:.1} cm", m.path_counts, m.path_cm)),
        ("Net movement", format!("{}, {} counts", m.net_x, m.net_y)),
        ("Straightness", format!("{:.3}", m.straightness)),
        ("Speed mean / peak", format!("{:.1} / {:.1} cm/s", m.mean_speed_cm_s, m.peak_speed_cm_s)),
        ("Speed variation (CV)", format!("{:.2}", m.speed_cv)),
        ("Largest report", format!("{:.0} counts", m.max_counts_per_report)),
    ];
    let mut table = String::new();
    for (k, v) in &rows {
        let _ = write!(table, "<tr><th>{}</th><td>{}</td></tr>", esc(k), esc(v));
    }
    let findings: String = stats::findings(st).iter().map(|f| format!("<li>{}</li>", esc(f))).collect();
    let headline = st.nominal_hz.map_or(format!("{:.0} Hz", st.median_hz), |h| format!("{h:.0} Hz"));

    format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Mouse input report</title>
<style>
:root{{--bg:#14161a;--panel:#1d2026;--line:#2c313a;--text:#e6e8eb;--dim:#9aa3af;--accent:#6aa9ff;--warn:#ffb86b}}
@media (prefers-color-scheme:light){{:root{{--bg:#f5f6f8;--panel:#fff;--line:#d9dde3;--text:#1b1f24;--dim:#5b6573;--accent:#1f6feb;--warn:#b35900}}}}
*{{box-sizing:border-box}}
body{{margin:0;background:var(--bg);color:var(--text);font:15px/1.5 system-ui,"Segoe UI",sans-serif}}
main{{max-width:1100px;margin:0 auto;padding:24px 16px 48px}}
h1{{font-size:22px;margin:0}} .sub{{color:var(--dim);margin:2px 0 18px}}
.cards{{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:12px;margin-bottom:16px}}
.card,.panel,figure{{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:12px 14px;margin:0}}
.card b{{display:block;font-size:24px;font-variant-numeric:tabular-nums}} .card span{{color:var(--dim);font-size:13px}}
.charts{{display:grid;align-items:start;grid-template-columns:repeat(auto-fit,minmax(min(100%,460px),1fr));gap:12px;margin:16px 0}}
svg{{width:100%;height:auto;display:block}} figcaption{{color:var(--dim);font-size:13px;margin-top:4px}}
.grid{{stroke:var(--line);stroke-width:1}} .tick{{fill:var(--dim);font-size:10px}} .label{{fill:var(--dim);font-size:11px}}
.dot{{fill:var(--accent);opacity:.55}} .dot.clip{{fill:var(--warn);opacity:.9}} .bar{{fill:var(--accent)}}
.line{{fill:none;stroke:var(--accent);stroke-width:1.3}}
table{{width:100%;border-collapse:collapse;font-variant-numeric:tabular-nums}} th{{text-align:left;font-weight:400;color:var(--dim)}} td{{text-align:right}} th,td{{padding:4px 0;border-bottom:1px solid var(--line)}}
ul{{margin:0;padding-left:20px}}
</style></head><body><main>
<h1>Mouse input report</h1>
<p class="sub">{desc} &middot; {cpi} CPI &middot; {n} reports</p>
<div class="cards">
<div class="card"><b>{headline}</b><span>polling rate</span></div>
<div class="card"><b>{stdev:.3} ms</b><span>interval stdev</span></div>
<div class="card"><b>{p999:.3} ms</b><span>99.9th percentile</span></div>
<div class="card"><b>{gaps}</b><span>gaps</span></div>
<div class="card"><b>{bunched}</b><span>bunched reports</span></div>
</div>
<div class="panel"><ul>{findings}</ul></div>
<div class="charts">{scatter_svg}{hist_svg}{rate_svg}{speed_svg}{path_svg}<div class="panel"><table>{table}</table></div></div>
</main></body></html>
"#,
        desc = esc(&session.description),
        cpi = session.cpi,
        n = st.reports,
        stdev = st.interval.stdev,
        p999 = st.interval.p999,
        gaps = st.gaps.count,
        bunched = st.bunched,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Sample;
    use crate::stats::{analyze, Config};

    fn demo() -> (Session, Stats) {
        let samples: Vec<Sample> = (0..3000)
            .map(|i| Sample {
                dx: (i % 7) - 3,
                dy: (i % 5) - 2,
                t_ms: i as f64 + if i % 2 == 0 { 0.1 } else { -0.1 },
            })
            .collect();
        let s = Session::new("a <b> & \"c\"", 800.0, samples);
        let st = analyze(&s, Config { pause_ms: None }).unwrap();
        (s, st)
    }

    #[test]
    fn ticks_are_round_and_inside_the_range() {
        assert_eq!(nice_ticks(0.0, 10.0, 5), vec![0.0, 2.0, 4.0, 6.0, 8.0, 10.0]);
        let t = nice_ticks(0.0, 1.3, 5);
        assert!(t.iter().all(|v| *v >= 0.0 && *v <= 1.3));
        assert_eq!(nice_ticks(5.0, 5.0, 5), vec![5.0]);
    }

    #[test]
    fn report_has_every_chart_and_escapes_text() {
        let (s, st) = demo();
        let h = html(&s, &st);
        assert_eq!(h.matches("<svg").count(), 5);
        assert!(h.contains("a &lt;b&gt; &amp; &quot;c&quot;"));
        assert!(!h.contains("NaN") && !h.contains("inf"), "non-finite value leaked into the page");
        assert!(!h.contains("<script"));
    }
}
