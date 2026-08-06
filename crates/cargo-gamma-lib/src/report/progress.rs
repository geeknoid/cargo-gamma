//! The live progress display, rendered as a `cargo`-style bar.

use core::time::Duration;
use std::io::Write;
use std::time::Instant;

use super::{Styler, VERB_WIDTH, quantity};
use crate::advise::human;
use crate::commands::Host;
use crate::model::Outcome;

/// Shortest interval between redraws.
///
/// Redrawing on every event makes a fast run spend real time on terminal writes, and produces a
/// flicker nobody can read anyway.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// Width of the gauge itself, in columns.
///
/// The gauge is a fixed size rather than whatever the caption leaves over. Sizing it from the
/// remainder makes it collapse to nothing the moment the caption grows, and makes it twitch by a
/// column every time a counter gains a digit.
const BAR_WIDTH: usize = 25;

/// The live progress display.
#[derive(Debug)]
pub struct Progress {
    enabled: bool,

    /// Whether [`begin`](Self::begin) has written a line that [`end`](Self::end) has not closed.
    open: bool,
    styler: Styler,
    width: usize,
    last_draw: Option<Instant>,
    dirty: bool,
    total: usize,
    done: usize,
    missed: usize,
    timeouts: usize,
    started: Option<Instant>,
}

impl Progress {
    /// Creates a display.
    ///
    /// `enabled` is the already-resolved decision, so this type never has to know what a terminal
    /// is; `width` is the terminal width if there is one.
    #[must_use]
    pub fn new(enabled: bool, styler: Styler, width: Option<u16>) -> Self {
        Self {
            enabled,
            open: false,
            styler,
            width: width.map_or(80, |value| usize::from(value).max(20)),
            last_draw: None,
            dirty: false,
            total: 0,
            done: 0,
            missed: 0,
            timeouts: 0,
            started: None,
        }
    }

    /// Sets the number of mutants that are about to be tested, and starts the clock the time
    /// estimate is derived from.
    pub fn set_total(&mut self, total: usize) {
        self.total = total;
        self.started = Some(Instant::now());
        self.dirty = true;
    }

    /// Records one evaluated mutant.
    pub const fn record(&mut self, outcome: Outcome) {
        self.done += 1;

        match outcome {
            Outcome::Survived => self.missed += 1,
            Outcome::Timeout => self.timeouts += 1,
            _ => {}
        }

        self.dirty = true;
    }

    /// Returns the completed fraction, clamped to `0.0..=1.0`.
    #[must_use]
    pub fn fraction(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }

        #[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
        let fraction = self.done as f64 / self.total as f64;

        fraction.clamp(0.0, 1.0)
    }

    /// Estimates the time left, by extrapolating from the rate achieved so far.
    ///
    /// Extrapolation beats the up-front projection here because it needs no model: it absorbs the
    /// job count, the machine's actual throughput, and the share of mutants that hang, all of which
    /// the projection can only guess at. It is worthless until a few mutants have finished, so it
    /// is reported as absent rather than as a wild number.
    fn remaining(&self) -> Option<Duration> {
        let started = self.started?;

        if self.done == 0 || self.done >= self.total {
            return None;
        }

        #[expect(clippy::cast_precision_loss, reason = "a mutant count far exceeds any plausible workspace")]
        let (done, left) = (self.done as f64, (self.total - self.done) as f64);

        Duration::try_from_secs_f64(started.elapsed().as_secs_f64() / done * left).ok()
    }

    /// Writes a completed status line, above the progress bar.
    ///
    /// Status lines belong to the progress display and are suppressed with it. They also duplicate
    /// what the summary reports, so emitting them when progress is off would put every survivor on
    /// screen twice.
    pub fn status<H: Host>(&mut self, host: &mut H, verb: &str, subject: &str) {
        let label = self.styler.verb(verb);

        self.line(host, &label, subject);
    }

    /// Opens a status line and leaves it open, so that what the phase found can be added to it once
    /// it is known.
    ///
    /// A phase that names what it is about to do and then goes quiet for a minute is easier to sit
    /// through than one that says nothing until it is done — but the counts it reports do not exist
    /// until then, and putting them on a second line makes the sequence twice as long as the work
    /// it describes.
    pub fn begin<H: Host>(&mut self, host: &mut H, verb: &str, subject: &str) {
        if !self.enabled {
            return;
        }

        self.clear(host);

        let label = self.styler.verb(verb);
        let mut stream = host.error();

        let _ = write!(stream, "{label} {subject}");
        let _ = stream.flush();

        self.open = true;
        self.dirty = true;
    }

    /// Closes the line [`begin`](Self::begin) opened.
    pub fn end<H: Host>(&mut self, host: &mut H, subject: &str) {
        if !self.enabled {
            return;
        }

        let mut stream = host.error();

        let _ = writeln!(stream, "{subject}");
        let _ = stream.flush();

        self.open = false;
        self.dirty = true;
    }

    /// Ends an open phase line that will never get the ending it was waiting for.
    ///
    /// A phase names what it is about to do and holds the line open until it can say what it
    /// found. When it fails instead, nothing closes the line, and whatever is printed next — an
    /// error, most of the time — is run onto the end of it, so the failure reads as part of the
    /// sentence that was describing the work.
    pub fn abandon<H: Host>(&mut self, host: &mut H) {
        if !self.open {
            return;
        }

        let mut stream = host.error();

        let _ = writeln!(stream);
        let _ = stream.flush();

        self.open = false;
        self.dirty = true;
    }

    /// Writes a status line under a caller-supplied label, which must already be styled and
    /// aligned. Used where the label is not a verb, so that one thing is not given two names.
    pub fn labelled<H: Host>(&mut self, host: &mut H, label: &str, subject: &str) {
        self.line(host, label, subject);
    }

    /// Writes one line above the bar.
    fn line<H: Host>(&mut self, host: &mut H, label: &str, subject: &str) {
        if !self.enabled {
            return;
        }

        self.clear(host);

        let mut stream = host.error();

        let _ = writeln!(stream, "{label} {subject}");
        let _ = stream.flush();

        self.dirty = true;
    }

    /// Whether anything is actually drawn.
    ///
    /// A caller that has to avoid saying the same thing twice needs to know whether this said it
    /// the first time.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Redraws the progress bar if enough time has passed and something changed.
    pub fn tick<H: Host>(&mut self, host: &mut H) {
        if !self.enabled || !self.dirty {
            return;
        }

        let now = Instant::now();

        if self.last_draw.is_some_and(|last| now.duration_since(last) < REDRAW_INTERVAL) {
            return;
        }

        self.last_draw = Some(now);
        self.dirty = false;

        let line = self.render();
        let mut stream = host.error();

        let _ = write!(stream, "\r\x1b[2K{line}");
        let _ = stream.flush();
    }

    /// Erases the progress bar.
    pub fn clear<H: Host>(&mut self, host: &mut H) {
        if !self.enabled || self.last_draw.is_none() {
            return;
        }

        self.last_draw = None;

        let mut stream = host.error();
        let _ = write!(stream, "\r\x1b[2K");
        let _ = stream.flush();
    }

    /// Erases the progress bar for good.
    pub fn finish<H: Host>(&mut self, host: &mut H) {
        self.clear(host);
        self.dirty = false;
    }

    /// Renders the bar as a string.
    ///
    /// The arrowhead is counted as part of the filled run, exactly as cargo does it, so the bar
    /// does not gain a column when it starts and lose one when it completes.
    ///
    /// On a narrow terminal the caption is shortened rather than cut. Truncation takes the columns
    /// from the right, which is where the time remaining lives, so a terminal a few columns short
    /// used to lose the most volatile part of the line and leave a dangling ellipsis. Dropping the
    /// running verdict counts instead keeps a line that is still worth reading: how far along the
    /// run is, and how much longer it has. The counts are recoverable from the survivors already
    /// printed above, and from the summary at the end.
    #[must_use]
    pub fn render(&self) -> String {
        let estimate = self.remaining().map_or_else(String::new, |remaining| format!(", ~{} to go", human(remaining)));

        #[expect(clippy::cast_precision_loss, reason = "the operand is a bar width")]
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "the value is bounded by the bar width"
        )]
        let filled = (self.fraction() * BAR_WIDTH as f64) as usize;

        let filled = filled.min(BAR_WIDTH);
        let mut bar = String::with_capacity(BAR_WIDTH);

        if filled > 0 {
            for _ in 0..filled - 1 {
                bar.push('=');
            }

            bar.push(if filled == BAR_WIDTH { '=' } else { '>' });
        }

        for _ in filled..BAR_WIDTH {
            bar.push(' ');
        }

        let room = self.width.saturating_sub(VERB_WIDTH + 1);
        let verdicts = format!(" ({} missed, {})", self.missed, quantity(self.timeouts, "timeout"));
        let counted = format!("[{bar}] {}/{} mutants evaluated", self.done, self.total);
        let full = format!("{counted}{verdicts}{estimate}");

        let body = if full.chars().count() <= room {
            full
        } else {
            // Everything after the bar is optional, in the order it is least useful.
            let shorter = format!("{counted}{estimate}");

            if shorter.chars().count() <= room { shorter } else { truncate(&shorter, room) }
        };

        // The verb is styled after truncating, because the escape sequences that style it are not
        // columns and would otherwise be counted as though they were.
        format!("{} {body}", self.styler.verb("Testing"))
    }
}

/// Truncates a string to a column count, appending an ellipsis when it does.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }

    let keep = width.saturating_sub(3);

    text.chars().take(keep).chain("...".chars()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::Sink;

    /// Renders a bar over a population of a hundred mutants, `done` of which have been evaluated.
    fn bar(done: usize, width: u16) -> String {
        let mut progress = Progress::new(true, Styler::new(false), Some(width));

        progress.set_total(100);

        for _ in 0..done {
            progress.record(Outcome::Killed);
        }

        progress.render()
    }

    #[test]
    fn an_empty_bar_has_no_arrowhead() {
        let rendered = bar(0, 80);

        assert!(rendered.contains("[    "), "{rendered}");
        assert!(!rendered.contains('>'), "{rendered}");
    }

    #[test]
    fn a_partial_bar_ends_in_an_arrowhead() {
        let rendered = bar(50, 80);

        assert!(rendered.contains("=>"), "{rendered}");
    }

    #[test]
    fn a_full_bar_has_no_arrowhead() {
        let rendered = bar(100, 80);

        assert!(!rendered.contains('>'), "{rendered}");
        assert!(rendered.contains("==="), "{rendered}");
    }

    /// The bracketed bar on its own, without the caption that follows it.
    fn gauge(rendered: &str) -> String {
        rendered
            .split_once('[')
            .and_then(|(_, tail)| tail.split_once(']'))
            .map(|(bar, _)| bar.to_owned())
            .expect("the bar is bracketed")
    }

    #[test]
    fn the_arrowhead_is_counted_inside_the_filled_run() {
        // Otherwise the bar would visibly widen at the start of the run and narrow at the end.
        let empty = gauge(&bar(0, 80));
        let half = gauge(&bar(50, 80));
        let full = gauge(&bar(100, 80));

        assert_eq!(empty.chars().count(), half.chars().count());
        assert_eq!(half.chars().count(), full.chars().count());
    }

    #[test]
    fn a_narrow_terminal_drops_the_verdict_counts_rather_than_cutting_the_time_remaining() {
        // Truncation takes columns from the right, which is where the time remaining lives, so a
        // narrow terminal used to lose the most useful part of the line and gain an ellipsis.
        let wide = bar(50, 140);
        let narrow = bar(50, 80);

        assert!(wide.contains("missed"), "{wide}");
        assert!(!narrow.contains("missed"), "{narrow}");
        assert!(narrow.contains("to go"), "{narrow}");
        assert!(!narrow.contains('…'), "{narrow}");
    }

    #[test]
    fn the_time_remaining_is_marked_approximate_rather_than_spelled_out() {
        let rendered = bar(50, 140);

        assert!(rendered.contains('~'), "{rendered}");
        assert!(!rendered.contains("estimating"), "{rendered}");
    }

    #[test]
    fn the_bar_never_exceeds_the_terminal_width() {
        for width in [20_u16, 40, 80, 200] {
            let rendered = bar(50, width);

            assert!(
                rendered.chars().count() <= usize::from(width)
            );
        }
    }

    #[test]
    fn a_narrow_terminal_still_renders_something() {
        let rendered = bar(50, 20);

        assert!(!rendered.is_empty());
    }

    #[test]
    fn the_fraction_is_clamped() {
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        progress.set_total(2);

        for _ in 0..10 {
            progress.record(Outcome::Killed);
        }

        assert!((progress.fraction() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn no_total_means_no_progress() {
        let progress = Progress::new(true, Styler::new(false), Some(80));

        assert!(progress.fraction().abs() < f64::EPSILON);
    }

    #[test]
    fn the_caption_counts_evaluated_mutants_and_what_they_found() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(10);
        progress.record(Outcome::Killed);
        progress.record(Outcome::Survived);
        progress.record(Outcome::Timeout);
        progress.record(Outcome::Timeout);

        let rendered = progress.render();

        assert!(rendered.contains("4/10 mutants evaluated (1 missed, 2 timeouts)"), "{rendered}");
    }

    #[test]
    fn a_lone_timeout_is_not_pluralized() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(10);
        progress.record(Outcome::Timeout);

        assert!(progress.render().contains("(0 missed, 1 timeout)"), "{}", progress.render());
    }

    #[test]
    fn the_gauge_keeps_its_width_however_long_the_caption_grows() {
        // Sizing the gauge from what the caption leaves over collapsed it to nothing.
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(1_000_000);

        let empty = progress.render();

        for _ in 0..10 {
            progress.record(Outcome::Survived);
        }

        let busy = progress.render();

        assert!(empty.contains(&format!("[{}]", " ".repeat(BAR_WIDTH))), "{empty}");
        assert!(busy.contains(&format!("[{}]", " ".repeat(BAR_WIDTH))), "{busy}");
    }

    #[test]
    fn a_time_estimate_appears_once_there_is_something_to_extrapolate_from() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(10);

        assert!(!progress.render().contains("to go"), "{}", progress.render());

        progress.record(Outcome::Killed);

        assert!(progress.render().contains("to go"), "{}", progress.render());
    }

    #[test]
    fn a_finished_run_has_no_time_left_to_report() {
        let mut progress = Progress::new(true, Styler::new(false), Some(200));

        progress.set_total(1);
        progress.record(Outcome::Killed);

        assert!(!progress.render().contains("to go"), "{}", progress.render());
    }

    #[test]
    fn truncation_appends_an_ellipsis() {
        assert_eq!(truncate("abcdefghij", 6), "abc...");
        assert_eq!(truncate("abc", 6), "abc");
    }

    #[test]
    fn truncation_counts_characters_not_bytes() {
        assert_eq!(truncate("ééééé", 5).chars().count(), 5);
    }


    fn written(steps: impl FnOnce(&mut Progress, &mut Sink)) -> String {
        let mut host = Sink::default();
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        steps(&mut progress, &mut host);

        host.err()
    }

    /// Progress is chatter, so none of it may land on the stream carrying the results.
    #[test]
    fn progress_writes_to_the_diagnostic_stream_and_never_to_the_result_stream() {
        let mut host = Sink::default();
        let mut progress = Progress::new(true, Styler::new(false), Some(80));

        progress.begin(&mut host, "Baseline", "building the test binaries");
        progress.end(&mut host, ", done");

        assert!(host.out().is_empty(), "{}", host.out());
        assert!(host.err().contains("Baseline"), "{}", host.err());
    }

    #[test]
    fn an_abandoned_phase_line_is_closed_so_what_follows_starts_on_its_own_line() {
        let text = written(|progress, host| {
            progress.begin(host, "Baseline", "building the test binaries");
            progress.abandon(host);
        });

        assert!(text.ends_with('\n'), "{text:?}");
    }

    #[test]
    fn abandoning_a_line_that_was_already_closed_writes_nothing_extra() {
        let closed = written(|progress, host| {
            progress.begin(host, "Baseline", "building");
            progress.end(host, ", done");
        });

        let abandoned = written(|progress, host| {
            progress.begin(host, "Baseline", "building");
            progress.end(host, ", done");
            progress.abandon(host);
        });

        assert_eq!(closed, abandoned);
    }

    #[test]
    fn abandoning_without_a_phase_at_all_writes_nothing() {
        assert!(written(Progress::abandon).is_empty());
    }

    #[test]
    fn a_disabled_display_writes_nothing() {
        let mut host = Sink::default();
        let mut progress = Progress::new(false, Styler::new(false), None);

        progress.set_total(10);
        progress.record(Outcome::Killed);
        progress.tick(&mut host);
        progress.finish(&mut host);

        assert!(host.err().is_empty(), "{}", host.err());
        assert!(!host.is_terminal());
        assert_eq!(host.terminal_width(), None);
    }
}
