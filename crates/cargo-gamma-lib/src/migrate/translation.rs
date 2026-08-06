//! The result of translating a configuration file.

use core::fmt::Write as _;
use toml::Value;

/// The result of translating a configuration file.
#[derive(Debug, Default)]
pub struct Translation {
    /// The generated `gamma.toml` text.
    pub text: String,

    /// How many keys were translated into working settings.
    pub translated: usize,

    /// Recognised keys that need no setting here, each carrying the reason why.
    ///
    /// Kept apart from [`Self::preserved`] because the two ask different things of the reader. A
    /// settled key has been looked at already and the answer is on the line; an unknown one still
    /// needs a decision. Reporting them as one number was how `all_features` came to be described
    /// as having no equivalent when it has an exact one.
    pub settled: Vec<String>,

    /// Keys preserved as commented `TODO`s because this tool does not recognise them.
    pub preserved: Vec<String>,
}

impl Translation {
    /// Writes one translated line, annotated with the key it replaces.
    pub(super) fn emit(&mut self, name: &str, from: &str, value: &Value) {
        let _ = writeln!(self.text, "{name} = {value}  # was {from}");
        self.translated += 1;
    }

    /// Preserves a key this tool does not recognise.
    ///
    /// Left as a `TODO` because that is the honest description: a key from a newer cargo-mutants
    /// than this code knows about may well be translatable once somebody looks at it.
    pub(super) fn preserve(&mut self, key: &str, value: &Value) {
        self.preserved.push(format!("# TODO: {key} = {value}\n"));
    }

    /// Records a recognised key that needs no setting here, saying why.
    ///
    /// Two unrelated cases share this, because they read the same to the person holding the old
    /// file: a setting gamma has no way to express, and one whose behaviour gamma already has and
    /// therefore never needed a key for. Both are settled, and in both the reason is the useful
    /// part.
    pub(super) fn settled(&mut self, key: &str, value: &Value, why: &str) {
        self.settled.push(format!("# {key} = {value}  # {why}\n"));
    }

    /// How many keys the input held, which is what the three buckets must add up to.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.translated + self.settled.len() + self.preserved.len()
    }
}
