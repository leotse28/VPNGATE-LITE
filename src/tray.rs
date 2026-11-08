//! System tray (v3.12, Windows only): minimize-to-tray like
//! v2rayN / NekoBox — the X button hides the main window instead of
//! quitting, and the tray icon's menu offers 显示主界面 / 重启客户端 /
//! 退出.
//!
//! Only the icon/menu plumbing is Windows-only (`tray-icon` 0.19,
//! which pairs with eframe 0.36's winit 0.30). Everything testable
//! without a window — the close decision, the menu-id dispatch, the
//! restart command construction, the .ico parsing — is plain code in
//! this module and runs on Linux too, so `cargo test` covers it.
//! On Linux the GUI compiles a stub: no tray handle, X quits as
//! before.

use std::path::PathBuf;

/// What the tray asked the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayRequest {
    /// Show the main window again (tray click / menu 显示主界面).
    Show,
    /// Relaunch the exe and quit this instance (menu 重启客户端).
    Restart,
    /// Really quit (menu 退出).
    Quit,
}

/// Tray menu item ids — also the `MenuId` strings in the tray menu.
pub const MENU_SHOW: &str = "show";
pub const MENU_RESTART: &str = "restart";
pub const MENU_QUIT: &str = "quit";

/// Map a tray menu item id to the request it makes. Unknown ids
/// (a stale menu, a typo) map to nothing — never crash on a menu
/// event we did not build.
pub fn menu_action(id: &str) -> Option<TrayRequest> {
    match id {
        MENU_SHOW => Some(TrayRequest::Show),
        MENU_RESTART => Some(TrayRequest::Restart),
        MENU_QUIT => Some(TrayRequest::Quit),
        _ => None,
    }
}

/// What an X click (a `close_requested` flag) means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// Hide the window to the tray; the app keeps running.
    HideToTray,
    /// Really quit: disconnect, restore the system proxy, save.
    Quit,
}

/// Decide what a close request does. With a live tray an X click
/// hides; an explicit quit — File menu 退出 or tray menu 退出,
/// both of which set `quit_requested` first — always really quits.
/// Without a tray (Linux, or tray creation failed on Windows) every
/// close request quits, exactly as before v3.12.
pub fn decide_close(tray_active: bool, quit_requested: bool) -> CloseDecision {
    if tray_active && !quit_requested {
        CloseDecision::HideToTray
    } else {
        CloseDecision::Quit
    }
}

/// Build the command that restarts the client: the current exe path
/// with the current arguments, so the new instance starts exactly
/// like this one was started (but fresh and disconnected — a
/// restart never restores the tunnel; the user reconnects by hand).
/// Only the command is built here; spawning it is the GUI's job,
/// so this stays unit-testable without launching a process.
pub fn restart_command() -> std::io::Result<(PathBuf, Vec<String>)> {
    let exe = std::env::current_exe()?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    Ok((exe, args))
}

/// The line written once — status bar and log — the first time the
/// window hides to the tray, so the user does not think the program
/// vanished.
pub const TRAY_HINT: &str = "已最小化到托盘，右键托盘图标可退出/显示主界面。";

/// PNG file magic (first 8 bytes).
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// Extract the largest PNG entry from a .ico file's bytes.
///
/// A .ico is a 6-byte header (count at bytes 4–5) plus one 16-byte
/// directory entry per image: width, height, colour count, reserved,
/// planes (u16), bit count (u16), byte size (u32 LE), offset
/// (u32 LE). Vista-style .ico files embed PNGs; our
/// `assets/icon.ico` holds four (16/32/48/256 px) and the 256 px
/// one is the tray icon. Entries that are not PNGs (old BMP entries)
/// are skipped; a malformed file yields `None`.
pub fn largest_png_in_ico(ico: &[u8]) -> Option<&[u8]> {
    if ico.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut best: Option<&[u8]> = None;
    for i in 0..count {
        let off = 6 + i * 16;
        if off + 16 > ico.len() {
            break;
        }
        let size = u32::from_le_bytes(
            ico[off + 8..off + 12].try_into().ok()?,
        ) as usize;
        let at = u32::from_le_bytes(
            ico[off + 12..off + 16].try_into().ok()?,
        ) as usize;
        let data = ico.get(at..at.checked_add(size)?)?;
        if data.len() >= PNG_MAGIC.len() && &data[..8] == PNG_MAGIC {
            if best.map_or(true, |b: &[u8]| data.len() > b.len()) {
                best = Some(data);
            }
        }
    }
    best
}

/// Decode the tray icon's RGBA pixels from the .ico bytes:
/// `(rgba bytes, width, height)`. The caller feeds this to
/// `tray_icon::Icon::from_rgba` (Windows only); the decode itself
/// is plain PNG work, so it is tested on Linux too.
pub fn decode_tray_icon_rgba(ico: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let png = largest_png_in_ico(ico)?;
    let img = image::load_from_memory(png).ok()?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some((rgba.into_raw(), w, h))
}

// ---------------------------------------------------------------------------
// Windows-only tray plumbing (tray-icon 0.19)
// ---------------------------------------------------------------------------

/// The live tray icon. Dropping it removes the icon from the tray
/// (tray-icon's Windows backend issues NIM_DELETE on drop), so the
/// quit path must `take()` the handle — never leak it.
#[cfg(windows)]
pub struct TrayHandle {
    _tray: tray_icon::TrayIcon,
}

#[cfg(windows)]
impl TrayHandle {
    /// Build the tray icon (from `assets/icon.ico`, the program's
    /// own icon) and its right-click menu: 显示主界面 / 重启客户端 /
    /// 退出. Any failure returns the reason as a string — the caller
    /// logs it and runs without a tray, so a broken tray never
    /// breaks the program (X then quits as before).
    pub fn new() -> Result<Self, String> {
        let (rgba, w, h) =
            decode_tray_icon_rgba(include_bytes!("../assets/icon.ico"))
                .ok_or_else(|| "托盘图标解码失败（assets/icon.ico）".to_string())?;
        let icon = tray_icon::Icon::from_rgba(rgba, w, h)
            .map_err(|e| format!("托盘图标无效：{e}"))?;
        let menu = tray_icon::menu::Menu::new();
        for (id, label) in [
            (MENU_SHOW, "显示主界面"),
            (MENU_RESTART, "重启客户端"),
        ] {
            menu.append(&tray_icon::menu::MenuItem::with_id(
                id, label, true, None,
            ))
            .map_err(|e| format!("托盘菜单创建失败：{e}"))?;
        }
        menu.append(&tray_icon::menu::PredefinedMenuItem::separator())
            .map_err(|e| format!("托盘菜单创建失败：{e}"))?;
        menu.append(&tray_icon::menu::MenuItem::with_id(
            MENU_QUIT, "退出", true, None,
        ))
        .map_err(|e| format!("托盘菜单创建失败：{e}"))?;
        let tray = tray_icon::TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("VPNGate 客户端")
            .with_icon(icon)
            .build()
            .map_err(|e| format!("托盘创建失败：{e}"))?;
        Ok(Self { _tray: tray })
    }

    /// Drain pending tray-icon and menu events into requests.
    /// Left single-click (button released) and left double-click
    /// both show the main window; anything else on the icon itself
    /// is ignored. The right-click menu is the tray-icon default:
    /// right-clicking the icon pops the menu, no code needed.
    pub fn drain_requests(&self) -> Vec<TrayRequest> {
        let mut out = Vec::new();
        while let Ok(ev) = tray_icon::TrayIconEvent::receiver().try_recv() {
            match ev {
                tray_icon::TrayIconEvent::Click {
                    button: tray_icon::MouseButton::Left,
                    button_state: tray_icon::MouseButtonState::Up,
                    ..
                } => out.push(TrayRequest::Show),
                tray_icon::TrayIconEvent::DoubleClick {
                    button: tray_icon::MouseButton::Left,
                    ..
                } => out.push(TrayRequest::Show),
                _ => {}
            }
        }
        while let Ok(ev) = tray_icon::menu::MenuEvent::receiver().try_recv() {
            if let Some(req) = menu_action(&ev.id.0) {
                out.push(req);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_close_hides_only_with_tray_and_no_quit_request() {
        assert_eq!(
            decide_close(true, false),
            CloseDecision::HideToTray,
            "X with a live tray hides"
        );
        assert_eq!(
            decide_close(true, true),
            CloseDecision::Quit,
            "an explicit quit (tray/File 退出) really quits"
        );
        assert_eq!(
            decide_close(false, false),
            CloseDecision::Quit,
            "no tray (Linux, or tray creation failed) quits as before"
        );
        assert_eq!(decide_close(false, true), CloseDecision::Quit);
    }

    #[test]
    fn menu_action_maps_all_three_items_and_nothing_else() {
        assert_eq!(menu_action(MENU_SHOW), Some(TrayRequest::Show));
        assert_eq!(menu_action(MENU_RESTART), Some(TrayRequest::Restart));
        assert_eq!(menu_action(MENU_QUIT), Some(TrayRequest::Quit));
        assert_eq!(menu_action(""), None);
        assert_eq!(menu_action("show-me"), None);
    }

    #[test]
    fn restart_command_is_the_current_exe_with_current_args() {
        let (exe, args) = restart_command().expect("current_exe must work");
        assert!(
            exe.is_file(),
            "the restart target must be a real file, got: {}",
            exe.display()
        );
        assert_eq!(
            args,
            std::env::args().skip(1).collect::<Vec<_>>(),
            "arguments must round-trip (the new instance starts like this one)"
        );
    }

    #[test]
    fn largest_png_in_ico_finds_the_256px_entry() {
        let ico = include_bytes!("../assets/icon.ico");
        let png = largest_png_in_ico(ico).expect("icon.ico must hold a PNG");
        assert_eq!(&png[..8], PNG_MAGIC);
        // The 256 px entry is the largest of the four (v3.13 bold-V
        // icon; was 3902 bytes for the old globe + swoosh).
        assert_eq!(png.len(), 2266);
    }

    #[test]
    fn largest_png_in_ico_rejects_garbage() {
        assert_eq!(largest_png_in_ico(&[]), None);
        assert_eq!(largest_png_in_ico(&[0, 0, 1, 0]), None);
        assert_eq!(largest_png_in_ico(b"not an icon at all"), None);
    }

    #[test]
    fn decode_tray_icon_rgba_is_256x256() {
        let ico = include_bytes!("../assets/icon.ico");
        let (rgba, w, h) =
            decode_tray_icon_rgba(ico).expect("icon.ico must decode");
        assert_eq!((w, h), (256, 256));
        assert_eq!(rgba.len(), 256 * 256 * 4);
    }
}
