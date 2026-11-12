//! Machine-wide CPU usage.

use crate::error::Result;
use crate::pdh::CounterArray;

/// The counter Task Manager uses for overall CPU usage (Windows 8 and later). It
/// accounts for frequency scaling, so it can exceed 100 when turbo boosting.
const CPU_UTILITY_COUNTER: &str = r"\Processor Information(_Total)\% Processor Utility";

/// Samples overall CPU utilisation. Keep one alive and call
/// [`sample`](Self::sample) periodically.
pub struct MachineCpu {
    counter: CounterArray,
}

impl MachineCpu {
    /// Opens the counter and takes the baseline reading.
    pub fn new() -> Result<Self> {
        Ok(Self {
            counter: CounterArray::open(CPU_UTILITY_COUNTER)?,
        })
    }

    /// CPU utilisation since the previous call, in percent. Not capped at 100.
    pub fn sample(&mut self) -> Result<f64> {
        let mut total = 0.0;
        self.counter.collect(|_, value| total = value)?;
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn samples_machine_cpu() {
        let mut cpu = MachineCpu::new().expect("open CPU counter");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let value = cpu.sample().expect("sample CPU counter");
        assert!(value >= 0.0);
    }
}
