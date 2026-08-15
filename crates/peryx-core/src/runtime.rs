use std::sync::Arc;

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub trait PrometheusSource: Send + Sync {
    fn write_metrics(&self, body: &mut String);
}
