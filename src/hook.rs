use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetForegroundWindow, GetGUIThreadInfo, GetMessageW,
    GetWindowThreadProcessId, GUITHREADINFO, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLKHF_UP,
    PostMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx, MSG, WH_KEYBOARD_LL,
};

use crate::{clipboard, logln, shellx, MAIN_HWND, PAUSED, WM_APP_PASTE};

/// Код виртуальной клавиши перехвата (0x56 = 'V'). Свой бинд — поменяйте здесь
/// и при необходимости набор модификаторов в `mods_ok()`.
pub const BIND_VK: u32 = 0x56;

static LAST_POST_MS: AtomicU64 = AtomicU64::new(0);

pub fn start() {
    std::thread::Builder::new()
        .name("kbd-hook".into())
        .spawn(|| unsafe {
            let Ok(inst) = GetModuleHandleW(None) else {
                logln("GetModuleHandleW: ошибка");
                return;
            };
            let Ok(hook) = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), inst, 0) else {
                logln("не удалось установить клавиатурный хук");
                return;
            };
            logln("клавиатурный хук установлен");
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                let _ = TranslateMessage(&msg);
                let _ = DispatchMessageW(&msg);
            }
            let _ = UnhookWindowsHookEx(hook);
        })
        .expect("spawn hook thread");
}

unsafe extern "system" fn ll_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code < 0 {
        return CallNextHookEx(None, code, wp, lp);
    }
    let kb = &*(lp.0 as *const KBDLLHOOKSTRUCT);
    let skip = kb.flags.0 & (LLKHF_UP.0 | LLKHF_INJECTED.0) != 0
        || kb.vkCode != BIND_VK
        || !mods_ok();
    if skip {
        return CallNextHookEx(None, code, wp, lp);
    }

    let fg = GetForegroundWindow();
    if fg.0 as usize == 0 {
        return CallNextHookEx(None, code, wp, lp);
    }
    let class = shellx::class_of(fg);
    let in_explorer =
        matches!(class.as_str(), "CabinetWClass" | "ExploreWClass" | "Progman" | "WorkerW");

    // фокус в поле ввода (переименование, адресная строка, поиск) — не мешаем
    if !in_explorer
        || focus_is_edit(fg)
        || PAUSED.load(Ordering::SeqCst)
        || !clipboard::has_files()
    {
        return CallNextHookEx(None, code, wp, lp);
    }

    // анти-дребезг автоповтора клавиатуры
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let last = LAST_POST_MS.load(Ordering::SeqCst);
    if now.saturating_sub(last) < 250 {
        return LRESULT(1);
    }
    LAST_POST_MS.store(now, Ordering::SeqCst);

    let hwnd = HWND(MAIN_HWND.load(Ordering::SeqCst) as _);
    let _ = PostMessageW(hwnd, WM_APP_PASTE, WPARAM(fg.0 as usize), LPARAM(0));
    logln(&format!("перехват Ctrl+V (окно класса {})", class));
    LRESULT(1)
}

fn mods_ok() -> bool {
    unsafe {
        let down = |vk: i32| (GetAsyncKeyState(vk) as u16) & 0x8000 != 0;
        down(VK_CONTROL.0 as i32)
            && !down(VK_SHIFT.0 as i32)
            && !down(VK_MENU.0 as i32)
            && !down(VK_LWIN.0 as i32)
            && !down(VK_RWIN.0 as i32)
    }
}

fn focus_is_edit(fg: HWND) -> bool {
    unsafe {
        let tid = GetWindowThreadProcessId(fg, None);
        if tid == 0 {
            return false;
        }
        let mut gi = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(tid, &mut gi).is_err() {
            return false;
        }
        if gi.hwndFocus.0 as usize == 0 {
            return false;
        }
        let c = shellx::class_of(gi.hwndFocus);
        matches!(
            c.as_str(),
            "Edit" | "RichEdit20W" | "RICHEDIT50W" | "Windows.UI.Core.CoreWindow" | "SearchBox"
        )
    }
}
