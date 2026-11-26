use std::sync::atomic::Ordering;

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_ERROR, NIIF_INFO,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{LoadIconW, IDI_APPLICATION};

use crate::MAIN_HWND;

pub const TRAY_ID: u32 = 1;

fn cur_hwnd() -> HWND {
    HWND(MAIN_HWND.load(Ordering::Relaxed) as _)
}

unsafe fn base_nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n = NOTIFYICONDATAW::default();
    n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = TRAY_ID;
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    n.uCallbackMessage = crate::WM_APP_TRAY;
    n.hIcon = LoadIconW(None, IDI_APPLICATION).unwrap_or_default();
    fill(&mut n.szTip, "RCP");
    n
}

pub fn add(hwnd: HWND) {
    unsafe {
        let n = base_nid(hwnd);
        let _ = Shell_NotifyIconW(NIM_ADD, &n);
    }
}

pub fn remove(hwnd: HWND) {
    unsafe {
        let n = base_nid(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &n);
    }
}

pub fn set_tip(text: &str) {
    unsafe {
        let h = cur_hwnd();
        if h.0 as usize == 0 {
            return;
        }
        let mut n = base_nid(h);
        fill(&mut n.szTip, text);
        let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
    }
}

pub fn balloon(title: &str, text: &str, is_error: bool) {
    unsafe {
        let h = cur_hwnd();
        if h.0 as usize == 0 {
            crate::logln(&format!("{} | {}", title, text));
            return;
        }
        let mut n = base_nid(h);
        n.uFlags = NIF_INFO;
        fill(&mut n.szInfoTitle, title);
        fill(&mut n.szInfo, text);
        n.dwInfoFlags = if is_error { NIIF_ERROR } else { NIIF_INFO };
        let _ = Shell_NotifyIconW(NIM_MODIFY, &n);
    }
}

fn fill(dst: &mut [u16], s: &str) {
    let wide: Vec<u16> = s.encode_utf16().collect();
    let max = dst.len() - 1; // место под завершающий нуль
    let n = wide.len().min(max);
    dst[..n].copy_from_slice(&wide[..n]);
}
