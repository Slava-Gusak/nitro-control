//! Autostart via Task Scheduler: the daemon runs elevated at logon without a UAC prompt.
//! Called from the (already elevated) daemon.

use std::os::windows::process::CommandExt;
use std::process::Command;
use windows::Win32::UI::Shell::{IsUserAnAdmin, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MessageBoxW, SW_HIDE};
use windows::core::{HSTRING, w};

const TASK_NAME: &str = "NitroControl";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn schtasks(args: &[&str]) -> bool {
    Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .is_ok_and(|s| s.success())
}

/// Starts the daemon through the task. Works without elevation and without a UAC prompt.
pub fn run_task() -> bool {
    schtasks(&["/Run", "/TN", TASK_NAME])
}

/// Creates (or refreshes) the logon task. Must run elevated.
pub fn create() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let user = format!(
        "{}\\{}",
        std::env::var("USERDOMAIN").unwrap_or_default(),
        std::env::var("USERNAME").unwrap_or_default()
    );
    let xml = task_xml(&user, &exe.display().to_string());
    let path = crate::config::dir().join("task.xml");
    // schtasks expects UTF-16 with a BOM for this encoding declaration.
    let bytes: Vec<u8> = std::iter::once(0xFEFF)
        .chain(xml.encode_utf16())
        .flat_map(u16::to_le_bytes)
        .collect();
    if std::fs::create_dir_all(crate::config::dir()).is_err() || std::fs::write(&path, bytes).is_err() {
        return false;
    }
    let ok = schtasks(&["/Create", "/TN", TASK_NAME, "/XML", &path.display().to_string(), "/F"]);
    let _ = std::fs::remove_file(&path);
    crate::config::log(&format!("autostart task create: {}", if ok { "ok" } else { "FAILED" }));
    ok
}

/// `nitro-control.exe uninstall`: undoes everything the app set up (autostart task, game power
/// plan, settings and log) and hands the fans back to the BIOS. Re-launches itself elevated.
pub fn uninstall() {
    if !unsafe { IsUserAnAdmin() }.as_bool() {
        if let Ok(exe) = std::env::current_exe() {
            unsafe {
                ShellExecuteW(None, w!("runas"), &HSTRING::from(exe.as_os_str()), w!("uninstall"), None, SW_HIDE);
            }
        }
        return;
    }
    // The daemon sets Auto fan mode when it quits.
    let _ = crate::ipc::request(&crate::ipc::Request::Quit);
    std::thread::sleep(std::time::Duration::from_millis(500));
    schtasks(&["/Delete", "/TN", TASK_NAME, "/F"]);
    crate::game::delete_game_plan(crate::config::load().restore_plan.as_deref());
    let _ = std::fs::remove_dir_all(crate::config::dir());
    unsafe {
        MessageBoxW(
            None,
            w!("Nitro Control удалён: автозапуск, игровой план питания и настройки.
Вентиляторы снова под управлением BIOS. Сам файл программы можно просто удалить."),
            w!("Nitro Control"),
            MB_ICONINFORMATION,
        );
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn task_xml(user: &str, exe: &str) -> String {
    let (user, exe) = (xml_escape(user), xml_escape(exe));
    // No battery restrictions and no time limit: the defaults would stop the daemon on battery
    // and after 72 hours.
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>Nitro Control fan daemon</Description></RegistrationInfo>
  <Triggers><LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger></Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>HighestAvailable</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>4</Priority>
    <RestartOnFailure><Interval>PT1M</Interval><Count>3</Count></RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec><Command>"{exe}"</Command><Arguments>daemon</Arguments></Exec>
  </Actions>
</Task>
"#
    )
}
