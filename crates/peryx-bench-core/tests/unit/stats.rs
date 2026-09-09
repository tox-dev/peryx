use statrs::statistics::Data;

use super::{Summary, geometric_mean, tukey_fences};

#[test]
fn summary_reduces_rounds() {
    assert_eq!(
        Summary::of(&[1.0, 2.0, 3.0, 4.0, 100.0]),
        Some(Summary {
            median: 3.0,
            min: 1.0,
            max: 100.0,
            cv: 1.982_620_771_596_762_5,
            outliers: 1,
            n: 5,
        })
    );
}

#[test]
fn summary_handles_empty_single_and_zero_samples() {
    assert_eq!(
        (Summary::of(&[]), Summary::of(&[4.0]), Summary::of(&[0.0, 0.0])),
        (
            None,
            Some(Summary {
                median: 4.0,
                min: 4.0,
                max: 4.0,
                cv: 0.0,
                outliers: 0,
                n: 1,
            }),
            Some(Summary {
                median: 0.0,
                min: 0.0,
                max: 0.0,
                cv: 0.0,
                outliers: 0,
                n: 2,
            }),
        )
    );
}

#[test]
fn summary_flags_wide_spread() {
    assert!(Summary::of(&[1.0, 2.0]).expect("samples are present").noisy());
    assert!(!Summary::of(&[1.0, 1.0]).expect("samples are present").noisy());
}

#[test]
fn geometric_mean_uses_positive_ratios() {
    assert_eq!(
        (geometric_mean(&[]), geometric_mean(&[-1.0, 0.0, 2.0, 8.0])),
        (None, Some(4.0))
    );
}

const fn spread(cv: f64) -> Summary {
    Summary {
        median: 1.0,
        min: 1.0,
        max: 1.0,
        cv,
        outliers: 0,
        n: 2,
    }
}

#[test]
fn noise_flag_reads_the_threshold_as_exclusive() {
    assert_eq!((spread(0.05).noisy(), spread(0.051).noisy()), (false, true));
}

#[test]
fn tukey_fences_sit_one_and_a_half_iqrs_outside_the_quartiles() {
    let mut data = Data::new(vec![1.0, 2.0, 3.0, 4.0, 5.0]);
    assert_eq!(tukey_fences(&mut data), (-2.333_333_333_333_333, 8.333_333_333_333_332));
}
