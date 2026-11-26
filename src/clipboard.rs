use std::path::PathBuf;

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::DataExchange::{
    CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_HDROP;
use windows::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows::core::w;

/// Быстрая проверка (без открытия буфера), что в буфере лежат файлы (CF_HDROP).
pub fn has_files() -> bool {
    unsafe { IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() }
}

/// Читает список файлов из буфера обмена.
/// Возвращает (пути, is_move) — is_move=true, если это вставка после Ctrl+X (вырезали).
pub fn read_file_list() -> Option<(Vec<PathBuf>, bool)> {
    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        let res = (|| {
            let Ok(h) = GetClipboardData(CF_HDROP.0 as u32) else {
                return None;
            };
            let drop = HDROP(h.0);
            let count = DragQueryFileW(drop, u32::MAX, None);
            let mut paths = Vec::with_capacity(count as usize);
            for i in 0..count {
                let len = DragQueryFileW(drop, i, None);
                let mut buf = vec![0u16; len as usize + 1];
                let got = DragQueryFileW(drop, i, Some(buf.as_mut_slice()));
                paths.push(PathBuf::from(String::from_utf16_lossy(
                    &buf[..got as usize],
                )));
            }

            // Preferred DropEffect: 2 = перемещение (Ctrl+X), 1 = копирование
            let mut is_move = false;
            let cf = RegisterClipboardFormatW(w!("Preferred DropEffect"));
            if cf != 0 {
                if let Ok(h) = GetClipboardData(cf) {
                    let g = HGLOBAL(h.0);
                    let p = GlobalLock(g) as *const u32;
                    if !p.is_null() {
                        let effect = *p;
                        is_move = effect & 2 != 0 && effect & 1 == 0;
                        let _ = GlobalUnlock(g);
                    }
                }
            }

            if paths.is_empty() {
                None
            } else {
                Some((paths, is_move))
            }
        })();
        let _ = CloseClipboard();
        res
    }
}
