//! Low-level keyboard hook for the NitroSense key (vk 0xFF, extended scan code E0 75).
//! The key is swallowed so it no longer reaches Acer's own agent.

use std::sync::OnceLock;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

const NITRO_VK: u32 = 0xFF;
const NITRO_SCAN: u32 = 0x75;

static ON_PRESS: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

unsafe extern "system" fn hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        if kb.vkCode == NITRO_VK && kb.scanCode == NITRO_SCAN {
            if matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                if let Some(f) = ON_PRESS.get() {
                    f();
                }
            }
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Installs the hook and pumps messages on the calling thread forever.
/// `on_press` runs inside the hook, so it must return quickly.
pub fn run(on_press: impl Fn() + Send + Sync + 'static) {
    let _ = ON_PRESS.set(Box::new(on_press));
    unsafe {
        let module = GetModuleHandleW(None).ok();
        if SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), module.map(Into::into), 0).is_err() {
            crate::config::log("keyboard hook failed");
            return;
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Raises `hwnd` above everything and gives it focus. Windows normally refuses
/// SetForegroundWindow from background processes, so temporarily share the input queue
/// of the current foreground window and flash the window topmost.
pub fn bring_to_front(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let fg_thread = GetWindowThreadProcessId(GetForegroundWindow(), None);
        let me = GetCurrentThreadId();
        let attached = fg_thread != 0 && fg_thread != me && AttachThreadInput(me, fg_thread, true).as_bool();
        let flags = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW;
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, flags);
        let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, flags);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        if attached {
            let _ = AttachThreadInput(me, fg_thread, false);
        }
    }
}

/// Brings an already open window to the front. Returns false if there is none.
pub fn focus_existing_ui() -> bool {
    let title = windows::core::HSTRING::from(crate::ui::WINDOW_TITLE);
    match unsafe { FindWindowW(None, &title) } {
        Ok(h) => {
            bring_to_front(h);
            true
        }
        Err(_) => false,
    }
}

/// Focuses the existing window or starts a new UI process.
pub fn open_ui() {
    if focus_existing_ui() {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        // Let the new process take the foreground (we just received the key press).
        unsafe {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
        }
        // The daemon is elevated; start the UI through Explorer so it runs with the user's
        // normal rights instead of inheriting ours.
        let _ = std::process::Command::new("explorer.exe").arg(exe).spawn();
    }
}
