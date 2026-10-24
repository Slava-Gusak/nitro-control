//! Discrete GPU utilisation from the Windows "GPU Engine" performance counters.
//! These come from the graphics kernel, so reading them never wakes a sleeping dGPU
//! (unlike NVML).

use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::Performance::*;
use windows::core::w;

const NVIDIA: u32 = 0x10DE;

#[derive(Default, Clone, Copy)]
pub struct GpuSample {
    pub total: f32,
    pub graphics: f32,
}

pub struct GpuLoad {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    /// e.g. "luid_0x00000000_0x0000d3f2", matched against counter instance names.
    luid: String,
}

unsafe impl Send for GpuLoad {}

impl GpuLoad {
    pub fn new() -> Option<Self> {
        let luid = nvidia_luid()?;
        let mut query = PDH_HQUERY::default();
        let mut counter = PDH_HCOUNTER::default();
        unsafe {
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            if PdhAddEnglishCounterW(query, w!(r"\GPU Engine(*)\Utilization Percentage"), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            PdhCollectQueryData(query); // first sample primes the rate counter
        }
        Some(Self { query, counter, luid })
    }

    /// Like Task Manager: the busiest engine type (3D, Copy, VideoDecode, ...), summed over
    /// processes; plus the 3D/compute part alone, which video playback doesn't touch.
    pub fn sample(&mut self) -> Option<GpuSample> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None);
            if size == 0 {
                return Some(GpuSample::default());
            }
            let mut buf = vec![0u8; size as usize];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return None;
            }
            let mut per_type: Vec<(String, f64)> = Vec::new();
            for item in std::slice::from_raw_parts(items, count as usize) {
                let name = item.szName.to_string().unwrap_or_default().to_ascii_lowercase();
                if !name.contains(&self.luid) {
                    continue;
                }
                let Some(engtype) = name.rsplit("engtype_").next() else { continue };
                let v = item.FmtValue.Anonymous.doubleValue;
                match per_type.iter_mut().find(|(t, _)| t == engtype) {
                    Some((_, sum)) => *sum += v,
                    None => per_type.push((engtype.to_string(), v)),
                }
            }
            let max = |f: &dyn Fn(&str) -> bool| {
                per_type.iter().filter(|(t, _)| f(t)).map(|(_, v)| *v).fold(0.0, f64::max).min(100.0) as f32
            };
            Some(GpuSample {
                total: max(&|_| true),
                graphics: max(&|t| t.starts_with("3d") || t.starts_with("compute")),
            })
        }
    }
}

impl Drop for GpuLoad {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

fn nvidia_luid() -> Option<String> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        let mut i = 0;
        while let Ok(adapter) = factory.EnumAdapters1(i) {
            i += 1;
            let Ok(desc) = adapter.GetDesc1() else { continue };
            if desc.VendorId == NVIDIA {
                let l = desc.AdapterLuid;
                return Some(format!("luid_0x{:08x}_0x{:08x}", l.HighPart as u32, l.LowPart));
            }
        }
        None
    }
}
