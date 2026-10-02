//! GPU health readings (M10.2): temperature, power, fan, clocks, memory and what is limiting the card, from NVML (NVIDIA's
//! management library, through `nvml-wrapper`, which loads it at run time: **a machine without NVML, or a card that does not report
//! something, gives `None` for it and is not an error**, and mining never depends on it).
//!
//! What these are: **readings from the driver, for a person to look at.** Nothing here decides anything about mining. What is *not* here
//! is a memory bandwidth in bytes a second: NVML does not report it (it reports how much of the time the memory controller was busy,
//! `mem_busy_pct`), so the bandwidth the work implies is worked out from the attempt rate by [`implied_read_bytes_per_sec`] and is
//! labelled as that wherever it is shown.
//!
//! The NVML device is found by the same number as the CUDA device. With one GPU they are the same; with several the two orders can
//! differ (CUDA may put the fastest first), which is not handled and not tested here.

use nvml_wrapper::bitmasks::device::ThrottleReasons;
use nvml_wrapper::enum_wrappers::device::{Clock, TemperatureSensor};
use nvml_wrapper::Nvml;

/// One look at the card. Every figure is optional: a card or a driver may not report it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GpuReading {
    pub name: Option<String>,
    pub temp_c: Option<u32>,
    pub power_w: Option<f64>,
    pub fan_pct: Option<u32>,
    /// Graphics (shader) clock and memory clock, in MHz.
    pub core_mhz: Option<u32>,
    pub mem_mhz: Option<u32>,
    pub mem_used_mib: Option<u64>,
    pub mem_total_mib: Option<u64>,
    /// The share of the last sample period in which the GPU was running something, and in which its memory was being read or written.
    pub busy_pct: Option<u32>,
    pub mem_busy_pct: Option<u32>,
    /// What the driver says is holding the clocks down right now, if anything that matters here (`power cap`, `temperature`).
    pub limited_by: Option<&'static str>,
}

/// What one attempt reads of the dataset: a slice of `k * nb` bytes (16 MiB on the chain's parameters).
pub const SLICE_BYTES: u64 = (tenero_core::matmulhash::Params::DEFAULT.k
    * tenero_core::matmulhash::Params::DEFAULT.nb) as u64;

/// An open connection to NVML for one card.
pub struct GpuProbe {
    nvml: Nvml,
    index: u32,
}

impl GpuProbe {
    /// Opens NVML and checks that card number `ordinal` is there. `Err` says why not (no NVIDIA driver, no such card).
    pub fn open(ordinal: usize) -> Result<GpuProbe, String> {
        let nvml = Nvml::init().map_err(|e| format!("NVML is not available: {e}"))?;
        let index =
            u32::try_from(ordinal).map_err(|_| "the GPU number is too large".to_string())?;
        nvml.device_by_index(index)
            .map_err(|e| format!("NVML cannot open GPU {ordinal}: {e}"))?;
        Ok(GpuProbe { nvml, index })
    }

    /// One reading. Never fails: what cannot be read is `None`.
    pub fn read(&self) -> GpuReading {
        let Ok(d) = self.nvml.device_by_index(self.index) else {
            return GpuReading::default();
        };
        let mem = d.memory_info().ok();
        let util = d.utilization_rates().ok();
        let throttle = d.current_throttle_reasons().ok();
        GpuReading {
            name: d.name().ok(),
            temp_c: d.temperature(TemperatureSensor::Gpu).ok(),
            power_w: d.power_usage().ok().map(|mw| f64::from(mw) / 1000.0),
            fan_pct: d.fan_speed(0).ok(),
            core_mhz: d.clock_info(Clock::Graphics).ok(),
            mem_mhz: d.clock_info(Clock::Memory).ok(),
            mem_used_mib: mem.as_ref().map(|m| m.used / (1024 * 1024)),
            mem_total_mib: mem.as_ref().map(|m| m.total / (1024 * 1024)),
            busy_pct: util.as_ref().map(|u| u.gpu),
            mem_busy_pct: util.as_ref().map(|u| u.memory),
            limited_by: throttle.and_then(limited_by),
        }
    }
}

/// What, among the driver's reasons for a lower clock, is worth telling a miner about.
pub fn limited_by(r: ThrottleReasons) -> Option<&'static str> {
    if r.intersects(
        ThrottleReasons::HW_THERMAL_SLOWDOWN
            | ThrottleReasons::SW_THERMAL_SLOWDOWN
            | ThrottleReasons::HW_SLOWDOWN,
    ) {
        Some("temperature")
    } else if r.intersects(ThrottleReasons::SW_POWER_CAP | ThrottleReasons::HW_POWER_BRAKE_SLOWDOWN)
    {
        Some("power cap")
    } else {
        None
    }
}

/// The memory reads the work implies at `attempts_per_sec`: each attempt reads one slice of `slice_bytes` (`k * nb`, 16 MiB on the
/// chain's parameters). **An estimate from the rate, not a measurement of the memory bus** (see the top of this file).
pub fn implied_read_bytes_per_sec(attempts_per_sec: f64, slice_bytes: u64) -> f64 {
    attempts_per_sec * slice_bytes as f64
}
