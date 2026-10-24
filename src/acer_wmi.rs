//! Thin wrapper over the `root\WMI` BIOS interfaces `AcerGamingFunction` and `APGeAction`
//! (requires admin).
//!
//! Protocol notes for AN515-44 (verified by probing):
//! - GetGamingSysInfo(0x01 | sensor << 8) -> status in bits 0..8, value in bits 8..24.
//! - GetGamingFanBehavior(0x09) -> byte 1 holds CPU mode in bits 0..2 and GPU mode in bits 6..8
//!   (1 = auto, 2 = max, 3 = custom).

use windows::Win32::System::Com::*;
use windows::Win32::System::Rpc::*;
use windows::Win32::System::Variant::*;
use windows::Win32::System::Wmi::*;
use windows::core::{BSTR, HSTRING, PCWSTR, Result, w};

pub const SENSOR_CPU_TEMP: u8 = 0x01;
pub const SENSOR_CPU_FAN: u8 = 0x02;
pub const SENSOR_GPU_FAN: u8 = 0x06;
pub const SENSOR_GPU_TEMP: u8 = 0x0A;

const CMD_READ_SENSOR: u32 = 0x01;
const FAN_CPU: u32 = 0x01;
const FAN_GPU: u32 = 0x08;

/// Fan index used by Get/SetGamingFanSpeed (differs from the FanBehavior bitmap).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fan {
    Cpu = 0x01,
    Gpu = 0x04,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanMode {
    Auto,
    Max,
    Custom,
    Unknown(u8),
}

impl FanMode {
    fn from_bits(b: u8) -> Self {
        match b {
            1 => FanMode::Auto,
            2 => FanMode::Max,
            3 => FanMode::Custom,
            x => FanMode::Unknown(x),
        }
    }
}

/// APGeAction.GetFunction / SetFunction commands (from NitroSense's own calls).
const ACTION_GET_COOLBOOST: u32 = 0x207;
const ACTION_SET_COOLBOOST: u64 = 0x07;

/// One WMI class instance plus the names of its in/out method parameters.
struct WmiClass {
    class: IWbemClassObject,
    path: BSTR,
    input: PCWSTR,
    output: PCWSTR,
}

pub struct AcerWmi {
    svc: IWbemServices,
    gaming: WmiClass,
    /// Absent on models without the APGeAction interface.
    action: Option<WmiClass>,
}

impl AcerWmi {
    pub fn connect() -> Result<Self> {
        unsafe {
            // S_FALSE / RPC_E_CHANGED_MODE are fine: COM is already initialised on this thread.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = CoInitializeSecurity(
                None,
                -1,
                None,
                None,
                RPC_C_AUTHN_LEVEL_DEFAULT,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
                None,
            );

            let locator: IWbemLocator = CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER)?;
            let svc = locator.ConnectServer(
                &BSTR::from("ROOT\\WMI"),
                &BSTR::new(),
                &BSTR::new(),
                &BSTR::new(),
                0,
                &BSTR::new(),
                None,
            )?;
            CoSetProxyBlanket(
                &svc,
                RPC_C_AUTHN_WINNT,
                RPC_C_AUTHZ_NONE,
                None,
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )?;

            let gaming = Self::open_class(&svc, "AcerGamingFunction", w!("gmInput"), w!("gmOutput"))?;
            let action = Self::open_class(&svc, "APGeAction", w!("uiInput"), w!("uiOutput")).ok();
            Ok(Self { svc, gaming, action })
        }
    }

    unsafe fn open_class(svc: &IWbemServices, name: &str, input: PCWSTR, output: PCWSTR) -> Result<WmiClass> {
        unsafe {
            let mut class = None;
            svc.GetObject(&BSTR::from(name), WBEM_FLAG_RETURN_WBEM_COMPLETE, None, Some(&mut class), None)?;
            let class = class.ok_or_else(|| wbem_err(WBEM_E_NOT_FOUND))?;
            let path = Self::instance_path(svc, name)?;
            Ok(WmiClass { class, path, input, output })
        }
    }

    unsafe fn instance_path(svc: &IWbemServices, class: &str) -> Result<BSTR> {
        unsafe {
            let en = svc.ExecQuery(
                &BSTR::from("WQL"),
                &BSTR::from(format!("SELECT * FROM {class}")),
                WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                None,
            )?;
            let mut objs = [None];
            let mut n = 0;
            en.Next(WBEM_INFINITE, &mut objs, &mut n).ok()?;
            let obj = objs[0]
                .take()
                .ok_or_else(|| wbem_err(WBEM_E_NOT_FOUND))?;
            let mut v = VARIANT::default();
            obj.Get(w!("__PATH"), 0, &mut v, None, None)?;
            let path = variant_bstr(&v);
            VariantClear(&mut v)?;
            path.ok_or_else(|| wbem_err(WBEM_E_INVALID_OBJECT_PATH))
        }
    }

    /// Calls an AcerGamingFunction method whose input is uint32 and output is uint64.
    fn call_u32(&self, method: &str, input: u32) -> Result<u64> {
        // WMI marshals uint32 as VT_I4.
        self.call(&self.gaming, method, Some(variant_i32(input as i32)))
    }

    /// Calls an AcerGamingFunction method whose input is uint64 (marshalled as a decimal string).
    fn call_u64(&self, method: &str, input: u64) -> Result<u64> {
        self.call(&self.gaming, method, Some(variant_bstr_new(&input.to_string())))
    }

    fn action(&self) -> Result<&WmiClass> {
        self.action.as_ref().ok_or_else(|| wbem_err(WBEM_E_NOT_FOUND))
    }

    fn call(&self, c: &WmiClass, method: &str, input: Option<VARIANT>) -> Result<u64> {
        unsafe {
            let name = HSTRING::from(method);
            let mut in_sig = None;
            c.class.GetMethod(&name, 0, &mut in_sig, std::ptr::null_mut())?;
            let in_inst = match (in_sig, input) {
                (Some(sig), Some(mut val)) => {
                    let inst = sig.SpawnInstance(0)?;
                    let put = inst.Put(c.input, 0, &val, 0);
                    VariantClear(&mut val)?;
                    put?;
                    Some(inst)
                }
                _ => None,
            };

            let mut out = None;
            self.svc.ExecMethod(
                &c.path,
                &BSTR::from(method),
                WBEM_FLAG_RETURN_WBEM_COMPLETE,
                None,
                in_inst.as_ref(),
                Some(&mut out),
                None,
            )?;
            let out = out.ok_or_else(|| wbem_err(WBEM_E_FAILED))?;
            let mut v = VARIANT::default();
            out.Get(c.output, 0, &mut v, None, None)?;
            let n = variant_to_u64(&v);
            VariantClear(&mut v)?;
            n.ok_or_else(|| wbem_err(WBEM_E_TYPE_MISMATCH))
        }
    }

    /// Reads a sensor; returns `None` when the BIOS reports it as unsupported.
    pub fn sensor(&self, id: u8) -> Result<Option<u16>> {
        let raw = self.call_u32("GetGamingSysInfo", CMD_READ_SENSOR | (id as u32) << 8)?;
        Ok(if raw & 0xFF == 0 { Some((raw >> 8) as u16) } else { None })
    }

    pub fn fan_modes(&self) -> Result<(FanMode, FanMode)> {
        let raw = self.call_u32("GetGamingFanBehavior", FAN_CPU | FAN_GPU)?;
        let b = (raw >> 8) as u8;
        Ok((FanMode::from_bits(b & 0x03), FanMode::from_bits(b >> 6)))
    }

    /// Sets both fans to the same mode. Returns the BIOS status (0 = ok).
    /// Layout: fan bitmap in bits 0..8, CPU mode in bits 16..18, GPU mode in bits 22..24.
    pub fn set_fan_mode(&self, mode: FanMode) -> Result<u32> {
        let m: u64 = match mode {
            FanMode::Auto => 1,
            FanMode::Max => 2,
            FanMode::Custom => 3,
            FanMode::Unknown(x) => x as u64,
        };
        let input = (FAN_CPU | FAN_GPU) as u64 | m << 16 | m << 22;
        Ok(self.call_u64("SetGamingFanBehavior", input)? as u32)
    }

    /// Target duty (0..=100 %) for a fan in Custom mode. Returns the BIOS status (0 = ok).
    pub fn set_fan_speed(&self, fan: Fan, percent: u8) -> Result<u32> {
        let input = fan as u64 | (percent.min(100) as u64) << 8;
        Ok(self.call_u64("SetGamingFanSpeed", input)? as u32)
    }

    /// CoolBoost (higher maximum fan speed in Auto mode). `None` if the BIOS rejects the query.
    pub fn coolboost(&self) -> Result<Option<bool>> {
        let raw = self.call(self.action()?, "GetFunction", Some(variant_i32(ACTION_GET_COOLBOOST as i32)))?;
        Ok((raw & 0xFF == 0).then_some((raw >> 8) & 0xFF == 1))
    }

    /// Returns the BIOS status (0 = ok).
    pub fn set_coolboost(&self, on: bool) -> Result<u32> {
        let input = ACTION_SET_COOLBOOST | (on as u64) << 16;
        Ok(self.call(self.action()?, "SetFunction", Some(variant_bstr_new(&input.to_string())))? as u32)
    }

    /// Currently requested Custom duty in %, or `None` if the BIOS rejects the query.
    pub fn fan_speed(&self, fan: Fan) -> Result<Option<u8>> {
        let raw = self.call_u32("GetGamingFanSpeed", fan as u32)?;
        Ok((raw & 0xFF == 0).then_some((raw >> 8) as u8))
    }
}

/// True if the error means this laptop has no Acer gaming WMI interface at all (another
/// brand or an unsupported model), as opposed to a transient failure.
pub fn is_unsupported(e: &windows::core::Error) -> bool {
    [WBEM_E_NOT_FOUND, WBEM_E_INVALID_CLASS, WBEM_E_INVALID_NAMESPACE].iter().any(|s| e.code().0 == s.0)
}

fn wbem_err(status: WBEMSTATUS) -> windows::core::Error {
    windows::core::Error::from_hresult(windows::core::HRESULT(status.0))
}

fn variant_i32(n: i32) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_I4;
        (*v.Anonymous.Anonymous).Anonymous.lVal = n;
    }
    v
}

fn variant_bstr_new(s: &str) -> VARIANT {
    let mut v = VARIANT::default();
    unsafe {
        (*v.Anonymous.Anonymous).vt = VT_BSTR;
        (*v.Anonymous.Anonymous).Anonymous.bstrVal = std::mem::ManuallyDrop::new(BSTR::from(s));
    }
    v
}

fn variant_bstr(v: &VARIANT) -> Option<BSTR> {
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        (inner.vt == VT_BSTR).then(|| (*inner.Anonymous.bstrVal).clone())
    }
}

/// WMI returns uint64 as a decimal VT_BSTR; smaller ints come back as VT_I4.
fn variant_to_u64(v: &VARIANT) -> Option<u64> {
    unsafe {
        let inner = &v.Anonymous.Anonymous;
        match inner.vt {
            VT_BSTR => inner.Anonymous.bstrVal.to_string().parse().ok(),
            VT_I4 | VT_UI4 => Some(inner.Anonymous.lVal as u32 as u64),
            VT_I8 | VT_UI8 => Some(inner.Anonymous.llVal as u64),
            _ => None,
        }
    }
}
