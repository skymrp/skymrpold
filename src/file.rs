use crate::paths;
use std::collections::HashMap;
use std::ffi::CStr;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::raw::{c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

lazy_static::lazy_static! {
    static ref FILE_MAP: Mutex<HashMap<u32, File>> = Mutex::new(HashMap::new());
    static ref FILE_COUNT: Mutex<u32> = Mutex::new(0);

    // Directory iteration state.
    static ref DIR_MAP: Mutex<HashMap<u32, DirState>> = Mutex::new(HashMap::new());
    static ref DIR_COUNT: Mutex<u32> = Mutex::new(0);
}

// Simulate readdir returning static memory. This is not thread-safe, but it
// preserves the behavior expected by the legacy C API.
static mut CURRENT_DIR_ENTRY_NAME: [u8; 256] = [0; 256];

struct DirState {
    entries: std::vec::IntoIter<fs::DirEntry>,
}

const MR_FILE_RDONLY: u32 = 1;
const MR_FILE_WRONLY: u32 = 2;
const MR_FILE_RDWR: u32 = 4;
const MR_FILE_CREATE: u32 = 8;
const MR_SUCCESS: i32 = 0;
const MR_FAILED: i32 = -1;

const MR_IS_INVALID: i32 = 0;
const MR_IS_FILE: i32 = 1;
const MR_IS_DIR: i32 = 2;

fn resolve_host_path(path: &str) -> PathBuf {
    let resolved = paths::resolve_mythroad_path(path);
    if resolved.is_absolute() || resolved.exists() {
        return resolved;
    }

    resolved
}

pub fn open_cstr(filename: &CStr, mode: u32) -> i32 {
    let path = match filename.to_str() {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let path = resolve_host_path(path);

    let mut options = OpenOptions::new();
    if mode == 0 || (mode & MR_FILE_RDONLY) != 0 {
        options.read(true);
    }
    if (mode & MR_FILE_WRONLY) != 0 {
        options.write(true);
    }
    if (mode & MR_FILE_RDWR) != 0 {
        options.read(true).write(true);
    }
    if (mode & MR_FILE_CREATE) != 0 {
        options.create(true);
    }

    match options.open(&path) {
        Ok(file) => {
            let mut count = FILE_COUNT.lock().unwrap();
            *count += 1;
            let fd = *count;

            let mut map = FILE_MAP.lock().unwrap();
            map.insert(fd, file);
            fd as i32
        }
        Err(_) => 0,
    }
}

pub fn close(fd: i32) -> i32 {
    let mut map = FILE_MAP.lock().unwrap();
    if map.remove(&(fd as u32)).is_some() {
        // file is closed when dropped
        MR_SUCCESS
    } else {
        MR_FAILED
    }
}

pub fn seek(fd: i32, pos: i32, method: c_int) -> i32 {
    let mut map = FILE_MAP.lock().unwrap();
    if let Some(file) = map.get_mut(&(fd as u32)) {
        let seek_from = match method {
            0 => SeekFrom::Start(pos as u64),   // SEEK_SET
            1 => SeekFrom::Current(pos as i64), // SEEK_CUR
            2 => SeekFrom::End(pos as i64),     // SEEK_END
            _ => return MR_FAILED,
        };
        match file.seek(seek_from) {
            Ok(_) => MR_SUCCESS,
            Err(_) => MR_FAILED,
        }
    } else {
        MR_FAILED
    }
}

pub fn read_into(fd: i32, buf: &mut [u8]) -> i32 {
    let mut map = FILE_MAP.lock().unwrap();
    if let Some(file) = map.get_mut(&(fd as u32)) {
        match file.read(buf) {
            Ok(bytes_read) => bytes_read as i32,
            Err(_) => MR_FAILED,
        }
    } else {
        MR_FAILED
    }
}

pub fn write_from(fd: i32, buf: &[u8]) -> i32 {
    let mut map = FILE_MAP.lock().unwrap();
    if let Some(file) = map.get_mut(&(fd as u32)) {
        match file.write(buf) {
            Ok(bytes_written) => bytes_written as i32,
            Err(_) => MR_FAILED,
        }
    } else {
        MR_FAILED
    }
}

pub fn rename_cstr(oldname: &CStr, newname: &CStr) -> i32 {
    let old_str = oldname.to_str().unwrap_or("");
    let new_str = newname.to_str().unwrap_or("");
    match fs::rename(resolve_host_path(old_str), resolve_host_path(new_str)) {
        Ok(_) => MR_SUCCESS,
        Err(_) => MR_FAILED,
    }
}

pub fn remove_cstr(filename: &CStr) -> i32 {
    let c_str = filename.to_str().unwrap_or("");
    match fs::remove_file(resolve_host_path(c_str)) {
        Ok(_) => MR_SUCCESS,
        Err(_) => MR_FAILED,
    }
}

pub fn get_len_cstr(filename: &CStr) -> i32 {
    let c_str = filename.to_str().unwrap_or("");
    match fs::metadata(resolve_host_path(c_str)) {
        Ok(meta) => meta.len() as i32,
        Err(_) => -1,
    }
}

pub fn mkdir_cstr(name: &CStr) -> i32 {
    let c_str = name.to_str().unwrap_or("");
    let path = resolve_host_path(c_str);
    if path.exists() {
        return MR_SUCCESS;
    }
    match fs::create_dir(path) {
        Ok(_) => MR_SUCCESS,
        Err(_) => MR_FAILED,
    }
}

pub fn rmdir_cstr(name: &CStr) -> i32 {
    let c_str = name.to_str().unwrap_or("");
    match fs::remove_dir(resolve_host_path(c_str)) {
        Ok(_) => MR_SUCCESS,
        Err(_) => MR_FAILED,
    }
}

pub fn info_cstr(filename: &CStr) -> i32 {
    let c_str = filename.to_str().unwrap_or("");
    match fs::metadata(resolve_host_path(c_str)) {
        Ok(meta) => {
            if meta.is_dir() {
                MR_IS_DIR
            } else if meta.is_file() {
                MR_IS_FILE
            } else {
                MR_IS_INVALID
            }
        }
        Err(_) => MR_IS_INVALID,
    }
}

pub fn opendir_cstr(name: &CStr) -> i32 {
    let c_str = name.to_str().unwrap_or("");
    match fs::read_dir(resolve_host_path(c_str)) {
        Ok(entries) => {
            let valid_entries: Vec<fs::DirEntry> = entries.filter_map(Result::ok).collect();
            let mut count = DIR_COUNT.lock().unwrap();
            *count += 1;
            let fd = *count;

            let mut map = DIR_MAP.lock().unwrap();
            map.insert(
                fd,
                DirState {
                    entries: valid_entries.into_iter(),
                },
            );
            fd as i32
        }
        Err(_) => MR_FAILED,
    }
}

pub fn readdir_name(fd: i32) -> Option<Vec<u8>> {
    let mut map = DIR_MAP.lock().unwrap();
    let state = map.get_mut(&(fd as u32))?;
    let entry = state.entries.next()?;
    Some(entry.file_name().as_encoded_bytes().to_vec())
}

#[no_mangle]
pub extern "C" fn my_open(filename: *const c_char, mode: u32) -> i32 {
    if filename.is_null() {
        return 0; // 0 means failed in my_open
    }
    open_cstr(unsafe { CStr::from_ptr(filename) }, mode)
}

#[no_mangle]
pub extern "C" fn my_close(fd: i32) -> i32 {
    close(fd)
}

#[no_mangle]
pub extern "C" fn my_seek(fd: i32, pos: i32, method: c_int) -> i32 {
    seek(fd, pos, method)
}

#[no_mangle]
pub extern "C" fn my_read(fd: i32, buf: *mut c_void, len: u32) -> i32 {
    if buf.is_null() && len != 0 {
        return MR_FAILED;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), len as usize) };
    read_into(fd, buf)
}

#[no_mangle]
pub extern "C" fn my_write(fd: i32, buf: *const c_void, len: u32) -> i32 {
    if buf.is_null() && len != 0 {
        return MR_FAILED;
    }
    let buf = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len as usize) };
    write_from(fd, buf)
}

#[no_mangle]
pub extern "C" fn my_rename(oldname: *const c_char, newname: *const c_char) -> i32 {
    if oldname.is_null() || newname.is_null() {
        return MR_FAILED;
    }
    rename_cstr(unsafe { CStr::from_ptr(oldname) }, unsafe {
        CStr::from_ptr(newname)
    })
}

#[no_mangle]
pub extern "C" fn my_remove(filename: *const c_char) -> i32 {
    if filename.is_null() {
        return MR_FAILED;
    }
    remove_cstr(unsafe { CStr::from_ptr(filename) })
}

#[no_mangle]
pub extern "C" fn my_getLen(filename: *const c_char) -> i32 {
    if filename.is_null() {
        return -1;
    }
    get_len_cstr(unsafe { CStr::from_ptr(filename) })
}

#[no_mangle]
pub extern "C" fn my_mkDir(name: *const c_char) -> i32 {
    if name.is_null() {
        return MR_FAILED;
    }
    mkdir_cstr(unsafe { CStr::from_ptr(name) })
}

#[no_mangle]
pub extern "C" fn my_rmDir(name: *const c_char) -> i32 {
    if name.is_null() {
        return MR_FAILED;
    }
    rmdir_cstr(unsafe { CStr::from_ptr(name) })
}

#[no_mangle]
pub extern "C" fn my_info(filename: *const c_char) -> i32 {
    if filename.is_null() {
        return MR_IS_INVALID;
    }
    info_cstr(unsafe { CStr::from_ptr(filename) })
}

#[no_mangle]
pub extern "C" fn my_opendir(name: *const c_char) -> i32 {
    if name.is_null() {
        return MR_FAILED;
    }
    opendir_cstr(unsafe { CStr::from_ptr(name) })
}

#[no_mangle]
pub extern "C" fn my_readdir(fd: i32) -> *mut c_char {
    if let Some(name_bytes) = readdir_name(fd) {
        let len = std::cmp::min(name_bytes.len(), 255);
        unsafe {
            std::ptr::copy_nonoverlapping(
                name_bytes.as_ptr(),
                CURRENT_DIR_ENTRY_NAME.as_mut_ptr(),
                len,
            );
            CURRENT_DIR_ENTRY_NAME[len] = 0; // null terminator
            return CURRENT_DIR_ENTRY_NAME.as_mut_ptr() as *mut c_char;
        }
    }
    std::ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn my_closedir(fd: i32) -> i32 {
    let mut map = DIR_MAP.lock().unwrap();
    if map.remove(&(fd as u32)).is_some() {
        MR_SUCCESS
    } else {
        MR_FAILED
    }
}

pub fn write_file_cstr(filename: &CStr, mut data: &[u8]) {
    if data.is_empty() {
        return;
    }
    let fd = open_cstr(filename, MR_FILE_CREATE | MR_FILE_RDWR);
    if fd == 0 {
        return;
    }

    while !data.is_empty() {
        let chunk_size = data.len().min(1000);
        let w_len = write_from(fd, &data[..chunk_size]);
        if w_len == MR_FAILED {
            break;
        }
        if w_len == 0 {
            break;
        }
        data = &data[w_len as usize..];
    }
    close(fd);
}

#[no_mangle]
pub extern "C" fn writeFile(filename: *const c_char, data: *const c_void, length: u32) {
    if filename.is_null() || data.is_null() || length == 0 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(data.cast::<u8>(), length as usize) };
    write_file_cstr(unsafe { CStr::from_ptr(filename) }, data);
}

// C's original readFile implementation from fileLib.c doesn't take filelen parameter,
// that was my misunderstanding. The other.c implementation does, but fileLib.c doesn't.
pub fn read_file_alloc_cstr(filename: &CStr) -> *mut c_char {
    let data = match read_file_cstr(filename) {
        Ok(data) => data,
        Err(_) => return std::ptr::null_mut(),
    };
    if data.is_empty() {
        return std::ptr::null_mut();
    }

    let p = unsafe { libc::malloc(data.len()) } as *mut u8;
    if p.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
    }
    p as *mut c_char
}

pub fn read_file_cstr(filename: &CStr) -> Result<Vec<u8>, String> {
    let path = match filename.to_str() {
        Ok(path) => path,
        Err(err) => return Err(format!("path is not UTF-8: {err}")),
    };
    let resolved = resolve_host_path(path);
    fs::read(&resolved).map_err(|err| format!("read {} failed: {err}", resolved.display()))
}

#[no_mangle]
pub extern "C" fn readFile(filename: *const c_char) -> *mut c_char {
    if filename.is_null() {
        return std::ptr::null_mut();
    }
    read_file_alloc_cstr(unsafe { CStr::from_ptr(filename) })
}
