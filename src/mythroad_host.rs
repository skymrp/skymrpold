//! Host symbols required by the external Mythroad C VM.
//!
//! These are intentionally thin compatibility shims for the direct-MRP
//! experiment. The long-term direction is to route them into MythroadServices.

use std::alloc::{alloc, dealloc, realloc, Layout};
use std::cell::RefCell;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr;
use std::slice;
use std::time::{SystemTime, UNIX_EPOCH};

use libc::uint16_t;
use skymrp_loader::MrpPackage;

const ALIGN: usize = mythroad::ffi::SYS_MIN_ALIGN;

thread_local! {
    static CURRENT_MRP_PACKAGE: RefCell<Option<MrpPackage>> = RefCell::new(None);
}

struct CurrentMrpPackageGuard {
    previous: Option<MrpPackage>,
}

impl Drop for CurrentMrpPackageGuard {
    fn drop(&mut self) {
        CURRENT_MRP_PACKAGE.with(|current| {
            current.replace(self.previous.take());
        });
    }
}

pub fn with_mrp_package<T>(package: MrpPackage, f: impl FnOnce() -> T) -> T {
    let previous = CURRENT_MRP_PACKAGE.with(|current| current.replace(Some(package)));
    let _guard = CurrentMrpPackageGuard { previous };
    f()
}

fn layout(size: u32) -> Layout {
    Layout::from_size_align((size as usize).max(1), ALIGN).expect("valid allocation layout")
}

#[no_mangle]
pub extern "C" fn mr_malloc(len: u32) -> *mut c_void {
    unsafe { alloc(layout(len)).cast::<c_void>() }
}

#[no_mangle]
pub extern "C" fn mr_free(ptr: *mut c_void, len: u32) {
    if !ptr.is_null() {
        unsafe { dealloc(ptr.cast::<u8>(), layout(len)) };
    }
}

#[no_mangle]
pub extern "C" fn mr_realloc(ptr: *mut c_void, old_len: u32, len: u32) -> *mut c_void {
    if ptr.is_null() {
        return mr_malloc(len);
    }
    if len == 0 {
        mr_free(ptr, old_len);
        return ptr::null_mut();
    }
    unsafe { realloc(ptr.cast::<u8>(), layout(old_len), len as usize).cast::<c_void>() }
}

#[no_mangle]
pub extern "C" fn mr_printf(format: *const c_char) {
    if format.is_null() {
        return;
    }
    let message = unsafe { CStr::from_ptr(format) }.to_string_lossy();
    log!("mythroad C VM: {message}");
}

#[no_mangle]
pub extern "C" fn mr_ferrno() -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn _mr_pcall(_nargs: c_int, _nresults: c_int) -> c_int {
    -1
}

#[no_mangle]
pub extern "C" fn _mr_readFile(
    filename: *const c_char,
    file_len: *mut c_int,
    lookfor: c_int,
) -> *mut c_void {
    if !file_len.is_null() {
        unsafe { *file_len = 0 };
    }

    if filename.is_null() {
        return ptr::null_mut();
    }

    let filename = unsafe { CStr::from_ptr(filename) }.to_string_lossy();
    let Some(data) = CURRENT_MRP_PACKAGE.with(|current| {
        current
            .borrow()
            .as_ref()
            .and_then(|package| read_package_file(package, &filename))
    }) else {
        log!("mythroad C VM: _mr_readFile({filename:?}) not found");
        return ptr::null_mut();
    };

    if !file_len.is_null() {
        unsafe { *file_len = data.len() as c_int };
    }

    if lookfor == 1 {
        return 1usize as *mut c_void;
    }

    let ptr = mr_malloc(data.len() as u32);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    unsafe {
        slice::from_raw_parts_mut(ptr.cast::<u8>(), data.len()).copy_from_slice(&data);
    }
    log!(
        "mythroad C VM: _mr_readFile({filename:?}, lookfor={lookfor}) -> {} bytes",
        data.len()
    );
    ptr
}

fn read_package_file(package: &MrpPackage, filename: &str) -> Option<Vec<u8>> {
    for candidate in filename_candidates(filename) {
        match package.read_file_unzipped(&candidate) {
            Ok(Some(data)) => return Some(data),
            Ok(None) => {}
            Err(err) => {
                log!("mythroad C VM: failed to read {candidate:?} from MRP: {err}");
                return None;
            }
        }
    }
    None
}

fn filename_candidates(filename: &str) -> Vec<String> {
    let mut candidates = Vec::new();
    let mut normalized = filename.replace('\\', "/");
    while let Some(stripped) = normalized.strip_prefix('@') {
        normalized = stripped.to_string();
    }
    while let Some(stripped) = normalized.strip_prefix('/') {
        normalized = stripped.to_string();
    }
    while let Some(stripped) = normalized.strip_prefix("./") {
        normalized = stripped.to_string();
    }

    push_candidate(&mut candidates, filename);
    push_candidate(&mut candidates, &normalized);

    if let Some(name) = normalized.rsplit('/').next() {
        push_candidate(&mut candidates, name);
    }

    candidates
}

fn push_candidate(candidates: &mut Vec<String>, value: &str) {
    if !value.is_empty() && !candidates.iter().any(|candidate| candidate == value) {
        candidates.push(value.to_string());
    }
}

#[no_mangle]
pub extern "C" fn mr_wstrlen(text: *const c_char) -> c_int {
    if text.is_null() {
        return 0;
    }
    let bytes = unsafe { CStr::from_ptr(text).to_bytes() };
    (bytes.len() / 2) as c_int
}

#[no_mangle]
pub extern "C" fn mr_Gb2312toUnicode(_state: *mut mythroad::ffi::mrp_State) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn _mr_GetDatetime(_datetime: *mut c_void) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn _mr_GetSysInfo(_info: *mut c_void) -> c_int {
    0
}

#[no_mangle]
pub extern "C" fn mr_getTime() -> c_int {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as c_int)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn mr_drawBitmap(_bmp: *mut uint16_t, x: i16, y: i16, w: u16, h: u16) {
    log!("mythroad C VM: mr_drawBitmap x={x} y={y} w={w} h={h}");
}

#[no_mangle]
pub extern "C" fn mr_bufToScreen(x: i16, y: i16, w: u16, h: u16) {
    log!("mythroad C VM: mr_bufToScreen x={x} y={y} w={w} h={h}");
}

#[no_mangle]
pub extern "C" fn mr_drawText(text: *mut c_char, x: i16, y: i16, color: u32) {
    let text = if text.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned()
    };
    log!("mythroad C VM: mr_drawText text={text:?} x={x} y={y} color=0x{color:X}");
}

#[no_mangle]
pub extern "C" fn mr_timerStart(ms: u16) -> c_int {
    log!("mythroad C VM: mr_timerStart({ms})");
    0
}

#[no_mangle]
pub extern "C" fn mr_timerStop() -> c_int {
    log!("mythroad C VM: mr_timerStop()");
    0
}

macro_rules! stub_i32 {
    ($($name:ident),+ $(,)?) => {
        $(
            #[no_mangle]
            pub extern "C" fn $name() -> c_int {
                -1
            }
        )+
    };
}

stub_i32!(
    mr_asyn_read,
    mr_asyn_write,
    mr_call,
    mr_close,
    mr_closeNetwork,
    mr_closeSocket,
    mr_connect,
    mr_connectWAP,
    mr_dialogCreate,
    mr_dialogRefresh,
    mr_dialogRelease,
    mr_editCreate,
    mr_editGetText,
    mr_editRelease,
    mr_exit,
    mr_findGetNext,
    mr_findStart,
    mr_findStop,
    mr_getCharBitmap,
    mr_getDatetime,
    mr_getHostByName,
    mr_getLen,
    mr_getNetworkID,
    mr_getScreenBuf,
    mr_getScreenInfo,
    mr_getUserInfo,
    mr_info,
    mr_initNetwork,
    mr_menuCreate,
    mr_menuRefresh,
    mr_menuRelease,
    mr_menuSetFocus,
    mr_menuSetItem,
    mr_menuShow,
    mr_mkDir,
    mr_open,
    mr_plat,
    mr_platDrawCharReal,
    mr_platEx,
    mr_playSound,
    mr_read,
    mr_recv,
    mr_recvfrom,
    mr_remove,
    mr_rename,
    mr_rmDir,
    mr_seek,
    mr_send,
    mr_sendSms,
    mr_sendto,
    mr_sleep,
    mr_socket,
    mr_startShake,
    mr_stopShake,
    mr_stopSound,
    mr_textCreate,
    mr_textRefresh,
    mr_textRelease,
    mr_winCreate,
    mr_winRelease,
    mr_write,
);
