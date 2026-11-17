use std::{
    any::Any,
    fs::OpenOptions,
    io::Write,
    panic::PanicHookInfo,
    path::{Path, PathBuf},
    sync::Once,
    time::{SystemTime, UNIX_EPOCH},
};

static INSTALL: Once = Once::new();

/// Records panics before the previous hook runs. Release builds use
/// `panic = "abort"`, so a panic inside an OLE callback would otherwise
/// terminate the host application without any trace.
pub fn install() {
    INSTALL.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            record(info);
            previous(info);
        }));
    });
}

fn record(info: &PanicHookInfo<'_>) {
    let unix_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let thread = std::thread::current();
    let location = info.location().map(|l| (l.file(), l.line(), l.column()));
    let line = format_line(
        unix_secs,
        thread.name().unwrap_or("<unnamed>"),
        location,
        payload_message(info.payload()),
    );
    append_line(&log_path(), &line);
    #[cfg(target_os = "windows")]
    output_debug_string(&line);
}

fn log_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap_or_default();
    log_path_for(&exe, &std::env::temp_dir())
}

fn log_path_for(exe: &Path, dir: &Path) -> PathBuf {
    let stem = exe.file_stem().and_then(|s| s.to_str()).unwrap_or("app");
    dir.join(format!("{stem}-native-panic.log"))
}

fn payload_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.as_str()
    } else {
        "<non-string payload>"
    }
}

fn format_line(
    unix_secs: u64,
    thread: &str,
    location: Option<(&str, u32, u32)>,
    message: &str,
) -> String {
    let location = match location {
        Some((file, line, column)) => format!("{file}:{line}:{column}"),
        None => "<unknown location>".to_owned(),
    };
    // A panic must stay one log line whatever its message contains.
    format!("unix={unix_secs} thread='{thread}' location={location} message={message}")
        .replace(['\n', '\r'], " ")
}

fn append_line(path: &Path, line: &str) {
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{line}");
    }
}

#[cfg(target_os = "windows")]
fn output_debug_string(line: &str) {
    use windows::{core::PCWSTR, Win32::System::Diagnostics::Debug::OutputDebugStringW};

    let wide: Vec<u16> = format!("{line}\n")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe { OutputDebugStringW(PCWSTR(wide.as_ptr())) };
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{append_line, format_line, install, log_path, log_path_for};

    #[test]
    fn log_file_name_is_derived_from_exe_stem() {
        for exe in ["/opt/pcc/pcc.exe", "/opt/pcc/pcc"] {
            let path = log_path_for(Path::new(exe), Path::new("/tmp"));
            assert!(path.ends_with("pcc-native-panic.log"), "{path:?}");
        }
    }

    #[test]
    fn panic_line_is_single_line_with_location_and_message() {
        let line = format_line(42, "main", Some(("src/x.rs", 10, 5)), "first\nsecond");
        assert!(!line.contains('\n'));
        assert!(line.contains("src/x.rs:10:5"));
        assert!(line.contains("first second"));
    }

    #[test]
    fn append_line_writes_each_line_terminated_in_order() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "pcc-panic-log-test-{}-{nanos}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        append_line(&path, "first");
        append_line(&path, "second");
        let content = std::fs::read_to_string(&path);
        let _ = std::fs::remove_file(&path);
        assert_eq!(content.unwrap(), "first\nsecond\n");
    }

    #[test]
    fn installed_hook_records_panic_and_calls_previous_hook() {
        static PREVIOUS_CALLS: AtomicUsize = AtomicUsize::new(0);
        // Replaces the default hook, so echo panics to keep assertion failures readable.
        std::panic::set_hook(Box::new(|info| {
            PREVIOUS_CALLS.fetch_add(1, Ordering::SeqCst);
            eprintln!("{info}");
        }));
        install();

        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let marker = format!("panic-log-test-{}-{nanos}", std::process::id());
        let result = std::panic::catch_unwind(|| panic!("{marker}"));

        let path = log_path();
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
        assert!(PREVIOUS_CALLS.load(Ordering::SeqCst) >= 1);
        assert!(
            content.lines().any(|line| line.contains(&marker)),
            "no panic line with {marker} in {path:?}"
        );
    }
}
