//! Tiny logging hook: the CLI prints lines to the terminal, the GUI installs a sink
//! so the same messages show up in its log panel.

use std::sync::{Mutex, OnceLock};

type Sink = Box<dyn Fn(String) + Send + Sync>;

static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();

pub fn set_sink(sink: Sink) {
    let cell = SINK.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = Some(sink);
    }
}

pub fn emit(line: String) {
    if let Some(cell) = SINK.get() {
        if let Ok(guard) = cell.lock() {
            if let Some(sink) = guard.as_ref() {
                sink(line);
                return;
            }
        }
    }
    println!("{line}");
}

#[macro_export]
macro_rules! lprintln {
    ($($arg:tt)*) => {
        $crate::log::emit(format!($($arg)*))
    };
}
