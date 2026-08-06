use std::io::Write as _;

use crate::report::{Progress, Styler};

use super::host::Host;

/// Drives the console progress display from execution events.
pub(super) struct ConsoleEvents<'host, H: Host> {
    pub(super) host: &'host mut H,
    pub(super) progress: Progress,
    pub(super) styler: Styler,

    /// Whether `--estimate` asked for a projection of the wait still to come.
    pub(super) estimate: bool,
}

impl<H: Host> ConsoleEvents<'_, H> {
    /// Closes a phase line whose phase failed, so the error that follows starts on its own line.
    pub(super) fn abandon(&mut self) {
        self.progress.abandon(self.host);
    }
}

impl<H: Host> crate::exec::Events for ConsoleEvents<'_, H> {
    fn phase(&mut self, verb: &str, detail: &str) {
        self.progress.status(self.host, verb, detail);
    }

    fn begin(&mut self, verb: &str, detail: &str) {
        self.progress.begin(self.host, verb, detail);
    }

    fn end(&mut self, detail: &str) {
        self.progress.end(self.host, detail);
    }

    fn outcome(&mut self, detail: &str) {
        self.progress.labelled(self.host, &crate::report::continuation(), detail);
    }

    fn mutant(&mut self, mutant: &crate::model::Mutant) {
        // A survivor is the entire point of the exercise; a timeout is the most expensive thing a
        // run can find; and a mutant stopped by its memory ceiling is usually a sign the ceiling is
        // wrong rather than a finding about the code. All three are printed as they happen rather
        // than held back for the summary. Everything else only moves the bar. The label is the one
        // the summary would use, so the same mutant is never named two different things.
        match mutant.outcome {
            crate::model::Outcome::Survived => {
                let label = self.styler.outcome(crate::model::Outcome::Survived);

                self.progress.labelled(self.host, &label, &mutant.describe());
            }

            // Both carry a note that says something the label cannot: which test a timeout stalled
            // in, and what a memory kill peaked at against what ceiling. Neither is worth repeating
            // the label for, so only the note is appended.
            outcome @ (crate::model::Outcome::Timeout | crate::model::Outcome::OutOfMemory) => {
                let label = self.styler.outcome(outcome);
                let detail = mutant
                    .note
                    .as_deref()
                    .map_or_else(|| mutant.describe(), |note| format!("{}: {note}", mutant.describe()));

                self.progress.labelled(self.host, &label, &detail);
            }

            _ => {}
        }

        self.progress.record(mutant.outcome);
        self.progress.tick(self.host);
    }

    fn measured(&mut self, plan: &crate::discover::Plan, _session: &crate::exec::Session, estimate: &crate::estimate::Estimate) {
        // The bar's scale is the population that is about to be tested, which is not known until
        // every package has been scanned — and this is the moment that becomes true.
        let live = plan.mutants.iter().filter(|mutant| mutant.ordinal > 0).count();

        self.progress.set_total(live);

        if !self.estimate {
            return;
        }

        // Written straight to the stream rather than through the progress display, because the
        // display goes quiet when output is piped and an explicitly requested estimate must not.
        self.progress.clear(self.host);

        let projection = format!("{} {}", self.styler.verb("Estimate"), crate::estimate::render(estimate));
        let mut stream = self.host.error();

        let _ = writeln!(stream, "{projection}");
        let _ = stream.flush();
    }
}

#[cfg(test)]
mod tests {
    use crate::exec::Events as _;
    use crate::model::{Mutant, Outcome};
    use crate::ops::collect::Shape;

    use super::*;
    use crate::testing::Sink;

    fn mutant(outcome: Outcome, note: Option<&str>) -> Mutant {
        Mutant {
            id: "m1".to_owned(),
            ordinal: 1,
            file: camino::Utf8PathBuf::from("src/lib.rs"),
            package: "subject".to_owned(),
            span: 0..1,
            line: 3,
            column: 4,
            mutator: "relational.lt_to_le".to_owned(),
            item_path: "subject::less".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a < b".to_owned(),
            replacement: "a <= b".to_owned(),
            shape: Shape::Expr,
            outcome,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: note.map(str::to_owned),
        }
    }

    #[test]
    fn survivors_and_timeouts_are_announced_as_they_happen() {
        let mut host = Sink::default();
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(80)),
            styler: Styler::new(false),
            estimate: false,
        };

        events.mutant(&mutant(Outcome::Survived, None));
        events.mutant(&mutant(Outcome::Timeout, Some("stalled, last test named was `slow`")));

        let err = String::from_utf8(host.err).expect("utf-8");

        assert!(err.contains("MISSED"), "{err}");
        assert!(err.contains("TIMEOUT"), "{err}");
        assert!(err.contains("stalled, last test named was `slow`"), "{err}");
    }

    /// The phase verbs all reach the display, and a caught mutant is left for the summary.
    #[test]
    fn phase_events_are_displayed_and_ordinary_verdicts_are_not_announced() {
        let mut host = Sink::default().terminal(80);
        let mut events = ConsoleEvents {
            host: &mut host,
            progress: Progress::new(true, Styler::new(false), Some(80)),
            styler: Styler::new(false),
            estimate: false,
        };

        events.phase("Baseline", "measuring the suite");
        events.begin("Building", "the test binaries");
        events.end(", done");
        events.outcome("withdrew 2 mutants");
        events.mutant(&mutant(Outcome::Killed, None));

        let err = host.err();

        assert!(err.contains("Baseline"), "{err}");
        assert!(err.contains("Building"), "{err}");
        assert!(err.contains("withdrew 2 mutants"), "{err}");
        assert!(!err.contains("MISSED"), "{err}");
    }
}
