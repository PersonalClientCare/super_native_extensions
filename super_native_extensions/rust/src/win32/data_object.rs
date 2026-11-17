use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    mem::{size_of, ManuallyDrop},
    rc::Rc,
    slice,
    sync::Arc,
    time::Duration,
};

use irondash_message_channel::IsolateId;
use irondash_run_loop::RunLoop;
use threadpool::ThreadPool;
use windows::{
    core::{implement, HRESULT, HSTRING},
    Win32::{
        Foundation::{
            GlobalFree, BOOL, DATA_S_SAMEFORMATETC, DV_E_FORMATETC, E_INVALIDARG, E_NOTIMPL,
            E_OUTOFMEMORY, HGLOBAL, OLE_E_ADVISENOTSUPPORTED, POINT, S_FALSE, S_OK,
        },
        System::{
            Com::{
                IAdviseSink, IBindCtx, IDataObject, IDataObject_Impl, IStream, DATADIR_GET,
                FORMATETC, STGMEDIUM, STGMEDIUM_0, STREAM_SEEK_END, STREAM_SEEK_SET, TYMED,
                TYMED_HGLOBAL, TYMED_ISTREAM,
            },
            DataExchange::RegisterClipboardFormatW,
            Memory::{GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GLOBAL_ALLOC_FLAGS},
            Ole::{ReleaseStgMedium, CF_DIB, CF_DIBV5, CF_HDROP, DROPEFFECT},
        },
        UI::Shell::{
            IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl, SHCreateMemStream,
            SHCreateStdEnumFmtEtc, CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTOR,
            CFSTR_LOGICALPERFORMEDDROPEFFECT, CFSTR_PERFORMEDDROPEFFECT, DROPFILES, FD_ATTRIBUTES,
            FD_PROGRESSUI, FILEDESCRIPTORW,
        },
    },
};

use crate::{
    api_model::{DataProviderValueId, DataRepresentation, VirtualFileStorage},
    data_provider_manager::{DataProviderHandle, PlatformDataProviderDelegate, VirtualFileResult},
    log::OkLog,
    segmented_queue::{new_segmented_queue, QueueConfiguration},
    util::DropNotifier,
    value_coerce::{CoerceToData, StringFormat},
    value_promise::{Promise, ValuePromise, ValuePromiseResult},
};

use super::{
    add_stream_entry,
    common::{
        as_u8_slice, format_from_string, format_to_string, make_format_with_tymed,
        make_format_with_tymed_index, read_stream_fully,
    },
    image_conversion::convert_to_dib,
    pump::pump_until_timeout,
    virtual_file_stream::{VirtualFileStream, VirtualStreamSession},
    PlatformDataProvider,
};

const DATA_E_FORMATETC: HRESULT = HRESULT(-2147221404 + 1);

// Another app's drag loop calls us; longer blocks make Windows flag our window as hung.
const LAZY_DATA_WAIT: Duration = Duration::from_secs(2);

struct ProviderEntry {
    provider: Rc<PlatformDataProvider>,
    _handle: Arc<DataProviderHandle>,
}

#[implement(IDataObject, IDataObjectAsyncCapability)]
pub struct DataObject {
    providers: Vec<ProviderEntry>,
    extra_data: RefCell<HashMap<u16, Vec<u8>>>,
    /// Promises of lazy values that timed out. A later request polls one of these once with no
    /// wait and fails fast until the value has arrived; the delegate is not asked again.
    lazy_pending: RefCell<HashMap<DataProviderValueId, Arc<ValuePromise>>>,
    in_operation: Cell<bool>, // async stream
    virtual_stream_notifiers: RefCell<Vec<Arc<DropNotifier>>>,
    thread_pool: RefCell<Option<ThreadPool>>,
}

/// These formats are not commonly supported on Windows. If they
/// are present as payload, DataObject will provide on-demand
/// DIB and DIBV5 representation (unless the payload already contains
/// DIB or DIBV5)
static FOREIGN_IMAGE_FORMATS: &[&str] = &["PNG", "GIF", "JFIF"];

impl DataObject {
    pub fn create(
        providers: Vec<(Rc<PlatformDataProvider>, Arc<DataProviderHandle>)>,
    ) -> IDataObject {
        let data_object = Self {
            providers: providers
                .into_iter()
                .map(|p| ProviderEntry {
                    provider: p.0,
                    _handle: p.1,
                })
                .collect(),
            extra_data: RefCell::new(HashMap::new()),
            lazy_pending: RefCell::new(HashMap::new()),
            in_operation: Cell::new(false),
            virtual_stream_notifiers: RefCell::new(Vec::new()),
            thread_pool: RefCell::new(None),
        };
        data_object.into()
    }

    fn global_from_data(&self, data: &[u8]) -> windows::core::Result<HGLOBAL> {
        unsafe {
            let global = GlobalAlloc(GLOBAL_ALLOC_FLAGS(0), data.len())?;
            let global_data = GlobalLock(global);
            if global_data.is_null() {
                GlobalFree(global)?;
                Err(E_OUTOFMEMORY.into())
            } else {
                std::ptr::copy_nonoverlapping(data.as_ptr(), global_data as *mut u8, data.len());
                GlobalUnlock(global).ok();
                Ok(global)
            }
        }
    }

    fn lazy_data_for_id(
        &self,
        provider: &PlatformDataProvider,
        id: DataProviderValueId,
    ) -> Option<Vec<u8>> {
        // Held across the pump so the delegate outlives the wait.
        let delegate = provider.delegate.upgrade();
        // The statement ends the borrow before pumping, which may reach GetData or SetData.
        let pending = self.lazy_pending.borrow_mut().remove(&id);
        let (promise, wait) = match (pending, &delegate) {
            (Some(promise), _) => (promise, Duration::ZERO),
            (None, Some(delegate)) => (
                delegate.get_lazy_data(provider.isolate_id, id, None),
                LAZY_DATA_WAIT,
            ),
            (None, None) => return None,
        };
        let mut result = None;
        pump_until_timeout(
            || {
                result = promise.try_take();
                result.is_some()
            },
            wait,
        );
        match result {
            Some(ValuePromiseResult::Ok { value }) => {
                value.coerce_to_data(StringFormat::Utf16NullTerminated)
            }
            Some(ValuePromiseResult::Cancelled) => None,
            // Not arrived yet, or WM_QUIT during shutdown: the next request polls
            // it once, without waiting.
            None => {
                self.lazy_pending.borrow_mut().insert(id, promise);
                None
            }
        }
    }

    fn data_for_format(&self, format: u32, index: usize) -> Option<Vec<u8>> {
        let provider = self.providers.get(index).as_ref().cloned();
        if let Some(provider) = provider {
            let provider = &provider.provider;
            let format_string = format_to_string(format);
            for representation in &provider.data.representations {
                match representation {
                    DataRepresentation::Simple { format, data } => {
                        if &format_string == format {
                            return data.coerce_to_data(StringFormat::Utf16NullTerminated);
                        }
                    }
                    DataRepresentation::Lazy { format, id } if &format_string == format => {
                        return self.lazy_data_for_id(provider, *id);
                    }
                    _ => {}
                }
            }
            None
        } else {
            None
        }
    }

    /// Bundles slice of utf16 encoded string into CF_HDROP
    pub fn bundle_files(files: &[Vec<u8>]) -> Vec<u8> {
        let mut res = Vec::new();

        let drop_files = DROPFILES {
            pFiles: size_of::<DROPFILES>() as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: false.into(),
            fWide: true.into(),
        };

        let drop_files = unsafe { as_u8_slice(&drop_files) };
        res.extend_from_slice(drop_files);

        for f in files {
            res.extend_from_slice(f);
        }
        res.extend_from_slice(&[0, 0]);

        res
    }

    fn data_for_hdrop(&self) -> Option<Vec<u8>> {
        let n_items = self.providers.len();
        let files: Vec<_> = (0..n_items)
            .filter_map(|i| self.data_for_format(CF_HDROP.0 as u32, i))
            .collect();
        if files.is_empty() {
            None
        } else {
            Some(Self::bundle_files(&files))
        }
    }

    fn get_source_stream_for_synthesized_bitmap(&self) -> windows::core::Result<IStream> {
        let foreign_formats = Self::foreign_formats();
        let formats = self.get_formats();
        for format in formats {
            let format = format.cfFormat as u32;
            if foreign_formats.contains(&format) {
                return self.get_stream(format);
            }
        }
        Err(windows::core::Error::new(
            DATA_E_FORMATETC,
            "Did not find original image stream".into(),
        ))
    }

    fn synthesize_bitmap_data(&self, use_v5: bool) -> windows::core::Result<Vec<u8>> {
        let input_stream = self.get_source_stream_for_synthesized_bitmap()?;
        convert_to_dib(input_stream, use_v5)
    }

    fn foreign_formats() -> Vec<u32> {
        FOREIGN_IMAGE_FORMATS
            .iter()
            .map(|f| unsafe { RegisterClipboardFormatW(&HSTRING::from(*f)) })
            .collect()
    }

    /// If there are any image formats not supported by windows natively
    /// and no DIB or DIBV5 we need to generate those.
    fn needs_synthesize_bitmap(&self) -> bool {
        let foreign_formats = Self::foreign_formats();
        let mut has_bmp = false;
        let mut has_foreign = false;
        for provider in &self.providers {
            for repr in &provider.provider.data.representations {
                let repr_format = format_from_string(repr.format());
                has_bmp |= repr_format == CF_DIBV5.0 as u32 || repr_format == CF_DIB.0 as u32;
                has_foreign |= foreign_formats.contains(&repr_format);
            }
        }
        has_foreign && !has_bmp
    }

    fn get_formats(&self) -> Vec<FORMATETC> {
        let mut res = Vec::<_>::new();
        let mut index = 0;
        // Put virtual files first
        for provider in &self.providers {
            for repr in &provider.provider.data.representations {
                if repr.is_virtual_file() {
                    if index == 0 {
                        res.push(make_format_with_tymed(
                            unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTOR) },
                            TYMED_HGLOBAL,
                        ));
                    }
                    res.push(make_format_with_tymed_index(
                        unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) },
                        TYMED_ISTREAM,
                        index,
                    ));
                    index += 1;
                }
            }
        }
        // Regular and lazy items second
        let first_provider = self.providers.first();
        if let Some(provider) = first_provider {
            for representation in &provider.provider.data.representations {
                match representation {
                    DataRepresentation::Simple { format, data: _ } => {
                        let format = format_from_string(format);
                        res.push(make_format_with_tymed(format, TYMED_HGLOBAL));
                    }
                    DataRepresentation::Lazy { format, id: _ } => {
                        let format = format_from_string(format);
                        res.push(make_format_with_tymed(format, TYMED_HGLOBAL));
                    }
                    _ => {}
                }
            }
        }

        if self.needs_synthesize_bitmap() {
            res.push(make_format_with_tymed(CF_DIB.0 as u32, TYMED_HGLOBAL));
            res.push(make_format_with_tymed(CF_DIBV5.0 as u32, TYMED_HGLOBAL));
        }

        // Extra data (set through SetData) last
        let extra_data = self.extra_data.borrow();
        for format in extra_data.keys() {
            res.push(make_format_with_tymed(
                *format as u32,
                TYMED(TYMED_HGLOBAL.0 | TYMED_ISTREAM.0),
            ));
        }
        res
    }

    fn file_descriptor_for_item(file_name: &str) -> FILEDESCRIPTORW {
        let mut name_buf: [u16; 260] = [0; 260];
        let mut name_str: Vec<_> = file_name.encode_utf16().collect();
        name_str.truncate(name_buf.len() - 1);
        name_buf[0..name_str.len()].copy_from_slice(&name_str);
        FILEDESCRIPTORW {
            dwFlags: (FD_ATTRIBUTES.0 | FD_PROGRESSUI.0) as u32,
            cFileName: name_buf,
            ..FILEDESCRIPTORW::default()
        }
    }

    fn data_for_file_group_descritor(&self) -> Option<Vec<u8>> {
        let mut cnt = 0;
        let mut descriptors = Vec::<FILEDESCRIPTORW>::new();
        for provider in &self.providers {
            let data = &provider.provider.data;
            if data.representations.iter().any(|a| a.is_virtual_file()) {
                cnt += 1;
                let name = data
                    .suggested_name
                    .as_ref()
                    .cloned()
                    .unwrap_or_else(|| format!("File {cnt}"));
                descriptors.push(Self::file_descriptor_for_item(&name));
            }
        }
        let mut res = Vec::new();
        let len = descriptors.len() as u32;
        if len == 0 {
            return None;
        }
        res.extend_from_slice(unsafe { as_u8_slice(&len) });
        for descriptor in &descriptors {
            res.extend_from_slice(unsafe { as_u8_slice(descriptor) });
        }
        Some(res)
    }

    fn create_virtual_stream_session(
        delegate: Rc<dyn PlatformDataProviderDelegate>,
        isolate_id: IsolateId,
        virtual_file_id: DataProviderValueId,
        configuration: QueueConfiguration,
    ) -> VirtualStreamSession {
        let (writer, reader) = new_segmented_queue(configuration);
        let stream_handle = add_stream_entry(writer);
        let size_promise = Arc::new(Promise::<Option<i64>>::new());
        let size_promise_clone = size_promise.clone();
        let error_promise = Arc::new(Promise::<String>::new());
        let error_promise_clone = error_promise.clone();
        let session_handle = delegate.get_virtual_file(
            isolate_id,
            virtual_file_id,
            stream_handle,
            Box::new(move |size| size_promise_clone.set(size)),
            Box::new(move |_progress| {}),
            Box::new(move |result| {
                if let VirtualFileResult::Error { message } = result {
                    error_promise_clone.set(message);
                }
            }),
        );
        VirtualStreamSession {
            reader,
            size_promise,
            error_promise,
            handle: session_handle,
        }
    }

    fn stream_for_virtual_file(
        &self,
        provider: &PlatformDataProvider,
        virtual_file_id: DataProviderValueId,
        storage_suggestion: &Option<VirtualFileStorage>,
        agile: bool,
    ) -> Option<IStream> {
        if let Some(delegate) = provider.delegate.upgrade() {
            let storage_suggestion =
                storage_suggestion.unwrap_or(VirtualFileStorage::TemporaryFile);
            let configuration = if storage_suggestion == VirtualFileStorage::TemporaryFile {
                QueueConfiguration {
                    memory_segment_max_size: 1024 * 1024 * 4,
                    file_segment_max_length: 1024 * 1024 * 30,
                    max_memory_usage: Some(1024 * 1024 * 12),
                }
            } else {
                QueueConfiguration {
                    memory_segment_max_size: 1024 * 1024 * 4,
                    file_segment_max_length: 0,
                    max_memory_usage: None,
                }
            };
            let isolate_id = provider.isolate_id;
            let provider = move || {
                Self::create_virtual_stream_session(
                    delegate,
                    isolate_id,
                    virtual_file_id,
                    configuration,
                )
            };
            let (stream, notifier) = if agile {
                VirtualFileStream::create_agile(provider)
            } else {
                let mut thread_pool = self.thread_pool.borrow_mut();
                let thread_pool = thread_pool.get_or_insert_with(|| ThreadPool::new(1));
                VirtualFileStream::create_marshalled_on_background_thread(provider, thread_pool)
            };

            // The drop notifier will be invoked when DataObject gets released
            // That will ensure that the stream is destroyed when data object
            // is dropped in case the client leaks the stream.
            self.virtual_stream_notifiers.borrow_mut().push(notifier);
            Some(stream)
        } else {
            None
        }
    }

    fn stream_for_virtual_file_index(&self, mut index: usize, agile: bool) -> Option<IStream> {
        for provider in &self.providers {
            let provider = &provider.provider;
            // Skip all virtual files before the requested one.
            if index > 0
                && provider
                    .data
                    .representations
                    .iter()
                    .any(|r| r.is_virtual_file())
            {
                index -= 1;
                continue;
            }
            for repr in &provider.data.representations {
                if let DataRepresentation::VirtualFile {
                    id,
                    format: _,
                    storage_suggestion,
                } = repr
                {
                    return self.stream_for_virtual_file(provider, *id, storage_suggestion, agile);
                }
            }
        }
        None
    }
}

thread_local! {
    static IS_LOCAL_REQUEST: Cell<bool> = const { Cell::new(false) };
}

impl DataObject {
    pub fn with_local_request<T, F: FnOnce() -> T>(f: F) -> T {
        let prev = IS_LOCAL_REQUEST.with(|a| a.replace(true));
        let res = f();
        IS_LOCAL_REQUEST.with(|a| a.set(prev));
        res
    }

    fn is_local_request() -> bool {
        IS_LOCAL_REQUEST.with(|f| f.get())
    }
}

impl Drop for DataObject {
    fn drop(&mut self) {
        // Keep the streams alive for one second after disposing data object
        // to give the client chance to interact with stream.
        // Otherwise the streams will be disposed to prevent leaks.
        // See VirtualFileStream::dispose()
        let notifiers: Vec<_> = self
            .virtual_stream_notifiers
            .borrow_mut()
            .drain(0..)
            .collect();
        RunLoop::current()
            .schedule(Duration::from_secs(1), move || {
                let _notifiers = notifiers;
            })
            .detach();
    }
}

#[allow(non_snake_case)]
impl IDataObject_Impl for DataObject {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let format = unsafe { &*pformatetcin };
        let format_file_descriptor = unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTOR) };
        let format_file_contents = unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) };

        if format.cfFormat as u32 == format_file_contents {
            let stream = self
                .stream_for_virtual_file_index(format.lindex as usize, Self::is_local_request())
                .ok_or_else(|| windows::core::Error::from(DV_E_FORMATETC))?;
            return Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                pUnkForRelease: ManuallyDrop::new(None),
            });
        }

        let needs_generate_bitmap = self.needs_synthesize_bitmap();

        // Ends the borrow before the lazy path below, which pumps the run loop
        // and may reach SetData.
        let cached = self.extra_data.borrow().get(&format.cfFormat).cloned();
        let data = cached.or_else(|| {
            if format.cfFormat as u32 == format_file_descriptor {
                self.data_for_file_group_descritor()
            } else if format.cfFormat == CF_HDROP.0 {
                self.data_for_hdrop()
            } else if needs_generate_bitmap && format.cfFormat == CF_DIB.0 {
                self.synthesize_bitmap_data(false).ok_log()
            } else if needs_generate_bitmap && format.cfFormat == CF_DIBV5.0 {
                self.synthesize_bitmap_data(true).ok_log()
            } else {
                self.data_for_format(format.cfFormat as u32, 0)
            }
        });

        // println!("DATA {:?} {:?}", data, format_to_string(format.cfFormat as u32));

        match data {
            Some(data) => {
                if (format.tymed & TYMED_HGLOBAL.0 as u32) != 0 {
                    let global = self.global_from_data(&data)?;
                    Ok(STGMEDIUM {
                        tymed: TYMED_HGLOBAL.0 as u32,
                        u: STGMEDIUM_0 { hGlobal: global },
                        pUnkForRelease: ManuallyDrop::new(None),
                    })
                } else if (format.tymed & TYMED_ISTREAM.0 as u32) != 0 {
                    let stream = unsafe { SHCreateMemStream(Some(&data)) };
                    let stream =
                        stream.ok_or_else(|| windows::core::Error::from(DV_E_FORMATETC))?;
                    unsafe {
                        stream.Seek(0, STREAM_SEEK_END, None)?;
                    }
                    Ok(STGMEDIUM {
                        tymed: TYMED_ISTREAM.0 as u32,
                        u: STGMEDIUM_0 {
                            pstm: ManuallyDrop::new(Some(stream)),
                        },
                        pUnkForRelease: ManuallyDrop::new(None),
                    })
                } else {
                    Err(DV_E_FORMATETC.into())
                }
            }
            None => Err(DV_E_FORMATETC.into()),
        }
    }

    fn GetDataHere(
        &self,
        _pformatetc: *const windows::Win32::System::Com::FORMATETC,
        _pmedium: *mut windows::Win32::System::Com::STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(
        &self,
        pformatetc: *const windows::Win32::System::Com::FORMATETC,
    ) -> windows::core::HRESULT {
        let format = unsafe { &*pformatetc };
        let index = self.get_formats().iter().position(|e| {
            e.cfFormat == format.cfFormat
                && (e.tymed & format.tymed) != 0
                && e.dwAspect == format.dwAspect
                && e.lindex == format.lindex
        });
        match index {
            Some(_) => S_OK,
            None => {
                // possibly extra data
                if (format.tymed == TYMED_HGLOBAL.0 as u32
                    || format.tymed == TYMED_ISTREAM.0 as u32)
                    && self.extra_data.borrow().contains_key(&format.cfFormat)
                {
                    S_OK
                } else {
                    S_FALSE
                }
            }
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        pformatectin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> ::windows::core::HRESULT {
        let fmt_out = unsafe { &mut *pformatetcout };
        let fmt_in = unsafe { &*pformatectin };
        *fmt_out = *fmt_in;
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        pformatetc: *const windows::Win32::System::Com::FORMATETC,
        pmedium: *const windows::Win32::System::Com::STGMEDIUM,
        frelease: windows::Win32::Foundation::BOOL,
    ) -> windows::core::Result<()> {
        let format = unsafe { &*pformatetc };

        if format.tymed == TYMED_HGLOBAL.0 as u32 {
            unsafe {
                let medium = &*pmedium;
                let size = GlobalSize(medium.u.hGlobal);
                let global_data = GlobalLock(medium.u.hGlobal);
                if global_data.is_null() {
                    return Err(E_INVALIDARG.into());
                }

                let v = slice::from_raw_parts(global_data as *const u8, size);
                let global_data: Vec<u8> = v.into();

                GlobalUnlock(medium.u.hGlobal).ok();
                self.extra_data
                    .borrow_mut()
                    .insert(format.cfFormat, global_data);

                if frelease.as_bool() {
                    ReleaseStgMedium(pmedium as *mut _);
                }
            }

            Ok(())
        } else if format.tymed == TYMED_ISTREAM.0 as u32 {
            unsafe {
                let medium = &*pmedium;
                let stream = medium.u.pstm.as_ref().cloned();

                let stream_data = if let Some(stream) = stream {
                    stream.Seek(0, STREAM_SEEK_SET, None)?;
                    read_stream_fully(&stream)?
                } else {
                    Vec::new()
                };

                self.extra_data
                    .borrow_mut()
                    .insert(format.cfFormat, stream_data);

                if frelease.as_bool() {
                    ReleaseStgMedium(pmedium as *mut _);
                }
            }

            Ok(())
        } else {
            Err(DATA_E_FORMATETC.into())
        }
    }

    fn EnumFormatEtc(
        &self,
        dwdirection: u32,
    ) -> windows::core::Result<windows::Win32::System::Com::IEnumFORMATETC> {
        if dwdirection == DATADIR_GET.0 as u32 {
            unsafe { SHCreateStdEnumFmtEtc(&self.get_formats()) }
        } else {
            Err(E_NOTIMPL.into())
        }
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Option<&IAdviseSink>,
    ) -> ::windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<windows::Win32::System::Com::IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[allow(non_snake_case)]
impl IDataObjectAsyncCapability_Impl for DataObject {
    fn SetAsyncMode(&self, _fdoopasync: BOOL) -> windows::core::Result<()> {
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows::core::Result<BOOL> {
        Ok(true.into())
    }

    fn StartOperation(&self, _pbcreserved: Option<&IBindCtx>) -> windows::core::Result<()> {
        self.in_operation.replace(true);
        Ok(())
    }

    fn InOperation(&self) -> windows::core::Result<BOOL> {
        Ok(self.in_operation.get().into())
    }

    fn EndOperation(
        &self,
        _hresult: windows::core::HRESULT,
        _pbcreserved: Option<&IBindCtx>,
        _dweffects: u32,
    ) -> windows::core::Result<()> {
        self.in_operation.replace(false);
        Ok(())
    }
}

pub trait GetData {
    unsafe fn do_get_data(
        &self,
        pformatetcin: *const FORMATETC,
    ) -> windows::core::Result<STGMEDIUM>;

    unsafe fn do_query_get_data(&self, format: *const FORMATETC) -> HRESULT;

    fn get_data(&self, format: u32) -> windows::core::Result<Vec<u8>> {
        let format = make_format_with_tymed(format, TYMED(TYMED_ISTREAM.0 | TYMED_HGLOBAL.0));

        unsafe {
            let mut medium = self.do_get_data(&format as *const _)?;
            let res = if medium.tymed == TYMED_ISTREAM.0 as u32 {
                let stream = medium.u.pstm.as_ref().cloned();
                if let Some(stream) = stream {
                    // IDataObject streams need to be rewound
                    stream.Seek(0, STREAM_SEEK_SET, None)?;
                    read_stream_fully(&stream)
                } else {
                    Ok(Vec::new())
                }
            } else if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                let size = GlobalSize(medium.u.hGlobal);
                let data = GlobalLock(medium.u.hGlobal);
                if data.is_null() {
                    Err(E_INVALIDARG.into())
                } else {
                    let v = slice::from_raw_parts(data as *const u8, size);
                    let res: Vec<u8> = v.into();

                    GlobalUnlock(medium.u.hGlobal).ok();

                    Ok(res)
                }
            } else {
                Err(DATA_E_FORMATETC.into())
            };
            ReleaseStgMedium(&mut medium as *mut STGMEDIUM);
            res
        }
    }

    fn get_stream(&self, format: u32) -> windows::core::Result<IStream> {
        let format = make_format_with_tymed(format, TYMED(TYMED_ISTREAM.0 | TYMED_HGLOBAL.0));
        let res = unsafe {
            let mut medium = self.do_get_data(&format as *const _)?;
            let res: windows::core::Result<Option<IStream>> =
                if medium.tymed == TYMED_ISTREAM.0 as u32 {
                    Ok(medium.u.pstm.as_ref().cloned())
                } else if medium.tymed == TYMED_HGLOBAL.0 as u32 {
                    let size = GlobalSize(medium.u.hGlobal);
                    let data = GlobalLock(medium.u.hGlobal);
                    if data.is_null() {
                        Err(E_INVALIDARG.into())
                    } else {
                        let data = slice::from_raw_parts(data as *const u8, size);
                        let res = SHCreateMemStream(Some(data));
                        GlobalUnlock(medium.u.hGlobal).ok();
                        Ok(res)
                    }
                } else {
                    Ok(None)
                };
            ReleaseStgMedium(&mut medium as *mut STGMEDIUM);
            res
        };
        res?.ok_or_else(|| DATA_E_FORMATETC.into())
    }

    fn has_data_for_format(&self, format: &FORMATETC) -> bool {
        unsafe { self.do_query_get_data(format as *const _) == S_OK }
    }

    fn has_data(&self, format: u32) -> bool {
        let format = make_format_with_tymed(format, TYMED_HGLOBAL);
        self.has_data_for_format(&format)
    }
}

pub trait DataObjectExt {
    fn performed_drop_effect(&self) -> Option<DROPEFFECT>;
}

impl GetData for IDataObject {
    unsafe fn do_get_data(
        &self,
        pformatetcin: *const FORMATETC,
    ) -> windows::core::Result<STGMEDIUM> {
        self.GetData(pformatetcin)
    }

    unsafe fn do_query_get_data(&self, format: *const FORMATETC) -> HRESULT {
        let res = self.QueryGetData(format);
        // Workaround for Windows Explorer:
        // When pasting data from Explorer QueryGetData will return DV_E_FORMATETC
        // for CFSTR_FILECONTENTS when index is specified in format.
        // This only affects QueryGetData, actually getting the data works as
        // expected.
        // https://github.com/superlistapp/super_native_extensions/issues/86
        if res == DV_E_FORMATETC {
            let format = unsafe { *format };
            if format.lindex != -1
                && format.cfFormat as u32 == unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) }
            {
                let format = FORMATETC {
                    cfFormat: format.cfFormat,
                    ptd: format.ptd,
                    dwAspect: format.dwAspect,
                    lindex: -1,
                    tymed: format.tymed,
                };
                return self.QueryGetData(&format as *const _);
            }
        }
        res
    }
}

impl GetData for DataObject {
    unsafe fn do_get_data(
        &self,
        pformatetcin: *const FORMATETC,
    ) -> windows::core::Result<STGMEDIUM> {
        self.GetData(pformatetcin)
    }

    unsafe fn do_query_get_data(&self, format: *const FORMATETC) -> HRESULT {
        match self.GetData(format) {
            Ok(_) => S_OK,
            Err(e) => e.into(),
        }
    }

    fn has_data_for_format(&self, format: &FORMATETC) -> bool {
        format.tymed == TYMED_HGLOBAL.0 as u32
            && format.lindex == 0
            && self.has_data(format.cfFormat as u32)
    }
}

impl<T> DataObjectExt for T
where
    T: GetData,
{
    fn performed_drop_effect(&self) -> Option<DROPEFFECT> {
        let format = unsafe { RegisterClipboardFormatW(CFSTR_PERFORMEDDROPEFFECT) };
        let logical_format = unsafe { RegisterClipboardFormatW(CFSTR_LOGICALPERFORMEDDROPEFFECT) };
        let data = self
            .get_data(logical_format)
            .ok()
            .or_else(|| self.get_data(format).ok());

        if let Some(data) = data {
            if data.len() == 4 {
                return Some(DROPEFFECT(u32::from_ne_bytes(data.try_into().unwrap())));
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
        sync::Arc,
        time::{Duration, Instant},
    };

    use async_trait::async_trait;
    use irondash_message_channel::{IsolateId, Value};
    use irondash_run_loop::RunLoop;
    use windows::{
        core::HSTRING,
        Win32::{
            Foundation::S_OK,
            System::{
                DataExchange::RegisterClipboardFormatW,
                Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GLOBAL_ALLOC_FLAGS},
                Ole::CF_UNICODETEXT,
            },
        },
    };

    use super::*;
    use crate::{
        api_model::{DataProvider, DataProviderValueId, DataRepresentation},
        data_provider_manager::{VirtualFileResult, VirtualSessionHandle},
        platform_impl::platform::test_support::run_on_thread,
        util::DropNotifier,
        value_promise::{ValuePromise, ValuePromiseSetCancel},
    };

    /// The limit leaves room for the two-second lazy wait in the timeout tests.
    const RUN_LIMIT: Duration = Duration::from_secs(10);

    fn hglobal_with(bytes: &[u8]) -> windows::core::Result<HGLOBAL> {
        unsafe {
            let global = GlobalAlloc(GLOBAL_ALLOC_FLAGS(0), bytes.len())?;
            let dest = GlobalLock(global);
            assert!(!dest.is_null());
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dest as *mut u8, bytes.len());
            GlobalUnlock(global).ok();
            Ok(global)
        }
    }

    /// Stores `bytes` under `format` through IDataObject::SetData, as a client would.
    fn set_data(data_object: &IDataObject, format: u32, bytes: &[u8]) -> windows::core::Result<()> {
        let format = make_format_with_tymed(format, TYMED_HGLOBAL);
        let medium = STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: hglobal_with(bytes)?,
            },
            pUnkForRelease: ManuallyDrop::new(None),
        };
        // frelease = TRUE: SetData takes ownership of the HGLOBAL.
        unsafe { data_object.SetData(&format, &medium, true) }
    }

    #[derive(Default)]
    struct Probe {
        set_data_succeeded: Cell<bool>,
    }

    /// Lazy provider whose data is delivered from a run-loop task that first calls
    /// SetData on the same DataObject and only then fulfils the promise.
    struct ReentrantDelegate {
        data_object: RefCell<Option<IDataObject>>,
        probe: Rc<Probe>,
        other_format: u32,
        other_bytes: Vec<u8>,
    }

    #[async_trait(?Send)]
    impl PlatformDataProviderDelegate for ReentrantDelegate {
        fn get_lazy_data(
            &self,
            _isolate_id: IsolateId,
            _data_id: DataProviderValueId,
            _on_done: Option<Box<dyn FnOnce()>>,
        ) -> Arc<ValuePromise> {
            let promise = Arc::new(ValuePromise::new());
            let promise_clone = promise.clone();
            let data_object = self.data_object.borrow().clone().expect("data object");
            let probe = self.probe.clone();
            let other_format = self.other_format;
            let other_bytes = self.other_bytes.clone();
            RunLoop::current()
                .schedule_next(move || {
                    // A borrow still held by the pending GetData makes this panic inside
                    // the COM method, which aborts the test binary.
                    let result = set_data(&data_object, other_format, &other_bytes);
                    probe.set_data_succeeded.set(result.is_ok());
                    promise_clone.set_value(Value::String("lazy".to_owned()));
                })
                .detach();
            promise
        }

        async fn get_lazy_data_async(
            &self,
            _isolate_id: IsolateId,
            _data_id: DataProviderValueId,
        ) -> ValuePromiseResult {
            unimplemented!()
        }

        fn get_virtual_file(
            &self,
            _isolate_id: IsolateId,
            _virtual_file_id: DataProviderValueId,
            _stream_handle: i32,
            _on_size_known: Box<dyn Fn(Option<i64>)>,
            _on_progress: Box<dyn Fn(f64)>,
            _on_done: Box<dyn FnOnce(VirtualFileResult)>,
        ) -> Arc<VirtualSessionHandle> {
            unimplemented!()
        }
    }

    #[test]
    fn get_data_survives_set_data_from_the_pump() {
        run_on_thread(RUN_LIMIT, || {
            let _ole = super::super::OleInitializer::new();
            RunLoop::current();

            let probe = Rc::new(Probe::default());
            let other_format =
                unsafe { RegisterClipboardFormatW(&HSTRING::from("PccTestSetDuringPump")) };
            let other_bytes: Vec<u8> = vec![1, 2, 3, 4];
            let delegate = Rc::new(ReentrantDelegate {
                data_object: RefCell::new(None),
                probe: probe.clone(),
                other_format,
                other_bytes: other_bytes.clone(),
            });
            let delegate_dyn: Rc<dyn PlatformDataProviderDelegate> = delegate.clone();
            let data_object = lazy_text_data_object(&delegate_dyn);
            *delegate.data_object.borrow_mut() = Some(data_object.clone());

            let lazy_text = data_object
                .get_data(CF_UNICODETEXT.0 as u32)
                .expect("lazy text");

            assert!(
                probe.set_data_succeeded.get(),
                "SetData did not run or failed while GetData was pending"
            );
            let expected_text = utf16_nul("lazy");
            // GlobalSize may round the allocation up, so compare the prefix.
            assert!(lazy_text.starts_with(&expected_text), "{lazy_text:?}");

            let stored = data_object
                .get_data(other_format)
                .expect("stored by SetData");
            assert!(stored.starts_with(&other_bytes), "{stored:?}");
            assert!(data_object.has_data(other_format));
            let format = make_format_with_tymed(other_format, TYMED_HGLOBAL);
            let query = unsafe { data_object.QueryGetData(&format) };
            assert_eq!(query, S_OK);
            true
        });
    }

    /// Lazy provider that hands out a promise, keeps a clone and never fulfils it on
    /// its own. Counts `get_lazy_data` calls.
    #[derive(Default)]
    struct PendingDelegate {
        promise: RefCell<Option<Arc<ValuePromise>>>,
        calls: Cell<usize>,
    }

    #[async_trait(?Send)]
    impl PlatformDataProviderDelegate for PendingDelegate {
        fn get_lazy_data(
            &self,
            _isolate_id: IsolateId,
            _data_id: DataProviderValueId,
            _on_done: Option<Box<dyn FnOnce()>>,
        ) -> Arc<ValuePromise> {
            self.calls.set(self.calls.get() + 1);
            let promise = Arc::new(ValuePromise::new());
            *self.promise.borrow_mut() = Some(promise.clone());
            promise
        }

        async fn get_lazy_data_async(
            &self,
            _isolate_id: IsolateId,
            _data_id: DataProviderValueId,
        ) -> ValuePromiseResult {
            unimplemented!()
        }

        fn get_virtual_file(
            &self,
            _isolate_id: IsolateId,
            _virtual_file_id: DataProviderValueId,
            _stream_handle: i32,
            _on_size_known: Box<dyn Fn(Option<i64>)>,
            _on_progress: Box<dyn Fn(f64)>,
            _on_done: Box<dyn FnOnce(VirtualFileResult)>,
        ) -> Arc<VirtualSessionHandle> {
            unimplemented!()
        }
    }

    /// Expected CF_UNICODETEXT bytes for `text`: UTF-16LE with a terminating NUL.
    fn utf16_nul(text: &str) -> Vec<u8> {
        text.encode_utf16()
            .chain(std::iter::once(0))
            .flat_map(|u| u.to_le_bytes())
            .collect()
    }

    /// DataObject with one lazy CF_UNICODETEXT value served by `delegate`. The caller
    /// keeps `delegate` alive, because the provider holds only a Weak reference to it.
    fn lazy_text_data_object(delegate: &Rc<dyn PlatformDataProviderDelegate>) -> IDataObject {
        let provider = Rc::new(PlatformDataProvider::new(
            Rc::downgrade(delegate),
            IsolateId(1),
            DataProvider {
                representations: vec![DataRepresentation::Lazy {
                    id: DataProviderValueId::try_from(Value::I64(1)).unwrap(),
                    format: format_to_string(CF_UNICODETEXT.0 as u32),
                }],
                suggested_name: None,
            },
        ));
        let handle = Arc::new(DataProviderHandle::from(DropNotifier::new(|| {})));
        DataObject::create(vec![(provider, handle)])
    }

    #[test]
    fn slow_lazy_value_times_out_once_then_fails_fast() {
        run_on_thread(RUN_LIMIT, || {
            let _ole = super::super::OleInitializer::new();
            RunLoop::current();

            let delegate = Rc::new(PendingDelegate::default());
            let delegate_dyn: Rc<dyn PlatformDataProviderDelegate> = delegate.clone();
            let data_object = lazy_text_data_object(&delegate_dyn);

            let start = Instant::now();
            let first = data_object.get_data(CF_UNICODETEXT.0 as u32);
            let first_took = start.elapsed();
            assert!(
                first.is_err(),
                "first request got data before the wait ended"
            );
            // Windows flags a window as hung after about 5 s, so stay well below that.
            assert!(
                first_took >= Duration::from_millis(1500) && first_took < Duration::from_secs(4),
                "first request took {first_took:?}"
            );

            let start = Instant::now();
            let second = data_object.get_data(CF_UNICODETEXT.0 as u32);
            let second_took = start.elapsed();
            assert!(second.is_err());
            assert!(
                second_took < Duration::from_millis(500),
                "second request took {second_took:?}"
            );
            assert_eq!(delegate.calls.get(), 1);
            true
        });
    }

    #[test]
    fn late_lazy_value_is_delivered_on_the_next_request() {
        run_on_thread(RUN_LIMIT, || {
            let _ole = super::super::OleInitializer::new();
            RunLoop::current();

            let delegate = Rc::new(PendingDelegate::default());
            let delegate_dyn: Rc<dyn PlatformDataProviderDelegate> = delegate.clone();
            let data_object = lazy_text_data_object(&delegate_dyn);

            assert!(data_object.get_data(CF_UNICODETEXT.0 as u32).is_err());
            let promise = delegate
                .promise
                .borrow()
                .clone()
                .expect("get_lazy_data was not called");
            promise.set_value(Value::String("late".to_owned()));

            let late = data_object
                .get_data(CF_UNICODETEXT.0 as u32)
                .expect("late lazy text");
            let expected_text = utf16_nul("late");
            // GlobalSize may round the allocation up, so compare the prefix.
            assert!(late.starts_with(&expected_text), "{late:?}");
            assert_eq!(delegate.calls.get(), 1);
            true
        });
    }

    #[test]
    fn get_file_contents_without_virtual_file_at_index_is_dv_e_formatetc() {
        RunLoop::current();
        let data_object = DataObject::create(Vec::new());
        let file_contents = unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) };
        let format = make_format_with_tymed_index(file_contents, TYMED_ISTREAM, 5);
        let err = match unsafe { data_object.GetData(&format) } {
            Ok(_) => panic!("GetData returned a stream for a missing virtual file"),
            Err(err) => err,
        };
        assert_eq!(err.code(), DV_E_FORMATETC);
    }
}
