//! Host symbols required by the external Mythroad C VM.
//!
//! These are intentionally thin compatibility shims for the direct-MRP
//! experiment. The long-term direction is to route them into MythroadServices.

use std::alloc::{alloc, dealloc, realloc, Layout};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::slice;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{cmp, ptr};

use libc::uint16_t;
use skymrp_loader::MrpPackage;

const ALIGN: usize = mythroad::ffi::SYS_MIN_ALIGN;

thread_local! {
    static CURRENT_MRP_PACKAGE: RefCell<Option<MrpPackage>> = RefCell::new(None);
    static CURRENT_MR_STATE: RefCell<Option<*mut mythroad::ffi::mrp_State>> = RefCell::new(None);
    static LAST_PCALL_ERROR: RefCell<Option<String>> = RefCell::new(None);
    static CURRENT_FILE_ROOT: RefCell<Option<PathBuf>> = RefCell::new(None);
    static OPEN_FILES: RefCell<HostFiles> = RefCell::new(HostFiles::default());
}

struct CurrentRuntimeGuard {
    previous: Option<MrpPackage>,
    previous_state: Option<*mut mythroad::ffi::mrp_State>,
    previous_file_root: Option<PathBuf>,
}

#[derive(Default)]
struct HostFiles {
    next_handle: c_int,
    files: HashMap<c_int, File>,
}

impl Drop for CurrentRuntimeGuard {
    fn drop(&mut self) {
        CURRENT_MRP_PACKAGE.with(|current| {
            current.replace(self.previous.take());
        });
        CURRENT_MR_STATE.with(|current| {
            current.replace(self.previous_state.take());
        });
        CURRENT_FILE_ROOT.with(|current| {
            current.replace(self.previous_file_root.take());
        });
        OPEN_FILES.with(|files| files.borrow_mut().files.clear());
    }
}

pub fn with_mrp_runtime<T>(
    package: MrpPackage,
    state: *mut mythroad::ffi::mrp_State,
    file_root: PathBuf,
    f: impl FnOnce() -> T,
) -> T {
    let previous = CURRENT_MRP_PACKAGE.with(|current| current.replace(Some(package)));
    let previous_state = CURRENT_MR_STATE.with(|current| current.replace(Some(state)));
    let previous_file_root = CURRENT_FILE_ROOT.with(|current| current.replace(Some(file_root)));
    LAST_PCALL_ERROR.with(|error| error.replace(None));
    OPEN_FILES.with(|files| {
        let mut files = files.borrow_mut();
        files.next_handle = 1;
        files.files.clear();
    });
    let _guard = CurrentRuntimeGuard {
        previous,
        previous_state,
        previous_file_root,
    };
    f()
}

pub fn take_last_pcall_error() -> Option<String> {
    LAST_PCALL_ERROR.with(|error| error.take())
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
pub extern "C" fn _mr_pcall(nargs: c_int, nresults: c_int) -> c_int {
    let Some(state) = CURRENT_MR_STATE.with(|current| *current.borrow()) else {
        log!("mythroad C VM: _mr_pcall({nargs}, {nresults}) without current state");
        return -1;
    };

    unsafe {
        ensure_sysinfo_global(state);
    }
    let status = unsafe { mythroad::ffi::mrp_pcall(state, nargs, nresults, 0) };
    if status != 0 {
        let message = unsafe {
            let ptr = mythroad::ffi::mrp_tostring(state, -1);
            if ptr.is_null() {
                "unknown MR VM error".to_string()
            } else {
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            }
        };
        let sysinfo_type = unsafe { global_type_name(state, "sysinfo") };
        log!(
            "mythroad C VM: _mr_pcall({nargs}, {nresults}) failed: {message}; sysinfo={sysinfo_type}"
        );
        LAST_PCALL_ERROR.with(|error| error.replace(Some(message)));
    } else {
        LAST_PCALL_ERROR.with(|error| error.replace(None));
    }
    status
}

unsafe fn ensure_sysinfo_global(state: *mut mythroad::ffi::mrp_State) {
    let name = CString::new("sysinfo").expect("global name has no interior NUL");
    mythroad::ffi::mrp_getglobal(state, name.as_ptr());
    let missing = mythroad::ffi::mrp_isnoneornil(state, -1);
    mythroad::ffi::mrp_pop(state, 1);
    if !missing {
        return;
    }

    push_sysinfo_table(state);
    mythroad::ffi::mrp_setglobal(state, name.as_ptr());
    log!("mythroad C VM: restored global sysinfo");
}

unsafe fn push_sysinfo_table(state: *mut mythroad::ffi::mrp_State) {
    mythroad::ffi::mrp_newtable(state);
    set_table_string(state, "vmver", "1968");
    set_table_string(state, "packname", "dsm_gm.mrp");
    set_table_string(state, "PackName", "dsm_gm.mrp");
    set_table_string(state, "IMEI", "000000000000000");
    set_table_string(state, "IMSI", "000000000000000");
    set_table_string(state, "hsman", "skymrp");
    set_table_string(state, "hstype", "skymrp");
    set_table_number(state, "ScreenW", 240.0);
    set_table_number(state, "ScreenH", 320.0);
    set_table_number(state, "scrw", 240.0);
    set_table_number(state, "scrh", 320.0);
    set_table_number(state, "ChineseWidth", 16.0);
    set_table_number(state, "ChineseHigh", 16.0);
    set_table_number(state, "EnglishWidth", 8.0);
    set_table_number(state, "EnglishHigh", 16.0);
    set_table_number(state, "chw", 16.0);
    set_table_number(state, "chh", 16.0);
    set_table_number(state, "ascw", 8.0);
    set_table_number(state, "asch", 16.0);
    set_table_number(state, "hsver", 0.0);
}

unsafe fn set_table_string(state: *mut mythroad::ffi::mrp_State, key: &str, value: &str) {
    let key = CString::new(key).expect("table key has no interior NUL");
    mythroad::ffi::mrp_pushstring(state, key.as_ptr());
    let value = CString::new(value).expect("table value has no interior NUL");
    mythroad::ffi::mrp_pushstring(state, value.as_ptr());
    mythroad::ffi::mrp_settable(state, -3);
}

unsafe fn set_table_number(state: *mut mythroad::ffi::mrp_State, key: &str, value: f64) {
    let key = CString::new(key).expect("table key has no interior NUL");
    mythroad::ffi::mrp_pushstring(state, key.as_ptr());
    mythroad::ffi::mrp_pushnumber(state, value);
    mythroad::ffi::mrp_settable(state, -3);
}

unsafe fn global_type_name(state: *mut mythroad::ffi::mrp_State, name: &str) -> String {
    let name = CString::new(name).expect("global name has no interior NUL");
    mythroad::ffi::mrp_getglobal(state, name.as_ptr());
    let type_id = mythroad::ffi::mrp_type(state, -1);
    let type_name = mythroad::ffi::mrp_typename(state, type_id);
    let type_name = if type_name.is_null() {
        "unknown".to_string()
    } else {
        CStr::from_ptr(type_name).to_string_lossy().into_owned()
    };
    mythroad::ffi::mrp_pop(state, 1);
    type_name
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

#[no_mangle]
pub extern "C" fn mr_open(filename: *const c_char, mode: u32) -> c_int {
    let filename = c_string_lossy(filename);
    let Some(path) = resolve_host_file_path(&filename) else {
        log!("mythroad C VM: mr_open({filename:?}, mode=0x{mode:X}) -> 0");
        return 0;
    };

    let mut options = OpenOptions::new();
    options.read(mode & 0x1 != 0 || mode & 0x4 != 0);
    options.write(mode & 0x2 != 0 || mode & 0x4 != 0);
    if mode & 0x8 != 0 || mode & 0x10 != 0 {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        options.create(true);
    }
    if mode & 0x10 != 0 {
        options.truncate(true);
    }

    let Ok(file) = options.open(&path) else {
        log!(
            "mythroad C VM: mr_open({filename:?}, mode=0x{mode:X}) path={} -> 0",
            path.display()
        );
        return 0;
    };

    let handle = OPEN_FILES.with(|files| {
        let mut files = files.borrow_mut();
        files.next_handle = cmp::max(files.next_handle, 1);
        let handle = files.next_handle;
        files.next_handle = files.next_handle.saturating_add(1);
        files.files.insert(handle, file);
        handle
    });
    log!(
        "mythroad C VM: mr_open({filename:?}, mode=0x{mode:X}) path={} -> {handle}",
        path.display()
    );
    handle
}

#[no_mangle]
pub extern "C" fn mr_close(file: c_int) -> c_int {
    OPEN_FILES.with(|files| {
        files.borrow_mut().files.remove(&file);
    });
    log!("mythroad C VM: mr_close({file}) -> 0");
    0
}

#[no_mangle]
pub extern "C" fn mr_read(file: c_int, buffer: *mut c_void, len: u32) -> c_int {
    if buffer.is_null() || len == 0 {
        return 0;
    }

    OPEN_FILES.with(|files| {
        let mut files = files.borrow_mut();
        let Some(file_ref) = files.files.get_mut(&file) else {
            log!("mythroad C VM: mr_read({file}, len={len}) invalid handle -> -1");
            return -1;
        };
        let buffer = unsafe { slice::from_raw_parts_mut(buffer.cast::<u8>(), len as usize) };
        match file_ref.read(buffer) {
            Ok(read) => read as c_int,
            Err(err) => {
                log!("mythroad C VM: mr_read({file}, len={len}) failed: {err}");
                -1
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn mr_write(file: c_int, buffer: *const c_void, len: u32) -> c_int {
    if buffer.is_null() || len == 0 {
        return 0;
    }

    OPEN_FILES.with(|files| {
        let mut files = files.borrow_mut();
        let Some(file_ref) = files.files.get_mut(&file) else {
            log!("mythroad C VM: mr_write({file}, len={len}) invalid handle -> -1");
            return -1;
        };
        let buffer = unsafe { slice::from_raw_parts(buffer.cast::<u8>(), len as usize) };
        match file_ref.write(buffer) {
            Ok(written) => written as c_int,
            Err(err) => {
                log!("mythroad C VM: mr_write({file}, len={len}) failed: {err}");
                -1
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn mr_seek(file: c_int, pos: c_int, method: c_int) -> c_int {
    OPEN_FILES.with(|files| {
        let mut files = files.borrow_mut();
        let Some(file_ref) = files.files.get_mut(&file) else {
            log!("mythroad C VM: mr_seek({file}, pos={pos}, method={method}) invalid handle -> -1");
            return -1;
        };
        let seek_from = match method {
            0 => SeekFrom::Start(pos.max(0) as u64),
            1 => SeekFrom::Current(pos as i64),
            2 => SeekFrom::End(pos as i64),
            _ => return -1,
        };
        match file_ref.seek(seek_from) {
            Ok(_) => 0,
            Err(err) => {
                log!("mythroad C VM: mr_seek({file}, pos={pos}, method={method}) failed: {err}");
                -1
            }
        }
    })
}

#[no_mangle]
pub extern "C" fn mr_getLen(filename: *const c_char) -> c_int {
    let filename = c_string_lossy(filename);
    let Some(path) = resolve_host_file_path(&filename) else {
        log!("mythroad C VM: mr_getLen({filename:?}) -> 0");
        return 0;
    };
    match std::fs::metadata(&path) {
        Ok(metadata) => {
            let len = metadata.len().min(c_int::MAX as u64) as c_int;
            log!(
                "mythroad C VM: mr_getLen({filename:?}) path={} -> {len}",
                path.display()
            );
            len
        }
        Err(_) => {
            log!(
                "mythroad C VM: mr_getLen({filename:?}) path={} -> 0",
                path.display()
            );
            0
        }
    }
}

fn c_string_lossy(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    }
}

fn resolve_host_file_path(filename: &str) -> Option<PathBuf> {
    let root = CURRENT_FILE_ROOT.with(|root| root.borrow().clone())?;
    let normalized = normalize_host_filename(filename);
    if normalized.is_empty() {
        return None;
    }
    Some(root.join(Path::new(&normalized)))
}

fn normalize_host_filename(filename: &str) -> String {
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
    let parts = normalized
        .split('/')
        .filter(|part| !part.is_empty() && *part != "." && *part != "..");
    parts.collect::<Vec<_>>().join("/")
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
    mr_plat,
    mr_platDrawCharReal,
    mr_platEx,
    mr_playSound,
    mr_recv,
    mr_recvfrom,
    mr_remove,
    mr_rename,
    mr_rmDir,
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
);
