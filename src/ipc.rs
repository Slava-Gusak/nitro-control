//! Line-delimited JSON over a named pipe: one request and one response per connection.
//! The elevated daemon owns the pipe; the non-elevated UI is the client.

use crate::config::{Config, Mode};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::windows::io::FromRawHandle;
use std::time::Duration;
use windows::Win32::Foundation::*;
use windows::Win32::Security::Authorization::*;
use windows::Win32::Security::*;
use windows::Win32::Storage::FileSystem::*;
use windows::Win32::System::Pipes::*;
use windows::core::w;

pub const PIPE_NAME: &str = r"\\.\pipe\nitro-control";

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    SetMode { mode: Mode },
    SetCustom { cpu: u8, gpu: u8 },
    SetPollInterval { secs: u8 },
    SetCoolBoost { on: bool },
    SetGameMode { on: bool },
    Quit,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Status {
    pub cpu_temp: Option<u16>,
    pub gpu_temp: Option<u16>,
    pub cpu_rpm: Option<u16>,
    pub gpu_rpm: Option<u16>,
    pub cpu_load: Option<f32>,
    pub gpu_load: Option<f32>,
    pub config: Config,
    /// This laptop has no Acer gaming WMI interface; the daemon exits shortly.
    pub unsupported: bool,
    /// Custom mode is overridden by Max because a temperature limit was hit.
    pub safety_active: bool,
    /// Game mode detected a game: fans at Max, game power plan.
    pub game_active: bool,
    /// CoolBoost state as reported by the BIOS.
    pub coolboost: Option<bool>,
    pub error: Option<String>,
}

pub fn request(req: &Request) -> io::Result<Status> {
    let mut last_err = None;
    // The single pipe instance is briefly absent between connections, so retry a little.
    for _ in 0..10 {
        match OpenOptions::new().read(true).write(true).open(PIPE_NAME) {
            Ok(f) => {
                let mut line = serde_json::to_string(req)?;
                line.push('\n');
                (&f).write_all(line.as_bytes())?;
                let mut resp = String::new();
                BufReader::new(&f).read_line(&mut resp)?;
                return serde_json::from_str(&resp).map_err(io::Error::other);
            }
            Err(e) => {
                last_err = Some(e);
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    Err(last_err.unwrap())
}

pub struct PipeServer {
    sa: SECURITY_ATTRIBUTES,
    first: Option<HANDLE>,
}

// The security descriptor is created once and never freed; the handle is only used by one thread.
unsafe impl Send for PipeServer {}

impl PipeServer {
    /// Fails if another daemon already owns the pipe.
    pub fn bind() -> io::Result<Self> {
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            // SYSTEM and Administrators: full; interactive (non-elevated) users: read/write.
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                w!("D:(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)"),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )?;
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let mut s = Self { sa, first: None };
        s.first = Some(s.create(true)?);
        Ok(s)
    }

    fn create(&self, first: bool) -> io::Result<HANDLE> {
        let flags = if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { FILE_FLAGS_AND_ATTRIBUTES(0) };
        let name: Vec<u16> = PIPE_NAME.encode_utf16().chain([0]).collect();
        let h = unsafe {
            CreateNamedPipeW(
                windows::core::PCWSTR(name.as_ptr()),
                PIPE_ACCESS_DUPLEX | flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                1,
                4096,
                4096,
                0,
                Some(&self.sa),
            )
        };
        if h.is_invalid() { Err(io::Error::last_os_error()) } else { Ok(h) }
    }

    pub fn run(mut self, handler: impl Fn(Request) -> Status) -> ! {
        loop {
            let h = match self.first.take().map_or_else(|| self.create(false), Ok) {
                Ok(h) => h,
                Err(e) => {
                    crate::config::log(&format!("pipe create failed: {e}"));
                    std::thread::sleep(Duration::from_secs(1));
                    continue;
                }
            };
            let connected = unsafe { ConnectNamedPipe(h, None) };
            if connected.is_ok() || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED {
                let f = unsafe { File::from_raw_handle(h.0) };
                let _ = Self::serve_one(&f, &handler);
                unsafe {
                    let _ = FlushFileBuffers(h);
                    let _ = DisconnectNamedPipe(h);
                }
                drop(f); // closes the handle
            } else {
                unsafe {
                    let _ = CloseHandle(h);
                }
            }
        }
    }

    fn serve_one(f: &File, handler: &impl Fn(Request) -> Status) -> io::Result<()> {
        let mut line = String::new();
        BufReader::new(f).read_line(&mut line)?;
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handler(req),
            Err(e) => Status { error: Some(format!("bad request: {e}")), ..Default::default() },
        };
        let mut out = serde_json::to_string(&resp)?;
        out.push('\n');
        let mut w = f;
        w.write_all(out.as_bytes())
    }
}
