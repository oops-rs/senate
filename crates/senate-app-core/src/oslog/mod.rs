//! The core's `tracing` events in the macOS unified log, under the app's
//! subsystem (`dev.senate.app`, category `core`), beside the Swift side's
//! own `os.Logger` lines: one place to read both (Console.app, `log
//! stream`). Levels map to log types: error is an error, warn the default
//! type, info info, debug and trace debug.

use std::ffi::{CString, c_char, c_void};
use std::fmt::Write as _;
use std::sync::OnceLock;

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

unsafe extern "C" {
    fn senate_os_log_create(subsystem: *const c_char, category: *const c_char) -> *mut c_void;
    fn senate_os_log(log: *mut c_void, kind: u8, message: *const c_char);
}

/// The app's subsystem, the one the Swift side logs under too.
pub const SUBSYSTEM: &str = "dev.senate.app";

/// An `os_log_t`, made once and kept for the process's life.
struct Handle(*mut c_void);
// SAFETY: an os_log_t is an immutable, thread-safe object.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

fn handle() -> &'static Handle {
    static HANDLE: OnceLock<Handle> = OnceLock::new();
    HANDLE.get_or_init(|| {
        let subsystem = CString::new(SUBSYSTEM).unwrap_or_default();
        let category = CString::new("core").unwrap_or_default();
        // SAFETY: both are valid NUL-terminated strings; os_log_create
        // copies what it needs.
        Handle(unsafe { senate_os_log_create(subsystem.as_ptr(), category.as_ptr()) })
    })
}

/// `os_log_type_t` for a `tracing` level.
fn log_type(level: &Level) -> u8 {
    match *level {
        Level::ERROR => 0x10, // OS_LOG_TYPE_ERROR
        Level::WARN => 0x00,  // OS_LOG_TYPE_DEFAULT
        Level::INFO => 0x01,  // OS_LOG_TYPE_INFO
        _ => 0x02,            // OS_LOG_TYPE_DEBUG
    }
}

/// The layer: each event as one line, `target: message key=value …`.
pub struct OsLogLayer;

impl<S: Subscriber> Layer<S> for OsLogLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let line = line(event);
        // A NUL inside would cut the line; replace it rather than drop it.
        let Ok(text) = CString::new(line.replace('\0', "\u{fffd}")) else {
            return;
        };
        // SAFETY: the handle is a live os_log_t; text is NUL-terminated
        // and outlives the call.
        unsafe {
            senate_os_log(
                handle().0,
                log_type(event.metadata().level()),
                text.as_ptr(),
            )
        };
    }
}

fn line(event: &Event<'_>) -> String {
    let mut visitor = Line::default();
    event.record(&mut visitor);
    let mut line = format!("{}: {}", event.metadata().target(), visitor.message);
    line.push_str(&visitor.fields);
    line
}

#[derive(Default)]
struct Line {
    message: String,
    fields: String,
}

impl Visit for Line {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            let _ = write!(self.fields, " {}={value}", field.name());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            let _ = write!(self.fields, " {}={value:?}", field.name());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn events_go_to_the_unified_log_without_panicking() {
        // What reaches the log is checked by hand (`log stream`); here, that
        // the layer formats and calls through for every level and odd text.
        let subscriber = tracing_subscriber::registry().with(OsLogLayer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::error!(code = 7, "an error with a field");
            tracing::warn!("a warning");
            tracing::info!(path = ?"/tmp/x", "info: 中文 and a \0 NUL");
            tracing::debug!("debug");
        });
        assert_eq!(log_type(&Level::ERROR), 0x10);
        assert_eq!(log_type(&Level::TRACE), 0x02);
    }
}
