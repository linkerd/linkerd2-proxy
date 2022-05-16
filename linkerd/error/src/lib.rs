#![deny(
    warnings,
    rust_2018_idioms,
    clippy::disallowed_methods,
    clippy::disallowed_types
)]
#![forbid(unsafe_code)]

pub mod recover;

pub use self::recover::Recover;
pub use std::convert::Infallible;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Determines whether the provided error was caused by an `E` typed error.
pub fn is_caused_by<E: std::error::Error + 'static>(
    mut error: &(dyn std::error::Error + 'static),
) -> bool {
    loop {
        if error.is::<E>() {
            return true;
        }
        error = match error.source() {
            Some(e) => e,
            None => return false,
        };
    }
}

/// Finds an `E` typed error in the provided error's sources.
pub fn cause_ref<'e, E: std::error::Error + 'static>(
    mut error: &'e (dyn std::error::Error + 'static),
) -> Option<&'e E> {
    loop {
        if let Some(e) = error.downcast_ref::<E>() {
            return Some(e);
        }
        error = error.source()?;
    }
}

#[cfg(test)]
mod tests {
    #[derive(Debug, thiserror::Error)]
    enum Outer {
        #[error("nada")]
        Nada,
        #[error("{0}")]
        Inner(#[source] Inner),
    }

    #[derive(Debug, thiserror::Error)]
    #[error("inner")]
    struct Inner;

    #[test]
    fn is_caused_by() {
        assert!(!super::is_caused_by::<Inner>(&Outer::Nada));
        assert!(super::is_caused_by::<Outer>(&Outer::Nada));
        assert!(super::is_caused_by::<Inner>(&Outer::Inner(Inner)));
        assert!(super::is_caused_by::<Outer>(&Outer::Inner(Inner)));
        assert!(super::is_caused_by::<Inner>(&Inner));
        assert!(!super::is_caused_by::<Outer>(&Inner));
    }

    #[test]
    fn cause_ref() {
        assert!(super::cause_ref::<Inner>(&Outer::Nada).is_none());
        assert!(super::cause_ref::<Outer>(&Outer::Nada).is_some());
        assert!(super::cause_ref::<Inner>(&Outer::Inner(Inner)).is_some());
        assert!(super::cause_ref::<Outer>(&Outer::Inner(Inner)).is_some());
        assert!(super::cause_ref::<Inner>(&Inner).is_some());
        assert!(super::cause_ref::<Outer>(&Inner).is_none());
    }
}
