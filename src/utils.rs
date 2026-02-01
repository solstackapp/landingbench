use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::warn;

pub fn get_current_timestamp() -> f64 {
    let now = SystemTime::now();
    let since_epoch: Duration = match now.duration_since(UNIX_EPOCH) {
        Ok(d) => d,
        Err(e) => {
            // System clock went backwards; log and clamp to 0
            warn!("SystemTime error (clock skew): {}", e);
            Duration::from_secs(0)
        }
    };
    since_epoch.as_secs_f64()
}

pub fn percentile(sorted_data: &[f64], p: f64) -> f64 {
    if sorted_data.is_empty() {
        return 0.0;
    }
    let index = (p * (sorted_data.len() - 1) as f64).round() as usize;
    sorted_data[index]
}
