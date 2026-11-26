//! Мини-окно прогресса копирования (как у Explorer).
//! Обновляется таймером GUI-потока (8 раз/с); пока окно выключено галкой в трее —
//! не тикает таймер, не считаются файлы/имена, окно не существует для пользователя.

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, Ordering};
use std::sync::Mutex;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS,
    DEFAULT_CHARSET, DEFAULT_PITCH, FW_NORMAL, HDC, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, PBM_SETBARCOLOR, PBM_SETBKCOLOR, PBM_SETRANGE32, PBM_SETPOS, PBS_SMOOTH,
    SetWindowTheme, INITCOMMONCONTROLSEX, ICC_PROGRESS_CLASS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, CreateWindowExW, DefWindowProcW, GetSystemMetrics, KillTimer, RegisterClassW,
    SendMessageW, SetTimer, SetWindowTextW, ShowWindow, SM_CXSCREEN, SM_CYSCREEN, SW_HIDE,
    SW_SHOWNOACTIVATE, WM_CLOSE, WM_COMMAND, WM_CTLCOLORSTATIC, WM_SETFONT, WM_TIMER, WNDCLASSW,
    WS_CHILD, WS_OVERLAPPED, WS_VISIBLE, WS_CAPTION, WS_SYSMENU, WINDOW_STYLE, WINDOW_EX_STYLE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, HMENU, BS_DEFPUSHBUTTON,
};

use crate::copyjob::{human, BYTES_DONE, BYTES_TOTAL};

pub static SHOW_UI: AtomicBool = AtomicBool::new(true);

/// 0 — пусто, 1 — идёт копирование, 2 — показан финал.
pub static PHASE: AtomicU8 = AtomicU8::new(0);
pub static FILES_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static FILES_DONE: AtomicU64 = AtomicU64::new(0);
pub static CUR_FILE: Mutex<String> = Mutex::new(String::new());
pub static TARGET_STR: Mutex<String> = Mutex::new(String::new());

static FINAL_MSG: Mutex<String> = Mutex::new(String::new());
static FINAL_ERR: AtomicBool = AtomicBool::new(false);
static JOB_START_MS: AtomicU64 = AtomicU64::new(0);

static PROG_HWND: AtomicIsize = AtomicIsize::new(0);
static INFO_HWND: AtomicIsize = AtomicIsize::new(0);
static BAR_HWND: AtomicIsize = AtomicIsize::new(0);
static BTN_HWND: AtomicIsize = AtomicIsize::new(0);
static BTN_PAUSE_HWND: AtomicIsize = AtomicIsize::new(0);
static FONT_H: AtomicIsize = AtomicIsize::new(0);
static BRUSH_DARK: AtomicIsize = AtomicIsize::new(0);

const TIMER_UPDATE: usize = 1;
const TIMER_HIDE: usize = 2;
const BTN_CANCEL: usize = 2001;
const BTN_PAUSE: usize = 2002;

/// Тёмная палитра (фон/текст/акцент как у тёмного Explorer).
const BG: u32 = 0x0020_2020;
const FG: u32 = 0x00E0_E0E0;
const ACCENT: u32 = 0x00FF_C24C; // #4CC2FF

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn hwnd_of(v: &AtomicIsize) -> HWND {
    HWND(v.load(Ordering::Relaxed) as _)
}

pub fn ui_enabled() -> bool {
    SHOW_UI.load(Ordering::Relaxed)
}

/// Создаёт скрытое окно один раз при старте.
pub fn create(inst: windows::Win32::Foundation::HMODULE) {
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);

        let class = w!("RcpProgressWnd");
        let brush = CreateSolidBrush(COLORREF(BG));
        BRUSH_DARK.store(brush.0 as isize, Ordering::Relaxed);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: inst.into(),
            lpszClassName: class,
            hbrBackground: brush,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let mut rc = RECT {
            left: 0,
            top: 0,
            right: 420,
            bottom: 164,
        };
        let _ = AdjustWindowRect(&mut rc, style, false);
        let ww = rc.right - rc.left;
        let hh = rc.bottom - rc.top;
        let cx = GetSystemMetrics(SM_CXSCREEN);
        let cy = GetSystemMetrics(SM_CYSCREEN);

        let Ok(h) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class,
            w!("Копирование — RCP"),
            style,
            cx - ww - 24,
            cy - hh - 24,
            ww,
            hh,
            None,
            None,
            inst,
            None,
        ) else {
            return;
        };
        PROG_HWND.store(h.0 as isize, Ordering::Relaxed);
        // тёмный заголовок (Win11 immersive dark mode; 19 — для старых сборок)
        let mut on: i32 = 1;
        let _ = DwmSetWindowAttribute(
            h,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &mut on as *mut _ as *const core::ffi::c_void,
            4,
        );
        let _ = DwmSetWindowAttribute(
            h,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(19),
            &mut on as *mut _ as *const core::ffi::c_void,
            4,
        );

        let font = CreateFontW(
            -12,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET.0 as u32,
            OUT_DEFAULT_PRECIS.0 as u32,
            CLIP_DEFAULT_PRECIS.0 as u32,
            CLEARTYPE_QUALITY.0 as u32,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        FONT_H.store(font.0 as isize, Ordering::Relaxed);
        let mk_child = |class: windows::core::PCWSTR,
                            text: &windows::core::HSTRING,
                            style: WINDOW_STYLE,
                            id: usize,
                            x: i32,
                            y: i32,
                            w_: i32,
                            h_: i32|
         -> Option<HWND> {
            let ch = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                text,
                style,
                x,
                y,
                w_,
                h_,
                h,
                HMENU(id as usize as _),
                inst,
                None,
            )
            .ok()?;
            let f = FONT_H.load(Ordering::Relaxed);
            let _ = SendMessageW(ch, WM_SETFONT, WPARAM(f as usize), LPARAM(1));
            Some(ch)
        };

        if let Some(info) = mk_child(
            w!("STATIC"),
            &HSTRING::new(),
            WS_CHILD | WS_VISIBLE,
            1,
            14,
            10,
            392,
            78,
        ) {
            INFO_HWND.store(info.0 as isize, Ordering::Relaxed);
        }
        if let Some(bar) = mk_child(
            w!("msctls_progress32"),
            &HSTRING::new(),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(PBS_SMOOTH),
            2,
            14,
            94,
            392,
            16,
        ) {
            BAR_HWND.store(bar.0 as isize, Ordering::Relaxed);
            let _ = SetWindowTheme(bar, w!("DarkMode_Explorer"), PCWSTR::null());
            let _ = SendMessageW(bar, PBM_SETRANGE32, WPARAM(0), LPARAM(100));
            let _ = SendMessageW(bar, PBM_SETBARCOLOR, WPARAM(0), LPARAM(ACCENT as isize));
            let _ = SendMessageW(bar, PBM_SETBKCOLOR, WPARAM(0), LPARAM(BG as isize));
        }
        if let Some(btn) = mk_child(
            w!("BUTTON"),
            &HSTRING::from("Пауза"),
            WS_CHILD | WS_VISIBLE,
            BTN_PAUSE,
            208,
            120,
            98,
            30,
        ) {
            BTN_PAUSE_HWND.store(btn.0 as isize, Ordering::Relaxed);
            let _ = SetWindowTheme(btn, w!("DarkMode_Explorer"), PCWSTR::null());
        }
        if let Some(btn) = mk_child(
            w!("BUTTON"),
            &HSTRING::from("Отмена"),
            WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            BTN_CANCEL,
            312,
            120,
            94,
            30,
        ) {
            BTN_HWND.store(btn.0 as isize, Ordering::Relaxed);
            let _ = SetWindowTheme(btn, w!("DarkMode_Explorer"), PCWSTR::null());
        }
    }
}

/// Вызывается на GUI-потоке в момент перехвата Ctrl+V — окно появляется мгновенно,
/// ещё до начала обхода источников.
pub fn job_begin(title: &str, target: &str) {
    if !ui_enabled() {
        return;
    }
    let h = hwnd_of(&PROG_HWND);
    if h.0 as usize == 0 {
        return;
    }
    *TARGET_STR.lock().unwrap() = target.to_string();
    *CUR_FILE.lock().unwrap() = String::new();
    FILES_TOTAL.store(0, Ordering::Relaxed);
    FILES_DONE.store(0, Ordering::Relaxed);
    FINAL_ERR.store(false, Ordering::Relaxed);
    JOB_START_MS.store(now_ms(), Ordering::Relaxed);
    PHASE.store(1, Ordering::Relaxed);
    crate::COPY_PAUSED.store(false, Ordering::SeqCst);
    unsafe {
        let _ = SetWindowTextW(h, &HSTRING::from(title));
        let bp = hwnd_of(&BTN_PAUSE_HWND);
        if bp.0 as usize != 0 {
            let _ = SetWindowTextW(bp, w!("Пауза"));
            let _ = ShowWindow(bp, SW_SHOWNOACTIVATE);
        }
        let btn = hwnd_of(&BTN_HWND);
        if btn.0 as usize != 0 {
            let _ = ShowWindow(btn, SW_SHOWNOACTIVATE);
        }
        let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        let _ = SetTimer(h, TIMER_UPDATE, 120, None);
    }
}

/// Вызывается из рабочего потока по завершении задания (безопасно: только атомики).
pub fn job_end(msg: String, is_err: bool) {
    if !ui_enabled() {
        return;
    }
    if PROG_HWND.load(Ordering::Relaxed) == 0 {
        return;
    }
    *FINAL_MSG.lock().unwrap() = msg;
    FINAL_ERR.store(is_err, Ordering::Relaxed);
    PHASE.store(2, Ordering::Relaxed);
}

/// Немедленно скрыть окно (например, галку выключили во время копирования).
pub fn hide_now() {
    let h = hwnd_of(&PROG_HWND);
    if h.0 as usize != 0 {
        unsafe {
            let _ = KillTimer(h, TIMER_UPDATE);
            let _ = KillTimer(h, TIMER_HIDE);
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
    PHASE.store(0, Ordering::Relaxed);
}

fn now_sec_elapsed() -> f64 {
    let el = now_ms().saturating_sub(JOB_START_MS.load(Ordering::Relaxed));
    (el as f64 / 1000.0).max(0.12)
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            match wp.0 {
                TIMER_UPDATE => {
                    match PHASE.load(Ordering::Relaxed) {
                        1 => update(),
                        2 => {
                            show_final(hwnd);
                            let _ = KillTimer(hwnd, TIMER_UPDATE);
                            let _ = SetTimer(hwnd, TIMER_HIDE, 4000, None);
                        }
                        _ => {}
                    }
                }
                TIMER_HIDE => {
                    let _ = KillTimer(hwnd, TIMER_HIDE);
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    PHASE.store(0, Ordering::Relaxed);
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_COMMAND if wp.0 & 0xffff == BTN_CANCEL => {
            cancel_current();
            LRESULT(0)
        }
        WM_COMMAND if wp.0 & 0xffff == BTN_PAUSE => {
            let p = !crate::COPY_PAUSED.load(Ordering::SeqCst);
            crate::COPY_PAUSED.store(p, Ordering::SeqCst);
            let bp = hwnd_of(&BTN_PAUSE_HWND);
            if bp.0 as usize != 0 {
                let _ = if p {
                    SetWindowTextW(bp, w!("Продолжить"))
                } else {
                    SetWindowTextW(bp, w!("Пауза"))
                };
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            // тёмный текст STATIC: светлые буквы на тёмной подложке
            let hdc = HDC(wp.0 as *mut core::ffi::c_void);
            SetTextColor(hdc, COLORREF(FG));
            SetBkMode(hdc, TRANSPARENT);
            LRESULT(BRUSH_DARK.load(Ordering::Relaxed))
        }
        WM_CLOSE => {
            // как у Explorer: закрытие окна = отмена
            cancel_current();
            hide_now();
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn cancel_current() {
    if let Some(t) = crate::CANCEL.lock().unwrap().clone() {
        t.cancel();
    }
    let info = hwnd_of(&INFO_HWND);
    if info.0 as usize != 0 {
        unsafe {
            let _ = SetWindowTextW(info, &HSTRING::from("Останавливаю…"));
        }
    }
}

unsafe fn update() {
    let done = BYTES_DONE.load(Ordering::Relaxed);
    let total = BYTES_TOTAL.load(Ordering::Relaxed);
    let fd = FILES_DONE.load(Ordering::Relaxed);
    let ft = FILES_TOTAL.load(Ordering::Relaxed);
    let el = now_sec_elapsed();
    let speed = done as f64 / el;
    let pct = if total > 0 {
        (done * 100 / total).min(100)
    } else {
        0
    };
    let cur = CUR_FILE.lock().unwrap().clone();
    let tgt = TARGET_STR.lock().unwrap().clone();

    let mut s = format!("→ {}\n", tgt);
    if ft > 0 {
        s += &format!("{} из {} файлов • ", fd, ft);
    }
    s += &format!("{} из {} ({} %)\n", human(done), human(total), pct);
    if crate::COPY_PAUSED.load(Ordering::SeqCst) {
        s += "⏸ пауза — передача приостановлена";
    } else {
        s += &format!("{}/с • прошло {:.0} с", human(speed as u64), el);
    }
    if !cur.is_empty() {
        let short: String = if cur.chars().count() > 58 {
            cur.chars().skip(cur.chars().count() - 58).collect()
        } else {
            cur
        };
        s += &format!("\nфайл: …{}", short.trim_start());
    }

    let info = hwnd_of(&INFO_HWND);
    if info.0 as usize != 0 {
        let _ = SetWindowTextW(info, &HSTRING::from(s.as_str()));
    }
    let bar = hwnd_of(&BAR_HWND);
    if bar.0 as usize != 0 {
        let _ = SendMessageW(bar, PBM_SETPOS, WPARAM(pct as usize), LPARAM(0));
    }
}

unsafe fn show_final(hwnd: HWND) {
    let msg = FINAL_MSG.lock().unwrap().clone();
    let err = FINAL_ERR.load(Ordering::Relaxed);
    let info = hwnd_of(&INFO_HWND);
    if info.0 as usize != 0 {
        let _ = SetWindowTextW(info, &HSTRING::from(msg.as_str()));
    }
    let bar = hwnd_of(&BAR_HWND);
    if bar.0 as usize != 0 {
        let _ = SendMessageW(bar, PBM_SETPOS, WPARAM(if err { 0 } else { 100 }), LPARAM(0));
    }
    let _ = SetWindowTextW(
        hwnd,
        &HSTRING::from(if err { "Ошибки — RCP" } else { "Готово — RCP" }),
    );
    let btn = hwnd_of(&BTN_HWND);
    if btn.0 as usize != 0 {
        let _ = ShowWindow(btn, SW_HIDE);
    }
    let bp = hwnd_of(&BTN_PAUSE_HWND);
    if bp.0 as usize != 0 {
        let _ = ShowWindow(bp, SW_HIDE);
    }
}
