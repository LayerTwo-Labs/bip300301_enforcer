//! Utility types and functions for displaying values

// TODO: remove and replace with [`std::fmt::FromFn`] once stable
// see https://github.com/rust-lang/rust/issues/117729
#[repr(transparent)]
pub struct DisplayFn<F>(F)
where
    F: Fn(&mut std::fmt::Formatter<'_>) -> std::fmt::Result;

impl<F> DisplayFn<F>
where
    F: Fn(&mut std::fmt::Formatter<'_>) -> std::fmt::Result,
{
    pub fn new(f: F) -> Self {
        Self(f)
    }
}

impl<F> std::fmt::Display for DisplayFn<F>
where
    F: Fn(&mut std::fmt::Formatter<'_>) -> std::fmt::Result,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        (self.0)(f)
    }
}

/// Display an error with causes.
/// This is useful for displaying errors without converting to
/// `miette::Report` or `anyhow::Error` first
pub struct ErrorChain<'a>(&'a (dyn std::error::Error));

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
