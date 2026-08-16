use core::error::Error as StdError;
use core::fmt::{self, Display, Formatter};
use std::io;

/// An engine error with the coordinator-facing classification preserved.
#[derive(Debug)]
pub struct Error {
    message: String,
    cause: Option<Box<dyn StdError + Send + Sync>>,
    usage: bool,
    skippable: bool,
}

impl Error {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cause: None,
            usage: false,
            skippable: false,
        }
    }

    #[must_use]
    pub const fn usage(mut self) -> Self {
        self.usage = true;
        self
    }

    #[must_use]
    pub const fn is_usage(&self) -> bool {
        self.usage
    }

    #[must_use]
    pub const fn skippable(mut self) -> Self {
        self.skippable = true;
        self
    }

    #[must_use]
    pub const fn is_skippable(&self) -> bool {
        self.skippable
    }

    #[must_use]
    pub fn caused_by(mut self, cause: impl StdError + Send + Sync + 'static) -> Self {
        self.cause = Some(Box::new(cause));
        self
    }

    #[must_use]
    pub fn into_parts(self) -> (String, Option<Box<dyn StdError + Send + Sync>>, bool, bool) {
        (self.message, self.cause, self.usage, self.skippable)
    }
}

impl Display for Error {
    #[expect(clippy::renamed_function_params, reason = "`f` is less clear than `formatter`")]
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)?;

        if let Some(cause) = &self.cause {
            write!(formatter, ": {cause}")?;
        }

        Ok(())
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.cause.as_ref().map(|cause| &**cause as &(dyn StdError + 'static))
    }
}

impl From<io::Error> for Error {
    fn from(value: io::Error) -> Self {
        Self::new("I/O error").caused_by(value)
    }
}

macro_rules! error {
    ($($arg:tt)*) => { $crate::error::Error::new(format!($($arg)*)) };
}

pub(crate) use error;
