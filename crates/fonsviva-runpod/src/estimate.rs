//! What a pod costs, shown before it starts. RunPod bills per second:
//! the GPU while running, the container disk while running, and the volume
//! both while running and (at twice the rate) while stopped
//! (docs.runpod.io/pods/pricing).

use serde::Serialize;

use crate::client::GpuType;
use crate::templates::PodTemplate;

/// RunPod's storage rates in USD per GB per month.
pub const CONTAINER_DISK_PER_GB_MONTH: f64 = 0.10;
pub const VOLUME_RUNNING_PER_GB_MONTH: f64 = 0.10;
pub const VOLUME_STOPPED_PER_GB_MONTH: f64 = 0.20;
/// Monthly storage rates are spread over a 30-day month, the shortest
/// month RunPod could mean, so the hourly figure is never understated.
pub const HOURS_PER_MONTH: f64 = 720.0;

#[derive(Debug, Clone, Serialize, serde::Deserialize, PartialEq)]
pub struct Estimate {
    pub template: String,
    pub gpu: String,
    pub gpu_name: String,
    pub gpu_count: u32,
    pub cloud: String,
    pub gpu_per_hour: f64,
    pub container_disk_gb: u32,
    pub container_disk_per_hour: f64,
    pub volume_gb: u32,
    pub volume_per_hour_running: f64,
    /// The figure the person confirms (`confirm_cost_per_hour`), rounded to
    /// four decimals.
    pub total_per_hour: f64,
    pub planned_hours: f64,
    pub planned_total: f64,
    /// What the volume costs after a stop, until it is terminated.
    pub stopped_volume_per_hour: f64,
    pub stopped_volume_per_month: f64,
}

pub fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

pub fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

pub fn estimate(template: &PodTemplate, gpu: &GpuType, planned_hours: f64) -> Estimate {
    let gpu_count = 1;
    let gpu_per_hour = gpu.price.secure * f64::from(gpu_count);
    let container_disk_per_hour =
        f64::from(template.container_disk_gb) * CONTAINER_DISK_PER_GB_MONTH / HOURS_PER_MONTH;
    let volume_per_hour_running =
        f64::from(template.volume_gb) * VOLUME_RUNNING_PER_GB_MONTH / HOURS_PER_MONTH;
    let total_per_hour = round4(gpu_per_hour + container_disk_per_hour + volume_per_hour_running);
    Estimate {
        template: template.id.to_string(),
        gpu: gpu.id.clone(),
        gpu_name: gpu.name.clone(),
        gpu_count,
        cloud: "SECURE".to_string(),
        gpu_per_hour: round4(gpu_per_hour),
        container_disk_gb: template.container_disk_gb,
        container_disk_per_hour: round4(container_disk_per_hour),
        volume_gb: template.volume_gb,
        volume_per_hour_running: round4(volume_per_hour_running),
        total_per_hour,
        planned_hours,
        planned_total: round4(total_per_hour * planned_hours),
        stopped_volume_per_hour: round4(
            f64::from(template.volume_gb) * VOLUME_STOPPED_PER_GB_MONTH / HOURS_PER_MONTH,
        ),
        stopped_volume_per_month: round2(
            f64::from(template.volume_gb) * VOLUME_STOPPED_PER_GB_MONTH,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::GpuPrice;
    use crate::templates::template;

    #[test]
    fn llama_cpp_on_an_rtx_4090() {
        let gpu = GpuType {
            id: "NVIDIA GeForce RTX 4090".into(),
            name: "RTX 4090".into(),
            memory: 24,
            secure: true,
            community: true,
            price: GpuPrice {
                secure: 0.89,
                community: 0.34,
                serverless: None,
            },
            max_count: None,
            availability: None,
        };
        let estimate = estimate(template("llama-cpp").unwrap(), &gpu, 3.0);
        // 0.89 GPU + 20 GB x 0.10 / 720 + 50 GB x 0.10 / 720 = 0.89972...
        assert_eq!(estimate.total_per_hour, 0.8997);
        assert_eq!(estimate.planned_total, 2.6991);
        // 50 GB x 0.20 / 720 per hour, $10.00 a month while stopped.
        assert_eq!(estimate.stopped_volume_per_hour, 0.0139);
        assert_eq!(estimate.stopped_volume_per_month, 10.0);
    }
}
