//! Exclusive output stays at the source rate.
//!
//! There is no in-app rate converter and no noise-shaper chain. 44.1 kHz
//! stays 44.1 kHz, and 48 kHz stays 48 kHz. Non-unity gain is dithered by
//! the ALSA writer's `apply_pcm_gain`. Saved keys from the old upsampler and
//! the noise-shaper menu are ignored by serde and cannot turn a converter
//! back on.

/// Device rate for this source. The exclusive writer keeps the source rate.
pub fn select_output_rate(source: u32) -> u32 {
    source
}

#[cfg(test)]
mod tests {
    use super::select_output_rate;

    #[test]
    fn source_rate_is_kept() {
        for rate in [
            44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
        ] {
            assert_eq!(select_output_rate(rate), rate);
        }
    }
}
