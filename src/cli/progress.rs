use sdeasyboot::console::{self, ProgressLine};
use sdeasyboot::fastboot::FlashProgress as TransferProgress;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_secs(2);
const PARTITION_WIDTH: usize = 32;

pub(super) struct FlashProgress {
    partition: String,
    started: Instant,
    terminal: bool,
    last_render: Option<Duration>,
    display: ProgressLine,
    name_announced: bool,
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

    fn statistics(&self, result: Option<bool>) -> Option<String> {
        let state = self.state?;
        let completed = state.completed.min(state.total);
        let percent = (u128::from(completed) * 100 / u128::from(state.total.max(1))) as usize;
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
            Some(true) => "done",
            Some(false) => "failed",
            None if completed == state.total => "waiting",
            None => "",
        };
        Some(format!(
            " {percent:3}%  {done:>10} / {total:>10}  {speed:>10}/s  ETA {eta:>8}  {status:>7}",
            done = bytes(completed as f64),
            total = bytes(state.total as f64),
            speed = bytes(speed),
        ))
    }

    fn line(&self, partition: &str, result: Option<bool>) -> Option<String> {
        let state = self.state?;
        let filled = (u128::from(state.completed.min(state.total)) * 24
            / u128::from(state.total.max(1))) as usize;
        let bar = format!("{}{}", "=".repeat(filled), " ".repeat(24 - filled));
        Some(format!(
            "{} [{bar}]{}",
            prefix(partition),
            self.statistics(result)?
        ))
    }
}

fn prefix(partition: &str) -> String {
    let characters: Vec<_> = partition.chars().collect();
    let label = if characters.len() <= PARTITION_WIDTH {
        partition.to_owned()
    } else {
        format!(
            "{}~{}",
            characters[..12].iter().collect::<String>(),
            characters[characters.len() - (PARTITION_WIDTH - 13)..]
                .iter()
                .collect::<String>()
        )
    };
    format!("  Flash {label:<PARTITION_WIDTH$}")
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
        let partition: String = partition
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let display = ProgressLine::new(prefix(&partition));
        let terminal = !display.is_hidden();
        Self {
            partition,
            started: Instant::now(),
            terminal,
            last_render: None,
            display,
            name_announced: false,
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
        let Some(state) = self.meter.state else {
            return;
        };
        if !self.name_announced && self.partition.chars().count() > PARTITION_WIDTH {
            sdeasyboot::statusln!("  Flash {}", self.partition);
            self.name_announced = true;
        }
        if self.terminal {
            let message = self.meter.statistics(result).unwrap();
            self.display.draw(
                state.completed,
                state.total,
                message.clone(),
                console::columns(),
            );
            if result.is_some() {
                self.display.finish();
            }
        } else {
            sdeasyboot::statusln!("{}", self.meter.line(&self.partition, result).unwrap());
        }
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
        assert!(meter.line("rootfs", None).unwrap().contains("--:--"));
        meter.record(Duration::from_secs(1), state(mib, 180 * mib));
        meter.record(Duration::from_secs(10), state(40 * mib, 180 * mib));
        meter.record(Duration::from_secs(12), state(44 * mib, 180 * mib));
        let line = meter.line("rootfs", None).unwrap();
        assert!(
            line.split_whitespace()
                .collect::<Vec<_>>()
                .windows(10)
                .any(|words| words
                    == ["24%", "44.0", "MiB", "/", "180.0", "MiB", "2.0", "MiB/s", "ETA", "01:08"]),
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
            line.split_whitespace()
                .collect::<Vec<_>>()
                .windows(10)
                .any(|words| words
                    == ["75%", "3.0", "KiB", "/", "4.0", "KiB", "1.0", "KiB/s", "ETA", "00:01"]),
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
        assert!(meter.line("boot", Some(false)).unwrap().contains("50%"));
        meter.record(Duration::ZERO, state(100, 100));
        assert!(meter.line("boot", None).unwrap().ends_with("waiting"));
        assert!(meter.line("boot", Some(true)).unwrap().ends_with("done"));
        assert!(meter
            .line("boot", Some(false))
            .unwrap()
            .ends_with(" failed"));
    }

    #[test]
    fn partition_labels_and_statistic_fields_keep_fixed_columns() {
        let gib = 1024 * 1024 * 1024;
        let mut small = Meter::default();
        small.record(Duration::ZERO, state(0, gib));
        small.record(Duration::from_secs(1), state(512, gib));
        let mut large = Meter::default();
        large.record(Duration::ZERO, state(0, 1000 * gib));
        large.record(Duration::from_secs(600), state(gib, 1000 * gib));
        let small_line = small.line("ospi1$da", None).unwrap();
        let large_line = large.line("emmc1$ap2$cluster_rootfs", None).unwrap();
        assert_eq!(small_line.find('['), large_line.find('['));
        assert_eq!(small_line.find(" / "), large_line.find(" / "));
        assert_eq!(small_line.find("/s"), large_line.find("/s"));
        assert_eq!(small_line.find("ETA"), large_line.find("ETA"));
        assert_eq!(small_line.len(), large_line.len());
        let long = prefix("emmc1$a_very_long_partition_name_with_a_distinct_tail");
        assert_eq!(long.chars().count(), prefix("FDA").chars().count());
        assert!(long.contains('~') && long.ends_with("distinct_tail"));
    }
}
