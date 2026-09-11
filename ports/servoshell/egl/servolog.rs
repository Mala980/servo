/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! On-device diagnostics for the Android build: a `log` implementation that
//! appends every record to a file in the app's own external storage, plus an
//! `x-servolog:` protocol that renders the tail of that file as a page. This
//! makes the log readable on the device itself - through the file manager or
//! by typing `x-servolog:tail` in the URL bar - without needing adb.

use std::future::Future;
use std::io::Write;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use headers::{ContentType, HeaderMapExt};
use log::{Level, LevelFilter, Metadata, Record};
use servo::protocol_handler::{
    DoneChannel, FetchContext, ProtocolHandler, Request, ResourceFetchTiming, Response, ResponseBody,
};

/// Where the log file lives; set by the platform glue before logging starts.
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static START: OnceLock<Instant> = OnceLock::new();

/// The log file is capped so that a chatty page cannot fill the device
/// storage; when the cap is hit the file restarts.
const MAX_LOG_FILE_BYTES: u64 = 8 * 1024 * 1024;
/// How many trailing bytes of the log the `x-servolog:` page serves.
const TAIL_BYTES: usize = 160 * 1024;

pub(crate) fn log_file_path() -> Option<&'static PathBuf> {
    LOG_PATH.get()
}

/// Install the file logger. `module_filters` holds `(target prefix, max
/// level)` pairs with the same semantics the logcat filter had. Failing to
/// install (or to open the file) is not fatal; logging simply stays quiet.
pub(crate) fn init_file_logger(module_filters: Vec<(String, LevelFilter)>, path: PathBuf) {
    let _ = START.set(Instant::now());
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .ok();
    let logger = FileLogger {
        module_filters,
        path: path.clone(),
        state: Mutex::new(LoggerState { file, size: 0 }),
    };
    if log::set_boxed_logger(Box::new(logger)).is_ok() {
        log::set_max_level(LevelFilter::Debug);
    }
    let _ = LOG_PATH.set(path);
}

struct LoggerState {
    file: Option<std::fs::File>,
    size: u64,
}

struct FileLogger {
    module_filters: Vec<(String, LevelFilter)>,
    path: PathBuf,
    state: Mutex<LoggerState>,
}

impl FileLogger {
    fn allowed(&self, target: &str, level: Level) -> bool {
        self.module_filters
            .iter()
            .any(|(prefix, max)| target.starts_with(prefix) && level <= *max)
    }
}

impl log::Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.allowed(metadata.target(), metadata.level())
    }

    fn log(&self, record: &Record) {
        if !self.allowed(record.target(), record.level()) {
            return;
        }
        let elapsed = START.get().map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
        let line = format!(
            "[{:9.3}] {:5} [{}] {}\n",
            elapsed,
            record.level().as_str(),
            record.target(),
            record.args()
        );
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return,
        };
        if state.size > MAX_LOG_FILE_BYTES {
            state.file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&self.path)
                .ok();
            state.size = 0;
            if let Some(file) = state.file.as_mut() {
                let _ = writeln!(file, "[{:9.3}] LOG ROTATED", elapsed);
            }
        }
        if let Some(file) = state.file.as_mut() {
            if file.write_all(line.as_bytes()).is_ok() {
                state.size += line.len() as u64;
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(file) = state.file.as_mut() {
                let _ = file.flush();
            }
        }
    }
}

/// Serves the tail of the log file, newest line first, as an auto-refreshing
/// page so that the most recent output is always at the top of the viewport.
pub(crate) struct ServoLogProtocolHandler;

impl ProtocolHandler for ServoLogProtocolHandler {
    fn is_fetchable(&self) -> bool {
        true
    }

    fn load(
        &self,
        request: &mut Request,
        _done_chan: &mut DoneChannel,
        _context: &FetchContext,
    ) -> Pin<Box<dyn Future<Output = Response> + Send>> {
        let body = page_html();
        let mut response = Response::new(
            request.current_url(),
            ResourceFetchTiming::new(request.timing_type()),
        );
        response.headers.typed_insert(ContentType::html());
        *response.body.lock() = ResponseBody::Done(body.into_bytes());
        Box::pin(std::future::ready(response))
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn fallback(message: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>servo log</title></head><body style="background:#111;color:#ddd;font-family:monospace;font-size:14px;margin:12px">{}</body></html>"#,
        escape(message)
    )
}

fn page_html() -> String {
    let Some(path) = LOG_PATH.get() else {
        return fallback("The log file path was not initialized.");
    };
    let Ok(bytes) = std::fs::read(path) else {
        return fallback(&format!("Could not read {}.", path.display()));
    };
    let tail = if bytes.len() > TAIL_BYTES {
        &bytes[bytes.len() - TAIL_BYTES..]
    } else {
        &bytes[..]
    };
    let mut lines: Vec<&str> = String::from_utf8_lossy(tail).lines().collect();
    lines.reverse();
    let mut body = String::with_capacity(tail.len() * 6 / 5);
    for line in lines {
        body.push_str(&escape(line));
        body.push('\n');
    }
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><meta http-equiv="refresh" content="3"><title>servo log</title></head><body style="background:#111;color:#ddd;font-family:monospace;font-size:12px;white-space:pre-wrap;word-break:break-all;margin:8px">{}</body></html>"#,
        body
    )
}
