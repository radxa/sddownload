use sdeasyboot::fastboot::FlashProgress as TransferProgress;
use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_secs(2);

pub(super) struct FlashProgress {
    partition: String,
    started: Instant,
    terminal: bool,
    last_render: Option<Duration>,
    width: usize,
    meter: Meter,
    finished: bool,
}

#[derive(Default)]
struct Meter {
    state: Option<TransferProgress>,
    samples: VecDeque<(Duration, TransferProgress)>,
}

impl Meter {
    fn record(&mut self, elapsed: Duration, state: TransferProgress) {
        self.state = Some(state);
        self.samples.push_back((elapsed, state));
        let cutoff = elapsed.saturating_sub(WINDOW);
        while self.samples.len() > 2 && self.samples[1].0 <= cutoff {
            self.samples.pop_front();
        }
    }

    fn rates(&self) -> (f64, f64) {
        let Some((start, first)) = self.samples.front() else {
            return (0.0, 0.0);
        };
        let (end, last) = self.samples.back().unwrap();
        let seconds = end.saturating_sub(*start).as_secs_f64();
        if seconds == 0.0 {
            return (0.0, 0.0);
        }
        (
            last.transferred.saturating_sub(first.transferred) as f64 / seconds,
            last.completed.saturating_sub(first.completed) as f64 / seconds,
        )
    }

    fn line(&self, partition: &str, result: Option<bool>) -> Option<String> {
        let state = self.state?;
        let completed = state.completed.min(state.total);
        let percent = (u128::from(completed) * 100 / u128::from(state.total.max(1))) as usize;
        let filled = percent * 16 / 100;
        let bar = format!("{}{}", "=".repeat(filled), " ".repeat(16 - filled));
        let (speed, logical_speed) = self.rates();
        let eta = if completed == state.total {
            "00:00".into()
        } else if logical_speed > 0.0 {
            let seconds = ((state.total - completed) as f64 / logical_speed).ceil() as u64;
            format!("{:02}:{:02}", seconds / 60, seconds % 60)
        } else {
            "--:--".into()
        };
        let status = match result {
            Some(true) => " done",
            Some(false) => " failed",
            None if completed == state.total => " waiting for device",
            None => "",
        };
        Some(format!(
            "Flash {partition} [{bar}] {percent:3}% {}/{} {}/s ETA {eta}{status}",
            bytes(completed as f64),
            bytes(state.total as f64),
            bytes(speed),
        ))
    }
}

fn bytes(mut value: f64) -> String {
    let mut unit = "B";
    for next in ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"] {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    if unit == "B" {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}

impl FlashProgress {
    pub(super) fn new(partition: &str) -> Self {
        Self {
            partition: partition
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect(),
            started: Instant::now(),
            terminal: io::stderr().is_terminal(),
            last_render: None,
            width: 0,
            meter: Meter::default(),
            finished: false,
        }
    }

    pub(super) fn update(&mut self, state: TransferProgress) {
        let elapsed = self.started.elapsed();
        self.meter.record(elapsed, state);
        let interval = if self.terminal {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        if self
            .last_render
            .is_none_or(|last| elapsed.saturating_sub(last) >= interval)
        {
            self.render(None);
            self.last_render = Some(elapsed);
        }
    }

    fn render(&mut self, result: Option<bool>) {
        let Some(line) = self.meter.line(&self.partition, result) else {
            return;
        };
        let mut output = io::stderr().lock();
        if self.terminal {
            let padding = " ".repeat(self.width.saturating_sub(line.len()));
            let _ = write!(output, "\r{line}{padding}");
            if result.is_some() {
                let _ = writeln!(output);
            }
            self.width = line.len();
        } else {
            let _ = writeln!(output, "{line}");
        }
        let _ = output.flush();
    }

    pub(super) fn finish(&mut self, success: bool) {
        if !self.finished {
            self.render(Some(success));
            self.finished = true;
        }
    }
}

impl Drop for FlashProgress {
    fn drop(&mut self) {
        self.finish(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(completed: u64, total: u64) -> TransferProgress {
        TransferProgress {
            completed,
            total,
            transferred: completed,
        }
    }

    #[test]
    fn speed_sizes_and_eta_use_recent_samples() {
        let mib = 1024 * 1024;
        let mut meter = Meter::default();
        meter.record(Duration::ZERO, state(0, 180 * mib));
        assert!(meter.line("rootfs", None).unwrap().contains("ETA --:--"));
        meter.record(Duration::from_secs(1), state(mib, 180 * mib));
        meter.record(Duration::from_secs(10), state(40 * mib, 180 * mib));
        meter.record(Duration::from_secs(12), state(44 * mib, 180 * mib));
        let line = meter.line("rootfs", None).unwrap();
        assert!(
            line.contains("44.0 MiB/180.0 MiB 2.0 MiB/s ETA 01:08"),
            "{line}"
        );
    }

    #[test]
    fn sparse_holes_do_not_inflate_usb_speed() {
        let mut meter = Meter::default();
        meter.record(Duration::ZERO, state(0, 4096));
        meter.record(
            Duration::from_secs(1),
            TransferProgress {
                completed: 3072,
                total: 4096,
                transferred: 1024,
            },
        );
        let line = meter.line("rootfs", None).unwrap();
        assert!(
            line.contains("75% 3.0 KiB/4.0 KiB 1.0 KiB/s ETA 00:01"),
            "{line}"
        );
    }

    #[test]
    fn completion_waits_for_success_and_failure_keeps_position() {
        let mut meter = Meter::default();
        meter.record(Duration::ZERO, state(50, 100));
        assert!(meter
            .line("boot", Some(false))
            .unwrap()
            .ends_with(" failed"));
        assert!(meter
            .line("boot", Some(false))
            .unwrap()
            .contains("50% 50 B/100 B"));
        meter.record(Duration::ZERO, state(100, 100));
        assert!(meter
            .line("boot", None)
            .unwrap()
            .ends_with("ETA 00:00 waiting for device"));
        assert!(meter
            .line("boot", Some(true))
            .unwrap()
            .ends_with("ETA 00:00 done"));
        assert!(meter
            .line("boot", Some(false))
            .unwrap()
            .ends_with(" failed"));
    }
}
