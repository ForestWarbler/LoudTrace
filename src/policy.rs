pub const NORMAL_TARGET_LUFS: f64 = -14.0;
pub const POSITIVE_GAIN_CEILING_DBTP: f64 = -1.0;

// Reference model based on Spotify's public Normal playback description:
// https://support.spotify.com/us/artists/article/loudness-normalization/
// This is not Spotify's official implementation. It models playback gain only;
// transcoding, device behavior, and the Loud setting's limiter are not simulated.
pub fn normal_gain(integrated_lufs: Option<f64>, true_peak_dbtp: Option<f64>) -> Option<f64> {
    let loudness = integrated_lufs.filter(|value| value.is_finite())?;
    let peak = true_peak_dbtp.filter(|value| value.is_finite())?;
    let desired = NORMAL_TARGET_LUFS - loudness;
    if desired <= 0.0 {
        Some(desired)
    } else {
        // A master already above the peak ceiling is not attenuated merely
        // because it cannot be boosted. The ceiling constrains positive gain.
        Some(desired.min((POSITIVE_GAIN_CEILING_DBTP - peak).max(0.0)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spotify_normal_reference_cases() {
        let cases = [
            (-8.0, -0.3, -6.0),
            (-11.0, -1.0, -3.0),
            (-14.0, -1.0, 0.0),
            (-20.0, -5.0, 4.0), // Spotify's published headroom example.
            (-20.0, -10.0, 6.0),
            (-20.0, -1.0, 0.0),
            (-20.0, 0.5, 0.0),
            (-13.0, 3.0, -1.0),
        ];
        for (loudness, peak, expected) in cases {
            assert_eq!(normal_gain(Some(loudness), Some(peak)), Some(expected));
        }
    }

    #[test]
    fn unavailable_or_nonfinite_measurements_have_no_prediction() {
        assert_eq!(normal_gain(None, Some(-1.0)), None);
        assert_eq!(normal_gain(Some(-14.0), None), None);
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(normal_gain(Some(invalid), Some(-1.0)), None);
            assert_eq!(normal_gain(Some(-14.0), Some(invalid)), None);
        }
    }
}
