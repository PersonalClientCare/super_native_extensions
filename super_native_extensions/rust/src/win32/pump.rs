use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{
        DispatchMessageW, MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage,
        TranslateMessage, MSG, MWMO_INPUTAVAILABLE, PM_NOYIELD, PM_QS_POSTMESSAGE,
        PM_QS_SENDMESSAGE, PM_REMOVE, QS_POSTMESSAGE, QS_SENDMESSAGE, QS_TIMER, WM_QUIT,
    },
};

/// Same bound irondash's poll_once waits; only caps how long done() can go unchecked.
const MAX_WAIT_MS: u32 = 7;

/// Dispatches posted, timer and sent messages of all windows on this thread
/// (including irondash's and Flutter's task runner windows) until `done()`
/// returns true. `done` is checked after every dispatched message, unlike
/// irondash's poll_once, which returns only once its queues are empty and so
/// could hold a drop open while a steady stream of Flutter tasks runs. `done`
/// is called repeatedly and may consume a value. Input stays queued. Sent
/// messages wake the wait early, as they would otherwise wait out the timeout.
/// Returns early on WM_QUIT after re-posting it, so the outer loop still sees it.
pub fn pump_until(mut done: impl FnMut() -> bool) {
    while !done() {
        unsafe {
            MsgWaitForMultipleObjectsEx(
                None,
                MAX_WAIT_MS,
                QS_POSTMESSAGE | QS_TIMER | QS_SENDMESSAGE,
                MWMO_INPUTAVAILABLE,
            );
            let mut msg = MSG::default();
            if PeekMessageW(
                &mut msg,
                HWND(0),
                0,
                0,
                PM_REMOVE | PM_QS_POSTMESSAGE | PM_QS_SENDMESSAGE | PM_NOYIELD,
            )
            .as_bool()
            {
                if msg.message == WM_QUIT {
                    PostQuitMessage(msg.wParam.0 as i32);
                    return;
                }
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        mem::size_of,
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc::{self, RecvTimeoutError},
            Arc,
        },
        time::Duration,
    };

    use irondash_run_loop::{RunLoop, RunLoopSender};
    use windows::{
        core::{w, PCWSTR},
        Win32::{
            Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, PeekMessageW, PostMessageW,
                PostQuitMessage, RegisterClassExW, HWND_MESSAGE, MSG, PM_REMOVE, WINDOW_EX_STYLE,
                WINDOW_STYLE, WM_QUIT, WM_USER, WNDCLASSEXW,
            },
        },
    };

    use super::pump_until;

    const TARGET: usize = 100;

    fn flood(sender: RunLoopSender, stop: Arc<AtomicBool>, count: Arc<AtomicUsize>) {
        count.fetch_add(1, Ordering::SeqCst);
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let next = sender.clone();
        sender.send(move || flood(next, stop, count));
    }

    /// Runs `check` on a fresh thread and fails the test unless it reports true.
    fn run_on_thread(check: impl FnOnce() -> bool + Send + 'static) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(check());
        });
        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(true) => {}
            Ok(false) => panic!("check failed on the test thread"),
            Err(RecvTimeoutError::Timeout) => panic!("pump_until did not return"),
            Err(RecvTimeoutError::Disconnected) => {
                panic!("test thread panicked before reporting")
            }
        }
    }

    #[test]
    fn returns_while_the_queue_never_drains() {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let run_loop = RunLoop::current();
            let count = Arc::new(AtomicUsize::new(0));
            flood(run_loop.new_sender(), thread_stop, count.clone());
            pump_until(|| count.load(Ordering::SeqCst) >= TARGET);
            let _ = tx.send(count.load(Ordering::SeqCst));
        });
        let result = rx.recv_timeout(Duration::from_secs(5));
        stop.store(true, Ordering::SeqCst);
        let seen = match result {
            Ok(seen) => seen,
            Err(RecvTimeoutError::Timeout) => {
                panic!("pump_until did not return while the queue was busy")
            }
            Err(RecvTimeoutError::Disconnected) => {
                panic!("test thread panicked before reporting")
            }
        };
        assert!(
            seen >= TARGET,
            "pump_until returned after only {seen} dispatches"
        );
    }

    #[test]
    fn reposts_quit_and_returns() {
        run_on_thread(|| unsafe {
            // Creates this thread's message queue before the quit is posted.
            let _run_loop = RunLoop::current();
            PostQuitMessage(7);
            pump_until(|| false);
            let mut msg = MSG::default();
            PeekMessageW(&mut msg, HWND(0), WM_QUIT, WM_QUIT, PM_REMOVE).as_bool()
                && msg.wParam.0 == 7
        });
    }

    static WINDOW_MESSAGES: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "system" fn count_wm_user(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_USER {
            WINDOW_MESSAGES.fetch_add(1, Ordering::SeqCst);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    #[test]
    fn dispatches_other_windows_on_the_thread() {
        run_on_thread(|| unsafe {
            let class = w!("PumpTestWindow");
            let instance = HINSTANCE(GetModuleHandleW(PCWSTR(std::ptr::null_mut())).unwrap().0);
            RegisterClassExW(&WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(count_wm_user),
                hInstance: instance,
                lpszClassName: class,
                ..Default::default()
            });
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                class,
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                None,
                instance,
                None,
            );
            PostMessageW(hwnd, WM_USER, WPARAM(0), LPARAM(0)).unwrap();
            pump_until(|| WINDOW_MESSAGES.load(Ordering::SeqCst) == 1);
            DestroyWindow(hwnd).unwrap();
            WINDOW_MESSAGES.load(Ordering::SeqCst) == 1
        });
    }
}
