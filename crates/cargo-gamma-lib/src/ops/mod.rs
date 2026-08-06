//! The mutation operators and the registry that names them.

pub mod collect;
pub mod registry;

pub use collect::{Candidate, collect as collect_candidates, into_mutants};
pub use registry::{Mutator, Profile, Selection, families, find, find_profile, resolve};
