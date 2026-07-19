use std::{
    io::{self, IsTerminal, Write},
    time::{Duration, Instant},
};

const TTY_WIDTH: usize = 24;
const TTY_REPORT_INTERVAL: Duration = Duration::from_millis(100);
const PLAIN_REPORT_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ProgressUnit {
    Bytes,
    Files,
    Series,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProgressEstimate {
    pub completed: u64,
    pub total: Option<u64>,
    pub elapsed: Duration,
    pub rate_per_second: Option<f64>,
    pub eta: Option<Duration>,
}

impl ProgressEstimate {
    pub fn at(completed: u64, total: Option<u64>, elapsed: Duration) -> Self {
        Self::with_session_baseline(completed, total, 0, elapsed)
    }

    pub fn with_session_baseline(
        completed: u64,
        total: Option<u64>,
        completed_before_session: u64,
        elapsed: Duration,
    ) -> Self {
        let session_completed = completed.saturating_sub(completed_before_session.min(completed));
        let rate_per_second = (session_completed > 0 && elapsed.as_secs_f64() >= 0.5)
            .then(|| session_completed as f64 / elapsed.as_secs_f64());
        let eta = match (total, rate_per_second) {
            (Some(total), Some(rate)) if total > completed && rate.is_finite() && rate > 0.0 => {
                Some(Duration::from_secs_f64((total - completed) as f64 / rate))
            }
            (Some(total), _) if total <= completed => Some(Duration::ZERO),
            _ => None,
        };
        Self {
            completed,
            total,
            elapsed,
            rate_per_second,
            eta,
        }
    }
}

pub struct Progress {
    label: String,
    unit: ProgressUnit,
    total: Option<u64>,
    completed: u64,
    completed_before_session: u64,
    started: Instant,
    last_plain_report: Instant,
    tty: bool,
    finished: bool,
}

impl Progress {
    pub fn spinner(label: impl Into<String>, unit: ProgressUnit) -> Self {
        Self::new(label, None, unit)
    }

    pub fn bounded(label: impl Into<String>, total: u64, unit: ProgressUnit) -> Self {
        Self::new(label, Some(total), unit)
    }

    fn new(label: impl Into<String>, total: Option<u64>, unit: ProgressUnit) -> Self {
        let now = Instant::now();
        let mut progress = Self {
            label: label.into(),
            unit,
            total,
            completed: 0,
            completed_before_session: 0,
            started: now,
            last_plain_report: now,
            tty: io::stderr().is_terminal(),
            finished: false,
        };
        progress.draw(true);
        progress
    }

    pub fn set(&mut self, completed: u64) {
        self.completed = self.total.map_or(completed, |total| completed.min(total));
        self.draw(false);
    }

    pub fn inc(&mut self, amount: u64) {
        self.set(self.completed.saturating_add(amount));
    }

    /// Add work restored from a durable checkpoint without treating it as
    /// throughput observed by this process. The amount still advances the
    /// visible completed/total counter and progress bar.
    pub fn inc_checkpointed(&mut self, amount: u64) {
        let previous = self.completed;
        self.completed = self.total.map_or_else(
            || previous.saturating_add(amount),
            |total| previous.saturating_add(amount).min(total),
        );
        self.completed_before_session = self
            .completed_before_session
            .saturating_add(self.completed - previous);
        self.draw(false);
    }

    pub fn completed(&self) -> u64 {
        self.completed
    }

    pub fn finish(&mut self) {
        if let Some(total) = self.total {
            self.completed = total;
        }
        self.finished = true;
        self.draw(true);
    }

    pub fn finish_at(&mut self, completed: u64) {
        self.completed = self.total.map_or(completed, |total| completed.min(total));
        self.finished = true;
        self.draw(true);
    }

    fn draw(&mut self, force: bool) {
        let now = Instant::now();
        if !render_due(self.tty, force, now.duration_since(self.last_plain_report)) {
            return;
        }
        let estimate = ProgressEstimate::with_session_baseline(
            self.completed,
            self.total,
            self.completed_before_session,
            now - self.started,
        );
        let line = render_progress(&self.label, self.unit, estimate, self.tty);
        let mut stderr = io::stderr().lock();
        if self.tty {
            let _ = write!(stderr, "\r\x1b[2K{line}");
            if self.finished {
                let _ = writeln!(stderr);
            }
        } else {
            let _ = writeln!(stderr, "{line}");
        }
        let _ = stderr.flush();
        self.last_plain_report = now;
    }
}

fn render_due(tty: bool, force: bool, since_last_render: Duration) -> bool {
    force
        || since_last_render
            >= if tty {
                TTY_REPORT_INTERVAL
            } else {
                PLAIN_REPORT_INTERVAL
            }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if self.tty && !self.finished {
            let mut stderr = io::stderr().lock();
            let _ = writeln!(stderr, "\r\x1b[2K{} interrupted", self.label);
            let _ = stderr.flush();
        }
    }
}

pub fn render_progress(
    label: &str,
    unit: ProgressUnit,
    estimate: ProgressEstimate,
    tty: bool,
) -> String {
    let amount = match estimate.total {
        Some(total) => {
            let percentage = if total == 0 {
                100.0
            } else {
                estimate.completed.min(total) as f64 * 100.0 / total as f64
            };
            format!(
                "{}/{} ({percentage:.1}%)",
                format_amount(estimate.completed, unit),
                format_amount(total, unit)
            )
        }
        None => format_amount(estimate.completed, unit),
    };
    let elapsed = format_duration(estimate.elapsed);
    let rate = estimate
        .rate_per_second
        .map(|rate| format!("{} /s", format_rate(rate, unit)))
        .unwrap_or_else(|| "rate calculating".into());
    let eta = match (estimate.total, estimate.eta) {
        (Some(_), Some(eta)) => format!("ETA {}", format_duration(eta)),
        (Some(_), None) => "ETA calculating".into(),
        (None, _) => format!("elapsed {elapsed}"),
    };
    let bar = if tty {
        estimate.total.map(|total| {
            let filled = if total == 0 {
                TTY_WIDTH
            } else {
                ((estimate.completed.min(total) as u128 * TTY_WIDTH as u128) / total as u128)
                    as usize
            };
            format!(
                " [{}{}]",
                "=".repeat(filled),
                " ".repeat(TTY_WIDTH - filled)
            )
        })
    } else {
        None
    };
    format!(
        "{label}{}  {amount}  {rate}  {eta}",
        bar.unwrap_or_default()
    )
}

fn format_amount(value: u64, unit: ProgressUnit) -> String {
    match unit {
        ProgressUnit::Bytes => format_bytes(value as f64),
        ProgressUnit::Files => format!("{value} files"),
        ProgressUnit::Series => format!("{value} series"),
    }
}

fn format_rate(value: f64, unit: ProgressUnit) -> String {
    match unit {
        ProgressUnit::Bytes => format_bytes(value),
        ProgressUnit::Files => format!("{value:.1} files"),
        ProgressUnit::Series => format!("{value:.2} series"),
    }
}

fn format_bytes(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut scaled = value.max(0.0);
    let mut index = 0;
    while scaled >= 1024.0 && index + 1 < UNITS.len() {
        scaled /= 1024.0;
        index += 1;
    }
    if index == 0 {
        format!("{scaled:.0} {}", UNITS[index])
    } else {
        format!("{scaled:.1} {}", UNITS[index])
    }
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_uses_observed_work_without_inventing_an_initial_rate() {
        let initial = ProgressEstimate::at(0, Some(100), Duration::from_secs(8));
        assert_eq!(initial.rate_per_second, None);
        assert_eq!(initial.eta, None);

        let measured = ProgressEstimate::at(25, Some(100), Duration::from_secs(5));
        assert_eq!(measured.rate_per_second, Some(5.0));
        assert_eq!(measured.eta, Some(Duration::from_secs(15)));
    }

    #[test]
    fn resumed_rate_and_eta_exclude_checkpointed_bytes() {
        let resumed =
            ProgressEstimate::with_session_baseline(75, Some(100), 50, Duration::from_secs(5));

        assert_eq!(resumed.completed, 75);
        assert_eq!(resumed.rate_per_second, Some(5.0));
        assert_eq!(resumed.eta, Some(Duration::from_secs(5)));

        let checkpoint_only =
            ProgressEstimate::with_session_baseline(50, Some(100), 50, Duration::from_secs(5));
        assert_eq!(checkpoint_only.rate_per_second, None);
        assert_eq!(checkpoint_only.eta, None);
    }

    #[test]
    fn render_has_bar_rate_elapsed_and_eta_for_tty() {
        let line = render_progress(
            "Uploading",
            ProgressUnit::Bytes,
            ProgressEstimate::at(
                5 * 1024 * 1024,
                Some(20 * 1024 * 1024),
                Duration::from_secs(2),
            ),
            true,
        );
        assert!(line.contains("[======                  ]"));
        assert!(line.contains("5.0 MiB/20.0 MiB"));
        assert!(line.contains("(25.0%)"));
        assert!(line.contains("2.5 MiB /s"));
        assert!(line.contains("ETA 00:06"));
    }

    #[test]
    fn unbounded_progress_is_honest_about_absent_eta() {
        let line = render_progress(
            "Indexing DICOMs",
            ProgressUnit::Files,
            ProgressEstimate::at(20, None, Duration::from_secs(4)),
            false,
        );
        assert!(!line.contains('['));
        assert!(!line.contains("ETA"));
        assert!(line.contains("elapsed 00:04"));
    }

    #[test]
    fn tty_updates_are_throttled_but_forced_and_final_renders_are_immediate() {
        assert!(!render_due(true, false, Duration::from_millis(99)));
        assert!(render_due(true, false, Duration::from_millis(100)));
        assert!(render_due(true, true, Duration::ZERO));
        assert!(!render_due(false, false, Duration::from_secs(14)));
    }
}
