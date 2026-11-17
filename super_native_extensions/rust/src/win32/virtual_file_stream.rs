use std::{
    cell::Cell,
    slice,
    sync::{Arc, Mutex},
};

use irondash_run_loop::{util::Capsule, RunLoop, RunLoopSender};
use threadpool::ThreadPool;
use windows::{
    core::{implement, ComInterface},
    Win32::{
        Foundation::{E_FAIL, E_NOTIMPL, STG_E_INVALIDPOINTER, S_FALSE, S_OK},
        System::Com::{
            CoInitialize, CoUninitialize, ISequentialStream_Impl, IStream, IStream_Impl,
            Marshal::CoMarshalInterThreadInterfaceInStream,
            StructuredStorage::CoGetInterfaceAndReleaseStream, LOCKTYPE, STATFLAG, STREAM_SEEK,
            STREAM_SEEK_CUR, STREAM_SEEK_SET,
        },
    },
};

use crate::{
    data_provider_manager::VirtualSessionHandle,
    segmented_queue::SegmentedQueueReader,
    util::{DropNotifier, Movable},
    value_promise::Promise,
};

struct StreamState {
    reader: SegmentedQueueReader,
    size_promise: Arc<Promise<Option<i64>>>,
    error_promise: Arc<Promise<String>>,
    _handle: Arc<VirtualSessionHandle>,
    position: Cell<i64>,
}

struct StreamInner {
    stream: Option<StreamState>,
    factory: Option<Box<dyn FnOnce() -> StreamState + Send>>,
    run_loop_sender: Option<RunLoopSender>,
}

struct Stream {
    inner: Mutex<StreamInner>,
}

impl Stream {
    fn initialize_if_needed(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(factory) = inner.factory.take() {
            inner.stream.replace(factory());
        }
    }

    fn dispose_inactive_stream(&self) {
        let mut inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            // Runs on the UI thread. A held lock means the drop target is inside
            // read/seek/stat, possibly waiting for data or a session that only the UI
            // thread can deliver, so waiting here would deadlock. Such a stream is in
            // use and not inactive, so there is nothing to dispose.
            Err(std::sync::TryLockError::WouldBlock) => return,
            Err(err @ std::sync::TryLockError::Poisoned(_)) => panic!("{err}"),
        };
        inner.factory.take();

        // If client didn't interact with the stream until now shut down the
        // RunLoop thread.
        if inner.stream.is_none() {
            if let Some(sender) = inner.run_loop_sender.take() {
                sender.send(move || {
                    RunLoop::current().stop();
                });
            }
        }
    }

    fn read(
        &self,
        pv: *mut core::ffi::c_void,
        cb: u32,
        pcbread: *mut u32,
    ) -> windows::core::HRESULT {
        if pv.is_null() && cb > 0 {
            return STG_E_INVALIDPOINTER;
        }
        // An empty read must not reach read_some, which would report a finished segment.
        if cb == 0 {
            if !pcbread.is_null() {
                unsafe { *pcbread = 0 };
            }
            return S_OK;
        }
        self.initialize_if_needed();
        let inner = self.inner.lock().unwrap();
        match inner.stream.as_ref() {
            Some(stream) => {
                // This doesn't entirely conform to ISequentialStream::Read documentation.
                // To avoid blocking we may read less data than requested and still
                // return S_OK.
                let data = stream.reader.read_some(cb as usize);
                let data_out = unsafe { slice::from_raw_parts_mut(pv as *mut u8, data.len()) };
                data_out.copy_from_slice(&data);
                if !pcbread.is_null() {
                    unsafe { *pcbread = data.len() as u32 };
                }

                stream
                    .position
                    .set(stream.position.get() + data.len() as i64);

                if let Some(_err) = stream.error_promise.try_clone() {
                    // TODO(knopp): Can we somehow pass the message?
                    return E_FAIL;
                }
                if !data.is_empty() {
                    S_OK
                } else {
                    S_FALSE
                }
            }
            None => S_FALSE,
        }
    }

    fn seek(&self, dlibmove: i64, dworigin: STREAM_SEEK) -> windows::core::Result<u64> {
        self.initialize_if_needed();
        let inner = self.inner.lock().unwrap();
        match inner.stream.as_ref() {
            Some(stream) => {
                let position = if dworigin == STREAM_SEEK_SET {
                    dlibmove
                } else if dworigin == STREAM_SEEK_CUR {
                    stream.position.get() + dlibmove
                } else {
                    -1
                };
                // Pretend that seek is supported as long as we don't really need to seek
                if position == stream.position.get() {
                    Ok(position as u64)
                } else {
                    Err(E_NOTIMPL.into())
                }
            }
            None => Err(E_NOTIMPL.into()),
        }
    }

    fn stat(
        &self,
        pstatstg: *mut windows::Win32::System::Com::STATSTG,
        _grfstatflag: &STATFLAG,
    ) -> windows::core::Result<()> {
        self.initialize_if_needed();
        let size_promise = {
            let inner = self.inner.lock().unwrap();
            inner
                .stream
                .as_ref()
                .map(|inner| inner.size_promise.clone())
        };

        let size = size_promise.and_then(|p| p.wait_clone());
        let statstg = unsafe { &mut *pstatstg };
        statstg.cbSize = size.unwrap_or(0) as u64;

        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let Some(sender) = self.inner.lock().unwrap().run_loop_sender.take() {
            sender.send(move || {
                RunLoop::current().stop();
            });
        }
    }
}

pub struct VirtualStreamSession {
    pub reader: SegmentedQueueReader,
    pub size_promise: Arc<Promise<Option<i64>>>,
    pub error_promise: Arc<Promise<String>>,
    pub handle: Arc<VirtualSessionHandle>,
}

// Represents the session provider as boxed factory method that is Send
fn session_provider_as_factory<F>(provider: F) -> Box<dyn FnOnce() -> StreamState + 'static + Send>
where
    F: FnOnce() -> VirtualStreamSession + 'static,
{
    let sender = RunLoop::current().new_sender();
    let mut provider = Capsule::new_with_sender(provider, sender.clone());
    Box::new(move || {
        let res = Arc::new(Promise::new());
        let res_clone = res.clone();
        sender.send(move || {
            let provider = provider.take().unwrap();
            let session = provider();
            res_clone.set(StreamState {
                reader: session.reader,
                size_promise: session.size_promise,
                error_promise: session.error_promise,
                _handle: session.handle,
                position: Cell::new(0),
            })
        });
        res.wait()
    })
}

#[implement(IStream)]
pub struct VirtualFileStream {
    stream: Arc<Stream>,
}

impl VirtualFileStream {
    /// Creates agile stream that can be accessed on any thread. This should be
    /// used when stream is accessed by local application on background thread.
    pub fn create_agile<F>(session_provider: F) -> (IStream, Arc<DropNotifier>)
    where
        F: FnOnce() -> VirtualStreamSession + 'static,
    {
        let factory = session_provider_as_factory(session_provider);
        let stream = Arc::new(Stream {
            inner: Mutex::new(StreamInner {
                stream: None,
                factory: Some(factory),
                run_loop_sender: None,
            }),
        });
        let stream: IStream = VirtualFileStream { stream }.into();
        (
            stream,
            Arc::new(DropNotifier::new(move || {
                // We don't let the stream leak, no need for manual dispose here.
            })),
        )
    }

    /// Moves the stream to another thread and return marshalled proxy.
    /// This should be used when providing stream to other applications.
    pub fn create_marshalled_on_background_thread<F>(
        session_provider: F,
        thread_pool: &mut ThreadPool,
    ) -> (IStream, Arc<DropNotifier>)
    where
        F: FnOnce() -> VirtualStreamSession + 'static,
    {
        let factory = session_provider_as_factory(session_provider);
        let promise = Arc::new(Promise::<(Movable<IStream>, Box<dyn FnOnce() + Send>)>::new());
        let promise_clone = promise.clone();

        // VSCode seems to periodically request the stream just to close it immediately.
        // To alleviate this a bit we try to reuse the thread. However we also want to
        // ensure that we don't wait on thread_pool because existing running tasks might be
        // blocking.
        if thread_pool.active_count() == thread_pool.max_count() {
            thread_pool.set_num_threads(thread_pool.max_count() + 1);
        }
        thread_pool.execute(move || unsafe {
            CoInitialize(None).ok();
            {
                let stream = Arc::new(Stream {
                    inner: Mutex::new(StreamInner {
                        stream: None,
                        factory: Some(factory),
                        run_loop_sender: Some(RunLoop::current().new_sender()),
                    }),
                });
                let weak_stream = Arc::downgrade(&stream);
                let stream: IStream = VirtualFileStream { stream }.into();
                let mashalled =
                    CoMarshalInterThreadInterfaceInStream(&IStream::IID, &stream).unwrap();
                // Ensure stream disposal when parent dataobject is disposed. This is
                // to ensure that when stream leaks it is at least destroyed (and thread
                // released)  when data object is destroyed.
                // https://github.com/microsoft/terminal/issues/13498
                let clean_up = Box::new(move || {
                    if let Some(stream) = weak_stream.upgrade() {
                        stream.dispose_inactive_stream();
                    }
                });
                promise_clone.set((Movable::new(mashalled), clean_up));
            }
            RunLoop::current().run(); // will be stopped in drop
            CoUninitialize();
        });

        let res = promise.wait();
        let stream = res.0.take();
        let stream: IStream = unsafe { CoGetInterfaceAndReleaseStream(&stream) }.unwrap();
        let drop_notifier = Arc::new(DropNotifier::new_with_boxed(res.1));
        (stream, drop_notifier)
    }
}

#[allow(non_snake_case)]
impl ISequentialStream_Impl for VirtualFileStream {
    fn Read(
        &self,
        pv: *mut core::ffi::c_void,
        cb: u32,
        pcbread: *mut u32,
    ) -> windows::core::HRESULT {
        self.stream.read(pv, cb, pcbread)
    }

    fn Write(
        &self,
        _pv: *const core::ffi::c_void,
        _cb: u32,
        _pcbwritten: *mut u32,
    ) -> windows::core::HRESULT {
        E_NOTIMPL
    }
}

#[allow(non_snake_case)]
impl IStream_Impl for VirtualFileStream {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        let position = self.stream.seek(dlibmove, dworigin)?;
        if !plibnewposition.is_null() {
            let new_position = &mut unsafe { *plibnewposition };
            *new_position = position;
        }
        Ok(())
    }

    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn CopyTo(
        &self,
        _pstm: Option<&IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Commit(
        &self,
        _grfcommitflags: &windows::Win32::System::Com::STGC,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Revert(&self) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn LockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: &LOCKTYPE,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn UnlockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: u32,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Stat(
        &self,
        pstatstg: *mut windows::Win32::System::Com::STATSTG,
        grfstatflag: &STATFLAG,
    ) -> windows::core::Result<()> {
        self.stream.stat(pstatstg, grfstatflag)
    }

    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        sync::{mpsc, Arc, Mutex},
        thread,
        time::{Duration, Instant},
    };

    use irondash_run_loop::{RunLoop, RunLoopSender};
    use windows::{
        core::HRESULT,
        Win32::Foundation::{STG_E_INVALIDPOINTER, S_FALSE, S_OK},
    };

    use super::{Stream, StreamInner, StreamState};
    use crate::{
        data_provider_manager::VirtualSessionHandle,
        segmented_queue::{
            new_segmented_queue, QueueConfiguration, SegmentedQueueReader, SegmentedQueueWriter,
        },
        value_promise::Promise,
    };

    const DEADLINE: Duration = Duration::from_secs(2);

    fn open_queue() -> (SegmentedQueueWriter, SegmentedQueueReader) {
        new_segmented_queue(QueueConfiguration {
            memory_segment_max_size: 1024,
            file_segment_max_length: 0,
            max_memory_usage: None,
        })
    }

    fn state_for(reader: SegmentedQueueReader) -> StreamState {
        StreamState {
            reader,
            size_promise: Arc::new(Promise::new()),
            error_promise: Arc::new(Promise::new()),
            _handle: Arc::new(VirtualSessionHandle::new_for_test()),
            position: Cell::new(0),
        }
    }

    fn stream_with(
        stream: Option<StreamState>,
        factory: Option<Box<dyn FnOnce() -> StreamState + Send>>,
        run_loop_sender: Option<RunLoopSender>,
    ) -> Stream {
        Stream {
            inner: Mutex::new(StreamInner {
                stream,
                factory,
                run_loop_sender,
            }),
        }
    }

    /// Stream whose queue already holds `bytes` and is completed, so `read` never blocks.
    fn stream_with_bytes(bytes: &[u8]) -> Stream {
        let (writer, reader) = open_queue();
        writer.write(bytes);
        writer.close();
        stream_with(Some(state_for(reader)), None, None)
    }

    #[test]
    fn read_accepts_null_pcbread() {
        RunLoop::current();
        let stream = stream_with_bytes(b"hello");
        let mut buf = [0u8; 4];
        let hr: HRESULT = stream.read(
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            std::ptr::null_mut(),
        );
        assert_eq!(hr, S_OK);
        assert_eq!(&buf, b"hell");
    }

    #[test]
    fn read_rejects_null_buffer_without_consuming_queue() {
        RunLoop::current();
        let stream = stream_with_bytes(b"hello");
        let mut count = 0u32;
        let hr = stream.read(std::ptr::null_mut(), 4, &mut count);
        assert_eq!(hr, STG_E_INVALIDPOINTER);

        let mut buf = [0u8; 5];
        let hr = stream.read(buf.as_mut_ptr() as *mut core::ffi::c_void, 5, &mut count);
        assert_eq!(hr, S_OK);
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn zero_length_read_does_not_consume_queue() {
        RunLoop::current();
        let stream = stream_with_bytes(b"hello");
        let mut buf = [0u8; 5];
        let mut count = 7u32;
        let hr = stream.read(buf.as_mut_ptr() as *mut core::ffi::c_void, 0, &mut count);
        assert_eq!(hr, S_OK);
        assert_eq!(count, 0);

        let hr = stream.read(buf.as_mut_ptr() as *mut core::ffi::c_void, 5, &mut count);
        assert_eq!(hr, S_OK);
        assert_eq!(count, 5);
        assert_eq!(&buf, b"hello");
    }

    #[test]
    fn zero_length_read_accepts_null_buffer_and_null_pcbread() {
        RunLoop::current();
        let stream = stream_with_bytes(b"hello");
        let hr = stream.read(std::ptr::null_mut(), 0, std::ptr::null_mut());
        assert_eq!(hr, S_OK);
    }

    fn read_into(stream: &Stream, buf: &mut [u8]) -> (HRESULT, u32) {
        let mut count = 0u32;
        let hr = stream.read(
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            &mut count,
        );
        (hr, count)
    }

    fn spawn_read(stream: &Arc<Stream>) -> thread::JoinHandle<(HRESULT, Vec<u8>)> {
        let stream = stream.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8];
            let (hr, count) = read_into(&stream, &mut buf);
            (hr, buf[..count as usize].to_vec())
        })
    }

    /// Returns false when the lock was never seen held in time.
    fn wait_until_locked(stream: &Stream) -> bool {
        let start = Instant::now();
        while start.elapsed() < DEADLINE {
            if stream.inner.try_lock().is_err() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    /// Runs disposal on a helper thread; false if it did not finish in time.
    /// A timed-out thread is detached so a failing run cannot hang the test.
    fn dispose_finishes_in_time(stream: &Arc<Stream>) -> bool {
        let (tx, rx) = mpsc::channel();
        let stream = stream.clone();
        thread::spawn(move || {
            stream.dispose_inactive_stream();
            tx.send(()).ok();
        });
        rx.recv_timeout(DEADLINE).is_ok()
    }

    #[test]
    fn dispose_does_not_wait_for_a_blocked_read() {
        RunLoop::current();
        let (writer, reader) = open_queue();
        let stream = Arc::new(stream_with(Some(state_for(reader)), None, None));
        let reader_thread = spawn_read(&stream);

        let blocked = wait_until_locked(&stream);
        let finished = blocked && dispose_finishes_in_time(&stream);

        // Always unblock the reader, even when an assertion below fails.
        writer.write(b"abc");
        writer.close();
        let (hr, bytes) = reader_thread.join().unwrap();

        assert!(blocked, "read never blocked inside the stream lock");
        assert!(
            finished,
            "dispose_inactive_stream waited for a blocked read"
        );
        assert_eq!(hr, S_OK);
        assert_eq!(bytes, b"abc");
    }

    #[test]
    fn dispose_does_not_wait_for_a_blocked_session_factory() {
        RunLoop::current();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (writer, reader) = open_queue();
        writer.write(b"xyz");
        writer.close();
        // Built here, not in the factory: when the state was built on the reader
        // thread, the plugin's `Capsule` panicked ("dropped on wrong thread with data
        // still in it") once the stream was dropped on another thread.
        let state = state_for(reader);
        let factory = Box::new(move || {
            release_rx.recv().ok();
            state
        });
        let stream = Arc::new(stream_with(None, Some(factory), None));
        let reader_thread = spawn_read(&stream);

        let blocked = wait_until_locked(&stream);
        let finished = blocked && dispose_finishes_in_time(&stream);

        release_tx.send(()).ok();
        let (hr, bytes) = reader_thread.join().unwrap();

        assert!(blocked, "read never blocked inside the session factory");
        assert!(
            finished,
            "dispose_inactive_stream waited for a blocked session factory"
        );
        assert_eq!(hr, S_OK);
        assert_eq!(bytes, b"xyz");
        assert!(stream.inner.lock().unwrap().stream.is_some());
    }

    #[test]
    fn dispose_clears_an_untouched_stream() {
        let sender = RunLoop::current().new_sender();
        let stream = stream_with(
            None,
            Some(Box::new(|| panic!("factory must not run"))),
            Some(sender),
        );
        stream.dispose_inactive_stream();
        let inner = stream.inner.lock().unwrap();
        assert!(inner.factory.is_none());
        assert!(inner.run_loop_sender.is_none());
    }

    #[test]
    fn dispose_leaves_a_used_stream_alone() {
        let (writer, reader) = open_queue();
        writer.write(b"hello");
        writer.close();
        let stream = stream_with(
            Some(state_for(reader)),
            None,
            Some(RunLoop::current().new_sender()),
        );
        let mut buf = [0u8; 2];
        let (hr, count) = read_into(&stream, &mut buf);
        assert_eq!((hr, count), (S_OK, 2));

        stream.dispose_inactive_stream();
        assert!(stream.inner.lock().unwrap().run_loop_sender.is_some());

        let mut rest = [0u8; 8];
        let (hr, count) = read_into(&stream, &mut rest);
        assert_eq!(hr, S_OK);
        assert_eq!(&rest[..count as usize], b"llo");
        let (hr, _) = read_into(&stream, &mut rest);
        assert_eq!(hr, S_FALSE);
    }
}
