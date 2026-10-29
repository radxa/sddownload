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
    finished_elapsed: Option<Duration>,
}

impl Meter {
    fn finish_at(&mut self, elapsed: Duration) {
        self.finished_elapsed = Some(elapsed);
    }

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
        let (recent_speed, logical_speed) = self.rates();
        let (speed, speed_label, time_label, time) = if result.is_some() {
            let elapsed = self.finished_elapsed.unwrap_or_else(|| {
                self.samples
                    .back()
                    .map(|(elapsed, _)| *elapsed)
                    .unwrap_or_default()
            });
            let seconds = elapsed.as_secs_f64();
            let average = if seconds > 0.0 {
                state.transferred as f64 / seconds
            } else {
                0.0
            };
            let time = format!(
                "{:02}:{:02}.{:03}",
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60,
                elapsed.subsec_millis()
            );
            (average, "AVG", "TIME", time)
        } else {
            let eta = if completed == state.total {
                "00:00".into()
            } else if logical_speed > 0.0 {
                let seconds = ((state.total - completed) as f64 / logical_speed).ceil() as u64;
                format!("{:02}:{:02}", seconds / 60, seconds % 60)
            } else {
                "--:--".into()
            };
            (recent_speed, "NOW", "ETA", eta)
        };
        let status = match result {
            Some(true) => "done",
            Some(false) => "failed",
            None if completed == state.total => "waiting",
            None => "",
        };
        let sizes = if result == Some(true) {
            format!("{:>10}", bytes(completed as f64))
        } else {
            format!(
                "{:>10} / {:>10}",
                bytes(completed as f64),
                bytes(state.total as f64)
            )
        };
        // ETA reserves the four positions occupied by TIME's .mmm suffix.
        let (time_width, fraction_padding) = if result.is_some() {
            (12, "")
        } else {
            (8, "    ")
        };
        Some(format!(
            " {percent:3}%  {sizes:<23}  {speed_label} {speed:>10}/s  {time_label:<4} {time:>time_width$}{fraction_padding}  {status:>7}",
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
            // Capture the whole partition operation, including time spent
            // waiting for input, verification, and the final device response.
            self.meter.finish_at(self.started.elapsed());
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
                .windows(11)
                .any(|words| words
                    == [
                        "24%", "44.0", "MiB", "/", "180.0", "MiB", "NOW", "2.0", "MiB/s", "ETA",
                        "01:08"
                    ]),
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
                .windows(11)
                .any(|words| words
                    == [
                        "75%", "3.0", "KiB", "/", "4.0", "KiB", "NOW", "1.0", "KiB/s", "ETA",
                        "00:01"
                    ]),
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

    #[test]
    fn completed_summary_uses_average_and_whole_partition_elapsed_time() {
        let mib = 1024 * 1024;
        let mut meter = Meter::default();
        // Include initial input/header work and a six-second final device wait,
        // rather than measuring only the interval between transfer callbacks.
        meter.record(Duration::from_secs(2), state(0, 100 * mib));
        meter.record(Duration::from_secs(4), state(100 * mib, 100 * mib));
        let live = meter.statistics(None).unwrap();
        assert!(
            live.contains("NOW") && live.contains("50.0 MiB/s"),
            "{live}"
        );
        meter.finish_at(Duration::from_secs(10));
        let summary = meter.statistics(Some(true)).unwrap();
        assert!(
            summary
                .split_whitespace()
                .collect::<Vec<_>>()
                .windows(6)
                .any(|fields| fields == ["AVG", "10.0", "MiB/s", "TIME", "00:10.000", "done"]),
            "{summary}"
        );
        assert!(!summary.contains("ETA"));
        assert!(live.contains(" / "));
        assert!(!summary.contains(" / "));
        assert_eq!(summary.matches("100.0 MiB").count(), 1);
        assert_eq!(live.find("NOW"), summary.find("AVG"));
        assert_eq!(live.find("ETA"), summary.find("TIME"));
        assert_eq!(live.find("00:00"), summary.find("00:10.000"));
        assert_eq!(live.len(), summary.len());
    }

    #[test]
    fn completed_summary_preserves_subsecond_time_and_handles_zero_duration() {
        let mib = 1024 * 1024;
        let mut meter = Meter::default();
        meter.record(Duration::ZERO, state(0, 8 * mib));
        meter.record(Duration::from_millis(100), state(8 * mib, 8 * mib));
        meter.finish_at(Duration::from_millis(250));
        let summary = meter.statistics(Some(true)).unwrap();
        assert!(
            summary.contains("32.0 MiB/s")
                && summary.contains("TIME   ")
                && summary.contains("00:00.250"),
            "{summary}"
        );
        meter.finish_at(Duration::ZERO);
        let summary = meter.statistics(Some(true)).unwrap();
        assert!(
            summary.contains("0 B/s") && summary.contains("00:00.000"),
            "{summary}"
        );
        assert!(!summary.contains("NaN") && !summary.contains("inf"));
    }

    #[test]
    fn final_average_counts_actual_payload_bytes_and_failures_keep_progress() {
        let gib = 1024 * 1024 * 1024;
        let mut meter = Meter::default();
        meter.record(Duration::ZERO, state(0, 8 * gib));
        meter.record(
            Duration::from_secs(1),
            TransferProgress {
                completed: 8 * gib,
                total: 8 * gib,
                transferred: 3 * gib,
            },
        );
        meter.finish_at(Duration::from_secs(3));
        let summary = meter.statistics(Some(true)).unwrap();
        assert!(
            summary.contains("1.0 GiB/s") && summary.contains("00:03.000"),
            "{summary}"
        );

        let mut failed = Meter::default();
        failed.record(Duration::ZERO, state(0, 100));
        failed.record(Duration::from_secs(1), state(50, 100));
        failed.finish_at(Duration::from_secs(10));
        let summary = failed.statistics(Some(false)).unwrap();
        assert!(
            summary.contains("50%")
                && summary.contains(" / ")
                && summary.contains("5 B/s")
                && summary.contains("00:10.000")
                && summary.ends_with("failed"),
            "{summary}"
        );
    }
}
