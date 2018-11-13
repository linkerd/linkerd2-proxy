use std::{error::Error, fmt};

/// A type representing a value that can never materialize.
///
/// This would be `!`, but it isn't stable yet.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Never {}

impl fmt::Display for Never {
    fn fmt(&self, _: &mut fmt::Formatter) -> fmt::Result {
        match *self {}
    }
}

impl Error for Never {}
