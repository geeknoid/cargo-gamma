//! The core data model: mutants, their identity, and their verdicts.

mod mutant;
mod outcome;
mod summary;
mod suppression;


use blake3::Hasher;
use camino::Utf8Path;

pub use mutant::{Expectation, Mutant, one_line};
pub use outcome::Outcome;
pub use summary::{Summary, yield_by_mutator};
pub use suppression::{Channel, Suppression};

/// Computes the stable, content-addressed identity of a mutant.
///
/// Deliberately *not* keyed on line and column. Inserting a line at the top of a file would
/// renumber every mutant below it, which would reshuffle every shard, orphan every cached verdict
/// and silently detach every configured expectation. The enclosing item path provides the same
/// disambiguation while surviving both reformatting and code motion within a file, and the
/// occurrence index handles two textually identical sites in one function.
#[must_use]
pub fn mutant_id(
    file: &Utf8Path,
    item_path: &str,
    mutator: &str,
    normalized_site_text: &str,
    occurrence: u32,
    replacement_index: u32,
) -> String {
    let mut hasher = Hasher::new();

    // Length-prefix every field so that no two different field splits can hash alike.
    for field in [file.as_str(), item_path, mutator, normalized_site_text] {
        let _ = hasher.update(&(field.len() as u64).to_le_bytes());
        let _ = hasher.update(field.as_bytes());
    }

    let _ = hasher.update(&occurrence.to_le_bytes());
    let _ = hasher.update(&replacement_index.to_le_bytes());

    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let mut out = String::with_capacity(12);

    for byte in bytes.iter().take(6) {
        out.push(HEX[usize::from(byte >> 4)]);
        out.push(HEX[usize::from(byte & 0x0f)]);
    }

    out
}

/// The lowercase hex alphabet, indexed by nibble.
const HEX: [char; 16] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f'];

/// Identifies a mutation site for the purpose of counting repeats of it within one item.
///
/// A digest rather than the text itself, because the counter has to be kept for every distinct
/// site in a file and holding the item path and the normalized source of each one costs two owned
/// strings per mutant that nothing ever reads back. At 128 bits a collision between two real sites
/// is not a thing that happens.
#[must_use]
pub fn site_key(item_path: &str, mutator: &str, normalized_site_text: &str) -> u128 {
    let mut hasher = Hasher::new();

    for field in [item_path, mutator, normalized_site_text] {
        let _ = hasher.update(&(field.len() as u64).to_le_bytes());
        let _ = hasher.update(field.as_bytes());
    }

    let mut key = [0_u8; 16];

    key.copy_from_slice(hasher.finalize().as_bytes().get(..16).unwrap_or(&[0; 16]));
    u128::from_le_bytes(key)
}

/// Normalizes the source text of a site for hashing.
///
/// Whitespace runs collapse to a single space and comments disappear; everything else is
/// preserved verbatim, including identifiers, literal values, literal suffixes and integer bases.
/// Preserving too little would let a `cargo fmt` run reshuffle the whole population; preserving
/// too little meaning would let a genuine edit keep its old identity and silently reattach a stale
/// verdict to code whose behavior changed. When in doubt, this preserves.
#[must_use]
pub fn normalize_site_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut pending_space = false;

    while let Some(character) = chars.next() {
        // Comments are trivia and must not affect identity.
        if character == '/' {
            match chars.peek() {
                Some('/') => {
                    for next in chars.by_ref() {
                        if next == '\n' {
                            break;
                        }
                    }

                    pending_space = true;
                    continue;
                }
                Some('*') => {
                    let _ = chars.next();
                    let mut depth = 1_u32;

                    while let Some(next) = chars.next() {
                        if next == '*' && chars.peek() == Some(&'/') {
                            let _ = chars.next();
                            depth -= 1;

                            if depth == 0 {
                                break;
                            }
                        } else if next == '/' && chars.peek() == Some(&'*') {
                            let _ = chars.next();
                            depth += 1;
                        }
                    }

                    pending_space = true;
                    continue;
                }
                _ => {}
            }
        }

        if character.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }

        if pending_space {
            out.push(' ');
            pending_space = false;
        }

        out.push(character);
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_site_key_separates_every_field_it_is_given() {
        let base = site_key("foo", "arith.add_to_sub", "a + b");

        assert_ne!(base, site_key("bar", "arith.add_to_sub", "a + b"));
        assert_ne!(base, site_key("foo", "arith.add_to_mul", "a + b"));
        assert_ne!(base, site_key("foo", "arith.add_to_sub", "a + c"));
        assert_eq!(base, site_key("foo", "arith.add_to_sub", "a + b"));

        // Without length prefixes these two would hash the same bytes and share a counter, which
        // would give two distinct sites the same occurrence index and so the same identity.
        assert_ne!(site_key("ab", "c", "d"), site_key("a", "bc", "d"));
    }

    #[test]
    fn ids_are_twelve_hex_characters() {
        let id = mutant_id(Utf8Path::new("src/lib.rs"), "foo", "arith.add_to_sub", "a + b", 0, 0);

        assert_eq!(id.len(), 12);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn ids_are_stable_for_identical_input() {
        let first = mutant_id(Utf8Path::new("src/lib.rs"), "foo", "arith.add_to_sub", "a + b", 0, 0);
        let second = mutant_id(Utf8Path::new("src/lib.rs"), "foo", "arith.add_to_sub", "a + b", 0, 0);

        assert_eq!(first, second);
    }

    #[test]
    fn every_field_participates_in_identity() {
        let base = mutant_id(Utf8Path::new("src/lib.rs"), "foo", "arith.add_to_sub", "a + b", 0, 0);

        let variants = [
            mutant_id(Utf8Path::new("src/other.rs"), "foo", "arith.add_to_sub", "a + b", 0, 0),
            mutant_id(Utf8Path::new("src/lib.rs"), "bar", "arith.add_to_sub", "a + b", 0, 0),
            mutant_id(Utf8Path::new("src/lib.rs"), "foo", "arith.add_to_mul", "a + b", 0, 0),
            mutant_id(Utf8Path::new("src/lib.rs"), "foo", "arith.add_to_sub", "a + c", 0, 0),
            mutant_id(Utf8Path::new("src/lib.rs"), "foo", "arith.add_to_sub", "a + b", 1, 0),
            mutant_id(Utf8Path::new("src/lib.rs"), "foo", "arith.add_to_sub", "a + b", 0, 1),
        ];

        for variant in variants {
            assert_ne!(base, variant);
        }
    }

    #[test]
    fn field_boundaries_cannot_be_confused() {
        // Without length prefixing these two would hash the same bytes in the same order.
        let first = mutant_id(Utf8Path::new("ab"), "c", "d", "e", 0, 0);
        let second = mutant_id(Utf8Path::new("a"), "bc", "d", "e", 0, 0);

        assert_ne!(first, second);
    }

    #[test]
    fn normalization_erases_formatting() {
        assert_eq!(normalize_site_text("a   +\n\t b"), "a + b");
        assert_eq!(normalize_site_text("a /* why */ + b"), "a + b");
        assert_eq!(normalize_site_text("a + b // trailing\n"), "a + b");
        assert_eq!(normalize_site_text("  a+b  "), "a+b");
    }

    #[test]
    fn normalization_handles_nested_block_comments() {
        assert_eq!(normalize_site_text("a /* outer /* inner */ still */ + b"), "a + b");
    }

    #[test]
    fn normalization_preserves_meaning() {
        // Literal suffixes and bases are meaning, not formatting.
        assert_ne!(normalize_site_text("1_000u64"), normalize_site_text("1000"));
        assert_ne!(normalize_site_text("0x10"), normalize_site_text("16"));
        assert_ne!(normalize_site_text("a + b"), normalize_site_text("a + c"));
    }

    #[test]
    fn division_is_not_mistaken_for_a_comment() {
        assert_eq!(normalize_site_text("a / b"), "a / b");
    }
}
