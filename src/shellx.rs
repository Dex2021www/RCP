use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;
use windows::core::{Interface, VARIANT};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, IServiceProvider};
use windows::Win32::System::Ole::IOleWindow;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, IFolderView, IFolderView2, IShellBrowser, IShellFolder, IShellItem,
    IShellWindows, IWebBrowser2, KF_FLAG_DEFAULT, ILFindLastID, ILFree,
    SHCreateItemFromParsingName, SHGetDesktopFolder, SHGetKnownFolderPath, ShellWindows,
    SID_STopLevelBrowser, SIGDN_NORMALDISPLAY, SVSI_DESELECTOTHERS, SVSI_ENSUREVISIBLE,
    SVSI_FOCUSED, SVSI_SELECT,
};
use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, GetWindowTextW, IsWindowVisible};

/// Класс окна Проводника в фокусе -> папка назначения.
pub fn folder_for(fg: HWND) -> Option<PathBuf> {
    let class = class_of(fg);
    match class.as_str() {
        "Progman" | "WorkerW" => desktop_dir(),
        "CabinetWClass" | "ExploreWClass" => explorer_folder(fg),
        _ => None,
    }
}

pub fn class_of(h: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn desktop_dir() -> Option<PathBuf> {
    unsafe {
        let pw = SHGetKnownFolderPath(&FOLDERID_Desktop, KF_FLAG_DEFAULT, None).ok()?;
        let s = pw_to_string(pw.0);
        CoTaskMemFree(Some(pw.0 as _));
        Some(PathBuf::from(s))
    }
}

unsafe fn pw_to_string(p: *mut u16) -> String {
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    let slice = std::slice::from_raw_parts(p, len);
    String::from_utf16_lossy(slice)
}

/// Перебирает открытые окна/вкладки Проводника и берёт папку окна с данным HWND.
/// У вкладок одного окна общий HWND. Активную вкладку выбираем каскадом:
/// 1) по заголовку окна (он равен отображаемому имени активной вкладки),
/// 2) по видимой shell view (работает не на всех сборках Windows),
/// 3) в худшем случае — первая вкладка окна (копирование хотя бы работает).
fn explorer_folder(fg: HWND) -> Option<PathBuf> {
    unsafe {
        let sw: IShellWindows = CoCreateInstance(&ShellWindows, None, CLSCTX_ALL).ok()?;
        let count = sw.Count().ok()?;
        let mut candidates: Vec<IWebBrowser2> = Vec::new();
        for i in 0..count {
            let Ok(disp) = sw.Item(&VARIANT::from(i)) else {
                continue;
            };
            let Ok(browser) = disp.cast::<IWebBrowser2>() else {
                continue;
            };
            let Ok(hwnd) = browser.HWND() else {
                continue;
            };
            if hwnd.0 == fg.0 as isize {
                candidates.push(browser);
            }
        }
        let browser = match candidates.len() {
            0 => return None,
            1 => candidates.pop()?,
            _ => pick_active_tab(fg, &candidates)?,
        };
        let url = browser.LocationURL().ok()?;
        let u = String::from_utf16_lossy(url.as_wide());
        file_url_to_path(&u)
    }
}

fn pick_active_tab(fg: HWND, candidates: &[IWebBrowser2]) -> Option<IWebBrowser2> {
    // 1) заголовок окна начинается с имени активной вкладки:
    //    «Изображения и еще 1 вкладка — проводник» / «Имя — проводник».
    //    Локализация учтена через display-имя («Загрузки» == Downloads).
    //    При нескольких матчах берём самый длинный (вложенные имена).
    let title = window_title(fg).trim().to_lowercase();
    if !title.is_empty() {
        let mut best: Option<(usize, &IWebBrowser2)> = None;
        for b in candidates {
            let Some(d) = url_of(b).and_then(|u| display_name_of_url(&u)) else {
                continue;
            };
            let dl = d.trim().to_lowercase();
            if !dl.is_empty() && title.starts_with(&dl) {
                if best.as_ref().map_or(true, |(len, _)| dl.len() > *len) {
                    best = Some((dl.len(), b));
                }
            }
        }
        if let Some((_, b)) = best {
            return Some(b.clone());
        }
    }
    // 2) видимая shell view (работает не на всех сборках Windows)
    if let Some(b) = candidates.iter().find(|b| tab_is_active(b)) {
        return Some(b.clone());
    }
    // 3) первая вкладка
    candidates.first().cloned()
}

fn url_of(b: &IWebBrowser2) -> Option<String> {
    let u = unsafe { b.LocationURL() }.ok()?;
    Some(String::from_utf16_lossy(u.as_wide()))
}

fn window_title(fg: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(fg, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// Отображаемое имя папки (как в Проводнике) по её пути.
fn display_name_of_url(url: &str) -> Option<String> {
    let path = file_url_to_path(url)?;
    unsafe {
        let item: IShellItem =
            SHCreateItemFromParsingName(&windows::core::HSTRING::from(path.as_os_str()), None)
                .ok()?;
        let name = item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?;
        let s = pw_to_string(name.0);
        CoTaskMemFree(Some(name.0 as _));
        Some(s)
    }
}

/// Активна ли вкладка: её shell view видима, у фоновых вкладок — скрыта.
fn tab_is_active(browser: &IWebBrowser2) -> bool {
    view_hwnd_of(browser)
        .map(|h| unsafe { IsWindowVisible(h).as_bool() })
        .unwrap_or(false)
}

/// HWND shell view вкладки (через SID_STopLevelBrowser -> IShellBrowser -> view).
fn view_hwnd_of(browser: &IWebBrowser2) -> Option<windows::Win32::Foundation::HWND> {
    unsafe {
        let sp = browser.cast::<IServiceProvider>().ok()?;
        let sb = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser).ok()?;
        let view = sb.QueryActiveShellView().ok()?;
        let ow = view.cast::<IOleWindow>().ok()?;
        ow.GetWindow().ok()
    }
}

/// Выделить вставленные элементы в папке назначения (как это делает Explorer
/// после своей вставки): выделить + снять выделение с остальных + прокрутить
/// к файлу + установить фокус. Вызывать на STA-потоке с окном (GUI).
pub unsafe fn select_items(target: &Path, tops: &[PathBuf]) {
    let Ok(sw) = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL) else {
        return;
    };
    let Ok(count) = sw.Count() else {
        return;
    };
    let canon_t = std::fs::canonicalize(target).ok();
    // ищем вкладку, показывающую целевую папку
    let mut chosen: Option<IWebBrowser2> = None;
    for i in 0..count {
        let Ok(disp) = sw.Item(&VARIANT::from(i)) else {
            continue;
        };
        let Ok(b) = disp.cast::<IWebBrowser2>() else {
            continue;
        };
        let Some(u) = url_of(&b) else { continue };
        let Some(p) = file_url_to_path(&u) else { continue };
        let same = match (&canon_t, std::fs::canonicalize(&p)) {
            (Some(a), Ok(b)) => a == &b,
            _ => false,
        };
        if same {
            chosen = Some(b);
            break;
        }
    }
    let Some(browser) = chosen else {
        crate::logln("select: вкладка с целевой папкой не найдена");
        return;
    };
    let Ok(sp) = browser.cast::<IServiceProvider>() else {
        crate::logln("select: нет IServiceProvider");
        return;
    };
    let Ok(sb) = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser) else {
        crate::logln("select: нет IShellBrowser");
        return;
    };
    let Ok(sv) = sb.QueryActiveShellView() else {
        crate::logln("select: нет IShellView");
        return;
    };
    let Ok(fv) = sv.cast::<IFolderView>() else {
        crate::logln("select: нет IFolderView");
        return;
    };
    let Ok(_isf) = fv.GetFolder::<IShellFolder>() else {
        crate::logln("select: нет IShellFolder");
        return;
    };
    // pidl вставленных элементов: абсолютный путь парсит десктопный IShellFolder,
    // для view нужен последний ID (относительный pidl)
    let Ok(desktop) = SHGetDesktopFolder() else {
        crate::logln("select: нет десктопного IShellFolder");
        return;
    };
    let mut pidls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(tops.len());
    let mut fulls: Vec<*mut ITEMIDLIST> = Vec::with_capacity(tops.len());
    for p in tops {
        let plain = crate::copyjob::disp(p);
        let mut full: *mut ITEMIDLIST = std::ptr::null_mut();
        let ok = desktop
            .ParseDisplayName(
                None,
                None,
                &windows::core::HSTRING::from(plain.as_str()),
                None,
                &mut full,
                std::ptr::null_mut(),
            )
            .is_ok();
        if ok && !full.is_null() {
            let rel = ILFindLastID(full);
            if !rel.is_null() {
                pidls.push(rel);
                fulls.push(full); // rel указывает внутрь full — чистим full целиком
            } else {
                ILFree(Some(full as *const _));
            }
        }
    }
    if pidls.is_empty() {
        crate::logln("select: pidl не построены");
        return;
    }
    // флаги применяются к каждому элементу отдельно, поэтому DESELECTOTHERS —
    // только на первом, остальные добавляются в выделение
    let first_flags =
        (SVSI_SELECT.0 | SVSI_DESELECTOTHERS.0 | SVSI_ENSUREVISIBLE.0 | SVSI_FOCUSED.0) as u32;
    let next_flags = (SVSI_SELECT.0 | SVSI_ENSUREVISIBLE.0) as u32;
    let mut n_ok = 0usize;
    for (i, pidl) in pidls.iter().enumerate() {
        let flags = if i == 0 { first_flags } else { next_flags };
        if sv.SelectItem(*pidl as *const _, flags).is_ok() {
            n_ok += 1;
        }
    }
    crate::logln(&format!("select: {} из {} выделено", n_ok, pidls.len()));
    for full in fulls {
        ILFree(Some(full as *const _));
    }
}

/// Диагностика: дамп всех записей IShellWindows (для --tabs).
pub unsafe fn dump_tabs() {
    println!("=== дамп IShellWindows ===");
    let Ok(sw) = CoCreateInstance::<_, IShellWindows>(&ShellWindows, None, CLSCTX_ALL) else {
        println!("ShellWindows: не создан");
        return;
    };
    let count = sw.Count().unwrap_or(0);
    println!("записей: {}", count);
    for i in 0..count {
        let Ok(disp) = sw.Item(&VARIANT::from(i)) else {
            println!("[{}] Item: ошибка", i);
            continue;
        };
        let Ok(b) = disp.cast::<IWebBrowser2>() else {
            println!("[{}] не IWebBrowser2", i);
            continue;
        };
        let hwnd = b.HWND().map(|h| h.0).unwrap_or(0);
        let url = b
            .LocationURL()
            .map(|u| String::from_utf16_lossy(u.as_wide()))
            .unwrap_or_else(|e| format!("<err {}>", e.code().0));
        let view = view_hwnd_of(&b)
            .map(|h| h.0 as usize)
            .unwrap_or(0);
        // гранулярная цепочка: что именно недоступно на этой сборке Windows
        let chain = unsafe {
            let sp_ok = b.cast::<IServiceProvider>().is_ok();
            let sb = b
                .cast::<IServiceProvider>()
                .ok()
                .and_then(|s| s.QueryService::<IShellBrowser>(&SID_STopLevelBrowser).ok());
            let sb_ok = sb.is_some();
            let sv = sb.and_then(|sb| sb.QueryActiveShellView().ok());
            let sv_ok = sv.is_some();
            let fv_ok = sv.and_then(|v| v.cast::<IFolderView>().ok()).is_some();
            format!("sp={} sb={} sv={} fv={}", sp_ok, sb_ok, sv_ok, fv_ok)
        };
        let vis = if view != 0 {
            IsWindowVisible(windows::Win32::Foundation::HWND(view as _)).as_bool()
        } else {
            false
        };
        let disp = display_name_of_url(&url).unwrap_or_else(|| "-".into());
        let wtitle = if hwnd != 0 {
            window_title(windows::Win32::Foundation::HWND(hwnd as _))
        } else {
            String::new()
        };
        println!(
            "[{}] hwnd=0x{:X} view=0x{:X} vis={} {} display=\"{}\" заголовок=\"{}\" url={}",
            i, hwnd as usize, view, vis, chain, disp, wtitle, url
        );
    }
    // проверка выбора активной вкладки для каждого окна с несколькими вкладками
    let mut by_hwnd: Vec<(isize, Vec<IWebBrowser2>)> = Vec::new();
    for i in 0..count {
        let Ok(disp) = sw.Item(&VARIANT::from(i)) else { continue };
        let Ok(b) = disp.cast::<IWebBrowser2>() else { continue };
        let h = b.HWND().map(|x| x.0).unwrap_or(0);
        if h == 0 { continue }
        match by_hwnd.iter_mut().find(|(k, _)| *k == h) {
            Some((_, v)) => v.push(b),
            None => by_hwnd.push((h, vec![b])),
        }
    }
    for (h, group) in by_hwnd {
        let title = window_title(HWND(h as _));
        let chosen = pick_active_tab(HWND(h as _), &group);
        let name = chosen.as_ref().and_then(|b| url_of(b)).unwrap_or_default();
        // текущее выделение активной вкладки
        let sel = chosen.and_then(|b| unsafe { selection_of(&b) }).unwrap_or_default();
        println!(
            "окно 0x{:X}: вкладок {}, заголовок \"{}\" → вкладка: {}, выделено: {}",
            h as usize,
            group.len(),
            title,
            name,
            if sel.is_empty() { "—".to_string() } else { sel.join(", ") }
        );
    }
}

/// Имена выделенных элементов активной вкладки (для --tabs).
unsafe fn selection_of(b: &IWebBrowser2) -> Option<Vec<String>> {
    let sp = b.cast::<IServiceProvider>().ok()?;
    let sb = sp.QueryService::<IShellBrowser>(&SID_STopLevelBrowser).ok()?;
    let sv = sb.QueryActiveShellView().ok()?;
    let fv2 = sv.cast::<IFolderView2>().ok()?;
    let arr = fv2.GetSelection(false).ok()?;
    let n = arr.GetCount().unwrap_or(0).min(5);
    let mut names = Vec::new();
    for i in 0..n {
        if let Ok(item) = arr.GetItemAt(i) {
            if let Ok(pw) = item.GetDisplayName(SIGDN_NORMALDISPLAY) {
                names.push(pw_to_string(pw.0));
                CoTaskMemFree(Some(pw.0 as _));
            }
        }
    }
    Some(names)
}

/// file:///C:/Users/... или file://server/share/... -> путь, если это существующая папка.
pub fn file_url_to_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file:")?;
    let (part, is_unc) = if let Some(r) = rest.strip_prefix("///") {
        (r, false)
    } else if let Some(r) = rest.strip_prefix("//") {
        (r, true)
    } else {
        return None;
    };
    let dec = percent_decode_str(part).decode_utf8().ok()?.into_owned();
    let norm = dec.replace('/', "\\");
    let path = if is_unc {
        PathBuf::from(format!("\\\\{}", norm))
    } else {
        PathBuf::from(norm)
    };
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => Some(path),
        _ => None,
    }
}
