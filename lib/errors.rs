//! Utility types and functions for errors

use fatality::{Fatality, Split};
use thiserror::Error;

/// Display an error with causes.
/// This is useful for displaying errors without converting to
/// `miette::Report` or `anyhow::Error` first
pub struct ErrorChain<'a>(&'a dyn std::error::Error);

impl<'a> ErrorChain<'a> {
    pub fn new<E>(err: &'a E) -> Self
    where
        E: std::error::Error,
    {
        Self(err)
    }
}

impl std::fmt::Display for ErrorChain<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)?;
        let mut source: Option<&dyn std::error::Error> = self.0.source();
        while let Some(cause) = source {
            std::fmt::Display::fmt(": ", f)?;
            std::fmt::Display::fmt(cause, f)?;
            source = cause.source();
        }
        Ok(())
    }
}

/// Transparent wrapper around an error that allows it to be split into unboxed
/// errors.
/// This is primarily useful when boxed.
#[derive(Debug, Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct Splittable<Err>(#[from] pub Err);

impl<Err> Fatality for Splittable<Err>
where
    Err: Fatality,
{
    fn is_fatal(&self) -> bool {
        self.0.is_fatal()
    }
}

impl<Err> Split for Splittable<Err>
where
    Err: Split,
{
    type Fatal = Err::Fatal;

    type Jfyi = Err::Jfyi;

    fn split(self) -> Result<Self::Jfyi, Self::Fatal> {
        self.0.split()
    }
}

impl<Err> Fatality for Box<Splittable<Err>>
where
    Err: Fatality,
{
    fn is_fatal(&self) -> bool {
        <Splittable<Err> as Fatality>::is_fatal(self)
    }
}

impl<Err> Split for Box<Splittable<Err>>
where
    Err: Split,
{
    type Fatal = <Splittable<Err> as Split>::Fatal;

    type Jfyi = <Splittable<Err> as Split>::Jfyi;

    fn split(self) -> Result<Self::Jfyi, Self::Fatal> {
        <Splittable<Err> as Split>::split(*self)
    }
}

#[cfg(test)]
mod tests {
    use serde::de::Expected;
    use thiserror::Error;

    use super::ErrorChain;

    #[test]
    fn test_error_chain_display() {
        // Display the alternative message in alternative formatting mode
        fn display_alt(f: &mut std::fmt::Formatter<'_>, msg: &str, alt: &str) -> std::fmt::Result {
            if f.alternate() {
                alt.fmt(f)
            } else {
                msg.fmt(f)
            }
        }

        #[derive(Debug, Error)]
        struct ErrorA;

        impl std::fmt::Display for ErrorA {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                display_alt(f, "error A", "error A (Alternate)")
            }
        }

        #[derive(Debug, Error)]
        #[error(transparent)]
        struct Transparent<T>(#[from] T);

        #[derive(Debug, Error)]
        struct ErrorB(#[from] Transparent<ErrorA>);

        impl std::fmt::Display for ErrorB {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                display_alt(f, "error B", "error B (Alternate)")
            }
        }

        #[derive(Debug, Error)]
        struct ErrorC(#[from] Transparent<ErrorB>);

        impl std::fmt::Display for ErrorC {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                display_alt(f, "error C", "error C (Alternate)")
            }
        }

        let err = ErrorC(ErrorB(ErrorA.into()).into());
        let err_chain = ErrorChain::new(&err);
        let err_chain_display = err_chain.to_string();
        const ERR_CHAIN_DISPLAY_EXPECTED: &str = "error C: error B: error A";
        assert_eq!(err_chain_display, ERR_CHAIN_DISPLAY_EXPECTED);
        let err_chain_display_alt = format!("{err_chain:#}");
        const ERR_CHAIN_DISPLAY_ALT_EXPECTED: &str =
            "error C (Alternate): error B (Alternate): error A (Alternate)";
        assert_eq!(err_chain_display_alt, ERR_CHAIN_DISPLAY_ALT_EXPECTED);
    }
}
