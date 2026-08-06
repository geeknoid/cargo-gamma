/// A named group of mutators.
#[derive(Debug, Clone, Copy)]
pub struct Profile {
    /// The profile name, written `@name` in selectors.
    pub name: &'static str,

    /// One-line description.
    pub description: &'static str,

    /// Families and names that make up the profile.
    pub members: &'static [&'static str],
}
