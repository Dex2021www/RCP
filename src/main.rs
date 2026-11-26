#![windows_subsystem = "windows"]

mod clipboard;
mod copyjob;
mod hook;
mod progress_win;
mod shellx;
mod tray;

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM, ERROR_ALREADY_EXISTS,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, MF_CHECKED, MF_SEPARATOR, MF_STRING,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SetForegroundWindow,
    TrackPopupMenuEx, TranslateMessage, MSG, TPM_BOTTOMALIGN, TPM_RIGHTBUTTON, WNDCLASSW,
    WM_APP, WM_COMMAND, WM_DESTROY, WM_RBUTTONUP, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_OVERLAPPED,
};

use crate::copyjob::Job;

pub const WM_APP_PASTE: u32 = WM_APP + 1;
pub const WM_APP_TRAY: u32 = WM_APP + 2;
pub const WM_APP_SELECT: u32 = WM_APP + 3;

/// Запрос на выделение вставленных файлов в Проводнике (обрабатывается на GUI-потоке).
static SELECT_QUEUE: Mutex<Vec<(std::path::PathBuf, Vec<std::path::PathBuf>)>> =
    Mutex::new(Vec::new());

pub fn request_select(target: &std::path::Path, tops: &[std::path::PathBuf]) {
    if tops.is_empty() {
        return;
    }
    let hwnd = HWND(MAIN_HWND.load(Ordering::SeqCst) as _);
    if hwnd.0 as usize == 0 {
        return; // нет GUI (selftest) — выделение не нужно
    }
    SELECT_QUEUE
        .lock()
        .unwrap()
        .push((target.to_path_buf(), tops.to_vec()));
    unsafe {
        let _ = PostMessageW(hwnd, WM_APP_SELECT, WPARAM(0), LPARAM(0));
    }
}

const IDM_PAUSE: usize = 1001;
const IDM_STOP: usize = 1002;
const IDM_EXIT: usize = 1003;
const IDM_UI: usize = 1004;

pub static MAIN_HWND: AtomicIsize = AtomicIsize::new(0);
pub static PAUSED: AtomicBool = AtomicBool::new(false);
/// Пауза текущей передачи из окна прогресса (движок замирает между кусками).
pub static COPY_PAUSED: AtomicBool = AtomicBool::new(false);
pub static BUSY: AtomicBool = AtomicBool::new(false);
pub static TX: OnceLock<tokio::sync::mpsc::UnboundedSender<Job>> = OnceLock::new();
pub static CANCEL: Mutex<Option<tokio_util::sync::CancellationToken>> = Mutex::new(None);
pub static LOGF: OnceLock<Mutex<std::fs::File>> = OnceLock::new();
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

pub fn logln(msg: &str) {
    let line = format!("[{}] {}", fmt_now(), msg);
    if let Some(f) = LOGF.get() {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{}", line);
        }
    }
    // без консоли это тихо игнорируется; в --selftest вывод виден
    let _ = writeln!(std::io::stdout(), "{}", line);
}

pub fn tip_text() -> String {
    if PAUSED.load(Ordering::SeqCst) {
        "RCP — ПАУЗА (Ctrl+V обрабатывает Проводник)".into()
    } else {
        "RCP — активен (Ctrl+C / Ctrl+V в Проводнике)".into()
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--selftest") {
        copyjob::selftest();
        return;
    }
    if args.iter().any(|a| a == "--tabs") {
        unsafe {
            let _ = windows::Win32::System::Console::AllocConsole();
            let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            shellx::dump_tabs();
        }
        std::thread::sleep(std::time::Duration::from_secs(10));
        return;
    }
    // тест выделения: rcp.exe --select <папка> <имя1> <имя2> ...
    if let Some(pos) = args.iter().position(|a| a == "--select") {
        if args.len() >= pos + 3 {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let target = std::path::PathBuf::from(&args[pos + 1]);
                let tops: Vec<std::path::PathBuf> = (pos + 2..args.len())
                    .map(|i| crate::copyjob::lp(&target).join(&args[i]))
                    .collect();
                shellx::select_items(&target, &tops);
            }
        }
        return;
    }
    if args.iter().any(|a| a == "--uitest") {
        ui_test();
        return;
    }

    // лог-файл
    if let Ok(tmp) = std::env::var("TEMP") {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(format!("{}\\rcp.log", tmp))
        {
            let _ = LOGF.set(Mutex::new(f));
        }
    }
    logln("=== запуск rcp ===");

    // единственный экземпляр
    unsafe {
        if let Ok(h) = CreateMutexW(None, false, w!("Global\\rcp-singleton")) {
            if GetLastError() == ERROR_ALREADY_EXISTS {
                logln("уже запущен — выходим");
                return;
            }
            let _ = h;
        }
    }

    // рабочий поток tokio: serialize заданий, копирование уходит в spawn_blocking (rayon внутри)
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
    let _ = TX.set(tx);
    std::thread::Builder::new()
        .name("tokio".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("tokio runtime");
            rt.block_on(async move {
                while let Some(job) = rx.recv().await {
                    BUSY.store(true, Ordering::SeqCst);
                    let token = tokio_util::sync::CancellationToken::new();
                    *CANCEL.lock().unwrap() = Some(token.clone());
                    let t = token.clone();
                    let _ = tokio::task::spawn_blocking(move || copyjob::run(job, t)).await;
                    *CANCEL.lock().unwrap() = None;
                    BUSY.store(false, Ordering::SeqCst);
                }
            });
        })
        .expect("spawn tokio thread");

    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let tb = RegisterWindowMessageW(w!("TaskbarCreated"));
        if tb != 0 {
            let _ = TASKBAR_CREATED.set(tb);
        }

        let inst = GetModuleHandleW(None).expect("GetModuleHandleW");
        let class_name = w!("RcpWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: inst.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            w!(""),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            inst,
            None,
        )
        .expect("CreateWindowExW");

        MAIN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        tray::add(hwnd);
        progress_win::create(inst);
        hook::start();

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
        tray::remove(hwnd);
        logln("выход");
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if let Some(&tb) = TASKBAR_CREATED.get() {
        if msg == tb && tb != 0 {
            tray::add(hwnd);
            return LRESULT(0);
        }
    }
    match msg {
        WM_APP_PASTE => {
            on_paste(HWND(wp.0 as _));
            LRESULT(0)
        }
        WM_APP_SELECT => {
            let jobs = std::mem::take(&mut *SELECT_QUEUE.lock().unwrap());
            for (target, tops) in &jobs {
                unsafe {
                    shellx::select_items(target, tops);
                }
            }
            LRESULT(0)
        }
        WM_APP_TRAY => {
            if lp.0 as u32 == WM_RBUTTONUP {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            match wp.0 & 0xffff {
                1001 => {
                    let p = !PAUSED.load(Ordering::SeqCst);
                    PAUSED.store(p, Ordering::SeqCst);
                    tray::set_tip(&tip_text());
                }
                1002 => {
                    if let Some(t) = CANCEL.lock().unwrap().clone() {
                        t.cancel();
                    }
                }
                1003 => {
                    let _ = DestroyWindow(hwnd);
                }
                1004 => {
                    let on = !progress_win::ui_enabled();
                    progress_win::SHOW_UI.store(on, Ordering::SeqCst);
                    if !on {
                        progress_win::hide_now();
                    }
                    tray::set_tip(&tip_text());
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn on_paste(fg: HWND) {
    if PAUSED.load(Ordering::SeqCst) {
        return;
    }
    let Some((paths, is_move)) = clipboard::read_file_list() else {
        logln("в буфере нет файлов — игнорируем");
        return;
    };
    let Some(target) = shellx::folder_for(fg) else {
        tray::balloon(
            "RCP",
            "Не удалось определить папку назначения (особая папка/архив?) — вставка отменена.",
            true,
        );
        return;
    };

    // защита от вставки папки в саму себя
    let canon_t = std::fs::canonicalize(&target).ok();
    for src in &paths {
        if let (Ok(cs), Some(ct)) = (std::fs::canonicalize(src), canon_t.as_ref()) {
            if ct == &cs || ct.starts_with(&cs) {
                tray::balloon("RCP", "Нельзя вставить папку в саму себя.", true);
                return;
            }
        }
    }

    logln(&format!(
        "задание: {} объект(ов), режим {}, цель {}",
        paths.len(),
        if is_move { "перемещение" } else { "копирование" },
        target.display()
    ));
    // окно прогресса показываем сразу, до начала обхода источников
    progress_win::job_begin(
        &format!(
            "{} — RCP",
            if is_move { "Перемещение" } else { "Копирование" }
        ),
        &target.display().to_string(),
    );
    let _ = TX.get().unwrap().send(Job {
        sources: paths,
        target,
        is_move,
    });
}

/// Диагностический режим --uitest: окно прогресса с симулированной передачей
/// (без хука и синглтона) — визуальная проверка тёмной темы/топмоста/кнопок.
fn ui_test() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let inst = GetModuleHandleW(None).expect("GetModuleHandleW");
        let class_name = w!("RcpWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: inst.into(),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class_name,
            w!(""),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            inst,
            None,
        )
        .expect("CreateWindowExW");
        MAIN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        tray::add(hwnd);
        progress_win::create(inst);

        std::thread::spawn(|| {
            use std::sync::atomic::Ordering as O;
            progress_win::job_begin(
                "Копирование — RCP",
                "C:\\Users\\User\\Downloads\\Видео",
            );
            progress_win::FILES_TOTAL.store(3, O::Relaxed);
            copyjob::BYTES_TOTAL.store(3_456_000_000, O::Relaxed);
            let names = ["кино_2026.mkv", "фото отпуск (1).jpg", "отчёт проект.pdf"];
            for (i, nm) in names.iter().enumerate() {
                *progress_win::CUR_FILE.lock().unwrap() = (*nm).to_string();
                for _ in 0..18 {
                    std::thread::sleep(std::time::Duration::from_millis(60));
                    copyjob::BYTES_DONE.fetch_add(64_000_000, O::Relaxed);
                }
                progress_win::FILES_DONE.store((i + 1) as u64, O::Relaxed);
            }
            progress_win::job_end(
                "✓ бит-в-бит OK\n3 файла, 3,2 ГБ, скопировано за 3,3 с — 1,0 ГБ/с. Сверка бит-в-бит: OK (3,2 ГБ)".into(),
                false,
            );
            std::thread::sleep(std::time::Duration::from_secs(6));
            std::process::exit(0);
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
        tray::remove(hwnd);
    }
}

unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let mut fl = MF_STRING;
    if PAUSED.load(Ordering::SeqCst) {
        fl |= MF_CHECKED;
    }
    let _ = AppendMenuW(menu, fl, IDM_PAUSE, w!("Пауза (Ctrl+V через Проводник)"));
    let mut fl_ui = MF_STRING;
    if progress_win::ui_enabled() {
        fl_ui |= MF_CHECKED;
    }
    let _ = AppendMenuW(menu, fl_ui, IDM_UI, w!("Окно прогресса"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_STOP, w!("Остановить текущее копирование"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT, w!("Выход"));

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    let _ = SetForegroundWindow(hwnd);
    let _ = TrackPopupMenuEx(
        menu,
        TPM_RIGHTBUTTON.0 | TPM_BOTTOMALIGN.0,
        pt.x,
        pt.y,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
}

fn fmt_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() as i64;
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}
