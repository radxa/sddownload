//! Shared stderr coordination for frontend progress and protocol diagnostics.
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{
    fmt,
    io::{self, Write},
    sync::LazyLock,
};

static PROGRESS: LazyLock<MultiProgress> =
    LazyLock::new(|| MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10)));

pub fn columns() -> usize {
    usize::from(::console::Term::stderr().size().1).max(1)
}

/// Temporarily clear live bars, perform output, and redraw them under one lock.
pub fn suspend<R>(operation: impl FnOnce() -> R) -> R {
    coordinated_output(&PROGRESS, operation)
}

fn coordinated_output<R>(progress: &MultiProgress, operation: impl FnOnce() -> R) -> R {
    progress.suspend(operation)
}

pub fn log(message: fmt::Arguments<'_>) {
    suspend(|| eprintln!("{message}"));
}

#[macro_export]
macro_rules! statusln {
    ($($arg:tt)*) => { $crate::console::log(format_args!($($arg)*)) };
}

pub struct ProgressLine {
    progress: MultiProgress,
    bar: ProgressBar,
    layout: Option<u8>,
    prefix: String,
    frame: Option<Frame>,
}

struct Frame {
    completed: u64,
    total: u64,
    message: String,
    width: usize,
}

fn layout(width: usize) -> u8 {
    if width >= 130 {
        0
    } else if width >= 100 {
        1
    } else {
        2
    }
}

fn snapshot(prefix: &str, frame: &Frame) -> String {
    let (before, after) = match layout(frame.width) {
        0 => (format!("{prefix} ["), format!("]{}", frame.message)),
        1 => (format!("{prefix}\n  ["), format!("]{}", frame.message)),
        _ => (format!("{prefix}\n  ["), format!("]\n{}", frame.message)),
    };
    let before_width = before.rsplit('\n').next().unwrap().chars().count();
    let after_width = after.split('\n').next().unwrap().chars().count();
    let bar_width = frame.width.saturating_sub(before_width + after_width);
    let filled = (u128::from(frame.completed.min(frame.total)) * bar_width as u128
        / u128::from(frame.total.max(1))) as usize;
    let bar = format!("{}{}", "=".repeat(filled), " ".repeat(bar_width - filled));
    format!("{before}{bar}{after}")
}

impl ProgressLine {
    pub fn new(prefix: String) -> Self {
        Self::with_progress(&PROGRESS, prefix)
    }

    fn with_progress(progress: &MultiProgress, prefix: String) -> Self {
        let bar = ProgressBar::hidden();
        bar.set_prefix(prefix.clone());
        let bar = progress.add(bar);
        Self {
            progress: progress.clone(),
            bar,
            layout: None,
            prefix,
            frame: None,
        }
    }

    pub fn is_hidden(&self) -> bool {
        self.bar.is_hidden()
    }

    pub fn draw(&mut self, completed: u64, total: u64, message: String, width: usize) {
        // Small terminals put the full label above the bar. Very narrow ones
        // give the bar its own row so statistics cannot consume its width.
        let layout = layout(width);
        if self.layout != Some(layout) {
            let template = match layout {
                0 => "{prefix} [{wide_bar}]{msg}",
                1 => "{prefix}\n  [{wide_bar}]{msg}",
                _ => "{prefix}\n  [{wide_bar}]\n{msg}",
            };
            self.bar.set_style(
                ProgressStyle::with_template(template)
                    .expect("static progress template")
                    .progress_chars("=> "),
            );
            self.layout = Some(layout);
        }
        self.bar.set_length(total);
        self.bar.set_message(message.clone());
        self.bar.set_position(completed);
        self.bar.force_draw();
        self.frame = Some(Frame {
            completed,
            total,
            message,
            width,
        });
    }

    pub fn finish(&mut self) {
        self.finish_with(|line| {
            let _ = writeln!(io::stderr().lock(), "{line}");
        });
    }

    fn finish_with(&mut self, output: impl FnOnce(&str)) {
        // Commit a completed bar as ordinary output. Keeping it as a dropped
        // MultiProgress member would let later log messages erase its history.
        self.bar.finish_and_clear();
        self.progress.remove(&self.bar);
        if let Some(frame) = self.frame.take() {
            let line = snapshot(&self.prefix, &frame);
            coordinated_output(&self.progress, || output(&line));
        }
    }
}

impl Drop for ProgressLine {
    fn drop(&mut self) {
        if !self.bar.is_finished() {
            self.bar.finish_and_clear();
        }
        self.progress.remove(&self.bar);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indicatif::TermLike;
    use std::sync::{Arc, Mutex};

    #[test]
    fn progress_bar_fills_remaining_width_and_adapts_to_narrow_terminals() {
        let prefix = format!("  Flash {:32}", "emmc1$ap2$cluster_rootfs");
        let message = "  25%     1.0 MiB /    4.0 MiB     1.0 MiB/s ETA    00:03";
        for width in [80, 100, 120, 130, 160, 220] {
            let frame = Frame {
                completed: 1,
                total: 4,
                message: message.into(),
                width,
            };
            let output = snapshot(&prefix, &frame);
            let bar_line = output.lines().find(|line| line.contains('[')).unwrap();
            assert_eq!(bar_line.chars().count(), width, "{output}");
            assert!(
                output.lines().all(|line| line.chars().count() <= width),
                "{output}"
            );
            assert!(bar_line.contains('=') && bar_line.contains(' '));
        }
    }

    #[derive(Debug, Default)]
    struct Screen {
        lines: Vec<Vec<char>>,
        row: usize,
        column: usize,
    }

    impl Screen {
        fn line(&mut self) -> &mut Vec<char> {
            self.lines
                .resize_with(self.lines.len().max(self.row + 1), Vec::new);
            &mut self.lines[self.row]
        }
        fn write(&mut self, text: &str, width: usize) {
            for character in text.chars() {
                match character {
                    '\r' => self.column = 0,
                    '\n' => {
                        self.row += 1;
                        self.column = 0;
                    }
                    _ => {
                        if self.column >= width {
                            self.row += 1;
                            self.column = 0;
                        }
                        let column = self.column;
                        let line = self.line();
                        line.resize(line.len().max(column + 1), ' ');
                        line[column] = character;
                        self.column += 1;
                    }
                }
            }
        }
    }

    #[derive(Clone, Debug)]
    struct Terminal(Arc<Mutex<Screen>>, u16);

    impl Terminal {
        fn contents(&self) -> String {
            self.0
                .lock()
                .unwrap()
                .lines
                .iter()
                .map(|line| line.iter().collect::<String>().trim_end().to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    impl TermLike for Terminal {
        fn width(&self) -> u16 {
            self.1
        }
        fn height(&self) -> u16 {
            100
        }
        fn move_cursor_up(&self, n: usize) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            screen.row = screen.row.saturating_sub(n);
            Ok(())
        }
        fn move_cursor_down(&self, n: usize) -> io::Result<()> {
            self.0.lock().unwrap().row += n;
            Ok(())
        }
        fn move_cursor_right(&self, n: usize) -> io::Result<()> {
            self.0.lock().unwrap().column += n;
            Ok(())
        }
        fn move_cursor_left(&self, n: usize) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            screen.column = screen.column.saturating_sub(n);
            Ok(())
        }
        fn write_line(&self, text: &str) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            screen.write(text, usize::from(self.1));
            screen.write("\n", usize::from(self.1));
            Ok(())
        }
        fn write_str(&self, text: &str) -> io::Result<()> {
            self.0.lock().unwrap().write(text, usize::from(self.1));
            Ok(())
        }
        fn clear_line(&self) -> io::Result<()> {
            let mut screen = self.0.lock().unwrap();
            screen.line().clear();
            screen.column = 0;
            Ok(())
        }
        fn flush(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn concurrent_logs_have_own_lines_and_completed_progress_is_preserved() {
        for width in [80, 120, 160] {
            let terminal = Terminal(Arc::default(), width);
            let manager = MultiProgress::with_draw_target(ProgressDrawTarget::term_like(Box::new(
                terminal.clone(),
            )));
            let mut progress =
                ProgressLine::with_progress(&manager, format!("  Flash {:32}", "rootfs"));
            progress.draw(1, 20, "  5% test statistics".into(), usize::from(width));
            std::thread::scope(|scope| {
                let terminal = terminal.clone();
                let manager = manager.clone();
                scope.spawn(move || {
                    for index in 0..20 {
                        coordinated_output(&manager, || {
                            terminal.write_line(&format!("Caching image-{index}"))
                        })
                        .unwrap();
                    }
                });
                for position in 2..20 {
                    progress.draw(
                        position,
                        20,
                        "progress statistics".into(),
                        usize::from(width),
                    );
                }
            });
            progress.draw(
                20,
                20,
                " 100% test statistics done".into(),
                usize::from(width),
            );
            progress.finish_with(|line| terminal.write_line(line).unwrap());
            coordinated_output(&manager, || terminal.write_line("PAC consumed")).unwrap();
            let output = terminal.contents();
            for index in 0..20 {
                let expected = format!("Caching image-{index}");
                assert_eq!(
                    output.lines().filter(|line| *line == expected).count(),
                    1,
                    "{output}"
                );
            }
            assert!(
                output.contains("  Flash rootfs")
                    && output.lines().any(|line| line.ends_with("done")),
                "{output}"
            );
            assert!(
                output.find("done").unwrap() < output.find("PAC consumed").unwrap(),
                "{output}"
            );
        }
    }
}
