//! Elevated background process: polls sensors, enforces the chosen fan mode and falls back
//! to safe settings when something goes wrong.

use crate::acer_wmi::{AcerWmi, Fan, FanMode, SENSOR_CPU_FAN, SENSOR_CPU_TEMP, SENSOR_GPU_FAN, SENSOR_GPU_TEMP};
use crate::config::{self, Config, Mode, POLL_CHOICES, log};
use crate::cpu_load::CpuLoad;
use crate::game::{self, GameDetect};
use crate::gpu_load::GpuLoad;
use crate::ipc::{PipeServer, Request, Status};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Safety override is released this many °C below the configured limits.
const SAFETY_HYSTERESIS: u16 = 10;
/// Consecutive failed sensor reads before handing control back to the BIOS.
const MAX_READ_FAILURES: u32 = 3;
/// Switching to Auto: the EC ramps down slowly in Auto but quickly in Custom, so hold
/// roughly Auto's idle duty for a moment first to make the switch feel instant.
const FAST_AUTO_DUTY: u8 = 30;
const FAST_AUTO_HOLD: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Auto,
    Max,
    Custom { cpu: u8, gpu: u8 },
}

impl Target {
    fn fan_mode(self) -> FanMode {
        match self {
            Target::Auto => FanMode::Auto,
            Target::Max => FanMode::Max,
            Target::Custom { .. } => FanMode::Custom,
        }
    }
}

struct Reading {
    cpu_temp: u16,
    gpu_temp: u16,
    cpu_rpm: Option<u16>,
    gpu_rpm: Option<u16>,
    modes: (FanMode, FanMode),
}

fn read(wmi: &AcerWmi) -> windows::core::Result<Reading> {
    let missing = || windows::core::Error::new(windows::core::HRESULT(-1), "temperature sensor unsupported");
    Ok(Reading {
        cpu_temp: wmi.sensor(SENSOR_CPU_TEMP)?.ok_or_else(missing)?,
        gpu_temp: wmi.sensor(SENSOR_GPU_TEMP)?.ok_or_else(missing)?,
        cpu_rpm: wmi.sensor(SENSOR_CPU_FAN)?,
        gpu_rpm: wmi.sensor(SENSOR_GPU_FAN)?,
        modes: wmi.fan_modes()?,
    })
}

fn apply(wmi: &AcerWmi, t: Target, modes: (FanMode, FanMode)) -> windows::core::Result<()> {
    let want = t.fan_mode();
    let set_duty = || match t {
        Target::Custom { cpu, gpu } => {
            wmi.set_fan_speed(Fan::Cpu, cpu)?;
            wmi.set_fan_speed(Fan::Gpu, gpu).map(drop)
        }
        _ => Ok(()),
    };
    // Write the duty before entering Custom: otherwise the EC briefly runs at its last
    // remembered duty, and if that is higher it then ramps down slowly (audible bump).
    set_duty()?;
    if modes != (want, want) {
        wmi.set_fan_mode(want)?;
        set_duty()?;
    }
    log(&format!("applied {t:?}"));
    Ok(())
}

pub fn run() {
    let server = match PipeServer::bind() {
        Ok(s) => s,
        Err(_) => return, // another daemon is already running
    };
    log("daemon started");

    // If we crash, leave the fans under BIOS control rather than stuck at a fixed duty.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("panic: {info}"));
        if let Ok(w) = AcerWmi::connect() {
            let _ = w.set_fan_mode(FanMode::Auto);
        }
        default_hook(info);
    }));

    let mut cfg = config::load();

    let shared = Arc::new(Mutex::new(Status { config: cfg.clone(), ..Default::default() }));
    let (tx, rx) = mpsc::channel::<Request>();

    std::thread::spawn(|| crate::keyhook::run(|| {
        std::thread::spawn(crate::keyhook::open_ui);
    }));

    let pipe_shared = shared.clone();
    std::thread::spawn(move || {
        server.run(move |req| {
            if !matches!(req, Request::Status) {
                let _ = tx.send(req);
            }
            pipe_shared.lock().unwrap().clone()
        })
    });

    let wmi = loop {
        match AcerWmi::connect() {
            Ok(w) => break w,
            Err(e) if crate::acer_wmi::is_unsupported(&e) => {
                // Not an Acer Nitro/Predator: tell the UI, never set up autostart, and leave.
                log(&format!("unsupported laptop: {e}"));
                let mut s = shared.lock().unwrap();
                s.unsupported = true;
                s.error = Some(format!("{e}"));
                drop(s);
                std::thread::sleep(Duration::from_secs(60));
                std::process::exit(0);
            }
            Err(e) => {
                log(&format!("WMI connect failed: {e}"));
                shared.lock().unwrap().error = Some(format!("WMI: {e}"));
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    };

    // Supported laptop: (re)create the autostart task, refreshing the exe path in case it was
    // moved (off the hot path: schtasks is slow).
    std::thread::spawn(crate::install::create);

    let mut cpu_load = CpuLoad::new();
    let mut gpu_load = GpuLoad::new();
    let mut game_detect = GameDetect::default();
    let mut in_game = false;
    // Stopped mid-game last time: give the user their plan back.
    if let Some(p) = cfg.restore_plan.take() {
        game::set_plan(&p);
        save(&cfg);
    }
    let mut fast_auto_until: Option<Instant> = None;
    let mut coolboost: Option<bool> = None;
    let mut applied: Option<Target> = None;
    let mut safety = false;
    let mut failures = 0;
    let mut tick: u64 = 0;
    let mut last_tick = Instant::now();

    loop {
        let poll = Duration::from_secs(cfg.poll_secs.max(1) as u64);
        // Wake up in time to end the fast-Auto hold even with a slow poll interval.
        let wait = fast_auto_until
            .map(|t| t.saturating_duration_since(Instant::now()))
            .filter(|d| !d.is_zero())
            .map_or(poll, |d| d.min(poll));
        match rx.recv_timeout(wait) {
            Ok(req) => {
                // Drain any burst of commands before touching the hardware.
                for req in std::iter::once(req).chain(rx.try_iter()) {
                    if handle(req, &mut cfg, &wmi, &mut fast_auto_until, applied) {
                        applied = None;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }

        // A long gap means the machine slept; the EC may have reset the fans.
        if last_tick.elapsed() > poll + Duration::from_secs(5) {
            log("resume detected");
            applied = None;
        }
        last_tick = Instant::now();
        tick += 1;

        let r = match read(&wmi) {
            Ok(r) => {
                failures = 0;
                r
            }
            Err(e) => {
                failures += 1;
                log(&format!("sensor read failed ({failures}): {e}"));
                if failures == MAX_READ_FAILURES {
                    let _ = wmi.set_fan_mode(FanMode::Auto);
                    applied = Some(Target::Auto);
                }
                let mut s = shared.lock().unwrap();
                s.error = Some(format!("sensors: {e}"));
                continue;
            }
        };

        let cpu_load = cpu_load.sample();
        let gpu = gpu_load.as_mut().and_then(GpuLoad::sample);
        let gpu_load = gpu.map(|g| g.total);

        let playing = cfg.game_mode && game_detect.update(gpu.map(|g| g.graphics), Instant::now());
        if playing != in_game {
            in_game = playing;
            log(if playing { "game started" } else { "game ended" });
            if playing {
                let current = game::active_plan();
                if current.as_deref() != Some(game::GAME_PLAN) && game::ensure_game_plan() {
                    cfg.restore_plan = current;
                    game::set_plan(game::GAME_PLAN);
                }
            } else if let Some(p) = cfg.restore_plan.take() {
                game::set_plan(&p);
            }
            save(&cfg);
            if !playing && cfg.mode == Mode::Auto {
                fast_auto_until = Some(Instant::now() + FAST_AUTO_HOLD);
            }
        }

        let hot = r.cpu_temp >= cfg.cpu_limit || r.gpu_temp >= cfg.gpu_limit;
        let cool = r.cpu_temp < cfg.cpu_limit - SAFETY_HYSTERESIS && r.gpu_temp < cfg.gpu_limit - SAFETY_HYSTERESIS;
        if cfg.mode != Mode::Custom || cool {
            if safety {
                log("safety released");
            }
            safety = false;
        } else if hot && !safety {
            log(&format!("safety engaged: CPU {}C GPU {}C", r.cpu_temp, r.gpu_temp));
            safety = true;
        }

        let target = match (safety, cfg.mode) {
            (true, _) | (false, Mode::Max) => Target::Max,
            (false, Mode::Auto) if in_game => Target::Max,
            (false, Mode::Auto) if fast_auto_until.is_some_and(|t| Instant::now() < t) => {
                Target::Custom { cpu: FAST_AUTO_DUTY, gpu: FAST_AUTO_DUTY }
            }
            (false, Mode::Auto) => Target::Auto,
            (false, Mode::Custom) => Target::Custom { cpu: cfg.cpu_pct, gpu: cfg.gpu_pct },
        };

        // Re-apply when the target changed, when something else (NitroSense, the EC after
        // sleep) switched the mode, or periodically when the custom duty was overwritten.
        let want = target.fan_mode();
        let mut drift = r.modes != (want, want);
        if let Target::Custom { cpu, gpu } = target {
            if tick % 5 == 0 {
                drift |= wmi.fan_speed(Fan::Cpu).ok().flatten() != Some(cpu)
                    || wmi.fan_speed(Fan::Gpu).ok().flatten() != Some(gpu);
            }
        }
        // CoolBoost: check every few ticks (and right after any change) and re-apply if the
        // BIOS state differs from the config, e.g. after a reboot or sleep.
        if applied.is_none() || tick % 5 == 0 {
            coolboost = wmi.coolboost().ok().flatten();
            if coolboost.is_some_and(|on| on != cfg.coolboost) {
                match wmi.set_coolboost(cfg.coolboost) {
                    Ok(0) => {
                        coolboost = Some(cfg.coolboost);
                        log(&format!("applied CoolBoost {}", cfg.coolboost));
                    }
                    other => log(&format!("set CoolBoost failed: {other:?}")),
                }
            }
        }

        if applied != Some(target) || drift {
            match apply(&wmi, target, r.modes) {
                Ok(()) => applied = Some(target),
                Err(e) => log(&format!("apply {target:?} failed: {e}")),
            }
        }

        let mut s = shared.lock().unwrap();
        *s = Status {
            cpu_temp: Some(r.cpu_temp),
            gpu_temp: Some(r.gpu_temp),
            cpu_rpm: r.cpu_rpm,
            gpu_rpm: r.gpu_rpm,
            cpu_load,
            gpu_load,
            config: cfg.clone(),
            unsupported: false,
            safety_active: safety,
            game_active: in_game,
            coolboost,
            error: None,
        };
    }
}

/// Updates the config from a UI command. Returns true if the fans need re-applying.
fn handle(
    req: Request,
    cfg: &mut Config,
    wmi: &AcerWmi,
    fast_auto_until: &mut Option<Instant>,
    applied: Option<Target>,
) -> bool {
    match req {
        Request::Status => return false,
        Request::SetMode { mode } => {
            // Only worth it when the fans spin faster than the hold duty; from a lower Custom
            // duty the hold itself would be a bump.
            let slow = matches!(applied, Some(Target::Custom { cpu, gpu }) if cpu.max(gpu) <= FAST_AUTO_DUTY);
            if mode == Mode::Auto && cfg.mode != Mode::Auto && !slow {
                *fast_auto_until = Some(Instant::now() + FAST_AUTO_HOLD);
            }
            cfg.mode = mode;
        }
        Request::SetCustom { cpu, gpu } => {
            cfg.cpu_pct = cpu.min(100);
            cfg.gpu_pct = gpu.min(100);
        }
        Request::SetCoolBoost { on } => cfg.coolboost = on,
        Request::SetGameMode { on } => cfg.game_mode = on,
        Request::SetPollInterval { secs } => {
            if POLL_CHOICES.contains(&secs) {
                cfg.poll_secs = secs;
            }
        }
        Request::Quit => {
            log("quit requested, restoring Auto");
            let _ = wmi.set_fan_mode(FanMode::Auto);
            std::process::exit(0);
        }
    }
    save(cfg);
    true
}

fn save(cfg: &Config) {
    if let Err(e) = config::save(cfg) {
        log(&format!("config save failed: {e}"));
    }
}
