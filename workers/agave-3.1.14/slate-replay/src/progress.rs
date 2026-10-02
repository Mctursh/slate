use std::io::{IsTerminal, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

static OPEN: AtomicBool = AtomicBool::new(false);

pub fn clear_line() {
    if OPEN.swap(false, Ordering::Relaxed) {
        ::std::eprint!("\r\x1b[2K");
    }
}

#[derive(Clone, Copy)]
pub enum Unit {
    Count,
    Bytes,
}

pub struct Progress {
    label: String,
    total: Option<u64>,
    unit: Unit,
    done: u64,
    start: Instant,
    last: Option<Instant>,
    quiet_for: Duration,
    tty: bool,
    open_line: bool,
}

impl Progress {
    pub fn new(label: impl Into<String>, total: Option<u64>, unit: Unit) -> Self {
        let mut p = Self::build(label, total, unit, Duration::ZERO);
        p.draw();
        p
    }

    pub fn lazy(label: impl Into<String>, total: Option<u64>, unit: Unit) -> Self {
        Self::build(label, total, unit, Duration::from_secs(2))
    }

    fn build(
        label: impl Into<String>,
        total: Option<u64>,
        unit: Unit,
        quiet_for: Duration,
    ) -> Self {
        Self {
            label: label.into(),
            total,
            unit,
            done: 0,
            start: Instant::now(),
            last: None,
            quiet_for,
            tty: std::io::stderr().is_terminal(),
            open_line: false,
        }
    }

    pub fn done(&self) -> u64 {
        self.done
    }

    pub fn add(&mut self, n: u64) {
        self.set(self.done + n);
    }

    pub fn set(&mut self, done: u64) {
        self.done = done;
        let now = Instant::now();
        if now.duration_since(self.start) < self.quiet_for {
            return;
        }
        let every = if self.tty {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(30)
        };
        if self.last.is_none_or(|t| now.duration_since(t) >= every) {
            self.draw();
        }
    }

    pub fn finish(mut self) {
        if self.last.is_none() {
            return;
        }
        let line = format!(
            "{}: done, {} in {}",
            self.label,
            self.amount(self.done),
            duration(self.start.elapsed())
        );
        self.emit(&line);
        self.close_line();
    }

    fn draw(&mut self) {
        let line = self.line();
        self.emit(&line);
        self.last = Some(Instant::now());
    }

    fn emit(&mut self, line: &str) {
        if self.tty {
            ::std::eprint!("\r\x1b[2K{line}");
            self.open_line = true;
            OPEN.store(true, Ordering::Relaxed);
        } else {
            ::std::eprintln!("{line}");
        }
    }

    fn close_line(&mut self) {
        if self.open_line {
            self.open_line = false;
            if OPEN.swap(false, Ordering::Relaxed) {
                ::std::eprintln!();
            }
        }
    }

    fn line(&self) -> String {
        let secs = self.start.elapsed().as_secs_f64();
        let rate = if secs > 0.0 {
            self.done as f64 / secs
        } else {
            0.0
        };
        let rate_text = match self.unit {
            Unit::Bytes => format!("{:.0} MB/s", rate / 1e6),
            Unit::Count => format!("{}/s", group(rate as u64)),
        };
        match self.total {
            Some(total) if total > 0 => {
                let pct = 100.0 * self.done as f64 / total as f64;
                let left = if rate > 0.0 && self.done > 0 {
                    format!(
                        ", ~{} left",
                        duration(Duration::from_secs_f64(
                            (total.saturating_sub(self.done)) as f64 / rate
                        ))
                    )
                } else {
                    String::new()
                };
                format!(
                    "{}: {}/{} ({pct:.0}%), {rate_text}{left}",
                    self.label,
                    self.bare(self.done),
                    self.amount(total)
                )
            }
            _ => format!("{}: {}, {rate_text}", self.label, self.amount(self.done)),
        }
    }

    fn amount(&self, n: u64) -> String {
        match self.unit {
            Unit::Bytes => format!("{:.1} GB", n as f64 / 1e9),
            Unit::Count => group(n),
        }
    }

    fn bare(&self, n: u64) -> String {
        match self.unit {
            Unit::Bytes => format!("{:.1}", n as f64 / 1e9),
            Unit::Count => group(n),
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        self.close_line();
    }
}

// Starts its clock on the first read, so a reader opened long before it is consumed reports a true rate.
pub struct ProgressReader<R> {
    inner: R,
    label: String,
    total: Option<u64>,
    progress: Option<Progress>,
}

impl<R> ProgressReader<R> {
    pub fn new(inner: R, label: impl Into<String>, total: Option<u64>) -> Self {
        Self {
            inner,
            label: label.into(),
            total,
            progress: None,
        }
    }
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        let (label, total) = (&self.label, self.total);
        self.progress
            .get_or_insert_with(|| Progress::lazy(label.clone(), total, Unit::Bytes))
            .add(n as u64);
        Ok(n)
    }
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped_by_thousands() {
        assert_eq!(group(0), "0");
        assert_eq!(group(999), "999");
        assert_eq!(group(1_000), "1,000");
        assert_eq!(group(1_097_283), "1,097,283");
    }

    #[test]
    fn durations_read_at_the_right_scale() {
        assert_eq!(duration(Duration::from_secs(42)), "42s");
        assert_eq!(duration(Duration::from_secs(125)), "2m 5s");
        assert_eq!(duration(Duration::from_secs(3_900)), "1h 5m");
    }

    #[test]
    fn a_progress_reader_passes_every_byte_through() {
        let data = vec![7u8; 100_000];
        let mut r = ProgressReader::new(&data[..], "test", Some(data.len() as u64));
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
        assert_eq!(r.progress.as_ref().map(Progress::done), Some(100_000));
    }
}
