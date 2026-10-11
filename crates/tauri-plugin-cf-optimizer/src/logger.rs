//! 极简日志器：stderr + 滚动文件，满足「日志可观测」验收项，
//! 不引入额外日志插件依赖。格式：`YYYY-MM-DD HH:MM:SS [LEVEL] target - message`。

use log::{LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

struct SimpleLogger {
    file: Mutex<Option<File>>,
}

impl Log for SimpleLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= LevelFilter::Info
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let now = time::OffsetDateTime::now_utc()
            .format(&time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second]"
            ))
            .unwrap_or_else(|_| "?".to_string());
        let line = format!(
            "{now} [{}] {} - {}\n",
            record.level(),
            record.target(),
            record.args()
        );
        // stderr：开发期 `tauri dev` 直接可见
        let _ = write!(std::io::stderr(), "{}", line);
        // 文件：打包后可从 app_log_dir 取证
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.write_all(line.as_bytes());
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut guard) = self.file.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = f.flush();
            }
        }
    }
}

static LOGGER: SimpleLogger = SimpleLogger {
    file: Mutex::new(None),
};

/// 初始化全局日志。`log_file` 为 None 时仅输出 stderr。重复调用安全（仅首次生效）。
pub fn init(log_file: Option<&Path>) {
    if let Some(p) = log_file {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(f) = OpenOptions::new().create(true).append(true).open(p) {
            if let Ok(mut guard) = LOGGER.file.lock() {
                *guard = Some(f);
            }
        }
    }
    // set_logger 只有第一次成功；后续调用返回 Err，忽略即可
    let _ = log::set_logger(&LOGGER).map(|()| log::set_max_level(LevelFilter::Info));
}
