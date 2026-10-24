//! Dev tool: loads the NVIDIA GPU with an ALU-heavy compute shader, for fan tests.
//! `gpuload [seconds]` — runs until killed when no duration is given.

use std::time::{Duration, Instant};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::Fxc::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{BOOL, Interface, PCSTR, Result, s};

const NVIDIA: u32 = 0x10DE;
const ELEMENTS: u32 = 65536;

const SHADER: &str = r#"
RWStructuredBuffer<float> o : register(u0);
[numthreads(256, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    float a = id.x * 1e-6, b = 1.0001, c = 0.9999;
    [loop] for (int i = 0; i < 4096; i++) { a = mad(a, b, c); b = mad(b, c, a); c = mad(c, a, b); }
    o[id.x & 0xFFFF] = a + b + c;
}
"#;

fn main() -> Result<()> {
    let limit = std::env::args().nth(1).and_then(|s| s.parse().ok()).map(Duration::from_secs);
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let adapter = (0..)
            .map_while(|i| factory.EnumAdapters1(i).ok())
            .find(|a| a.GetDesc1().is_ok_and(|d| d.VendorId == NVIDIA))
            .expect("no NVIDIA adapter");

        let (mut device, mut ctx) = (None, None);
        D3D11CreateDevice(
            &adapter.cast::<IDXGIAdapter>()?,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )?;
        let (device, ctx) = (device.unwrap(), ctx.unwrap());

        let mut blob = None;
        D3DCompile(
            SHADER.as_ptr() as _,
            SHADER.len(),
            PCSTR::null(),
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut blob,
            None,
        )?;
        let blob = blob.unwrap();
        let code = std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize());
        let mut cs = None;
        device.CreateComputeShader(code, None, Some(&mut cs))?;

        let desc = D3D11_BUFFER_DESC {
            ByteWidth: ELEMENTS * 4,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
            StructureByteStride: 4,
        };
        let mut buf = None;
        device.CreateBuffer(&desc, None, Some(&mut buf))?;
        let mut uav = None;
        device.CreateUnorderedAccessView(buf.as_ref().unwrap(), None, Some(&mut uav))?;
        ctx.CSSetShader(cs.as_ref(), None);
        ctx.CSSetUnorderedAccessViews(0, 1, Some(&uav), None);

        // Wait for each batch so the queue stays short (quick to stop, no TDR); poll with a
        // sleep so the CPU stays idle and doesn't skew the test.
        let mut q = None;
        device.CreateQuery(&D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 }, Some(&mut q))?;
        let q = q.unwrap();
        let start = Instant::now();
        while limit.is_none_or(|l| start.elapsed() < l) {
            for _ in 0..16 {
                ctx.Dispatch(1024, 1, 1);
            }
            ctx.End(&q);
            ctx.Flush();
            let mut done = BOOL(0);
            while done.0 == 0 {
                std::thread::sleep(Duration::from_millis(1));
                let _ = ctx.GetData(&q, Some(&mut done as *mut _ as _), size_of::<BOOL>() as u32, 0);
            }
        }
    }
    Ok(())
}
