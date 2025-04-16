//! Application UI. For now, this is mostly progress bars.
//!
//! This is adapted from `substudy` by Eric Kidd, which is licensed under
//! Apache-2.0 OR MIT. Used with permission.

use std::{borrow::Cow, io, sync::Arc, time::Duration};

use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Application UI state.
#[derive(Clone)]
pub struct Ui {
    /// Our progress bars. I'm not actually sure that this `Arc` is useful, but
    /// I'm playing it safe until I understand `MultiProgress` and `tokio`
    /// interactions better.
    multi_progress: Arc<MultiProgress>,
}

impl Ui {
    /// Create a new UI. This sets up logging and and progress bars.
    pub fn init() -> Ui {
        let multi_progress = Arc::new(MultiProgress::new());
        Ui { multi_progress }
    }

    /// Create a new UI for unit tests.returns_right_number_of_subs
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn init_for_tests() -> Ui {
        let multi_progress =
            Arc::new(MultiProgress::with_draw_target(ProgressDrawTarget::hidden()));
        Ui { multi_progress }
    }

    /// Hide all our progress bars completely, for when we're writing actual
    /// output to `stdout`.
    pub fn hide_progress_bars(&self) {
        self.multi_progress
            .set_draw_target(ProgressDrawTarget::hidden());
    }

    /// Get a writer than can be used to write to stderr, for use with `tracing`
    /// and other output code.
    pub fn get_stderr_writer(&self) -> SafeStderrWriter {
        SafeStderrWriter { ui: self.clone() }
    }

    /// Get a reference to our progress bars.
    pub fn multi_progress(&self) -> &MultiProgress {
        &self.multi_progress
    }

    /// Create a new progress bar with default settings.
    pub fn new_progress_bar(&self, config: &ProgressConfig<'_>, len: u64) -> ProgressBar {
        let pb = ProgressBar::new(len).with_style(default_progress_style());
        let pb = self.multi_progress.add(pb);
        #[cfg(test)]
        pb.set_draw_target(ProgressDrawTarget::hidden());
        pb.set_prefix(config.emoji.to_owned());
        pb.set_message(config.msg.to_owned());
        pb.enable_steady_tick(Duration::from_millis(250));
        pb.with_finish(indicatif::ProgressFinish::WithMessage(Cow::Owned(
            config.done_msg.to_owned(),
        )))
    }

    /// Create a new spinner with default settings.
    pub fn new_spinner(&self, config: &ProgressConfig<'_>) -> ProgressBar {
        let sp = ProgressBar::new_spinner().with_style(default_spinner_style());
        let sp = self.multi_progress.add(sp);
        #[cfg(test)]
        sp.set_draw_target(ProgressDrawTarget::hidden());
        sp.set_prefix(config.emoji.to_owned());
        sp.set_message(config.msg.to_owned());
        sp.enable_steady_tick(Duration::from_millis(250));
        sp.with_finish(indicatif::ProgressFinish::WithMessage(Cow::Owned(
            config.done_msg.to_owned(),
        )))
    }

    /// Create a new progress bar or spinner based on a size hint.
    pub fn new_from_size_hint(
        &self,
        config: &ProgressConfig<'_>,
        size_hint: (usize, Option<usize>),
    ) -> ProgressBar {
        match size_hint {
            (_, Some(len)) if len > 0 => self.new_progress_bar(
                config,
                u64::try_from(len).expect("size hint too large"),
            ),
            _ => self.new_spinner(config),
        }
    }
}

/// Configuration for a progress bar.
pub struct ProgressConfig<'a> {
    /// Emoji to display in the progress bar.
    pub emoji: &'a str,
    /// Message to display in a running progress bar.
    pub msg: &'a str,
    /// Message to display in a progress bar when it is done.
    pub done_msg: &'a str,
}

fn default_progress_style() -> ProgressStyle {
    ProgressStyle::default_bar()
        .template("  {prefix:3}{msg:25} {pos:>4}/{len:4} {elapsed_precise} {wide_bar:.cyan/blue} {eta_precise}")
        .expect("bad progress bar template")
}

fn default_spinner_style() -> ProgressStyle {
    ProgressStyle::default_spinner()
        .template("{spinner} {prefix:3}{msg}")
        .expect("bad progress bar template")
}

/// A writer which can used to write to `stderr`. It will hide and show progress
/// bars as needed, so that they don't interfere with the output.
#[derive(Clone)]
pub struct SafeStderrWriter {
    ui: Ui,
}

// The `tracing-indicatif` crate suggests that we should implement the following
// methods.
impl io::Write for SafeStderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.ui.multi_progress().suspend(|| io::stderr().write(buf))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.ui.multi_progress().suspend(|| io::stderr().flush())
    }

    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.ui
            .multi_progress()
            .suspend(|| io::stderr().write_vectored(bufs))
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.ui
            .multi_progress()
            .suspend(|| io::stderr().write_all(buf))
    }

    fn write_fmt(&mut self, fmt: std::fmt::Arguments<'_>) -> io::Result<()> {
        self.ui
            .multi_progress()
            .suspend(|| io::stderr().write_fmt(fmt))
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SafeStderrWriter {
    type Writer = SafeStderrWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
