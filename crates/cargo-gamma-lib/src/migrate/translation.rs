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

    /// Keys recorded as commented `TODO`s because this tool does not recognise them.
    ///
    /// Their TOML values are retained semantically after parsing and rendering; comments and
    /// source formatting cannot survive that conversion.
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
    /// Left as a `TODO` because that is the honest description: a key from a newer version of the
    /// source schema than this code knows about may well be translatable once somebody looks at
    /// it.
    pub(super) fn preserve(&mut self, key: &str, value: &Value) {
        self.preserved.push(commented(&format!("TODO: {key} = {value}")));
    }

    /// Records a recognised key that needs no setting here, saying why.
    ///
    /// Two unrelated cases share this, because they read the same to the person holding the old
    /// file: a setting gamma has no way to express, and one whose behaviour gamma already has and
    /// therefore never needed a key for. Both are settled, and in both the reason is the useful
    /// part.
    pub(super) fn settled(&mut self, key: &str, value: &Value, why: &str) {
        self.settled.push(commented(&format!("{key} = {value}  # {why}")));
    }

    /// How many keys the input held, which is what the three buckets must add up to.
    #[must_use]
    pub const fn total(&self) -> usize {
        self.translated + self.settled.len() + self.preserved.len()
    }
}

fn commented(text: &str) -> String {
    let mut out = String::new();

    for line in text.lines() {
        let _ = writeln!(out, "# {line}");
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_of_a_multiline_value_is_commented() {
        let value = Value::String("a\nb".to_owned());
        let mut translation = Translation::default();

        translation.preserve("unknown", &value);
        translation.settled("known", &value, "not needed");

        for rendered in translation.preserved.iter().chain(&translation.settled) {
            assert!(rendered.lines().count() > 1, "{rendered}");
            assert!(rendered.lines().all(|line| line.starts_with("# ")), "{rendered}");
        }
    }
}
