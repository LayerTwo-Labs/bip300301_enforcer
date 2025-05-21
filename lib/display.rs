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
