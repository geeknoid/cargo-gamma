//! The stable, content-addressed identity of a mutant and its site.

use blake3::Hasher;
use camino::Utf8Path;
use compact_str::CompactString;

/// A mutant's compact, content-addressed identity.
pub type MutantId = CompactString;

/// The identity normalization contract emitted by this version.
///
/// Version 4 adds caller-supplied error replacement text to those mutants' identities. Every
/// identity whose replacement comes from the registry remains byte-identical.
pub const MUTANT_ID_VERSION: u32 = 4;

/// Computes the stable, content-addressed identity of a mutant.
///
/// Deliberately *not* keyed on line and column. Inserting a line at the top of a file would
/// renumber every mutant below it, which would reshuffle every shard, orphan every cached verdict
/// and silently detach every configured expectation. The enclosing item path provides the same
/// disambiguation while surviving both reformatting and code motion within a file, and the
/// occurrence index handles two textually identical sites in one function. Trait implementation
/// paths include both the self type and the implemented trait, so same-named methods from two
/// traits never depend on source order for that disambiguation.
#[must_use]
pub fn mutant_id(
    file: &Utf8Path,
    item_path: &str,
    mutator: &str,
    normalized_site_text: &str,
    occurrence: u32,
    replacement_index: u32,
) -> MutantId {
    mutant_id_with_discriminator(file, item_path, mutator, normalized_site_text, occurrence, replacement_index, None)
}

/// Computes an identity with additional caller-supplied replacement content.
#[must_use]
pub(crate) fn mutant_id_with_discriminator(
    file: &Utf8Path,
    item_path: &str,
    mutator: &str,
    normalized_site_text: &str,
    occurrence: u32,
    replacement_index: u32,
    discriminator: Option<&str>,
) -> MutantId {
    let mut hasher = Hasher::new();

    // Length-prefix every field so that no two different field splits can hash alike.
    for field in [file.as_str(), item_path, mutator, normalized_site_text] {
        let _ = hasher.update(&(field.len() as u64).to_le_bytes());
        let _ = hasher.update(field.as_bytes());
    }

    let _ = hasher.update(&occurrence.to_le_bytes());
    let _ = hasher.update(&replacement_index.to_le_bytes());
    if let Some(discriminator) = discriminator {
        let _ = hasher.update(&(discriminator.len() as u64).to_le_bytes());
        let _ = hasher.update(discriminator.as_bytes());
    }

    let digest = hasher.finalize();
    let bytes = digest.as_bytes();
    let mut out = MutantId::with_capacity(MUTANT_ID_HEX_LEN);

    for byte in bytes.iter().take(MUTANT_ID_BYTES) {
        out.push(HEX[usize::from(byte >> 4)]);
        out.push(HEX[usize::from(byte & 0x0f)]);
    }

    out
}

/// How much of the digest a mutant identifier keeps.
///
/// Six bytes is forty-eight bits, which by the birthday bound gives roughly even odds of one
/// collision only once a run holds about `2^24` — sixteen million — distinct mutants. This
/// workspace generates on the order of ten thousand, so the margin is three orders of magnitude,
/// and at a hundred thousand mutants the chance of any collision is still under one in three
/// thousand. Widening this is cheap and safe; narrowing it is not, because the identifier is what
/// cached verdicts, shard assignments and configured expectations are keyed on, so a collision
/// silently attaches one mutant's history to another.
const MUTANT_ID_BYTES: usize = 6;

/// The width of a rendered mutant identifier: two lowercase hex characters per kept digest byte.
pub const MUTANT_ID_HEX_LEN: usize = MUTANT_ID_BYTES * 2;

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
pub fn normalize_site_text(text: &str) -> CompactString {
    let mut out = CompactString::with_capacity(text.len());
    let comments = crate::parse::comment_spans(text);
    let mut comments = comments.iter().peekable();
    let mut offset = 0;
    let mut pending_space = false;

    while offset < text.len() {
        if let Some(comment) = comments.peek()
            && comment.start == offset
        {
            offset = comment.end;
            let _ = comments.next();
            pending_space = !out.is_empty();
            continue;
        }

        if let Some(end) = crate::parse::literal_end(text, offset) {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }

            out.push_str(text.get(offset..end).unwrap_or(""));
            offset = end;
            continue;
        }

        let Some(character) = text.get(offset..).and_then(|rest| rest.chars().next()) else {
            break;
        };
        offset += character.len_utf8();

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
