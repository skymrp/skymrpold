use crate::file;
use crate::mem::{MutPtr, MutVoidPtr, Ptr};
use crate::runtime;
use crate::unicorn;
use libc::{c_char, c_int, c_uchar, c_void, size_t};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::CStr;
use std::ptr;
use std::time::{SystemTime, UNIX_EPOCH};
use unicorn_engine::RegisterARM;

const MR_SUCCESS: c_int = 0;
const MR_FAILED: c_int = -1;

const MR_FILE_RDWR: u32 = 4;
const MR_FILE_CREATE: u32 = 8;

const UC_MEM_READ: c_int = 16;
const UC_MEM_WRITE: c_int = 17;
const UC_MEM_FETCH: c_int = 18;
const UC_MEM_READ_UNMAPPED: c_int = 19;
const UC_MEM_WRITE_UNMAPPED: c_int = 20;
const UC_MEM_FETCH_UNMAPPED: c_int = 21;
const UC_MEM_WRITE_PROT: c_int = 22;
const UC_MEM_READ_PROT: c_int = 23;
const UC_MEM_FETCH_PROT: c_int = 24;
const UC_MEM_READ_AFTER: c_int = 25;

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

#[derive(Default)]
struct FreeListAllocator {
    base: u32,
    len: u32,
    free: BTreeMap<u32, u32>,
    used: BTreeMap<u32, u32>,
}

impl FreeListAllocator {
    fn init(&mut self, base: u32, len: u32) {
        self.base = base;
        self.len = len;
        self.free.clear();
        self.used.clear();
        self.free.insert(base, len);
    }

    fn alloc(&mut self, len: u32) -> Option<u32> {
        let (&base, &chunk_len) = self.free.iter().find(|(_, chunk_len)| **chunk_len >= len)?;
        self.free.remove(&base);
        if chunk_len > len {
            self.free.insert(base + len, chunk_len - len);
        }
        self.used.insert(base, len);
        Some(base)
    }

    fn free(&mut self, base: u32) -> u32 {
        let Some(len) = self.used.remove(&base) else {
            return 0;
        };

        let mut merged_base = base;
        let mut merged_len = len;

        if let Some((&prev_base, &prev_len)) = self.free.range(..base).next_back() {
            if prev_base + prev_len == base {
                self.free.remove(&prev_base);
                merged_base = prev_base;
                merged_len += prev_len;
            }
        }

        let next_base = merged_base + merged_len;
        if let Some(next_len) = self.free.remove(&next_base) {
            merged_len += next_len;
        }

        self.free.insert(merged_base, merged_len);
        len
    }

    fn realloc(&mut self, old_base: u32, new_len: u32) -> Option<(u32, u32)> {
        let old_len = self.used.get(&old_base).copied()?;
        let new_base = self.alloc(new_len)?;
        Some((new_base, old_len))
    }
}

thread_local! {
    static FREE_LIST: RefCell<FreeListAllocator> = RefCell::new(FreeListAllocator::default());
}

fn real_lg_mem_size(len: u32) -> u32 {
    len.wrapping_add(7) & 0xfffffff8
}

fn alloc_guest(len: u32) -> Option<u32> {
    let addr = FREE_LIST.with(|free_list| free_list.borrow_mut().alloc(len))?;
    unsafe {
        LG_mem_left = LG_mem_left.saturating_sub(len);
        LG_mem_min = LG_mem_min.min(LG_mem_left);
        LG_mem_top = LG_mem_top.max(addr.saturating_sub(runtime::toMrpMemAddr(LG_mem_base.cast())));
    }
    Some(addr)
}

fn free_guest(addr: u32) {
    if addr == 0 {
        return;
    }
    let freed = FREE_LIST.with(|free_list| free_list.borrow_mut().free(addr));
    unsafe {
        LG_mem_left = LG_mem_left.saturating_add(freed).min(LG_mem_len);
    }
}

fn guest_host_ptr(addr: u32, count: u32) -> *mut c_void {
    runtime::guest_host_ptr_mut(addr, count)
}

#[no_mangle]
pub extern "C" fn initMemoryManager(base_address: u32, len: u32) {
    log!("initMemoryManager: baseAddress:0x{base_address:X} len: 0x{len:X}");
    unsafe {
        Origin_LG_mem_base = runtime::guest_host_ptr_mut(base_address, 1) as *mut c_char;
        Origin_LG_mem_len = len;

        LG_mem_base = ((Origin_LG_mem_base.add(3) as usize) & !3usize) as *mut c_char;
        LG_mem_len =
            (Origin_LG_mem_len - (LG_mem_base as usize - Origin_LG_mem_base as usize) as u32) & !3;
        LG_mem_end = LG_mem_base.add(LG_mem_len as usize);
        LG_mem_left = LG_mem_len;
        LG_mem_min = LG_mem_len;
        LG_mem_top = 0;
        FREE_LIST.with(|free_list| {
            free_list
                .borrow_mut()
                .init(runtime::toMrpMemAddr(LG_mem_base.cast()), LG_mem_len);
        });
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
            mem_len, mem_min, mem_left, mem_top
        );
        log!(".......base:{:p}, end:{:p}", mem_base, mem_end);
        log!(".......obase:{:p}, olen:{}", origin_base, origin_len);
    }
}

#[no_mangle]
pub extern "C" fn my_malloc(mut len: u32) -> *mut c_void {
    len = real_lg_mem_size(len);
    if len == 0 {
        log!("my_malloc invalid memory request");
        return ptr::null_mut();
    }
    if unsafe { len >= LG_mem_left } {
        log!("my_malloc no memory");
        return ptr::null_mut();
    }
    let Some(addr) = alloc_guest(len) else {
        log!("my_malloc no memory");
        return ptr::null_mut();
    };
    guest_host_ptr(addr, len)
}

#[no_mangle]
pub extern "C" fn my_free(p: *mut c_void, _len: u32) {
    if p.is_null() {
        return;
    }
    let addr = runtime::toMrpMemAddr(p);
    free_guest(addr);
}

#[no_mangle]
pub extern "C" fn my_realloc(p: *mut c_void, oldlen: u32, len: u32) -> *mut c_void {
    if p.is_null() {
        return my_malloc(len);
    }
    if len == 0 {
        my_free(p, oldlen);
        return ptr::null_mut();
    }
    let old_addr = runtime::toMrpMemAddr(p);
    let new_len = real_lg_mem_size(len);
    let Some((new_addr, old_len)) =
        FREE_LIST.with(|free_list| free_list.borrow_mut().realloc(old_addr, new_len))
    else {
        return ptr::null_mut();
    };
    runtime::with_guest_mem_mut(|mem| {
        mem.memmove(
            MutPtr::<c_void>::from_bits(new_addr),
            Ptr::<c_void, false>::from_bits(old_addr),
            oldlen.min(old_len).min(len),
        );
    });
    free_guest(old_addr);
    guest_host_ptr(new_addr, len)
}

pub fn malloc_ext(len: u32) -> *mut c_void {
    let ptr = malloc_ext_guest(len);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    guest_host_ptr(ptr.to_bits(), len)
}

pub fn malloc_ext_guest(len: u32) -> MutVoidPtr {
    if len == 0 {
        return MutVoidPtr::null();
    }
    let total = len + size_of::<u32>() as u32;
    let Some(header_addr) = alloc_guest(real_lg_mem_size(total)) else {
        return MutVoidPtr::null();
    };
    let header = MutPtr::<u32>::from_bits(header_addr);
    runtime::with_guest_mem_mut(|mem| mem.write(header, len));
    MutVoidPtr::from_bits(header_addr + size_of::<u32>() as u32)
}

#[no_mangle]
pub extern "C" fn my_mallocExt(len: u32) -> *mut c_void {
    malloc_ext(len)
}

pub fn malloc_ext_zeroed(len: u32) -> *mut c_void {
    let ptr = malloc_ext_zeroed_guest(len);
    if ptr.is_null() {
        return ptr::null_mut();
    }
    guest_host_ptr(ptr.to_bits(), len)
}

pub fn malloc_ext_zeroed_guest(len: u32) -> MutVoidPtr {
    let ptr = malloc_ext_guest(len);
    if !ptr.is_null() {
        runtime::with_guest_mem_mut(|mem| {
            mem.bytes_at_mut(ptr.cast::<u8>(), len).fill(0);
        });
    }
    ptr
}

#[no_mangle]
pub extern "C" fn my_mallocExt0(len: u32) -> *mut c_void {
    malloc_ext_zeroed(len)
}

pub fn free_ext(p: *mut c_void) {
    if p.is_null() {
        return;
    }
    let payload = runtime::toMrpMemAddr(p);
    free_ext_guest(MutVoidPtr::from_bits(payload));
}

pub fn free_ext_guest(ptr: MutVoidPtr) {
    let payload = ptr.to_bits();
    if payload == 0 {
        return;
    }
    let header = MutPtr::<u32>::from_bits(payload - size_of::<u32>() as u32);
    let _len: u32 = runtime::with_guest_mem(|mem| mem.read(header.cast_const()));
    free_guest(header.to_bits());
}

#[no_mangle]
pub extern "C" fn my_freeExt(p: *mut c_void) {
    free_ext(p);
}

pub fn realloc_ext(p: *mut c_void, new_len: u32) -> *mut c_void {
    let ptr = if p.is_null() {
        MutVoidPtr::null()
    } else {
        MutVoidPtr::from_bits(runtime::toMrpMemAddr(p))
    };
    let new_ptr = realloc_ext_guest(ptr, new_len);
    if new_ptr.is_null() {
        return ptr::null_mut();
    }
    guest_host_ptr(new_ptr.to_bits(), new_len)
}

pub fn realloc_ext_guest(ptr: MutVoidPtr, new_len: u32) -> MutVoidPtr {
    let payload = ptr.to_bits();
    if ptr.is_null() {
        return malloc_ext_guest(new_len);
    }
    if new_len == 0 {
        free_ext_guest(ptr);
        return MutVoidPtr::null();
    }
    let header = MutPtr::<u32>::from_bits(payload - size_of::<u32>() as u32);
    let old_len = runtime::with_guest_mem(|mem| mem.read(header.cast_const()));
    let new_block = malloc_ext_guest(new_len);
    if new_block.is_null() {
        return new_block;
    }
    runtime::with_guest_mem_mut(|mem| {
        mem.memmove(
            new_block,
            Ptr::<c_void, false>::from_bits(payload),
            old_len.min(new_len),
        );
    });
    free_ext_guest(ptr);
    new_block
}

#[no_mangle]
pub extern "C" fn my_reallocExt(p: *mut c_void, new_len: u32) -> *mut c_void {
    realloc_ext(p, new_len)
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
pub extern "C" fn memTypeStr(type_: c_int) -> *mut c_char {
    let s: &[u8] = match type_ {
        UC_MEM_READ => b"UC_MEM_READ\0",
        UC_MEM_WRITE => b"UC_MEM_WRITE\0",
        UC_MEM_FETCH => b"UC_MEM_FETCH\0",
        UC_MEM_READ_UNMAPPED => b"UC_MEM_READ_UNMAPPED\0",
        UC_MEM_WRITE_UNMAPPED => b"UC_MEM_WRITE_UNMAPPED\0",
        UC_MEM_FETCH_UNMAPPED => b"UC_MEM_FETCH_UNMAPPED\0",
        UC_MEM_WRITE_PROT => b"UC_MEM_WRITE_PROT\0",
        UC_MEM_READ_PROT => b"UC_MEM_READ_PROT\0",
        UC_MEM_FETCH_PROT => b"UC_MEM_FETCH_PROT\0",
        UC_MEM_READ_AFTER => b"UC_MEM_READ_AFTER\0",
        _ => b"<error type>\0",
    };
    s.as_ptr() as *mut c_char
}

#[no_mangle]
pub extern "C" fn cpsrToStr(v: u32, out: *mut c_char) {
    if out.is_null() {
        return;
    }

    unsafe {
        *out.add(0) = if v & (1 << 31) != 0 { b'N' } else { b'n' } as c_char;
        *out.add(1) = if v & (1 << 30) != 0 { b'Z' } else { b'z' } as c_char;
        *out.add(2) = if v & (1 << 29) != 0 { b'C' } else { b'c' } as c_char;
        *out.add(3) = if v & (1 << 28) != 0 { b'V' } else { b'v' } as c_char;
        *out.add(4) = 0;
    }
}

fn read_reg(uc: *mut c_void, reg: RegisterARM) -> u32 {
    unicorn::reg_read(uc, reg).unwrap_or(0)
}

#[no_mangle]
pub extern "C" fn dumpREG(uc: *mut c_void) {
    let cpsr = read_reg(uc, RegisterARM::CPSR);
    log!("==========================REG=================================");
    log!(
        " R0=0x{:08X}\tR1=0x{:08X}\t R2=0x{:08X}\t R3=0x{:08X}\tN:{}",
        read_reg(uc, RegisterARM::R0),
        read_reg(uc, RegisterARM::R1),
        read_reg(uc, RegisterARM::R2),
        read_reg(uc, RegisterARM::R3),
        (cpsr & (1 << 31)) >> 31
    );
    log!(
        " R4=0x{:08X}\tR5=0x{:08X}\t R6=0x{:08X}\t R7=0x{:08X}\tZ:{}",
        read_reg(uc, RegisterARM::R4),
        read_reg(uc, RegisterARM::R5),
        read_reg(uc, RegisterARM::R6),
        read_reg(uc, RegisterARM::R7),
        (cpsr & (1 << 30)) >> 30
    );
    log!(
        " R8=0x{:08X}\tR9=0x{:08X}\tR10=0x{:08X}\tR11=0x{:08X}\tC:{}",
        read_reg(uc, RegisterARM::R8),
        read_reg(uc, RegisterARM::R9),
        read_reg(uc, RegisterARM::R10),
        read_reg(uc, RegisterARM::R11),
        (cpsr & (1 << 29)) >> 29
    );
    log!(
        "R12=0x{:08X}\tSP=0x{:08X}\t LR=0x{:08X}\t PC=0x{:08X}\tV:{}",
        read_reg(uc, RegisterARM::R12),
        read_reg(uc, RegisterARM::SP),
        read_reg(uc, RegisterARM::LR),
        read_reg(uc, RegisterARM::PC),
        (cpsr & (1 << 28)) >> 28
    );
    log!("==============================================================");
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
    unsafe {
        if str_.is_null() {
            return 0;
        }
        let len = wstrlen(str_) as usize + 2;
        let p = malloc_ext(len as u32);
        if p.is_null() {
            return 0;
        }
        ptr::copy_nonoverlapping(str_ as *const u8, p as *mut u8, len);
        runtime::toMrpMemAddr(p)
    }
}

#[no_mangle]
pub extern "C" fn copyStrToMrp(str_: *mut c_char) -> u32 {
    unsafe {
        if str_.is_null() {
            return 0;
        }
        let len = CStr::from_ptr(str_).to_bytes_with_nul().len();
        let p = malloc_ext(len as u32);
        if p.is_null() {
            return 0;
        }
        ptr::copy_nonoverlapping(str_ as *const u8, p as *mut u8, len);
        runtime::toMrpMemAddr(p)
    }
}

#[no_mangle]
pub extern "C" fn get_uptime_ms() -> i64 {
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
