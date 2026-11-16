//! Romeo Disca's symmetric GAIN percentage, based on the later/former ratio.
//! https://romeo-disca.medium.com/a-more-robust-kpi-for-profits-and-losses-c0aa5ea115c2

/// Positive ratios map to `100 * (r - 1)` for r >= 1, and
/// `100 * (1 - 1/r)` for r < 1. Equal nonnegative values return zero.
/// Results are truncated toward zero to five decimal places.
/// Undefined or non-finite results return None (serialized as JSON null).
pub(crate) fn gain(former: f64, later: f64) -> Option<f64> {
    if !former.is_finite() || !later.is_finite() || former < 0.0 || later < 0.0 {
        return None;
    }
    if former == later {
        return Some(0.0);
    }
    if former == 0.0 || later == 0.0 {
        return None;
    }
    let value = if later >= former {
        100.0 * (later / former - 1.0)
    } else {
        100.0 * (1.0 - former / later)
    };
    if !value.is_finite() {
        return None;
    }
    let scaled = value * 10_000.0;
    // Values this large already have no fractional precision in f64.
    let truncated = if scaled.is_finite() {
        scaled.trunc() / 10_000.0
    } else {
        value
    };
    Some(if truncated == 0.0 { 0.0 } else { truncated })
}

#[cfg(test)]
mod tests {
    use super::gain;

    #[test]
    fn gains_and_losses_are_symmetric() {
        assert_eq!(gain(100.0, 125.0), Some(25.0));
        assert_eq!(gain(125.0, 100.0), Some(-25.0));
        assert_eq!(gain(10.0, 10.0), Some(0.0));
    }

    #[test]
    fn truncates_positive_and_negative_gains_without_rounding() {
        assert_eq!(gain(100.0, 101.23456789), Some(1.23456));
        assert_eq!(gain(101.23456789, 100.0), Some(-1.23456));
        assert_eq!(gain(100.0, 100.0000001), Some(0.0));
        assert_eq!(gain(100.0000001, 100.0), Some(0.0));
    }

    #[test]
    fn undefined_values_are_not_emitted_as_nonfinite_json_numbers() {
        assert_eq!(gain(0.0, 0.0), Some(0.0));
        for (former, later) in [
            (0.0, 1.0),
            (1.0, 0.0),
            (-1.0, 1.0),
            (f64::NAN, 1.0),
            (1.0, f64::INFINITY),
            (f64::MIN_POSITIVE, f64::MAX),
        ] {
            assert_eq!(gain(former, later), None);
        }
    }
}
