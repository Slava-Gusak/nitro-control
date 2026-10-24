use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Threading::GetSystemTimes;

/// Total CPU utilisation between successive `sample` calls.
pub struct CpuLoad {
    prev: Option<(u64, u64)>, // (idle, total)
}

fn ticks(t: FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}

impl CpuLoad {
    pub fn new() -> Self {
        Self { prev: None }
    }

    /// Percent 0..100; `None` on the first call.
    pub fn sample(&mut self) -> Option<f32> {
        let (mut idle, mut kernel, mut user) = Default::default();
        unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.ok()?;
        // Kernel time already includes idle time.
        let now = (ticks(idle), ticks(kernel) + ticks(user));
        let load = self.prev.and_then(|(pi, pt)| {
            let total = now.1.saturating_sub(pt);
            (total > 0).then(|| 100.0 * (1.0 - now.0.saturating_sub(pi) as f32 / total as f32))
        });
        self.prev = Some(now);
        load
    }
}
