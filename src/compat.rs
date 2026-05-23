use crate::bootstrap;
use crate::file;
use crate::mem::{Mem, MutPtr, MutVoidPtr, Ptr};
use libc::{c_char, c_int, c_uchar, c_void, size_t};
use std::ffi::CStr;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};

const MR_SUCCESS: c_int = 0;
const MR_FAILED: c_int = -1;

const MR_FILE_RDWR: u32 = 4;
const MR_FILE_CREATE: u32 = 8;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MrDatetime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

#[no_mangle]
pub static mut LG_mem_min: u32 = 0;
#[no_mangle]
pub static mut LG_mem_top: u32 = 0;
#[no_mangle]
pub static mut LG_mem_base: *mut c_char = ptr::null_mut();
#[no_mangle]
pub static mut LG_mem_len: u32 = 0;
#[no_mangle]
pub static mut Origin_LG_mem_base: *mut c_char = ptr::null_mut();
#[no_mangle]
pub static mut Origin_LG_mem_len: u32 = 0;
#[no_mangle]
pub static mut LG_mem_end: *mut c_char = ptr::null_mut();
#[no_mangle]
pub static mut LG_mem_left: u32 = 0;

fn real_lg_mem_size(len: u32) -> u32 {
    len.wrapping_add(7) & 0xfffffff8
}

fn alloc_guest(mem: &mut Mem, len: u32) -> Option<u32> {
    let addr = mem.try_alloc(len).map(|ptr| ptr.to_bits())?;
    unsafe {
        LG_mem_left = LG_mem_left.saturating_sub(len);
        LG_mem_min = LG_mem_min.min(LG_mem_left);
        LG_mem_top = LG_mem_top
            .max(addr.saturating_sub(bootstrap::to_mrp_mem_addr(mem, LG_mem_base.cast())));
    }
    Some(addr)
}

fn free_guest(mem: &mut Mem, addr: u32) -> u32 {
    if addr == 0 {
        return 0;
    }
    let freed = mem.free_with_size(MutVoidPtr::from_bits(addr));
    unsafe {
        LG_mem_left = LG_mem_left.saturating_add(freed).min(LG_mem_len);
    }
    freed
}

fn guest_host_ptr(mem: &mut Mem, addr: u32, count: u32) -> *mut c_void {
    bootstrap::guest_host_ptr_mut(mem, addr, count)
}

pub fn init_memory_manager(mem: &mut Mem, base_address: u32, len: u32) {
    log!("init_memory_manager: baseAddress:0x{base_address:X} len: 0x{len:X}");
    unsafe {
        Origin_LG_mem_base = bootstrap::guest_host_ptr_mut(mem, base_address, 1) as *mut c_char;
        Origin_LG_mem_len = len;

        LG_mem_base = ((Origin_LG_mem_base.add(3) as usize) & !3usize) as *mut c_char;
        LG_mem_len =
            (Origin_LG_mem_len - (LG_mem_base as usize - Origin_LG_mem_base as usize) as u32) & !3;
        LG_mem_end = LG_mem_base.add(LG_mem_len as usize);
        LG_mem_left = LG_mem_len;
        LG_mem_min = LG_mem_len;
        LG_mem_top = 0;
    }
}

#[no_mangle]
pub extern "C" fn printMemoryInfo() {
    unsafe {
        let mem_len = LG_mem_len;
        let mem_min = LG_mem_min;
        let mem_left = LG_mem_left;
        let mem_top = LG_mem_top;
        let mem_base = LG_mem_base;
        let mem_end = LG_mem_end;
        let origin_base = Origin_LG_mem_base;
        let origin_len = Origin_LG_mem_len;
        log!(
            ".......total:{}, min:{}, free:{}, top:{}",
            mem_len,
            mem_min,
            mem_left,
            mem_top
        );
        log!(".......base:{:p}, end:{:p}", mem_base, mem_end);
        log!(".......obase:{:p}, olen:{}", origin_base, origin_len);
    }
}

#[no_mangle]
pub extern "C" fn my_malloc(mut len: u32) -> *mut c_void {
    log!("my_malloc called without guest memory context");
    let _ = &mut len;
    ptr::null_mut()
}

pub fn my_malloc_in(mem: &mut Mem, mut len: u32) -> *mut c_void {
    len = real_lg_mem_size(len);
    if len == 0 {
        log!("my_malloc invalid memory request");
        return ptr::null_mut();
    }
    if unsafe { len >= LG_mem_left } {
        log!("my_malloc no memory");
        return ptr::null_mut();
    }
    let Some(addr) = alloc_guest(mem, len) else {
        log!("my_malloc no memory");
        return ptr::null_mut();
    };
    guest_host_ptr(mem, addr, len)
}

#[no_mangle]
pub extern "C" fn my_free(p: *mut c_void, _len: u32) {
    if !p.is_null() {
        log!("my_free called without guest memory context");
    }
}

pub fn my_free_in(mem: &mut Mem, p: *mut c_void, _len: u32) {
    if p.is_null() {
        return;
    }
    let addr = bootstrap::to_mrp_mem_addr(mem, p);
    let _ = free_guest(mem, addr);
}

#[no_mangle]
pub extern "C" fn my_realloc(p: *mut c_void, oldlen: u32, len: u32) -> *mut c_void {
    let _ = (p, oldlen, len);
    log!("my_realloc called without guest memory context");
    ptr::null_mut()
}

pub fn my_realloc_in(mem: &mut Mem, p: *mut c_void, oldlen: u32, len: u32) -> *mut c_void {
    if p.is_null() {
        return my_malloc_in(mem, len);
    }
    if len == 0 {
        my_free_in(mem, p, oldlen);
        return ptr::null_mut();
    }
    let new_len = real_lg_mem_size(len);
    let Some(new_addr) = alloc_guest(mem, new_len) else {
        return ptr::null_mut();
    };
    let old_addr = bootstrap::to_mrp_mem_addr(mem, p);
    mem.memmove(
        MutPtr::<c_void>::from_bits(new_addr),
        Ptr::<c_void, false>::from_bits(old_addr),
        oldlen.min(len),
    );
    let _ = free_guest(mem, old_addr);
    guest_host_ptr(mem, new_addr, len)
}

pub fn malloc_ext_in(mem: &mut Mem, len: u32) -> *mut c_void {
    let ptr = malloc_ext_guest_in(mem, len);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    guest_host_ptr(mem, ptr.to_bits(), len)
}

pub fn malloc_ext_guest_in(mem: &mut Mem, len: u32) -> MutVoidPtr {
    if len == 0 {
        return MutVoidPtr::null();
    }
    let total = len + size_of::<u32>() as u32;
    let Some(header_addr) = alloc_guest(mem, real_lg_mem_size(total)) else {
        return MutVoidPtr::null();
    };
    let header = MutPtr::<u32>::from_bits(header_addr);
    mem.write(header, len);
    MutVoidPtr::from_bits(header_addr + size_of::<u32>() as u32)
}

#[no_mangle]
pub extern "C" fn my_mallocExt(len: u32) -> *mut c_void {
    let _ = len;
    log!("my_mallocExt called without guest memory context");
    ptr::null_mut()
}

pub fn malloc_ext_zeroed_in(mem: &mut Mem, len: u32) -> *mut c_void {
    let ptr = malloc_ext_zeroed_guest_in(mem, len);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    guest_host_ptr(mem, ptr.to_bits(), len)
}

pub fn malloc_ext_zeroed_guest_in(mem: &mut Mem, len: u32) -> MutVoidPtr {
    let ptr = malloc_ext_guest_in(mem, len);
    if !ptr.is_null() {
        mem.bytes_at_mut(ptr.cast::<u8>(), len).fill(0);
    }
    ptr
}

#[no_mangle]
pub extern "C" fn my_mallocExt0(len: u32) -> *mut c_void {
    let _ = len;
    log!("my_mallocExt0 called without guest memory context");
    ptr::null_mut()
}

pub fn free_ext_in(mem: &mut Mem, p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let payload = bootstrap::to_mrp_mem_addr(mem, p);
    free_ext_guest_in(mem, MutVoidPtr::from_bits(payload));
}

pub fn free_ext_guest_in(mem: &mut Mem, ptr: MutVoidPtr) {
    let payload = ptr.to_bits();
    if payload == 0 {
        return;
    }
    let header = MutPtr::<u32>::from_bits(payload - size_of::<u32>() as u32);
    let _len: u32 = mem.read(header.cast_const());
    let _ = free_guest(mem, header.to_bits());
}

#[no_mangle]
pub extern "C" fn my_freeExt(p: *mut c_void) {
    if !p.is_null() {
        log!("my_freeExt called without guest memory context");
    }
}

pub fn realloc_ext_in(mem: &mut Mem, p: *mut c_void, new_len: u32) -> *mut c_void {
    let ptr = if p.is_null() {
        MutVoidPtr::null()
    } else {
        MutVoidPtr::from_bits(bootstrap::to_mrp_mem_addr(mem, p))
    };
    let new_ptr = realloc_ext_guest_in(mem, ptr, new_len);
    if new_ptr.is_null() {
        return ptr::null_mut();
    }
    guest_host_ptr(mem, new_ptr.to_bits(), new_len)
}

pub fn realloc_ext_guest_in(mem: &mut Mem, ptr: MutVoidPtr, new_len: u32) -> MutVoidPtr {
    let payload = ptr.to_bits();
    if ptr.is_null() {
        return malloc_ext_guest_in(mem, new_len);
    }
    if new_len == 0 {
        free_ext_guest_in(mem, ptr);
        return MutVoidPtr::null();
    }
    let header = MutPtr::<u32>::from_bits(payload - size_of::<u32>() as u32);
    let old_len = mem.read(header.cast_const());
    let new_block = malloc_ext_guest_in(mem, new_len);
    if new_block.is_null() {
        return new_block;
    }
    mem.memmove(
        new_block,
        Ptr::<c_void, false>::from_bits(payload),
        old_len.min(new_len),
    );
    free_ext_guest_in(mem, ptr);
    new_block
}

#[no_mangle]
pub extern "C" fn my_reallocExt(p: *mut c_void, new_len: u32) -> *mut c_void {
    let _ = (p, new_len);
    log!("my_reallocExt called without guest memory context");
    ptr::null_mut()
}

#[no_mangle]
pub extern "C" fn printScreen(filename: *mut c_char, buf: *mut u16) {
    const BMP_HEADER: [u8; 70] = [
        0x42, 0x4D, 0x48, 0x58, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46, 0x00, 0x00, 0x00, 0x38,
        0x00, 0x00, 0x00, 0xF0, 0x00, 0x00, 0x00, 0xC0, 0xFE, 0xFF, 0xFF, 0x01, 0x00, 0x10, 0x00,
        0x03, 0x00, 0x00, 0x00, 0x02, 0x58, 0x02, 0x00, 0x12, 0x0B, 0x00, 0x00, 0x12, 0x0B, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xF8, 0x00, 0x00, 0xE0, 0x07, 0x00, 0x00,
        0x1F, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];

    let fd = file::my_open(filename, MR_FILE_CREATE | MR_FILE_RDWR);
    file::my_write(
        fd,
        BMP_HEADER.as_ptr() as *const c_void,
        BMP_HEADER.len() as u32,
    );
    file::my_write(fd, buf as *const c_void, 240 * 320 * 2);
    let end: u16 = 0;
    file::my_write(
        fd,
        &end as *const u16 as *const c_void,
        size_of::<u16>() as u32,
    );
    file::my_close(fd);
}

#[no_mangle]
pub extern "C" fn dumpMemStr(ptr: *mut c_void, len: size_t) {
    if ptr.is_null() {
        return;
    }

    unsafe {
        for i in 0..len {
            let ch = *(ptr as *const c_uchar).add(i);
            libc::putchar(if ch.is_ascii_graphic() {
                ch as c_int
            } else {
                b'.' as c_int
            });
        }
    }
}

#[no_mangle]
pub extern "C" fn getSplitStr(str_: *mut c_char, split: c_char, n: c_int) -> *mut c_char {
    if str_.is_null() {
        return ptr::null_mut();
    }

    unsafe {
        let split = split as u8;
        let mut start = str_ as *const u8;
        let mut count = 0;

        if n != 0 {
            while *start != 0 {
                if *start != split {
                    start = start.add(1);
                    continue;
                }
                count += 1;
                while *start != 0 && *start == split {
                    start = start.add(1);
                }
                if count == n {
                    break;
                }
            }
            if count != n {
                return ptr::null_mut();
            }
        }

        let mut end = start;
        while *end != 0 && *end != split {
            end = end.add(1);
        }

        let len = end.offset_from(start) as usize;
        let ret = libc::malloc(len + 1) as *mut u8;
        if ret.is_null() {
            return ptr::null_mut();
        }
        ptr::copy_nonoverlapping(start, ret, len);
        *ret.add(len) = 0;
        ret as *mut c_char
    }
}

#[no_mangle]
pub extern "C" fn wstrlen(txt: *mut c_char) -> c_int {
    if txt.is_null() {
        return 0;
    }

    unsafe {
        let bytes = txt as *const u8;
        let mut i = 0;
        while *bytes.add(i) != 0 || *bytes.add(i + 1) != 0 {
            i += 2;
        }
        i as c_int
    }
}

#[no_mangle]
pub extern "C" fn copyWstrToMrp(str_: *mut c_char) -> u32 {
    let _ = str_;
    log!("copyWstrToMrp called without guest memory context");
    0
}

pub fn copy_wstr_to_mrp_in(mem: &mut Mem, str_: *mut c_char) -> u32 {
    unsafe {
        if str_.is_null() {
            return 0;
        }
        let len = wstrlen(str_) as usize + 2;
        let p = malloc_ext_in(mem, len as u32);
        if p.is_null() {
            return 0;
        }
        ptr::copy_nonoverlapping(str_ as *const u8, p as *mut u8, len);
        bootstrap::to_mrp_mem_addr(mem, p)
    }
}

pub fn copy_str_to_mrp_in(mem: &mut Mem, str_: *mut c_char) -> u32 {
    unsafe {
        if str_.is_null() {
            return 0;
        }
        let len = CStr::from_ptr(str_).to_bytes_with_nul().len();
        let p = malloc_ext_in(mem, len as u32);
        if p.is_null() {
            return 0;
        }
        ptr::copy_nonoverlapping(str_ as *const u8, p as *mut u8, len);
        bootstrap::to_mrp_mem_addr(mem, p)
    }
}

pub fn get_uptime_ms() -> i64 {
    static START: std::sync::OnceLock<SystemTime> = std::sync::OnceLock::new();
    START
        .get_or_init(SystemTime::now)
        .elapsed()
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn get_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(-1)
}

#[no_mangle]
pub extern "C" fn getDatetime(datetime: *mut MrDatetime) -> c_int {
    if datetime.is_null() {
        return MR_FAILED;
    }

    let Some(now) = current_datetime() else {
        return MR_FAILED;
    };
    unsafe {
        *datetime = now;
    }

    MR_SUCCESS
}

pub fn current_datetime() -> Option<MrDatetime> {
    unsafe {
        let mut now: libc::time_t = 0;
        libc::time(&mut now);
        let t = libc::localtime(&now);
        if t.is_null() {
            return None;
        }

        Some(MrDatetime {
            year: ((*t).tm_year + 1900) as u16,
            month: ((*t).tm_mon + 1) as u8,
            day: (*t).tm_mday as u8,
            hour: (*t).tm_hour as u8,
            minute: (*t).tm_min as u8,
            second: (*t).tm_sec as u8,
        })
    }
}
