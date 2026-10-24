//! Persistent daemon settings in %ProgramData%\NitroControl.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    // "Smart" was a curve-based mode, removed: Auto is as quiet and game mode covers games.
    #[serde(alias = "Smart")]
    Auto,
    Max,
    Custom,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Config {
    pub mode: Mode,
    pub cpu_pct: u8,
    pub gpu_pct: u8,
    /// Custom mode is overridden by Max at these temperatures (°C), until 10 °C below.
    pub cpu_limit: u16,
    pub gpu_limit: u16,
    /// Sensor polling interval in seconds.
    pub poll_secs: u8,
    /// Higher maximum fan speed in Auto mode.
    pub coolboost: bool,
    /// While a game runs (see `game.rs`): Max fans and our game power plan.
    pub game_mode: bool,
    /// The user's power plan to return to after the current game; persisted so a restart
    /// mid-game still restores it.
    pub restore_plan: Option<String>,
}

pub const POLL_CHOICES: [u8; 3] = [1, 2, 5];

impl Default for Config {
    fn default() -> Self {
        Self {
            mode: Mode::Auto,
            cpu_pct: 50,
            gpu_pct: 50,
            cpu_limit: 90,
            gpu_limit: 85,
            poll_secs: 1,
            coolboost: false,
            game_mode: false,
            restore_plan: None,
        }
    }
}

pub fn dir() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
    PathBuf::from(base).join("NitroControl")
}

fn path() -> PathBuf {
    dir().join("config.json")
}

pub fn load() -> Config {
    std::fs::read(path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Writes atomically (temp file + rename) so a crash never leaves a half-written config.
pub fn save(cfg: &Config) -> std::io::Result<()> {
    std::fs::create_dir_all(dir())?;
    let tmp = dir().join("config.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(cfg)?)?;
    std::fs::rename(tmp, path())
}

/// Appends a timestamped line to daemon.log, truncating it once it passes 1 MB.
pub fn log(msg: &str) {
    let p = dir().join("daemon.log");
    if std::fs::metadata(&p).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::remove_file(&p);
    }
    let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {msg}\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    let _ = std::fs::create_dir_all(dir());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
        let _ = f.write_all(line.as_bytes());
    }
}
