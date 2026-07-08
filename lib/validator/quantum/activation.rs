//! BIP 360 activation height gating.

/// Height at which BIP 360 P2MR/PQC rules become mandatory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bip360Activation(pub u32);

impl Bip360Activation {
    /// Returns whether `height` is at or past the configured activation height.
    #[must_use]
    pub fn is_active(self, height: u32) -> bool {
        height >= self.0
    }
}

#[cfg(test)]
mod tests {
    use super::Bip360Activation;

    #[test]
    fn activation_at_height_zero_is_always_active() {
        let activation = Bip360Activation(0);
        assert!(activation.is_active(0));
        assert!(activation.is_active(1));
    }

    #[test]
    fn activation_before_height_is_inactive() {
        let activation = Bip360Activation(100);
        assert!(!activation.is_active(99));
        assert!(activation.is_active(100));
    }
}
