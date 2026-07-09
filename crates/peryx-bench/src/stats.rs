//! Descriptive statistics for repeated measurements: a robust point estimate with enough
//! dispersion to tell a real change from laptop noise. The heavy lifting is `statrs`; this module
//! only picks the estimators the methodology calls for and names the noise threshold.

use statrs::statistics::{Data, OrderStatistics, Statistics};

/// CV past this means environmental noise rivals the effect sizes a regression check compares, so
/// the metric is flagged rather than trusted (the systems-benchmarking rule of thumb: under ~2% is
/// excellent, up to ~5% acceptable, beyond that dominated by the environment).
const NOISY_CV: f64 = 0.05;

/// A metric measured over `n` independent rounds, reduced to a robust estimate plus the spread that
/// says whether to trust it.
///
/// The median is the point estimate: unlike best-of-`n` (the old `min`) its bias does not grow with
/// the round count, so two runs measured with different counts stay comparable, and it shrugs off
/// the one-sided scheduling and thermal spikes a laptop adds. The coefficient of variation is the
/// noise gauge; the Tukey outlier count reports how many rounds landed past the `1.5·IQR` fence
/// without discarding them (dropping points silently would bias an A/B whenever the two sides drop a
/// different number).
pub struct Summary {
    pub median: f64,
    pub min: f64,
    pub max: f64,
    /// Sample standard deviation over the mean; dimensionless noise level.
    pub cv: f64,
    /// Rounds beyond the `1.5·IQR` Tukey fence: environmental spikes, not the measured cost.
    pub outliers: usize,
    pub n: usize,
}

impl Summary {
    /// Summarize `samples`, or `None` when there is nothing to summarize.
    #[must_use]
    pub fn of(samples: &[f64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mean = samples.mean();
        let std_dev = if samples.len() > 1 { samples.std_dev() } else { 0.0 };
        let mut data = Data::new(samples.to_vec());
        let (low, high) = tukey_fences(&mut data);
        Some(Self {
            median: data.median(),
            min: samples.iter().copied().fold(f64::INFINITY, f64::min),
            max: samples.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            cv: if mean == 0.0 { 0.0 } else { std_dev / mean.abs() },
            outliers: samples.iter().filter(|&&value| value < low || value > high).count(),
            n: samples.len(),
        })
    }

    /// Whether the spread is too wide to gate a regression on.
    #[must_use]
    pub fn noisy(&self) -> bool {
        self.cv > NOISY_CV
    }
}

/// The `[Q1 - 1.5·IQR, Q3 + 1.5·IQR]` Tukey fence: values outside it are mild outliers.
fn tukey_fences(data: &mut Data<Vec<f64>>) -> (f64, f64) {
    let (q1, q3) = (data.lower_quartile(), data.upper_quartile());
    let iqr = q3 - q1;
    (1.5f64.mul_add(-iqr, q1), 1.5f64.mul_add(iqr, q3))
}

/// The geometric mean of per-workload ratios: the normalization-invariant way to reduce a suite of
/// "B is k times A" ratios to one headline number. The arithmetic mean of ratios depends on the
/// arbitrary choice of which side is the reference; the geometric mean does not (Fleming & Wallace,
/// CACM 1986), which is why SPEC aggregates this way.
#[must_use]
pub fn geometric_mean(ratios: &[f64]) -> Option<f64> {
    let positive: Vec<f64> = ratios.iter().copied().filter(|&ratio| ratio > 0.0).collect();
    (!positive.is_empty()).then(|| positive.geometric_mean())
}
