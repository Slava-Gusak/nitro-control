//! Native window: Win32 + Direct2D (software render target) + DirectWrite.
//! Nothing touches either GPU, so an open window costs a few MB and never wakes the dGPU.
//! Talks to the daemon over the pipe from a worker thread so painting never blocks on I/O.

use crate::config::{Mode, POLL_CHOICES};
use crate::ipc::{self, Request, Status};
use std::cell::RefCell;
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{BOOL, HSTRING, PCWSTR, Result, w};

pub const WINDOW_TITLE: &str = "Nitro Control";

const WM_APP_STATUS: u32 = WM_APP + 1;
const WM_MOUSELEAVE: u32 = 0x02A3;

// Layout in device-independent pixels.
const W: f32 = 480.0;
const H: f32 = 432.0;
const M: f32 = 18.0;

type Rgb = u32;
const ACCENT: Rgb = 0xE5322D;
const BG: Rgb = 0x141518;
const CARD: Rgb = 0x1E2024;
const TRACK: Rgb = 0x2C2F35;
const TEXT: Rgb = 0xE8E9EB;
const MUTED: Rgb = 0x8B8F98;
const OK: Rgb = 0x4CC06A;
const WARN: Rgb = 0xF0A030;
const WHITE: Rgb = 0xFFFFFF;

/// Segoe MDL2 Assets glyphs.
const ICON_SETTINGS: &str = "\u{E713}";
const ICON_BACK: &str = "\u{E72B}";

/// Highest RPM observed at 100 % duty, used to scale the fan bars.
const CPU_FAN_MAX: f32 = 5000.0;
const GPU_FAN_MAX: f32 = 5800.0;

const MODES: [(Mode, &str); 3] = [(Mode::Auto, "Авто"), (Mode::Max, "Максимум"), (Mode::Custom, "Вручную")];

fn color(c: Rgb, alpha: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((c >> 16) & 0xFF) as f32 / 255.0,
        g: ((c >> 8) & 0xFF) as f32 / 255.0,
        b: (c & 0xFF) as f32 / 255.0,
        a: alpha,
    }
}

fn rect(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F { left: x, top: y, right: x + w, bottom: y + h }
}

fn contains(r: &D2D_RECT_F, x: f32, y: f32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

fn temp_color(t: u16) -> Rgb {
    match t {
        85.. => ACCENT,
        70.. => WARN,
        _ => OK,
    }
}

#[derive(Default)]
struct Shared {
    status: Option<Status>,
    connected: bool,
    /// Sticky: the daemon said this laptop isn't supported (it exits afterwards).
    unsupported: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum SliderId {
    CpuDuty,
    GpuDuty,
}

impl SliderId {
    /// (min, max, step)
    fn range(self) -> (i32, i32, i32) {
        (0, 100, 5)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Mode(Mode),
    Slider(SliderId),
    Launch,
    OpenSettings,
    Back,
    Poll(u8),
    CoolBoost,
    GameMode,
    Uninstall,
}

/// Actions that open a modal prompt (UAC, message box). Those pump messages back into
/// `wndproc`, so the caller runs them outside the window borrow.
#[derive(PartialEq)]
enum Modal {
    LaunchDaemon,
    Uninstall,
}

/// State of the settings page. Only exists while that page is open.
struct SettingsPage {
    poll_secs: u8,
    game_mode: bool,
}

struct Fonts {
    logo: IDWriteTextFormat,
    caption: IDWriteTextFormat,
    big: IDWriteTextFormat,
    label: IDWriteTextFormat,
    value: IDWriteTextFormat,
    center: IDWriteTextFormat,
    small: IDWriteTextFormat,
    small_right: IDWriteTextFormat,
    small_center: IDWriteTextFormat,
    heading: IDWriteTextFormat,
    icon: IDWriteTextFormat,
}

struct Window {
    hwnd: HWND,
    d2d: ID2D1Factory,
    fonts: Fonts,
    target: Option<(ID2D1HwndRenderTarget, ID2D1SolidColorBrush)>,
    shared: Arc<Mutex<Shared>>,
    tx: Sender<Request>,
    cpu_pct: u8,
    gpu_pct: u8,
    synced: bool,
    /// `Some` while the settings page is shown.
    settings: Option<SettingsPage>,
    hits: Vec<(D2D_RECT_F, Hit)>,
    slider_tracks: Vec<(SliderId, D2D_RECT_F)>,
    hover: Option<Hit>,
    drag: Option<SliderId>,
    tracking_leave: bool,
    launch_requested: bool,
}

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
}

fn text_format(
    dw: &IDWriteFactory,
    family: PCWSTR,
    size: f32,
    weight: DWRITE_FONT_WEIGHT,
    align: DWRITE_TEXT_ALIGNMENT,
) -> Result<IDWriteTextFormat> {
    unsafe {
        let f = dw.CreateTextFormat(family, None, weight, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_STRETCH_NORMAL, size, w!("ru-ru"))?;
        f.SetTextAlignment(align)?;
        f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER)?;
        f.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
        Ok(f)
    }
}

impl Fonts {
    fn new(dw: &IDWriteFactory) -> Result<Self> {
        let (l, c, r) = (DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_TRAILING);
        let ui = w!("Segoe UI");
        Ok(Self {
            logo: text_format(dw, ui, 20.0, DWRITE_FONT_WEIGHT_BOLD, l)?,
            caption: text_format(dw, ui, 11.5, DWRITE_FONT_WEIGHT_SEMI_BOLD, l)?,
            big: text_format(dw, ui, 36.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, l)?,
            label: text_format(dw, ui, 13.0, DWRITE_FONT_WEIGHT_NORMAL, l)?,
            value: text_format(dw, ui, 13.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, r)?,
            center: text_format(dw, ui, 14.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, c)?,
            small: text_format(dw, ui, 12.0, DWRITE_FONT_WEIGHT_NORMAL, l)?,
            small_right: text_format(dw, ui, 11.5, DWRITE_FONT_WEIGHT_NORMAL, r)?,
            small_center: text_format(dw, ui, 12.0, DWRITE_FONT_WEIGHT_NORMAL, c)?,
            heading: text_format(dw, ui, 16.0, DWRITE_FONT_WEIGHT_SEMI_BOLD, c)?,
            icon: text_format(dw, w!("Segoe MDL2 Assets"), 16.0, DWRITE_FONT_WEIGHT_NORMAL, c)?,
        })
    }
}

/// Drawing helpers bound to one frame.
struct Painter<'a> {
    rt: &'a ID2D1HwndRenderTarget,
    brush: &'a ID2D1SolidColorBrush,
}

impl Painter<'_> {
    fn fill(&self, r: D2D_RECT_F, radius: f32, c: Rgb, alpha: f32) {
        unsafe {
            self.brush.SetColor(&color(c, alpha));
            self.rt.FillRoundedRectangle(&D2D1_ROUNDED_RECT { rect: r, radiusX: radius, radiusY: radius }, self.brush);
        }
    }

    fn circle(&self, x: f32, y: f32, radius: f32, c: Rgb) {
        unsafe {
            self.brush.SetColor(&color(c, 1.0));
            let e = D2D1_ELLIPSE { point: windows_numerics::Vector2 { X: x, Y: y }, radiusX: radius, radiusY: radius };
            self.rt.FillEllipse(&e, self.brush);
        }
    }

    fn text(&self, s: &str, f: &IDWriteTextFormat, r: D2D_RECT_F, c: Rgb) {
        let wide: Vec<u16> = s.encode_utf16().collect();
        unsafe {
            self.brush.SetColor(&color(c, 1.0));
            self.rt.DrawText(&wide, f, &r, self.brush, D2D1_DRAW_TEXT_OPTIONS_NONE, DWRITE_MEASURING_MODE_NATURAL);
        }
    }

    fn toggle(&self, x: f32, y: f32, on: bool) {
        self.fill(rect(x, y, 40.0, 22.0), 11.0, if on { ACCENT } else { TRACK }, 1.0);
        self.circle(if on { x + 29.0 } else { x + 11.0 }, y + 11.0, 8.0, WHITE);
    }

    fn bar(&self, x: f32, y: f32, w: f32, frac: f32, c: Rgb) {
        self.fill(rect(x, y, w, 6.0), 3.0, TRACK, 1.0);
        let fw = w * frac.clamp(0.0, 1.0);
        if fw > 1.0 {
            self.fill(rect(x, y, fw.max(6.0), 6.0), 3.0, c, 1.0);
        }
    }
}

struct Device<'a> {
    title: &'a str,
    temp: Option<u16>,
    load: Option<f32>,
    rpm: Option<u16>,
    rpm_max: f32,
}

fn dash<T>(v: Option<T>, f: impl Fn(T) -> String) -> String {
    v.map_or("—".into(), f)
}

impl Window {
    fn create_target(&mut self) -> Result<()> {
        if self.target.is_some() {
            return Ok(());
        }
        unsafe {
            let mut rc = RECT::default();
            GetClientRect(self.hwnd, &mut rc)?;
            let dpi = GetDpiForWindow(self.hwnd) as f32;
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
                dpiX: dpi,
                dpiY: dpi,
                ..Default::default()
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd: self.hwnd,
                pixelSize: D2D_SIZE_U { width: rc.right as u32, height: rc.bottom as u32 },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let rt = self.d2d.CreateHwndRenderTarget(&props, &hwnd_props)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
            let brush = rt.CreateSolidColorBrush(&color(WHITE, 1.0), None)?;
            self.target = Some((rt, brush));
        }
        Ok(())
    }

    fn paint(&mut self) {
        if self.create_target().is_err() {
            return;
        }
        let (status, connected) = {
            let s = self.shared.lock().unwrap();
            (s.status.clone(), s.connected)
        };
        if let Some(st) = status.as_ref().filter(|_| !self.synced) {
            self.cpu_pct = st.config.cpu_pct;
            self.gpu_pct = st.config.gpu_pct;
            self.synced = true;
        }
        if connected {
            self.launch_requested = false;
        } else {
            self.settings = None;
        }

        let (rt, brush) = self.target.clone().unwrap();
        self.hits.clear();
        self.slider_tracks.clear();
        unsafe {
            rt.BeginDraw();
            rt.Clear(Some(&color(BG, 1.0)));
        }
        let p = Painter { rt: &rt, brush: &brush };
        let unsupported = self.shared.lock().unwrap().unsupported;
        match status.filter(|_| connected) {
            _ if unsupported => self.draw_unsupported(&p),
            Some(_) if self.settings.is_some() => self.draw_settings(&p),
            Some(st) => self.draw_main(&p, &st),
            None => self.draw_disconnected(&p),
        }
        if unsafe { rt.EndDraw(None, None) }.is_err() {
            self.target = None; // device lost; recreate on next paint
        }
    }

    fn icon_button(&mut self, p: &Painter, glyph: &str, r: D2D_RECT_F, hit: Hit) {
        if self.hover == Some(hit) {
            p.fill(r, 6.0, TRACK, 1.0);
        }
        p.text(glyph, &self.fonts.icon, r, TEXT);
        self.hits.push((r, hit));
    }

    fn draw_header(&mut self, p: &Painter, connected: bool) {
        p.text("NITRO", &self.fonts.logo, rect(M, M - 4.0, 80.0, 30.0), ACCENT);
        p.text("CONTROL", &self.fonts.logo, rect(M + 68.0, M - 4.0, 120.0, 30.0), TEXT);
        if connected {
            self.icon_button(p, ICON_SETTINGS, rect(W - M - 30.0, M - 4.0, 30.0, 30.0), Hit::OpenSettings);
        }
    }

    fn draw_card(&self, p: &Painter, x: f32, y: f32, w: f32, d: Device) {
        p.fill(rect(x, y, w, 178.0), 10.0, CARD, 1.0);
        let (ix, iw) = (x + 14.0, w - 28.0);
        p.text(d.title, &self.fonts.caption, rect(ix, y + 10.0, iw, 18.0), MUTED);
        let (txt, col) = match d.temp {
            Some(t) => (format!("{t}°C"), temp_color(t)),
            None => ("—".into(), MUTED),
        };
        p.text(&txt, &self.fonts.big, rect(ix, y + 28.0, iw, 46.0), col);
        p.bar(ix, y + 78.0, iw, d.temp.map_or(0.0, |t| t as f32 / 100.0), col);

        let rows = [
            ("Нагрузка", dash(d.load, |l| format!("{l:.0}%")), d.load.map_or(0.0, |l| l / 100.0), TEXT),
            ("Вентилятор", dash(d.rpm, |r| format!("{r} RPM")), d.rpm.map_or(0.0, |r| r as f32 / d.rpm_max), ACCENT),
        ];
        for (i, (label, value, frac, c)) in rows.into_iter().enumerate() {
            let ry = y + 96.0 + i as f32 * 38.0;
            p.text(label, &self.fonts.label, rect(ix, ry, iw, 20.0), MUTED);
            p.text(&value, &self.fonts.value, rect(ix, ry, iw, 20.0), TEXT);
            p.bar(ix, ry + 23.0, iw, frac, c);
        }
    }

    /// Segmented control: one rounded strip with equal-width options.
    fn segments(&mut self, p: &Painter, outer: D2D_RECT_F, items: &[(Hit, String, bool)]) {
        p.fill(outer, 9.0, CARD, 1.0);
        let h = outer.bottom - outer.top - 8.0;
        let seg_w = (outer.right - outer.left - 8.0) / items.len() as f32;
        for (i, (hit, label, selected)) in items.iter().enumerate() {
            let r = rect(outer.left + 4.0 + i as f32 * seg_w, outer.top + 4.0, seg_w, h);
            if *selected {
                p.fill(r, 7.0, ACCENT, 1.0);
            } else if self.hover == Some(*hit) {
                p.fill(r, 7.0, TRACK, 1.0);
            }
            p.text(label, &self.fonts.center, r, if *selected { WHITE } else { TEXT });
            self.hits.push((r, *hit));
        }
    }

    fn slider_value(&self, id: SliderId) -> i32 {
        match id {
            SliderId::CpuDuty => self.cpu_pct as i32,
            SliderId::GpuDuty => self.gpu_pct as i32,
        }
    }

    fn slider(&mut self, p: &Painter, id: SliderId, label: &str, value_text: String, y: f32, enabled: bool) {
        let (min, max, _) = id.range();
        let frac = (self.slider_value(id) - min) as f32 / (max - min) as f32;
        let alpha = if enabled { 1.0 } else { 0.35 };
        p.text(label, &self.fonts.label, rect(M, y, 40.0, 20.0), if enabled { MUTED } else { TRACK });
        let track = rect(M + 44.0, y + 7.0, W - 2.0 * M - 44.0 - 60.0, 6.0);
        p.fill(track, 3.0, TRACK, 1.0);
        let knob_x = track.left + (track.right - track.left) * frac;
        p.fill(D2D_RECT_F { right: knob_x.max(track.left + 6.0), ..track }, 3.0, ACCENT, alpha);
        let knob = if self.drag == Some(id) || self.hover == Some(Hit::Slider(id)) { 9.0 } else { 7.5 };
        p.circle(knob_x, track.top + 3.0, knob, if enabled { WHITE } else { TRACK });
        p.text(&value_text, &self.fonts.value, rect(W - M - 54.0, y, 54.0, 20.0), if enabled { TEXT } else { TRACK });
        self.slider_tracks.push((id, track));
        if enabled {
            self.hits.push((rect(track.left - 10.0, y - 4.0, track.right - track.left + 20.0, 28.0), Hit::Slider(id)));
        }
    }

    fn draw_main(&mut self, p: &Painter, st: &Status) {
        self.draw_header(p, true);
        let gap = 12.0;
        let cw = (W - 2.0 * M - gap) / 2.0;
        let y = 60.0;
        self.draw_card(p, M, y, cw, Device { title: "ПРОЦЕССОР", temp: st.cpu_temp, load: st.cpu_load, rpm: st.cpu_rpm, rpm_max: CPU_FAN_MAX });
        self.draw_card(p, M + cw + gap, y, cw, Device { title: "ВИДЕОКАРТА", temp: st.gpu_temp, load: st.gpu_load, rpm: st.gpu_rpm, rpm_max: GPU_FAN_MAX });

        p.text("ВЕНТИЛЯТОРЫ", &self.fonts.caption, rect(M, 256.0, 200.0, 18.0), MUTED);
        let items: Vec<_> = MODES.iter().map(|&(m, l)| (Hit::Mode(m), l.to_string(), st.config.mode == m)).collect();
        self.segments(p, rect(M, 280.0, W - 2.0 * M, 40.0), &items);

        // Controls for the active mode only.
        match st.config.mode {
            Mode::Custom => {
                self.slider(p, SliderId::CpuDuty, "CPU", format!("{}%", self.cpu_pct), 340.0, true);
                self.slider(p, SliderId::GpuDuty, "GPU", format!("{}%", self.gpu_pct), 372.0, true);
            }
            Mode::Auto => self.coolboost_row(p, st),
            Mode::Max => {
                p.text("Вентиляторы работают на максимальной скорости.", &self.fonts.small, rect(M, 340.0, W - 2.0 * M, 20.0), MUTED);
            }
        }

        let (msg, c) = if st.safety_active {
            ("Перегрев: временно включён максимум".to_string(), ACCENT)
        } else if st.game_active {
            ("Игровой режим: вентиляторы на максимуме".to_string(), OK)
        } else if let Some(e) = &st.error {
            (format!("Ошибка: {e}"), ACCENT)
        } else {
            (String::new(), MUTED)
        };
        p.text(&msg, &self.fonts.small, rect(M, H - M - 16.0, W - 2.0 * M, 18.0), c);
    }

    fn coolboost_row(&mut self, p: &Painter, st: &Status) {
        let row = rect(M, 334.0, W - 2.0 * M, 56.0);
        if self.hover == Some(Hit::CoolBoost) {
            p.fill(row, 10.0, TRACK, 1.0);
        } else {
            p.fill(row, 10.0, CARD, 1.0);
        }
        p.text("CoolBoost", &self.fonts.label, rect(M + 14.0, 342.0, 300.0, 20.0), TEXT);
        p.text("Выше максимальная скорость при нагрузке", &self.fonts.small, rect(M + 14.0, 362.0, 320.0, 18.0), MUTED);
        let on = st.config.coolboost;
        p.toggle(W - M - 54.0, 351.0, on);
        if st.coolboost.is_some_and(|hw| hw != on) {
            p.text("…", &self.fonts.small_right, rect(W - M - 90.0, 351.0, 30.0, 22.0), MUTED);
        }
        self.hits.push((row, Hit::CoolBoost));
    }

    fn draw_settings(&mut self, p: &Painter) {
        self.icon_button(p, ICON_BACK, rect(M - 6.0, M - 4.0, 30.0, 30.0), Hit::Back);
        p.text("Настройки", &self.fonts.logo, rect(M + 32.0, M - 4.0, 200.0, 30.0), TEXT);
        let Some(page) = &self.settings else { return };
        let poll = page.poll_secs;
        let game_mode = page.game_mode;

        p.text("Опрос датчиков", &self.fonts.label, rect(M, 64.0, 200.0, 36.0), TEXT);
        let items: Vec<_> = POLL_CHOICES.iter().map(|&s| (Hit::Poll(s), format!("{s} с"), poll == s)).collect();
        self.segments(p, rect(W - M - 180.0, 64.0, 180.0, 36.0), &items);

        let row = rect(M, 114.0, W - 2.0 * M, 62.0);
        p.fill(row, 10.0, if self.hover == Some(Hit::GameMode) { TRACK } else { CARD }, 1.0);
        p.text("Игровой режим", &self.fonts.label, rect(M + 14.0, 122.0, 300.0, 20.0), TEXT);
        p.text("Когда видеокарта занята игрой: максимум", &self.fonts.small, rect(M + 14.0, 141.0, 340.0, 16.0), MUTED);
        p.text("вентиляторов и игровой план питания", &self.fonts.small, rect(M + 14.0, 156.0, 340.0, 16.0), MUTED);
        p.toggle(W - M - 54.0, 134.0, game_mode);
        self.hits.push((row, Hit::GameMode));

        let b = rect(M - 8.0, H - M - 24.0, 190.0, 30.0);
        if self.hover == Some(Hit::Uninstall) {
            p.fill(b, 6.0, TRACK, 1.0);
        }
        p.text("Удалить Nitro Control…", &self.fonts.small, rect(b.left + 8.0, b.top, b.right - b.left - 8.0, 30.0), MUTED);
        self.hits.push((b, Hit::Uninstall));

        p.text(
            concat!("Nitro Control ", env!("CARGO_PKG_VERSION")),
            &self.fonts.small_right,
            rect(M, H - M - 16.0, W - 2.0 * M, 18.0),
            TRACK,
        );
    }

    fn draw_unsupported(&mut self, p: &Painter) {
        self.draw_header(p, false);
        let cx = rect(M, 0.0, W - 2.0 * M, 0.0);
        p.text("Этот ноутбук не поддерживается", &self.fonts.heading, D2D_RECT_F { top: 140.0, bottom: 166.0, ..cx }, TEXT);
        let lines = [
            "В BIOS нет интерфейса управления Acer (AcerGamingFunction).",
            "Nitro Control работает с Acer Nitro, проверен на AN515-44.",
        ];
        for (i, l) in lines.iter().enumerate() {
            let top = 176.0 + i as f32 * 22.0;
            p.text(l, &self.fonts.small_center, D2D_RECT_F { top, bottom: top + 20.0, ..cx }, MUTED);
        }
    }

    fn draw_disconnected(&mut self, p: &Painter) {
        self.draw_header(p, false);
        let cx = rect(M, 0.0, W - 2.0 * M, 0.0);
        p.text("Фоновая служба не запущена", &self.fonts.heading, D2D_RECT_F { top: 140.0, bottom: 166.0, ..cx }, TEXT);
        p.text(
            "Она управляет вентиляторами и требует прав администратора.",
            &self.fonts.center,
            D2D_RECT_F { top: 170.0, bottom: 192.0, ..cx },
            MUTED,
        );
        let b = rect(W / 2.0 - 100.0, 214.0, 200.0, 40.0);
        let hover = self.hover == Some(Hit::Launch);
        let (fill, alpha) = if self.launch_requested { (TRACK, 1.0) } else { (ACCENT, if hover { 0.85 } else { 1.0 }) };
        p.fill(b, 8.0, fill, alpha);
        p.text("Запустить службу", &self.fonts.center, b, WHITE);
        if self.launch_requested {
            p.text("Запускаю…", &self.fonts.center, D2D_RECT_F { top: 266.0, bottom: 288.0, ..cx }, MUTED);
        } else {
            self.hits.push((b, Hit::Launch));
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<Hit> {
        self.hits.iter().find(|(r, _)| contains(r, x, y)).map(|(_, h)| *h)
    }

    fn set_slider(&mut self, id: SliderId, x: f32) {
        let Some(&(_, t)) = self.slider_tracks.iter().find(|(s, _)| *s == id) else { return };
        let (min, max, step) = id.range();
        let frac = ((x - t.left) / (t.right - t.left)).clamp(0.0, 1.0);
        let raw = min as f32 + frac * (max - min) as f32;
        let v = ((raw / step as f32).round() as i32 * step).clamp(min, max);
        match id {
            SliderId::CpuDuty => self.cpu_pct = v as u8,
            SliderId::GpuDuty => self.gpu_pct = v as u8,
        }
    }

    fn on_down(&mut self, x: f32, y: f32) -> Option<Modal> {
        match self.hit(x, y) {
            Some(Hit::Mode(mode)) => {
                let current = self.shared.lock().unwrap().status.as_ref().map(|s| s.config.mode);
                if current != Some(mode) {
                    let _ = self.tx.send(Request::SetMode { mode });
                }
            }
            Some(Hit::Slider(id)) => {
                self.drag = Some(id);
                self.set_slider(id, x);
                unsafe { SetCapture(self.hwnd) };
            }
            Some(Hit::Launch) => {
                self.launch_requested = true;
                return Some(Modal::LaunchDaemon);
            }
            Some(Hit::Uninstall) => return Some(Modal::Uninstall),
            Some(Hit::OpenSettings) => {
                // Built only now, from the latest status; dropped again on Back.
                let cfg = self.shared.lock().unwrap().status.as_ref().map(|s| s.config.clone());
                self.settings = cfg.map(|c| SettingsPage {
                    poll_secs: c.poll_secs,
                    game_mode: c.game_mode,
                });
                self.hover = None;
            }
            Some(Hit::Back) => {
                self.settings = None;
                self.hover = None;
            }
            Some(Hit::CoolBoost) => {
                let current = self.shared.lock().unwrap().status.as_ref().map(|s| s.config.coolboost);
                if let Some(on) = current {
                    let _ = self.tx.send(Request::SetCoolBoost { on: !on });
                }
            }
            Some(Hit::GameMode) => {
                if let Some(page) = &mut self.settings {
                    page.game_mode = !page.game_mode;
                    let _ = self.tx.send(Request::SetGameMode { on: page.game_mode });
                }
            }
            Some(Hit::Poll(secs)) => {
                if let Some(page) = &mut self.settings {
                    page.poll_secs = secs;
                }
                let _ = self.tx.send(Request::SetPollInterval { secs });
            }
            None => {}
        }
        None
    }

    fn on_up(&mut self) {
        if self.drag.take().is_none() {
            return;
        }
        unsafe {
            let _ = ReleaseCapture();
        }
        let _ = self.tx.send(Request::SetCustom { cpu: self.cpu_pct, gpu: self.gpu_pct });
    }

    fn on_move(&mut self, x: f32, y: f32) -> bool {
        if !self.tracking_leave {
            let mut tme = TRACKMOUSEEVENT {
                cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: self.hwnd,
                dwHoverTime: 0,
            };
            self.tracking_leave = unsafe { TrackMouseEvent(&mut tme) }.is_ok();
        }
        if let Some(id) = self.drag {
            self.set_slider(id, x);
            return true;
        }
        let h = self.hit(x, y);
        let changed = h != self.hover;
        self.hover = h;
        changed
    }
}

/// Starts the daemon: via the autostart task if it exists (no prompt), otherwise elevated
/// directly (one UAC prompt).
fn launch_daemon() {
    if crate::install::run_task() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    unsafe {
        ShellExecuteW(None, w!("runas"), &HSTRING::from(exe.as_os_str()), w!("daemon"), None, SW_HIDE);
    }
}

fn confirm_uninstall(hwnd: HWND) {
    let answer = unsafe {
        MessageBoxW(
            Some(hwnd),
            w!("Удалить автозапуск, игровой план питания и настройки Nitro Control?
Вентиляторы вернутся под управление BIOS."),
            w!("Nitro Control"),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        )
    };
    if answer != IDYES {
        return;
    }
    let Ok(exe) = std::env::current_exe() else { return };
    let r = unsafe { ShellExecuteW(Some(hwnd), w!("runas"), &HSTRING::from(exe.as_os_str()), w!("uninstall"), None, SW_HIDE) };
    // > 32 means it started (the UAC prompt was accepted); the window has nothing left to show.
    if r.0 as isize > 32 {
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
    }
}

fn spawn_worker(shared: Arc<Mutex<Shared>>, hwnd: HWND) -> Sender<Request> {
    let (tx, rx) = mpsc::channel::<Request>();
    let hwnd = hwnd.0 as isize; // HWND is not Send
    std::thread::spawn(move || {
        let mut wait = Duration::ZERO;
        let mut tried_task = false;
        loop {
            // Commands go out immediately; otherwise poll status once a second.
            let (req, is_cmd) = match rx.recv_timeout(wait) {
                Ok(r) => (r, true),
                Err(mpsc::RecvTimeoutError::Timeout) => (Request::Status, false),
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            };
            let res = ipc::request(&req);
            // Daemon not running: quietly try the autostart task once before showing the button.
            if res.is_err() && !tried_task {
                tried_task = true;
                if crate::install::run_task() {
                    wait = Duration::from_millis(300);
                    continue;
                }
            }
            {
                let mut s = shared.lock().unwrap();
                s.connected = res.is_ok();
                if let Ok(st) = res {
                    s.unsupported |= st.unsupported;
                    s.status = Some(st);
                }
            }
            unsafe {
                let _ = PostMessageW(Some(HWND(hwnd as _)), WM_APP_STATUS, WPARAM(0), LPARAM(0));
            }
            // Re-poll soon after a command so the new state shows up without a 1 s lag.
            wait = if is_cmd { Duration::from_millis(250) } else { Duration::from_secs(1) };
        }
    });
    tx
}

/// Mouse position in DIPs.
fn mouse_pos(hwnd: HWND, lp: LPARAM) -> (f32, f32) {
    let scale = unsafe { GetDpiForWindow(hwnd) } as f32 / 96.0;
    let x = (lp.0 & 0xFFFF) as i16 as f32;
    let y = ((lp.0 >> 16) & 0xFFFF) as i16 as f32;
    (x / scale, y / scale)
}

/// Runs `f` on the window state. Re-entrant messages (sent while another handler holds the
/// state, e.g. from a modal loop) are skipped instead of panicking.
fn with_window<R>(f: impl FnOnce(&mut Window) -> R) -> Option<R> {
    WINDOW.with(|w| w.try_borrow_mut().ok()?.as_mut().map(f))
}

fn invalidate(hwnd: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            unsafe { BeginPaint(hwnd, &mut ps) };
            with_window(|w| w.paint());
            unsafe {
                let _ = EndPaint(hwnd, &ps);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_APP_STATUS => {
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = mouse_pos(hwnd, lp);
            let modal = with_window(|w| w.on_down(x, y)).flatten();
            invalidate(hwnd);
            match modal {
                Some(Modal::LaunchDaemon) => launch_daemon(),
                Some(Modal::Uninstall) => confirm_uninstall(hwnd),
                None => {}
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            with_window(|w| w.on_up());
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = mouse_pos(hwnd, lp);
            if with_window(|w| w.on_move(x, y)) == Some(true) {
                invalidate(hwnd);
            }
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            with_window(|w| {
                w.tracking_leave = false;
                w.hover = None;
            });
            invalidate(hwnd);
            LRESULT(0)
        }
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            let clickable = with_window(|w| w.hover.is_some() || w.drag.is_some()).unwrap_or(false);
            unsafe {
                let cursor = LoadCursorW(None, if clickable { IDC_HAND } else { IDC_ARROW }).ok();
                SetCursor(cursor);
            }
            LRESULT(1)
        }
        WM_SIZE => {
            with_window(|w| {
                if let Some((rt, _)) = &w.target {
                    let size = D2D_SIZE_U { width: (lp.0 & 0xFFFF) as u32, height: ((lp.0 >> 16) & 0xFFFF) as u32 };
                    unsafe {
                        let _ = rt.Resize(&size);
                    }
                }
            });
            LRESULT(0)
        }
        WM_DPICHANGED => {
            // Recreate the target at the new DPI and take the size Windows suggests.
            with_window(|w| w.target = None);
            let r = unsafe { &*(lp.0 as *const RECT) };
            unsafe {
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

/// Outer window size for a client area of W x H DIPs at the window's DPI.
fn fit_to_dpi(hwnd: HWND, style: WINDOW_STYLE) {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        let s = dpi as f32 / 96.0;
        let mut r = RECT { left: 0, top: 0, right: (W * s).round() as i32, bottom: (H * s).round() as i32 };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, WINDOW_EX_STYLE(0), dpi);
        let _ = SetWindowPos(hwnd, None, 0, 0, r.right - r.left, r.bottom - r.top, SWP_NOMOVE | SWP_NOZORDER);
    }
}

fn dark_title_bar(hwnd: HWND) {
    unsafe {
        let on: BOOL = true.into();
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, &on as *const _ as _, size_of::<BOOL>() as u32);
        // COLORREF is 0x00BBGGRR.
        let caption = ((BG & 0xFF) << 16) | (BG & 0xFF00) | ((BG >> 16) & 0xFF);
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CAPTION_COLOR, &caption as *const _ as _, 4);
    }
}

pub fn run() -> Result<()> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(None)?;
        let class = w!("NitroControlWindow");
        // Icon resource 1 (see app.rc); the small variant for the title bar and taskbar.
        let icon = |size: i32| {
            LoadImageW(Some(instance.into()), PCWSTR(1 as _), IMAGE_ICON, size, size, LR_DEFAULTCOLOR)
                .map(|h| HICON(h.0))
                .unwrap_or_default()
        };
        let wc = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hIcon: icon(GetSystemMetrics(SM_CXICON)),
            hIconSm: icon(GetSystemMetrics(SM_CXSMICON)),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &HSTRING::from(WINDOW_TITLE),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            None,
            None,
            Some(instance.into()),
            None,
        )?;
        fit_to_dpi(hwnd, style);
        dark_title_bar(hwnd);

        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let shared = Arc::new(Mutex::new(Shared::default()));
        let window = Window {
            hwnd,
            d2d: D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?,
            fonts: Fonts::new(&dwrite)?,
            target: None,
            tx: spawn_worker(shared.clone(), hwnd),
            shared,
            cpu_pct: 50,
            gpu_pct: 50,
            synced: false,
            settings: None,
            hits: Vec::new(),
            slider_tracks: Vec::new(),
            hover: None,
            drag: None,
            tracking_leave: false,
            launch_requested: false,
        };
        WINDOW.with(|w| *w.borrow_mut() = Some(window));

        let _ = ShowWindow(hwnd, SW_SHOW);
        crate::keyhook::bring_to_front(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    Ok(())
}
