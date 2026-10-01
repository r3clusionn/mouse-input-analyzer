# Mouse input analyzer

Records a mouse through Windows Raw Input and measures its real polling rate, interval jitter, gaps and motion, then writes a self-contained HTML report or a CSV. For anyone who wants to check what a mouse actually delivers, and for reading captures made with MouseTester or MousePlotter.

**Status:** v0.1.0, working. The recorder and report are verified with synthetic input; physical-mouse numbers are not published yet (see Limits).

![HTML report for a 3 second capture of a synthetic 125 Hz signal: polling rate card, interval scatter, histogram, rate, speed and path charts, and a statistics table](docs/images/report.png)

*The screenshot is a capture of a synthetic 125 Hz signal made with `examples/inject.rs`, not a physical mouse.*

## Features

- Locks onto one physical mouse, so a second device cannot mix into the capture.
- Stamps every report with the performance counter the moment Windows delivers it, on a time-critical thread with a 1 ms system timer.
- Reports polling rate, interval mean, median, stdev, min, max, 99th and 99.9th percentile, low 1% and 0.1% rate, and jitter.
- Separates a pause (hand stopped) from a gap (a report arrived late), and tells a delayed report from a lost one by looking for the catch-up burst after it.
- Motion statistics: path length, net movement, straightness, speed in cm/s, speed variation, zero-motion reports.
- Reads and writes the MouseTester CSV layout, and reads MousePlotter CSVs (extra columns are ignored).
- The HTML report has no scripts and no network use, and works offline.

## How to install

Windows 10 or later and a Rust toolchain.

```sh
git clone https://github.com/r3clusionn/mouse-input-analyzer
cd mouse-input-analyzer
cargo install --path .
```

`mia analyze` also builds and runs on other platforms; `record` and `devices` are Windows only.

## How to use

```sh
mia devices
mia record --seconds 10 --cpi 800 --out capture.csv --html report.html
mia analyze capture.csv --html report.html
```

`record` waits for the first mouse that moves, then records for `--seconds`. Move the mouse in
circles and straight strokes at varied speeds for the whole capture; pausing is fine and is not
counted against the mouse. Press Ctrl+C to stop early.

| Option | What it does |
|---|---|
| `--seconds N` | Record length after the first report (default 10). |
| `--cpi N` | Mouse CPI. Only used to convert counts to centimetres. |
| `--device` | `auto` (first to move), `any`, or a handle from `mia devices`. |
| `--out FILE` | Save the capture as CSV. |
| `--html FILE` | Write the HTML report. |
| `--pause-ms N` | Intervals longer than this are pauses. Default is the larger of 10 periods and 20 ms. |

## How it works

`WM_INPUT` messages arrive in a message-only window. The first thing the window procedure does is
read `QueryPerformanceCounter`; the report data is read after. Reports are classified by interval:
under 0.25 periods is bunched, over 1.5 periods is a gap, over the pause threshold is a pause. A gap
of `k` periods implies `k - 1` missing reports, and each one is counted as delayed if a short
interval follows it (the catch-up burst) and lost otherwise.

Compared with the two tools this was built beside: MouseTester contributed the statistics (interval
and frequency percentiles, velocity) and the CSV layout, and MousePlotter the capture practices
(single locked device, time-critical thread, 1 ms timer). No code was copied from either.

## Limits

- The timestamp is delivery time to this process. It includes scheduling delay, so it shows slightly
  worse jitter than the mouse sends. MousePlotter can read USB interrupt times through ETW with
  administrator rights, which is more accurate; that is not implemented here.
- Verified on Windows 11 with synthetic input from `SendInput` (`examples/inject.rs`), whose send
  times were checked to be on schedule (mean 0.002 ms late, slowest call 0.67 ms):
  - 125 Hz: median interval 8.000 ms, stdev 0.258 ms, no gaps.
  - 250, 500 and 1000 Hz: report counts and the mean interval are exact (for example 2001 reports in
    4.00 s at 500 Hz), but Windows delivers about one report in three one period late, in a burst. The
    same result came from a polling loop and a top-level window, so it is not the wait strategy or
    the window type. Treat synthetic input as a check of counts and low rates, not of timing.
- Timing numbers from a physical mouse have not been published yet. Run `mia record` on yours.

## Tests

`cargo test` runs 24 tests: statistics on constructed captures (perfect, jittery, lost report,
delayed report, long pause, zero motion), CSV round trips and both foreign file formats, report
generation, and the binary against a recorded fixture.

## License

MIT (see `LICENSE`).
