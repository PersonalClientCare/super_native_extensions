use std::time::{Duration, Instant};

use irondash_run_loop::RunLoop;
use windows::{
    core::{HSTRING, PCWSTR},
    Win32::{
        Foundation::HWND,
        System::Threading::GetCurrentThreadId,
        UI::WindowsAndMessaging::{
            DispatchMessageW, FindWindowExW, GetWindowThreadProcessId, MsgWaitForMultipleObjectsEx,
            PeekMessageW, TranslateMessage, HWND_MESSAGE, MSG,
            MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS, PM_NOREMOVE, PM_NOYIELD, PM_REMOVE,
            QS_POSTMESSAGE, QS_SENDMESSAGE, QS_TIMER, WM_QUIT,
        },
    },
};

// Private names: irondash_run_loop 0.6 adapter.rs and Flutter's task_runner_window.cc.
const IRONDASH_WINDOW_CLASS: &str = "IrondashCoreMessageWindow";
const FLUTTER_WINDOW_CLASS: &str = "FlutterTaskRunnerWindow";

/// Same bound irondash's poll_once waits; only caps how long done() can go unchecked.
const MAX_WAIT_MS: u32 = 7;

/// irondash's poll_once reaches Flutter's windows only after this (its timed_out gate).
const FLUTTER_WINDOWS_AFTER: Duration = Duration::from_millis(6);

/// Pumps only irondash's run-loop window and Flutter's task runner windows on this
/// thread; posted messages of other windows (e.g. OLE's drag loop that may be
/// calling us) stay queued, while sent messages are serviced by any peek, as in
/// irondash. Checks `done` after every message. `done` is called repeatedly and
/// may consume a value. Returns early when WM_QUIT is queued, leaving it for the
/// outer loop.
pub fn pump_until(mut done: impl FnMut() -> bool) {
    // Creates this thread's irondash window, so it is found below.
    RunLoop::current();
    let start = Instant::now();
    let mut windows = thread_windows(IRONDASH_WINDOW_CLASS);
    let mut flutter_pending = true;
    let mut next = 0;
    while !done() {
        if quit_queued() {
            return;
        }
        if flutter_pending && start.elapsed() >= FLUTTER_WINDOWS_AFTER {
            windows.extend(thread_windows(FLUTTER_WINDOW_CLASS));
            flutter_pending = false;
        }
        if !dispatch_next(&windows, &mut next) {
            unsafe {
                // Unpumped windows' queued messages would wake this wait at once and spin.
                MsgWaitForMultipleObjectsEx(
                    None,
                    MAX_WAIT_MS,
                    QS_POSTMESSAGE | QS_TIMER | QS_SENDMESSAGE,
                    MSG_WAIT_FOR_MULTIPLE_OBJECTS_EX_FLAGS(0),
                );
            }
        }
    }
}

fn thread_windows(class_name: &str) -> Vec<HWND> {
    let thread_id = unsafe { GetCurrentThreadId() };
    let class_name = HSTRING::from(class_name);
    let mut windows = Vec::new();
    let mut after = HWND(0);
    loop {
        let hwnd = unsafe {
            FindWindowExW(
                HWND_MESSAGE,
                after,
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
            )
        };
        if hwnd.0 == 0 {
            break;
        }
        if unsafe { GetWindowThreadProcessId(hwnd, None) } == thread_id {
            windows.push(hwnd);
        }
        after = hwnd;
    }
    windows
}

fn quit_queued() -> bool {
    let mut msg = MSG::default();
    unsafe { PeekMessageW(&mut msg, HWND(0), WM_QUIT, WM_QUIT, PM_NOREMOVE) }.as_bool()
}

/// Dispatches the next message of `windows`, scanning from `next` so that a busy
/// window cannot starve the others.
fn dispatch_next(windows: &[HWND], next: &mut usize) -> bool {
    for offset in 0..windows.len() {
        let index = (*next + offset) % windows.len();
        let mut msg = MSG::default();
        if unsafe { PeekMessageW(&mut msg, windows[index], 0, 0, PM_REMOVE | PM_NOYIELD) }.as_bool()
        {
            *next = (index + 1) % windows.len();
            unsafe {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            return true;
        }
    }
    false
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
        time::{Duration, Instant},
    };

    use irondash_run_loop::{RunLoop, RunLoopSender};
    use windows::{
        core::{HSTRING, PCWSTR},
        Win32::{
            Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
            System::LibraryLoader::GetModuleHandleW,
            UI::WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowLongPtrW, PeekMessageW,
                PostMessageW, PostQuitMessage, RegisterClassExW, SetWindowLongPtrW, GWLP_USERDATA,
                HWND_MESSAGE, MSG, PM_REMOVE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_QUIT, WM_USER,
                WNDCLASSEXW,
            },
        },
    };

    use super::{pump_until, FLUTTER_WINDOW_CLASS};

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

    /// Indexed by test window: 0 Flutter dispatch, 1 foreign, 2 spin, 3 Flutter under flood.
    static WM_USER_COUNTS: [AtomicUsize; 4] = [
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
        AtomicUsize::new(0),
    ];

    unsafe extern "system" fn count_wm_user(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_USER {
            let index = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as usize;
            WM_USER_COUNTS[index].fetch_add(1, Ordering::SeqCst);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }

    /// Posts one WM_USER to a new message-only window of `class_name` whose messages
    /// are counted in `WM_USER_COUNTS[index]`. Registering a class that already
    /// exists fails harmlessly, as the wndproc is the same for every class.
    fn post_to_counting_window(class_name: &str, index: usize) -> HWND {
        let name = HSTRING::from(class_name);
        let class = PCWSTR(name.as_ptr());
        unsafe {
            let instance = HINSTANCE(GetModuleHandleW(PCWSTR::null()).unwrap().0);
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
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, index as isize);
            PostMessageW(hwnd, WM_USER, WPARAM(0), LPARAM(0)).unwrap();
            hwnd
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
    fn dispatches_flutter_task_runner_windows() {
        run_on_thread(|| unsafe {
            let _run_loop = RunLoop::current();
            let window = post_to_counting_window(FLUTTER_WINDOW_CLASS, 0);
            pump_until(|| WM_USER_COUNTS[0].load(Ordering::SeqCst) == 1);
            let dispatched = WM_USER_COUNTS[0].load(Ordering::SeqCst) == 1;
            DestroyWindow(window).unwrap();
            dispatched
        });
    }

    #[test]
    fn serves_flutter_windows_while_the_run_loop_window_floods() {
        run_on_thread(|| unsafe {
            let run_loop = RunLoop::current();
            let stop = Arc::new(AtomicBool::new(false));
            flood(
                run_loop.new_sender(),
                stop.clone(),
                Arc::new(AtomicUsize::new(0)),
            );
            let window = post_to_counting_window(FLUTTER_WINDOW_CLASS, 3);
            pump_until(|| WM_USER_COUNTS[3].load(Ordering::SeqCst) == 1);
            stop.store(true, Ordering::SeqCst);
            let served = WM_USER_COUNTS[3].load(Ordering::SeqCst) == 1;
            DestroyWindow(window).unwrap();
            served
        });
    }

    #[test]
    fn leaves_other_windows_alone() {
        run_on_thread(|| unsafe {
            let run_loop = RunLoop::current();
            let foreign = post_to_counting_window("PumpTestForeignWindow", 1);
            let ran = Arc::new(AtomicBool::new(false));
            let ran_clone = ran.clone();
            run_loop
                .new_sender()
                .send(move || ran_clone.store(true, Ordering::SeqCst));
            pump_until(|| ran.load(Ordering::SeqCst));
            let mut msg = MSG::default();
            let left_alone = WM_USER_COUNTS[1].load(Ordering::SeqCst) == 0
                && PeekMessageW(&mut msg, foreign, WM_USER, WM_USER, PM_REMOVE).as_bool();
            DestroyWindow(foreign).unwrap();
            left_alone
        });
    }

    #[test]
    fn waits_without_spinning_while_other_windows_have_messages() {
        run_on_thread(|| {
            let _run_loop = RunLoop::current();
            let window = post_to_counting_window("PumpTestSpinWindow", 2);
            let start = Instant::now();
            let mut calls = 0usize;
            pump_until(|| {
                calls += 1;
                start.elapsed() >= Duration::from_millis(200)
            });
            unsafe { DestroyWindow(window).unwrap() };
            calls < 200
        });
    }

    #[test]
    fn returns_on_quit() {
        run_on_thread(|| unsafe {
            let _run_loop = RunLoop::current();
            PostQuitMessage(7);
            pump_until(|| false);
            let mut msg = MSG::default();
            PeekMessageW(&mut msg, HWND(0), WM_QUIT, WM_QUIT, PM_REMOVE).as_bool()
                && msg.wParam.0 == 7
        });
    }

    #[test]
    fn returns_on_quit_posted_while_dispatching() {
        run_on_thread(|| {
            let run_loop = RunLoop::current();
            let stop = Arc::new(AtomicBool::new(false));
            flood(
                run_loop.new_sender(),
                stop.clone(),
                Arc::new(AtomicUsize::new(0)),
            );
            run_loop.new_sender().send(|| unsafe { PostQuitMessage(3) });
            pump_until(|| false);
            stop.store(true, Ordering::SeqCst);
            let mut msg = MSG::default();
            unsafe { PeekMessageW(&mut msg, HWND(0), WM_QUIT, WM_QUIT, PM_REMOVE) }.as_bool()
                && msg.wParam.0 == 3
        });
    }
}
