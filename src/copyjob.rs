use std::cell::RefCell;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::windows::fs::{FileExt, MetadataExt, symlink_dir, symlink_file};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use tokio_util::sync::CancellationToken;
use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, SetFileAttributesW, SetFileTime, FILE_ATTRIBUTE_NORMAL,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
};

use crate::{logln, progress_win, tray, tip_text};

pub struct Job {
    pub sources: Vec<PathBuf>,
    pub target: PathBuf,
    pub is_move: bool,
}

const BUF: usize = 16 * 1024 * 1024; // буфер копирования на поток
const VBUF: usize = 1024 * 1024; // буфер сверки
const BIG_THRESHOLD: u64 = 128 * 1024 * 1024; // файлы крупнее этого — кусками
const BIG_CHUNKS: u64 = 3; // потоков на один большой файл
const HUGE_THRESHOLD: u64 = 1024 * 1024 * 1024; // гигантские — побольше потоков
const HUGE_CHUNKS: u64 = 4;
const ATTR_MASK: u32 = 0x1 | 0x2 | 0x4 | 0x20; // readonly|hidden|system|archive

pub static BYTES_DONE: AtomicU64 = AtomicU64::new(0);
pub static BYTES_TOTAL: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// Буферы живут столько же, сколько поток: выделяются один раз за всё время
    /// работы программы, мелкие файлы сразу «залетают» в готовый буфер.
    static TLS_CBUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static TLS_VA: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static TLS_VB: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

struct Times {
    created: Option<SystemTime>,
    modified: Option<SystemTime>,
    accessed: Option<SystemTime>,
}

impl Times {
    fn from_md(md: &fs::Metadata) -> Times {
        Times {
            created: md.created().ok(),
            modified: md.modified().ok(),
            accessed: md.accessed().ok(),
        }
    }
}

struct FileItem {
    src: PathBuf, // в \\?\-виде
    dst: PathBuf, // в \\?\-виде
    src_disp: String,
    size: u64,
    is_link: bool,
    attrs: u32,
    times: Times,
}

struct DirItem {
    src: PathBuf,
    dst: PathBuf,
    attrs: u32,
    times: Times,
}

struct Fail {
    path: String,
    err: String,
}

impl Fail {
    fn new(path: impl Into<String>, err: &impl std::fmt::Display) -> Fail {
        Fail {
            path: path.into(),
            err: err.to_string(),
        }
    }
}

/// Точка входа задания. Вызывается в блокирующем потоке tokio.
pub fn run(job: Job, token: CancellationToken) {
    let t0 = Instant::now();
    BYTES_DONE.store(0, Ordering::Relaxed);
    BYTES_TOTAL.store(0, Ordering::Relaxed);
    let fails = Mutex::new(Vec::<Fail>::new());
    let target_lp = lp(&job.target);

    // ---- быстрый путь: перемещение в пределах тома = rename, мгновенно ----
    let mut remaining: Vec<PathBuf> = Vec::new();
    let mut renamed_files = 0u64;
    let mut renamed_bytes = 0u64;
    let mut tops: Vec<PathBuf> = Vec::new(); // верхние пути вставленного (для выделения)
    if job.is_move {
        for src in &job.sources {
            // вырезать и вставить в ту же папку — как Explorer, ничего не делаем
            if in_target_folder(src, &job.target) {
                logln(&format!("перемещение в ту же папку, пропуск: {}", src.display()));
                continue;
            }
            let src_lp = lp(src);
            let dst_lp = target_lp.join(src.file_name().unwrap_or_default());
            match fs::rename(&src_lp, &dst_lp) {
                Ok(()) => {
                    let (f, b) = soft_stats(&dst_lp);
                    renamed_files += f;
                    renamed_bytes += b;
                    tops.push(dst_lp);
                    logln(&format!("мгновенное перемещение: {}", disp(src)));
                }
                Err(_) => remaining.push(src.clone()),
            }
        }
        if remaining.is_empty() {
            let msg = format!(
                "Перемещено мгновенно (один том): {} {}, {}",
                renamed_files,
                plural(renamed_files, ("файл", "файла", "файлов")),
                human(renamed_bytes)
            );
            progress_win::job_end(format!("✓ {}", msg), false);
            if !progress_win::ui_enabled() {
                tray::balloon("RCP: готово ✓", &msg, false);
            }
            logln(&format!("итог: {}", msg));
            std::thread::sleep(Duration::from_millis(150));
            crate::request_select(&job.target, &tops);
            return;
        }
    } else {
        remaining = job.sources.clone();
    }

    // ---- обход источников (jwalk, параллельно) ----
    let (mut dirs, mut files) = (Vec::new(), Vec::new());
    walk_sources(
        &remaining,
        &target_lp,
        &job.target,
        &mut tops,
        &mut dirs,
        &mut files,
        &mut fails.lock().unwrap(),
    );

    let total: u64 = files.iter().map(|f| f.size).sum();
    let ui = progress_win::ui_enabled();
    if ui {
        BYTES_TOTAL.store(total, Ordering::Relaxed);
        progress_win::FILES_TOTAL.store(files.len() as u64, Ordering::Relaxed);
    }
    let nfiles = files.len();
    logln(&format!(
        "план: {} {}, {}, {} папок, цель {}",
        nfiles,
        plural(nfiles as u64, ("файл", "файла", "файлов")),
        human(total),
        dirs.len(),
        disp(&target_lp)
    ));
    if nfiles == 0 && dirs.is_empty() {
        progress_win::job_end("Нечего копировать (пусто).".into(), false);
        if !ui {
            tray::balloon("RCP", "Нечего копировать (пусто).", false);
        }
        tray::set_tip(&tip_text());
        return;
    }

    if !ui {
        tray::balloon(
            if job.is_move {
                "RCP: перемещение"
            } else {
                "RCP: копирование"
            },
            &format!(
                "{} {} ({}) → {}",
                nfiles,
                plural(nfiles as u64, ("файл", "файла", "файлов")),
                human(total),
                job.target.display()
            ),
            false,
        );
    }

    // ---- создание папок (параллельно; create_dir_all сам создаёт родителей) ----
    dirs.par_iter().for_each(|d| {
        if let Err(e) = fs::create_dir_all(&d.dst) {
            let mut fl = fails.lock().unwrap();
            if fl.len() < 200 {
                fl.push(Fail::new(disp(&d.dst), &e));
            }
        }
    });

    // ---- параллельное копирование + мгновенная сверка (конвейер, rayon) ----
    // большие файлы первыми — равномернее загрузка и меньше «хвоста»
    if files.len() > 1 {
        files.sort_by(|a, b| b.size.cmp(&a.size));
    }
    let verified = AtomicU64::new(0);
    let cancelled = files
        .par_iter()
        .try_for_each(|f| {
            if token.is_cancelled() {
                return Err(());
            }
            if ui {
                *progress_win::CUR_FILE.lock().unwrap() = f
                    .src
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
            }
            let res = if f.is_link {
                recreate_link(f).map(|_| ())
            } else {
                TLS_CBUF.with(|c| {
                    let mut b = c.borrow_mut();
                    if b.len() < BUF {
                        b.resize(BUF, 0);
                    }
                    copy_file(f, &mut b[..BUF], &token)
                })
            };
            if let Err(e) = res {
                if token.is_cancelled() {
                    return Err(());
                }
                let mut fl = fails.lock().unwrap();
                if fl.len() < 200 {
                    fl.push(Fail::new(&f.src_disp, &e));
                }
                return Ok(());
            }
            // сверяем сразу, пока оба файла в кэше
            let v = TLS_VA.with(|ca| {
                TLS_VB.with(|cb| {
                    let (mut a, mut b) = (ca.borrow_mut(), cb.borrow_mut());
                    if a.len() < VBUF {
                        a.resize(VBUF, 0);
                    }
                    if b.len() < VBUF {
                        b.resize(VBUF, 0);
                    }
                    verify_with(&f.src, &f.dst, &mut a[..VBUF], &mut b[..VBUF])
                })
            });
            if ui {
                progress_win::FILES_DONE.fetch_add(1, Ordering::Relaxed);
            }
            match v {
                Ok(b) => {
                    verified.fetch_add(b, Ordering::Relaxed);
                    Ok(())
                }
                Err(msg) => {
                    let mut fl = fails.lock().unwrap();
                    if fl.len() < 200 {
                        fl.push(Fail {
                            path: f.src_disp.clone(),
                            err: format!("сверка: {}", msg),
                        });
                    }
                    Ok(())
                }
            }
        })
        .is_err();

    // ---- метаданные папок (после записи файлов) ----
    for d in &dirs {
        apply_dir_meta(d);
    }

    if cancelled {
        let d = BYTES_DONE.load(Ordering::Relaxed);
        let t = BYTES_TOTAL.load(Ordering::Relaxed);
        let msg = format!(
            "Остановлено: скопировано {} из {}. Сверено {}.",
            human(d),
            human(t),
            human(verified.load(Ordering::Relaxed))
        );
        progress_win::job_end(format!("⏹ {}", msg), true);
        if !ui {
            tray::balloon("RCP: остановлено", &msg, true);
        }
        tray::set_tip(&tip_text());
        logln(&msg);
        return;
    }

    // ---- перемещение: удалить источники после успешной сверки ----
    if job.is_move && fails.lock().unwrap().is_empty() {
        for f in &files {
            if !f.is_link {
                let _ = fs::remove_file(&f.src);
            } else {
                let _ = fs::remove_file(&f.src); // symlink удаляется как файл
            }
        }
        let mut src_dirs: Vec<&Path> = dirs.iter().map(|d| d.src.as_path()).collect();
        src_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
        for d in src_dirs {
            if let Err(e) = fs::remove_dir(d) {
                logln(&format!("не удалось удалить {}: {}", disp(d), e));
            }
        }
    }

    // ---- итоги ----
    let elapsed = t0.elapsed().as_secs_f64();
    let done_bytes = BYTES_DONE.load(Ordering::Relaxed);
    let fl = fails.lock().unwrap();
    let verb = if job.is_move {
        "перемещено"
    } else {
        "скопировано"
    };

    if fl.is_empty() {
        let speed = done_bytes as f64 / elapsed.max(0.001);
        let title = if job.is_move {
            "RCP: перемещено ✓"
        } else {
            "RCP: скопировано ✓"
        };
        let msg = format!(
            "{} {}, {}, {} за {} с — {}/с. Сверка бит-в-бит: OK ({})",
            nfiles as u64 + renamed_files,
            plural((nfiles as u64 + renamed_files).max(1), ("файл", "файла", "файлов")),
            human(total + renamed_bytes),
            verb,
            format!("{:.1}", elapsed).replace('.', ","),
            human(speed as u64),
            human(verified.load(Ordering::Relaxed))
        );
        progress_win::job_end(format!("✓ бит-в-бит OK\n{}", msg), false);
        if !ui {
            tray::balloon(title, &msg, false);
        }
        logln(&format!("итог: {}", msg));
        // выделить вставленное в Проводнике (лёгкая пауза — вид обновляет список)
        std::thread::sleep(Duration::from_millis(250));
        crate::request_select(&job.target, &tops);
    } else {
        let mut sample = String::new();
        for f in fl.iter().take(3) {
            sample.push_str(&format!("\n• {}: {}", f.path, f.err));
        }
        if fl.len() > 3 {
            sample.push_str(&format!("\n… и ещё {} (см. %TEMP%\\rcp.log)", fl.len() - 3));
        }
        for f in fl.iter() {
            logln(&format!("ОШИБКА {}: {}", f.path, f.err));
        }
        let msg = format!(
            "Скопировано {} из {} {}{}, проверка: {}",
            nfiles - fl.len(),
            nfiles,
            human(done_bytes),
            sample,
            human(verified.load(Ordering::Relaxed))
        );
        progress_win::job_end(format!("✗ есть ошибки\n{}", msg), true);
        tray::balloon("RCP: есть ошибки", &msg, true);
        logln(&format!("итог с ошибками: {} из {}", nfiles - fl.len(), nfiles));
    }
    tray::set_tip(&tip_text());
}

// ---------- планирование ----------

/// Источник лежит прямо в папке назначения (вставка в ту же папку).
fn in_target_folder(src: &Path, target: &Path) -> bool {
    let Some(parent) = src.parent() else {
        return false;
    };
    match (fs::canonicalize(parent), fs::canonicalize(target)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Свободное имя «имя - Копия(.ext)», далее «имя - Копия (2)(.ext)»… — как Explorer.
fn free_copy_name(target_lp: &Path, name: &std::ffi::OsStr, is_dir: bool) -> PathBuf {
    let s = name.to_string_lossy();
    let s = s.as_ref();
    let (stem, ext) = if is_dir {
        (s, "")
    } else {
        match s.rfind('.') {
            Some(i) if i > 0 => (&s[..i], &s[i..]),
            _ => (s, ""),
        }
    };
    let mut n = 1u64;
    loop {
        let cand = if n == 1 {
            format!("{} - Копия{}", stem, ext)
        } else {
            format!("{} - Копия ({}){}", stem, n, ext)
        };
        let p = target_lp.join(&cand);
        if fs::symlink_metadata(&p).is_err() {
            return p;
        }
        n += 1;
    }
}

fn walk_sources(
    sources: &[PathBuf],
    target_lp: &Path,
    target_orig: &Path,
    tops: &mut Vec<PathBuf>,
    dirs: &mut Vec<DirItem>,
    files: &mut Vec<FileItem>,
    fails: &mut Vec<Fail>,
) {
    for src in sources {
        let src_lp = lp(src);
        let Some(name) = src.file_name().map(|n| n.to_os_string()) else {
            continue;
        };
        let md = match fs::symlink_metadata(&src_lp) {
            Ok(m) => m,
            Err(e) => {
                fails.push(Fail::new(disp(src), &e));
                continue;
            }
        };
        // вставка в ту же папку, где лежит источник — как Explorer, «имя - Копия»
        let base = if in_target_folder(src, target_orig) {
            let b = free_copy_name(target_lp, &name, md.is_dir());
            logln(&format!(
                "та же папка: {} → {}",
                src.display(),
                disp(&b)
            ));
            b
        } else {
            target_lp.join(&name)
        };
        tops.push(base.clone());
        if md.file_type().is_symlink() {
            files.push(FileItem::link(src_lp.clone(), base, disp(src)));
            continue;
        }
        if md.is_dir() {
            for entry in jwalk::WalkDir::new(&src_lp).skip_hidden(false) {
                match entry {
                    Err(e) => fails.push(Fail::new(disp(src), &e)),
                    Ok(e) => {
                        let p = e.path();
                        let Ok(rel) = p.strip_prefix(&src_lp) else {
                            continue;
                        };
                        if rel.as_os_str().is_empty() {
                            dirs.push(DirItem {
                                src: p.to_path_buf(),
                                dst: base.clone(),
                                attrs: md.file_attributes() & ATTR_MASK,
                                times: Times::from_md(&md),
                            });
                            continue;
                        }
                        let dst = base.join(rel);
                        let md2 = match fs::symlink_metadata(&p) {
                            Ok(m) => m,
                            Err(er) => {
                                fails.push(Fail::new(disp(&p), &er));
                                continue;
                            }
                        };
                        if md2.file_type().is_symlink() {
                            files.push(FileItem::link(p.clone(), dst, disp(&p)));
                        } else if md2.is_dir() {
                            dirs.push(DirItem {
                                src: p.clone(),
                                dst,
                                attrs: md2.file_attributes() & ATTR_MASK,
                                times: Times::from_md(&md2),
                            });
                        } else {
                            files.push(FileItem::file(p.clone(), dst, disp(&p), &md2));
                        }
                    }
                }
            }
        } else {
            files.push(FileItem::file(src_lp.clone(), base, disp(src), &md));
        }
    }
}

impl FileItem {
    fn file(src: PathBuf, dst: PathBuf, src_disp: String, md: &fs::Metadata) -> FileItem {
        FileItem {
            src,
            dst,
            src_disp,
            size: md.len(),
            is_link: false,
            attrs: md.file_attributes() & ATTR_MASK,
            times: Times::from_md(md),
        }
    }

    fn link(src: PathBuf, dst: PathBuf, src_disp: String) -> FileItem {
        FileItem {
            src,
            dst,
            src_disp,
            size: 0,
            is_link: true,
            attrs: 0,
            times: Times {
                created: None,
                modified: None,
                accessed: None,
            },
        }
    }
}

// ---------- копирование ----------

fn copy_file(f: &FileItem, buf: &mut [u8], token: &CancellationToken) -> io::Result<()> {
    // снять read-only с существующего назначения, иначе File::create не откроет
    let _ = unsafe {
        SetFileAttributesW(
            &HSTRING::from(f.dst.as_os_str()),
            FILE_ATTRIBUTE_NORMAL,
        )
    };
    let s = File::open(&f.src)?;
    let d = File::create(&f.dst)?;
    // предвыделение имеет смысл только когда файл не влезает в один буфер
    if f.size > BUF as u64 {
        d.set_len(f.size)?;
    }

    if f.size >= BIG_THRESHOLD {
        let chunks = if f.size >= HUGE_THRESHOLD {
            HUGE_CHUNKS
        } else {
            BIG_CHUNKS
        };
        std::thread::scope(|sc| -> io::Result<()> {
            let (s, d) = (&s, &d);
            let mut handles = Vec::new();
            for k in 0..chunks {
                handles.push(sc.spawn(move || {
                    let per = f.size.div_ceil(chunks);
                    let start = (k * per).min(f.size);
                    let end = ((k + 1) * per).min(f.size);
                    let mut own = vec![0u8; BUF];
                    copy_range(s, d, start, end, token, &mut own)
                }));
            }
            for h in handles {
                h.join().unwrap()?;
            }
            Ok(())
        })?;
    } else {
        copy_range(&s, &d, 0, f.size, token, buf)?;
    }

    apply_file_meta(f, &d)?;
    drop(d);
    if f.attrs != 0 {
        unsafe {
            SetFileAttributesW(
                &HSTRING::from(f.dst.as_os_str()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(f.attrs),
            )
        }?;
    }
    Ok(())
}

fn copy_range(
    s: &File,
    d: &File,
    start: u64,
    end: u64,
    token: &CancellationToken,
    buf: &mut [u8],
) -> io::Result<u64> {
    if start >= end {
        return Ok(0);
    }
    let mut off = start;
    let mut done = 0u64;
    while off < end {
        // пауза из окна прогресса: передача реально останавливается (как Explorer)
        while crate::COPY_PAUSED.load(Ordering::Relaxed) && !token.is_cancelled() {
            std::thread::sleep(Duration::from_millis(50));
        }
        if token.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "отменено"));
        }
        let want = ((end - off) as usize).min(buf.len());
        let n = s.seek_read(&mut buf[..want], off)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "файл стал короче во время копирования",
            ));
        }
        let mut w = 0usize;
        while w < n {
            let m = d.seek_write(&buf[w..n], off + w as u64)?;
            w += m;
        }
        off += n as u64;
        done += n as u64;
        BYTES_DONE.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(done)
}

fn apply_file_meta(f: &FileItem, d: &File) -> io::Result<()> {
    unsafe {
        let h = HANDLE(d.as_raw_handle());
        let c = f.times.created.map(st_to_ft);
        let a = f.times.accessed.map(st_to_ft);
        let m = f.times.modified.map(st_to_ft);
        SetFileTime(h, pft(&c), pft(&a), pft(&m))
            .map_err(|e| io::Error::other(e.to_string()))?;
    }
    Ok(())
}

fn apply_dir_meta(d: &DirItem) {
    unsafe {
        if let Ok(h) = CreateFileW(
            &HSTRING::from(d.dst.as_os_str()),
            (FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            None,
        ) {
            let c = d.times.created.map(st_to_ft);
            let a = d.times.accessed.map(st_to_ft);
            let m = d.times.modified.map(st_to_ft);
            let _ = SetFileTime(h, pft(&c), pft(&a), pft(&m));
            let _ = CloseHandle(h);
        }
    }
    // атрибуты папок — без read-only
    let a = d.attrs & !0x1;
    if a != 0 {
        let _ = unsafe {
            SetFileAttributesW(
                &HSTRING::from(d.dst.as_os_str()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(a),
            )
        };
    }
}

fn recreate_link(f: &FileItem) -> io::Result<()> {
    let target = fs::read_link(&f.src)?;
    let target_is_dir = fs::metadata(&target).map(|m| m.is_dir()).unwrap_or(false);
    if fs::symlink_metadata(&f.dst).is_ok() {
        fs::remove_file(&f.dst)?;
    }
    if target_is_dir {
        symlink_dir(&target, &f.dst)
    } else {
        symlink_file(&target, &f.dst)
    }
}

// ---------- сверка ----------

/// Побайтовое сравнение двух файлов. Возвращает число сверённых байт.
pub fn verify_pair(src: &Path, dst: &Path) -> Result<u64, String> {
    let mut ba = vec![0u8; VBUF];
    let mut bb = vec![0u8; VBUF];
    verify_with(src, dst, &mut ba, &mut bb)
}

fn verify_with(src: &Path, dst: &Path, ba: &mut [u8], bb: &mut [u8]) -> Result<u64, String> {
    // символические ссылки — сверяем цель ссылки
    if fs::symlink_metadata(src)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return match (fs::read_link(src), fs::read_link(dst)) {
            (Ok(a), Ok(b)) if a == b => Ok(0),
            _ => Err("цель ссылки не совпадает".into()),
        };
    }
    let mut a = File::open(src).map_err(|e| format!("источник: {}", e))?;
    let mut b = File::open(dst).map_err(|e| format!("копия: {}", e))?;
    let (sa, sb) = (
        a.metadata().map_err(|e| e.to_string())?,
        b.metadata().map_err(|e| e.to_string())?,
    );
    if sa.len() != sb.len() {
        return Err(format!("размер: {} != {}", sa.len(), sb.len()));
    }
    let mut checked = 0u64;
    loop {
        let na = a.read(ba).map_err(|e| e.to_string())?;
        let nb = b.read(bb).map_err(|e| e.to_string())?;
        if na != nb {
            return Err("длина данных различается".into());
        }
        if na == 0 {
            return Ok(checked);
        }
        if ba[..na] != bb[..na] {
            let pos = ba[..na]
                .iter()
                .zip(bb[..na].iter())
                .position(|(x, y)| x != y)
                .unwrap_or(0);
            return Err(format!(
                "содержимое отличается (смещение {})",
                checked + pos as u64
            ));
        }
        checked += na as u64;
    }
}

// ---------- утилиты ----------

/// Обычный путь -> \\?\-путь (длинные пути, только backslash).
pub fn lp(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('/', "\\");
    if s.starts_with("\\\\?\\") {
        return PathBuf::from(s);
    }
    if let Some(rest) = s.strip_prefix("\\\\") {
        return PathBuf::from(format!("\\\\?\\UNC\\{}", rest));
    }
    PathBuf::from(format!("\\\\?\\{}", s))
}

/// \\?\-путь -> человекочитаемый.
pub fn disp(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Some(r) = s.strip_prefix("\\\\?\\UNC\\") {
        format!("\\\\{}", r)
    } else if let Some(r) = s.strip_prefix("\\\\?\\") {
        r.to_string()
    } else {
        s.into_owned()
    }
}

fn soft_stats(root: &Path) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    for entry in jwalk::WalkDir::new(root).skip_hidden(false) {
        if let Ok(e) = entry {
            if let Ok(md) = fs::symlink_metadata(e.path()) {
                if !md.is_dir() {
                    files += 1;
                    bytes += md.len();
                }
            }
        }
    }
    (files, bytes)
}

fn st_to_ft(t: SystemTime) -> FILETIME {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let v = d.as_nanos() as u64 / 100 + 116_444_736_000_000_000;
    FILETIME {
        dwLowDateTime: v as u32,
        dwHighDateTime: (v >> 32) as u32,
    }
}

fn pft(o: &Option<FILETIME>) -> Option<*const FILETIME> {
    o.as_ref().map(|x| x as *const FILETIME)
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["Б", "КБ", "МБ", "ГБ", "ТБ", "ПБ"];
    let mut x = bytes as f64;
    let mut i = 0;
    while x >= 1024.0 && i < 5 {
        x /= 1024.0;
        i += 1;
    }
    let num = if i == 0 {
        format!("{}", bytes)
    } else {
        format!("{:.1}", x).replace('.', ",")
    };
    format!("{} {}", num, UNITS[i])
}

pub fn plural<'a>(n: u64, forms: (&'a str, &'a str, &'a str)) -> &'a str {
    let n10 = n % 10;
    let n100 = n % 100;
    if n10 == 1 && n100 != 11 {
        forms.0
    } else if (2..=4).contains(&n10) && !(12..=14).contains(&n100) {
        forms.1
    } else {
        forms.2
    }
}

// ---------- самотест ----------

pub fn selftest() {
    unsafe {
        let _ = windows::Win32::System::Console::AllocConsole();
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
    }
    println!("=== rcp selftest: копирование + побайтовая сверка ===");

    let base = std::env::temp_dir().join("rcp_selftest");
    let src = base.join("источник с пробелом");
    let dst = base.join("приёмник");
    let _ = fs::remove_dir_all(&dst);
    let _ = fs::remove_dir_all(&src);
    fs::create_dir_all(src.join("папка а").join("папка б")).expect("mkdir");

    let mut rnd = 0x2545_F491_4F6C_DD1Du64;
    let wr = |p: &Path, n: usize, rnd: &mut u64| {
        let mut f = File::create(p).unwrap();
        let mut buf = vec![0u8; 1 << 20];
        let mut written = 0usize;
        while written < n {
            for chunk in buf.chunks_mut(8) {
                *rnd = rnd
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                chunk.copy_from_slice(&rnd.to_le_bytes());
            }
            let m = (1 << 20).min(n - written);
            f.write_all(&buf[..m]).unwrap();
            written += m;
        }
        f.sync_all().unwrap();
    };
    wr(&src.join("маленький файл.txt"), 100_000, &mut rnd);
    wr(&src.join("средний.bin"), 30 << 20, &mut rnd);
    wr(&src.join("папка а").join("папка б").join("большой.bin"), 300 << 20, &mut rnd);
    fs::write(src.join("папка а").join("крошка 🌞.dat"), b"x").unwrap();

    let job = Job {
        sources: vec![src.clone()],
        target: dst.clone(),
        is_move: false,
    };
    let token = CancellationToken::new();
    let t0 = Instant::now();
    run(job, token);
    println!("движок отработал за {:.1} с (включая сверку)", t0.elapsed().as_secs_f64());

    // тест вставки в ту же папку → «имя - Копия.ext», как Explorer
    let job2 = Job {
        sources: vec![src.join("маленький файл.txt")],
        target: src.clone(),
        is_move: false,
    };
    run(job2, CancellationToken::new());
    let copy_name = src.join("маленький файл - Копия.txt");
    match fs::metadata(&copy_name) {
        Ok(m) if m.len() == 100_000 => {
            println!("тест «- Копия» в той же папке: OK ({})", copy_name.display())
        }
        other => println!("тест «- Копия» в той же папке: ПРОВАЛ ({:?})", other),
    }
    // повторная вставка в ту же папку → «- Копия (2)»
    let job3 = Job {
        sources: vec![src.join("маленький файл.txt")],
        target: src.clone(),
        is_move: false,
    };
    run(job3, CancellationToken::new());
    match fs::metadata(src.join("маленький файл - Копия (2).txt")) {
        Ok(m) if m.len() == 100_000 => println!("тест «- Копия (2)»: OK"),
        other => println!("тест «- Копия (2)»: ПРОВАЛ ({:?})", other),
    }

    // негативный тест сверки: портим байт — детектор обязан сработать
    let small_copy = dst.join("источник с пробелом").join("маленький файл.txt");
    if let Ok(f) = File::options().write(true).open(&small_copy) {
        let _ = f.seek_write(&[0xAB], 5);
    }
    match verify_pair(&src.join("маленький файл.txt"), &small_copy) {
        Err(msg) => println!("негативный тест сверки: OK ({})", msg),
        Ok(_) => println!("негативный тест сверки: ПРОВАЛ — порча не обнаружена!"),
    }

    let _ = fs::remove_dir_all(&base);
    println!("=== selftest завершён ===");
    std::thread::sleep(Duration::from_secs(4));
}
