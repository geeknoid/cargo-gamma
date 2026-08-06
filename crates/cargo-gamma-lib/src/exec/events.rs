use crate::discover::Plan;
use crate::estimate::Estimate;
use crate::model::Mutant;

use super::session::Session;

/// Progress notifications, so this module needs to know nothing about terminals.
pub trait Events {
    /// A new phase started.
    fn phase(&mut self, verb: &str, detail: &str);

    /// A phase started, and will report what it found on the same line once it knows.
    fn begin(&mut self, verb: &str, detail: &str) {
        self.phase(verb, detail);
    }

    /// A phase that opened a line with [`begin`](Self::begin) is closing it.
    fn end(&mut self, detail: &str) {
        self.outcome(detail);
    }

    /// A phase that has already announced itself is reporting what it found.
    ///
    /// Rendered under the phase it belongs to rather than repeating the verb, since a phase and
    /// its result are one event to a reader even though they are two to the code.
    fn outcome(&mut self, detail: &str) {
        self.phase("", detail);
    }

    /// A mutant finished.
    fn mutant(&mut self, mutant: &Mutant);

    /// The fixed cost is paid, the tree compiles, and the first mutant is about to be tested.
    ///
    /// The only moment at which a projection of the run is both possible and useful: everything
    /// before it is measured, everything after it is the wait the user is deciding whether to sit
    /// through, so the projection is handed over here rather than recomputed by whoever wants it.
    fn measured(&mut self, _plan: &Plan, _session: &Session, _estimate: &Estimate) {}
}

#[cfg(test)]
mod tests {
    use camino::Utf8PathBuf;
    use core::time::Duration;

    use super::*;
    use crate::testing::Recorder;

    #[test]
    fn default_event_methods_are_expressed_in_terms_of_phase_and_outcome() {
        let mut events = Recorder::default();
        let plan = Plan {
            root: Utf8PathBuf::from("/workspace"),
            files: Vec::new(),
            mutants: Vec::new(),
            suppressed: 0,
            sharded_out: 0,
            settled_out: 0,
            reach: crate::HashMap::default(),
            specs: crate::HashMap::default(),
        };
        let session = Session {
            baseline: Duration::ZERO,
            quiet: Duration::ZERO,
            stall: None,
            timeout: Duration::ZERO,
            build: Duration::ZERO,
            metered: false,
            unbounded: None,
            withdrawn: 0,
            rounds: 0,
            binaries: Vec::new(),
            peak: None,
            footprint: 0,
            filtered: 0,
            not_built: 0,
            widened: false,
        };
        let estimate = Estimate {
            live: 0,
            withdrawn: 0,
            build: Duration::ZERO,
            baseline: Duration::ZERO,
            mutants: Duration::ZERO,
            jobs: 1,
            worst: Duration::ZERO,
        };

        events.begin("Doing", "the thing");
        events.end(", done");
        events.outcome(", noted");
        events.measured(&plan, &session, &estimate);
        events.mutant(&mutant());

        // Implementors only have to provide the primitive rendering hooks; the default helpers
        // keep their routing stable for plain reporters.
        assert_eq!(
            events.phases,
            vec![
                ("Doing".to_owned(), "the thing".to_owned()),
                (String::new(), ", done".to_owned()),
                (String::new(), ", noted".to_owned()),
            ]
        );
        assert_eq!(events.mutants, 1);
    }

    /// The one hook with no default has to be routed by the implementor, not by the trait.
    fn mutant() -> Mutant {
        Mutant {
            id: "m1".to_owned(),
            ordinal: 1,
            file: Utf8PathBuf::from("src/lib.rs"),
            package: "subject".to_owned(),
            span: 0..1,
            line: 1,
            column: 1,
            mutator: "relational.lt_to_le".to_owned(),
            item_path: "subject::less".to_owned(),
            occurrence: 0,
            replacement_index: 0,
            original: "a < b".to_owned(),
            replacement: "a <= b".to_owned(),
            shape: crate::ops::collect::Shape::Expr,
            outcome: crate::model::Outcome::Killed,
            suppression: None,
            expectation: None,
            elapsed_ms: 0,
            killed_by: None,
            note: None,
        }
    }
}
