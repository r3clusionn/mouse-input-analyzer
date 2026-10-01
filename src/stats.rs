//! Polling rate, jitter, gap and motion statistics for a capture.
//!
//! Terms used throughout:
//! - **interval**: time between two consecutive reports, in milliseconds.
//! - **pause**: an interval too long to be a lost report, so the hand simply stopped. Pauses are
//!   left out of every interval statistic. The threshold is `max(10 * period, 20 ms)`.
//! - **gap**: an interval between 1.5 and the pause threshold. It means one or more reports
//!   should have arrived and did not, on time.
//! - **bunched**: a report that arrived less than a quarter period after the previous one. After
//!   a gap this is the catch-up burst, which tells a delayed report from a lost one.

use crate::model::{Sample, Session};

const STANDARD_RATES_HZ: [f64; 8] = [125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 100.0];
const SPEED_BIN_MS: f64 = 10.0;

/// Distribution of one measured quantity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Dist {
    pub n: usize,
    pub mean: f64,
    /// Population standard deviation.
    pub stdev: f64,
    pub median: f64,
    pub min: f64,
    pub max: f64,
    pub p99: f64,
    pub p999: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Gaps {
    /// Intervals classified as gaps.
    pub count: usize,
    /// Reports that should have arrived inside those gaps.
    pub expected_missing: usize,
    /// Of those, reports that showed up late in a catch-up burst.
    pub delayed: usize,
    /// Of those, reports with no catch-up, so probably lost or never sent.
    pub lost: usize,
    pub worst_ms: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Motion {
    pub path_counts: f64,
    pub net_x: i64,
    pub net_y: i64,
    /// Straight-line distance divided by path length. 1.0 is a perfectly straight stroke.
    pub straightness: f64,
    pub zero_reports: usize,
    pub max_counts_per_report: f64,
    pub path_cm: f64,
    pub peak_speed_cm_s: f64,
    pub mean_speed_cm_s: f64,
    /// Coefficient of variation of speed over 10 ms bins that contain motion.
    pub speed_cv: f64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub reports: usize,
    pub duration_ms: f64,
    /// Reports per second over the whole capture.
    pub overall_hz: f64,
    /// 1000 / median interval, pauses excluded.
    pub median_hz: f64,
    /// Standard rate within 5% of `median_hz`, if any.
    pub nominal_hz: Option<f64>,
    pub period_ms: f64,
    pub pause_ms: f64,
    pub pauses: usize,
    pub interval: Dist,
    /// Mean absolute difference between consecutive intervals (RFC 3550 style jitter).
    pub jitter_ms: f64,
    pub gaps: Gaps,
    pub bunched: usize,
    pub motion: Motion,
}

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Override the pause threshold.
    pub pause_ms: Option<f64>,
}

#[derive(Debug, PartialEq)]
pub enum AnalyzeError {
    TooFewReports(usize),
    NoContinuousMotion,
}

impl std::fmt::Display for AnalyzeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnalyzeError::TooFewReports(n) => {
                write!(f, "need at least 10 reports to analyze, got {n}")
            }
            AnalyzeError::NoContinuousMotion => {
                write!(f, "no stretch of continuous motion found; move the mouse steadily")
            }
        }
    }
}

impl std::error::Error for AnalyzeError {}

/// Nearest-rank percentile of an ascending slice, `p` in 0..=100.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    let n = sorted.len();
    let rank = ((p / 100.0) * n as f64).ceil() as usize;
    sorted[rank.clamp(1, n) - 1]
}

pub fn dist(values: &[f64]) -> Dist {
    if values.is_empty() {
        return Dist::default();
    }
    let n = values.len();
    let mean = values.iter().sum::<f64>() / n as f64;
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64;
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    };
    Dist {
        n,
        mean,
        stdev: var.sqrt(),
        median,
        min: sorted[0],
        max: sorted[n - 1],
        p99: percentile(&sorted, 99.0),
        p999: percentile(&sorted, 99.9),
    }
}

/// Intervals between consecutive reports.
pub fn intervals(samples: &[Sample]) -> Vec<f64> {
    samples.windows(2).map(|w| w[1].t_ms - w[0].t_ms).collect()
}

/// Speed in counts per millisecond over fixed bins, only for bins that contain a report.
/// Returns `(bin start in ms, counts per ms)`.
pub fn speed_bins(samples: &[Sample]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut cur: Option<(i64, f64, f64)> = None;
    for s in samples {
        let bin = (s.t_ms / SPEED_BIN_MS).floor() as i64;
        match &mut cur {
            Some((b, sx, sy)) if *b == bin => {
                *sx += s.dx as f64;
                *sy += s.dy as f64;
            }
            _ => {
                if let Some((b, sx, sy)) = cur.take() {
                    out.push((b as f64 * SPEED_BIN_MS, sx.hypot(sy) / SPEED_BIN_MS));
                }
                cur = Some((bin, s.dx as f64, s.dy as f64));
            }
        }
    }
    if let Some((b, sx, sy)) = cur {
        out.push((b as f64 * SPEED_BIN_MS, sx.hypot(sy) / SPEED_BIN_MS));
    }
    out
}

fn counts_per_ms_to_cm_s(v: f64, cpi: f64) -> f64 {
    v * 1000.0 / cpi * 2.54
}

pub fn analyze(session: &Session, cfg: Config) -> Result<Stats, AnalyzeError> {
    let samples = &session.samples;
    if samples.len() < 10 {
        return Err(AnalyzeError::TooFewReports(samples.len()));
    }
    let all = intervals(samples);
    let duration_ms = samples[samples.len() - 1].t_ms - samples[0].t_ms;

    // Pass 1: a rough period from intervals below 50 ms, so that long pauses cannot hide it.
    let mut quick: Vec<f64> = all.iter().copied().filter(|d| *d > 0.0 && *d < 50.0).collect();
    if quick.len() < 5 {
        return Err(AnalyzeError::NoContinuousMotion);
    }
    quick.sort_by(|a, b| a.total_cmp(b));
    let rough_period = percentile(&quick, 50.0);
    let pause_ms = cfg.pause_ms.unwrap_or_else(|| (10.0 * rough_period).max(20.0));

    // Pass 2: everything below the pause threshold is an interval worth measuring.
    let active: Vec<f64> = all.iter().copied().filter(|d| *d <= pause_ms).collect();
    let pauses = all.len() - active.len();
    if active.len() < 5 {
        return Err(AnalyzeError::NoContinuousMotion);
    }
    let interval = dist(&active);
    let period_ms = interval.median;
    let median_hz = if period_ms > 0.0 { 1000.0 / period_ms } else { 0.0 };
    let nominal_hz = STANDARD_RATES_HZ
        .iter()
        .copied()
        .find(|r| (median_hz - r).abs() / r <= 0.05);

    let jitter_ms = if active.len() > 1 {
        active.windows(2).map(|w| (w[1] - w[0]).abs()).sum::<f64>() / (active.len() - 1) as f64
    } else {
        0.0
    };

    // Gaps and bunching, walking the original order so catch-up bursts line up with their gap.
    let gap_min = 1.5 * period_ms;
    let bunch_max = 0.25 * period_ms;
    let mut gaps = Gaps::default();
    let mut bunched = 0;
    for (i, d) in all.iter().copied().enumerate() {
        if d < bunch_max {
            bunched += 1;
        }
        if d > gap_min && d <= pause_ms {
            let missing = (d / period_ms).round() as usize - 1;
            if missing == 0 {
                continue;
            }
            let catch_up = all[i + 1..]
                .iter()
                .take(missing)
                .filter(|n| **n < 0.5 * period_ms)
                .count();
            gaps.count += 1;
            gaps.expected_missing += missing;
            gaps.delayed += catch_up;
            gaps.lost += missing - catch_up;
            gaps.worst_ms = gaps.worst_ms.max(d);
        }
    }

    Ok(Stats {
        reports: samples.len(),
        duration_ms,
        overall_hz: if duration_ms > 0.0 {
            (samples.len() - 1) as f64 / (duration_ms / 1000.0)
        } else {
            0.0
        },
        median_hz,
        nominal_hz,
        period_ms,
        pause_ms,
        pauses,
        interval,
        jitter_ms,
        gaps,
        bunched,
        motion: motion(session),
    })
}

fn motion(session: &Session) -> Motion {
    let samples = &session.samples;
    let (mut path, mut nx, mut ny, mut zero, mut max_c) = (0.0f64, 0i64, 0i64, 0usize, 0.0f64);
    for s in samples {
        let c = (s.dx as f64).hypot(s.dy as f64);
        path += c;
        nx += s.dx as i64;
        ny += s.dy as i64;
        if s.dx == 0 && s.dy == 0 {
            zero += 1;
        }
        max_c = max_c.max(c);
    }
    let straight = if path > 0.0 {
        (nx as f64).hypot(ny as f64) / path
    } else {
        0.0
    };
    let speeds: Vec<f64> = speed_bins(samples).into_iter().map(|(_, v)| v).collect();
    let d = dist(&speeds);
    Motion {
        path_counts: path,
        net_x: nx,
        net_y: ny,
        straightness: straight,
        zero_reports: zero,
        max_counts_per_report: max_c,
        path_cm: path / session.cpi * 2.54,
        peak_speed_cm_s: counts_per_ms_to_cm_s(d.max, session.cpi),
        mean_speed_cm_s: counts_per_ms_to_cm_s(d.mean, session.cpi),
        speed_cv: if d.mean > 0.0 { d.stdev / d.mean } else { 0.0 },
    }
}

/// Plain-language findings, most important first.
pub fn findings(s: &Stats) -> Vec<String> {
    let mut out = Vec::new();
    match s.nominal_hz {
        Some(hz) => out.push(format!(
            "Polling rate looks like {hz:.0} Hz (median interval {:.3} ms).",
            s.period_ms
        )),
        None => out.push(format!(
            "Median interval {:.3} ms ({:.0} Hz) does not match a standard polling rate.",
            s.period_ms, s.median_hz
        )),
    }
    let rel = s.interval.stdev / s.period_ms;
    out.push(format!(
        "Interval spread: stdev {:.3} ms ({:.0}% of the period), 99.9th percentile {:.3} ms.",
        s.interval.stdev,
        rel * 100.0,
        s.interval.p999
    ));
    if s.gaps.count == 0 {
        out.push("No gaps: every report arrived within 1.5 periods of the last.".into());
    } else {
        out.push(format!(
            "{} gaps, worst {:.2} ms: {} reports late in a catch-up burst, {} never caught up.",
            s.gaps.count, s.gaps.worst_ms, s.gaps.delayed, s.gaps.lost
        ));
    }
    if s.bunched > 0 {
        out.push(format!(
            "{} reports arrived bunched (under a quarter period apart): delayed dispatch, not new motion.",
            s.bunched
        ));
    }
    out.push(
        "Timestamps are taken when Windows delivers each report, so dispatch delay is included."
            .into(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config {
        Config { pause_ms: None }
    }

    /// Reports at a fixed period with one count each.
    fn steady(n: usize, period_ms: f64) -> Vec<Sample> {
        (0..n)
            .map(|i| Sample {
                dx: 1,
                dy: 0,
                t_ms: i as f64 * period_ms,
            })
            .collect()
    }

    fn session(samples: Vec<Sample>) -> Session {
        Session::new("t", 800.0, samples)
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<f64> = (1..=100).map(|i| i as f64).collect();
        assert_eq!(percentile(&v, 99.0), 99.0);
        assert_eq!(percentile(&v, 99.9), 100.0);
        assert_eq!(percentile(&v, 50.0), 50.0);
        assert_eq!(percentile(&[7.0], 99.0), 7.0);
    }

    #[test]
    fn dist_matches_hand_computation() {
        let d = dist(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(d.mean, 2.5);
        assert_eq!(d.median, 2.5);
        assert!((d.stdev - 1.118_033_988).abs() < 1e-6);
        assert_eq!((d.min, d.max), (1.0, 4.0));
    }

    #[test]
    fn perfect_1000hz_has_no_jitter_or_gaps() {
        let s = analyze(&session(steady(2000, 1.0)), cfg()).unwrap();
        assert_eq!(s.nominal_hz, Some(1000.0));
        assert!(s.interval.stdev < 1e-9);
        assert!(s.jitter_ms < 1e-9);
        assert_eq!((s.gaps.count, s.bunched, s.pauses), (0, 0, 0));
        assert!((s.overall_hz - 1000.0).abs() < 1.0);
    }

    #[test]
    fn detects_125hz() {
        let s = analyze(&session(steady(500, 8.0)), cfg()).unwrap();
        assert_eq!(s.nominal_hz, Some(125.0));
    }

    #[test]
    fn a_lost_report_is_a_gap_with_no_catch_up() {
        let mut v = steady(1000, 1.0);
        v.remove(500); // one report missing
        let s = analyze(&session(v), cfg()).unwrap();
        assert_eq!(s.gaps.count, 1);
        assert_eq!(s.gaps.expected_missing, 1);
        assert_eq!(s.gaps.lost, 1);
        assert_eq!(s.gaps.delayed, 0);
        assert!((s.gaps.worst_ms - 2.0).abs() < 1e-9);
    }

    #[test]
    fn a_late_report_with_a_catch_up_burst_is_delayed_not_lost() {
        // Reports 500..=503 are held back and delivered together just after t = 503.
        let mut v = steady(1000, 1.0);
        for (k, s) in v[500..=503].iter_mut().enumerate() {
            s.t_ms = 503.0 + k as f64 * 0.01;
        }
        let s = analyze(&session(v), cfg()).unwrap();
        assert_eq!(s.gaps.count, 1, "{:?}", s.gaps);
        assert_eq!(s.gaps.expected_missing, 3);
        assert_eq!((s.gaps.delayed, s.gaps.lost), (3, 0));
        assert_eq!(s.bunched, 3);
    }

    #[test]
    fn long_pause_is_not_a_gap_and_not_in_the_stats() {
        let mut v = steady(500, 1.0);
        let shift = 400.0;
        v.extend((0..500).map(|i| Sample {
            dx: 1,
            dy: 0,
            t_ms: 500.0 + shift + i as f64,
        }));
        let s = analyze(&session(v), cfg()).unwrap();
        assert_eq!(s.pauses, 1);
        assert_eq!(s.gaps.count, 0);
        assert!(s.interval.max < 1.01);
    }

    #[test]
    fn jitter_shows_up_in_spread_and_percentiles() {
        let mut v = steady(2000, 1.0);
        // Alternate each timestamp by +-0.2 ms.
        for (i, s) in v.iter_mut().enumerate() {
            s.t_ms += if i % 2 == 0 { 0.2 } else { -0.2 };
        }
        v[0].t_ms = 0.0;
        let s = analyze(&session(v), cfg()).unwrap();
        assert!(s.interval.stdev > 0.3 && s.interval.stdev < 0.5);
        assert!(s.interval.p999 > 1.3);
        assert!((s.jitter_ms - 0.8).abs() < 0.1);
    }

    #[test]
    fn motion_numbers_for_a_straight_stroke() {
        // 1000 reports of 2 counts in x over one second, at 800 CPI.
        let v: Vec<Sample> = (0..1000)
            .map(|i| Sample {
                dx: 2,
                dy: 0,
                t_ms: i as f64,
            })
            .collect();
        let s = analyze(&session(v), cfg()).unwrap();
        assert_eq!(s.motion.net_x, 2000);
        assert!((s.motion.straightness - 1.0).abs() < 1e-12);
        assert!((s.motion.path_cm - 2000.0 / 800.0 * 2.54).abs() < 1e-9);
        // 2 counts per ms = 2000 counts/s = 2.5 in/s = 6.35 cm/s.
        assert!((s.motion.mean_speed_cm_s - 6.35).abs() < 0.01);
        assert!(s.motion.speed_cv < 0.01);
        assert_eq!(s.motion.zero_reports, 0);
    }

    #[test]
    fn zero_motion_reports_are_counted() {
        let mut v = steady(100, 1.0);
        v[10].dx = 0;
        v[20].dx = 0;
        let s = analyze(&session(v), cfg()).unwrap();
        assert_eq!(s.motion.zero_reports, 2);
    }

    #[test]
    fn rejects_short_and_pause_only_captures() {
        assert_eq!(
            analyze(&session(steady(5, 1.0)), cfg()).unwrap_err(),
            AnalyzeError::TooFewReports(5)
        );
        let sparse: Vec<Sample> = (0..20)
            .map(|i| Sample {
                dx: 1,
                dy: 0,
                t_ms: i as f64 * 500.0,
            })
            .collect();
        assert_eq!(
            analyze(&session(sparse), cfg()).unwrap_err(),
            AnalyzeError::NoContinuousMotion
        );
    }

    #[test]
    fn findings_mention_the_rate() {
        let s = analyze(&session(steady(500, 1.0)), cfg()).unwrap();
        assert!(findings(&s)[0].contains("1000 Hz"));
    }
}
