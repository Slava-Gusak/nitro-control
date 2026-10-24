//! Game mode: while the discrete GPU's 3D/compute engines are busy, run the fans at Max (in
//! Auto mode; a manual Custom setting is left alone) and switch to our own performance power
//! plan; once the load is gone, back to whatever plan the user had.
//!
//! Max fans keep the CPU's STAPM boost (≈42 W instead of 34 W) going ~2.5x longer and avoid
//! its ~84 °C cap under combined CPU+GPU load. A cooler desktop plan between games lets that
//! thermal budget refill.

use crate::config::log;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_SUCCESS, HLOCAL, LocalFree};
use windows::Win32::System::Power::{
    PowerDeleteScheme, PowerDuplicateScheme, PowerGetActiveScheme, PowerReadFriendlyName, PowerSetActiveScheme, PowerWriteACValueIndex,
    PowerWriteFriendlyName,
};
use windows::core::GUID;

/// 3D/compute load (%) that counts as a game, and how long it has to last.
const ENTER_LOAD: f32 = 50.0;
const ENTER_AFTER: Duration = Duration::from_secs(10);
/// Below this for `EXIT_AFTER` the game is over (menus and loading screens stay above it or
/// are shorter).
const EXIT_LOAD: f32 = 25.0;
const EXIT_AFTER: Duration = Duration::from_secs(60);

/// Windows' built-in Balanced plan, present on every install: the base of our game plan.
const BALANCED: GUID = GUID::from_u128(0x381b4222_f694_41f0_9685_ff5bb260df2e);
/// Our game plan, created on first use. Fixed so it is found again after reinstalls.
pub const GAME_PLAN: &str = "4e17c0a5-3c1e-4d5e-9f6b-6e1170c0a7e1";
const GAME_PLAN_NAME: &str = "Nitro Control — Игры";

const SUB_PROCESSOR: GUID = GUID::from_u128(0x54533251_82be_4824_96c1_47b60b740d00);
const SUB_PCIE: GUID = GUID::from_u128(0x501a4d13_42af_4429_9fd1_a8218c268e20);
/// AC values that mattered in testing (see bench/): all cores awake (core parking caused the
/// difference between the plans), no clock ramp-up delay, no PCIe link power saving.
/// Battery (DC) values stay Balanced's.
const GAME_SETTINGS: [(GUID, GUID, u32); 6] = [
    (SUB_PROCESSOR, GUID::from_u128(0x893dee8e_2bef_41e0_89c6_b55d0929964c), 100), // min processor state %
    (SUB_PROCESSOR, GUID::from_u128(0xbc5038f7_23e0_4960_96da_33abaf5935ec), 100), // max processor state %
    (SUB_PROCESSOR, GUID::from_u128(0xbe337238_0d82_4146_a960_4f3749d470c7), 2),   // boost mode: aggressive
    (SUB_PROCESSOR, GUID::from_u128(0x36687f9e_e3a5_4dbf_b1dc_15eb381c6863), 0),   // EPP: performance
    (SUB_PROCESSOR, GUID::from_u128(0x0cc5b647_c1df_4637_891a_dec35c318583), 100), // core parking: min cores %
    (SUB_PCIE, GUID::from_u128(0xee12f906_d277_404b_b6da_e5fa1a576df5), 0),        // PCIe ASPM: off
];

#[derive(Default)]
pub struct GameDetect {
    since: Option<Instant>,
    active: bool,
}

impl GameDetect {
    /// Feeds one 3D-load sample; returns whether a game is running.
    pub fn update(&mut self, load: Option<f32>, now: Instant) -> bool {
        let load = load.unwrap_or(0.0);
        // `since` marks when the load first crossed the threshold towards the other state.
        let crossing = if self.active { load < EXIT_LOAD } else { load >= ENTER_LOAD };
        if !crossing {
            self.since = None;
            return self.active;
        }
        let since = *self.since.get_or_insert(now);
        if now.duration_since(since) >= if self.active { EXIT_AFTER } else { ENTER_AFTER } {
            self.active = !self.active;
            self.since = None;
        }
        self.active
    }
}

/// The active power plan as a lowercase GUID string.
pub fn active_plan() -> Option<String> {
    unsafe {
        let mut p: *mut GUID = std::ptr::null_mut();
        if PowerGetActiveScheme(None, &mut p) != ERROR_SUCCESS || p.is_null() {
            return None;
        }
        let s = format!("{:?}", *p).to_ascii_lowercase();
        let _ = LocalFree(Some(HLOCAL(p as _)));
        Some(s)
    }
}

/// Creates the game plan if needed and (re)writes its settings, so it heals if the user
/// edited or deleted it. Returns false if it can't be used.
pub fn ensure_game_plan() -> bool {
    let guid = GUID::try_from(GAME_PLAN).unwrap();
    unsafe {
        let mut size = 0u32;
        let exists = PowerReadFriendlyName(None, Some(&guid), None, None, None, &mut size) == ERROR_SUCCESS;
        if !exists {
            // With a non-null target, PowerDuplicateScheme uses that GUID instead of a new one.
            let mut target = &guid as *const GUID as *mut GUID;
            let r = PowerDuplicateScheme(None, &BALANCED, &mut target);
            if r != ERROR_SUCCESS {
                log(&format!("create game plan failed: {r:?}"));
                return false;
            }
            log("created game power plan");
        }
        let name: Vec<u8> = GAME_PLAN_NAME.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        let _ = PowerWriteFriendlyName(None, &guid, None, None, &name);
        for (sub, setting, value) in GAME_SETTINGS {
            let r = PowerWriteACValueIndex(None, &guid, Some(&sub), Some(&setting), value);
            if r != ERROR_SUCCESS {
                log(&format!("game plan setting {setting:?} failed: {r:?}"));
            }
        }
    }
    true
}

/// Removes the game plan (uninstall), switching away from it first if it is active.
pub fn delete_game_plan(restore: Option<&str>) {
    if active_plan().as_deref() == Some(GAME_PLAN) {
        let balanced = format!("{BALANCED:?}").to_ascii_lowercase();
        set_plan(restore.unwrap_or(&balanced));
    }
    let guid = GUID::try_from(GAME_PLAN).unwrap();
    let _ = unsafe { PowerDeleteScheme(None, &guid) };
}

pub fn set_plan(guid: &str) {
    let Ok(g) = GUID::try_from(guid) else {
        log(&format!("bad power plan GUID {guid}"));
        return;
    };
    if active_plan().as_deref() == Some(guid) {
        return;
    }
    let r = unsafe { PowerSetActiveScheme(None, Some(&g)) };
    log(&format!("power plan {guid}: {r:?}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enters_after_sustained_load_and_leaves_after_idle() {
        let mut g = GameDetect::default();
        let t0 = Instant::now();
        let at = |s| t0 + Duration::from_secs(s);
        assert!(!g.update(Some(90.0), at(0)));
        assert!(!g.update(Some(90.0), at(5)));
        assert!(!g.update(Some(10.0), at(8))); // a short spike doesn't count
        assert!(!g.update(Some(90.0), at(9)));
        assert!(g.update(Some(90.0), at(20)));
        assert!(g.update(Some(10.0), at(30))); // loading screen
        assert!(g.update(Some(80.0), at(60)));
        assert!(g.update(Some(10.0), at(70)));
        assert!(!g.update(Some(10.0), at(131)));
    }

    #[test]
    fn guid_round_trip() {
        let g = GUID::try_from(GAME_PLAN).unwrap();
        assert_eq!(format!("{g:?}").to_ascii_lowercase(), GAME_PLAN);
    }
}
