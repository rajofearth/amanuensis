use std::{
    fs::OpenOptions,
    io::Write,
    sync::{Mutex, OnceLock},
    time::Instant,
};

static START: OnceLock<Instant> = OnceLock::new();
static FILE: OnceLock<Mutex<std::fs::File>> = OnceLock::new();

pub fn init() {
    let _ = START.set(Instant::now());
    let path = std::env::current_dir()
        .unwrap_or_default()
        .join("dictation.log");
    if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = FILE.set(Mutex::new(file));
        log(
            "log",
            format!("session start, logging to {}", path.display()),
        );
    } else {
        eprintln!("[log] failed to open dictation.log; stderr only");
    }
}

pub fn log(tag: &str, message: impl AsRef<str>) {
    let elapsed = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    let line = format!("[{elapsed:10.3}s] [{tag}] {}", message.as_ref());
    eprintln!("{line}");
    if let Some(file) = FILE.get() {
        if let Ok(mut file) = file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

#[macro_export]
macro_rules! log {
    ($tag:expr, $($arg:tt)*) => {
        $crate::logging::log($tag, format!($($arg)*))
    };
}
