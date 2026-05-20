use crate::abi::{self, AbiReg, GuestArg, GuestRet, RegisterContext, StackMemoryContext};
use crate::mem::{ConstPtr, GuestUSize, MutPtr, Ptr, SafeWrite};
use crate::runtime;
use crate::syscall;
use crate::unicorn;
use crate::{app, compat, file, network};
use libc::{c_char, c_int, c_ushort, c_void};
use std::ffi::CStr;
use std::ptr;
use std::sync::Mutex;
use unicorn_engine::RegisterARM;

const MR_SUCCESS: c_int = 0;
const MR_FAILED: c_int = -1;

const CODE_ADDRESS: u32 = 0x80000;

const BRIDGE_VER: c_int = 20210701;
const DSM_INIT: c_int = -100;
const MR_START_DSM: c_int = -99;
const MR_PAUSEAPP: c_int = -98;
const MR_RESUMEAPP: c_int = -97;
const MR_TIMER: c_int = -96;
const MR_EVENT: c_int = -95;
const FLAG_USE_UTF8_EDIT: u32 = 1 << 1;

const READDIR_SHARED_MEM_SIZE: usize = 128;
const DSM_REQUIRE_FUNCS_SIZE: u32 = 0xd0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BridgeMapType {
    Data,
    Func,
}

type BridgeHandler = fn(&BridgeEntry, &mut BridgeContext);
type BridgeInit = fn(&BridgeEntry, *mut c_void, u32);

#[derive(Clone, Copy)]
struct BridgeEntry {
    pos: u32,
    type_: BridgeMapType,
    name: &'static str,
    init: Option<BridgeInit>,
    handler: Option<BridgeHandler>,
}

#[repr(C)]
struct MrCFunctionP {
    start_of_er_rw: u32,
    er_rw_length: u32,
    ext_type: c_int,
    mrc_ext_chunk: u32,
    stack: c_int,
}

#[repr(C)]
struct Event {
    code: c_int,
    p0: c_int,
    p1: c_int,
}

#[repr(C)]
struct Start {
    filename: u32,
    ext: u32,
    entry: u32,
}

static BRIDGE_LOCK: Mutex<()> = Mutex::new(());

static mut MR_TABLE: *mut c_void = ptr::null_mut();
static mut MR_C_FUNCTION_P: *mut MrCFunctionP = ptr::null_mut();
static mut DSM_REQUIRE_FUNCS: *mut c_void = ptr::null_mut();
static mut MR_C_EVENT: *mut Event = ptr::null_mut();
static mut DSM_EVENT: *mut Event = ptr::null_mut();
static mut MR_START_DSM_PARAM: *mut Start = ptr::null_mut();
static mut MR_EXT_HELPER_ADDR: u32 = 0;
static mut READDIR_SHARED_MEM: u32 = 0;
static mut UPTIME_MS: u64 = 0;

macro_rules! entry {
    ($pos:expr, data, $name:expr) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Data,
            name: $name,
            init: None,
            handler: None,
        }
    };
    ($pos:expr, func, $name:expr, $handler:expr) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Func,
            name: $name,
            init: None,
            handler: $handler,
        }
    };
    ($pos:expr, func, $name:expr, $init:expr, $handler:expr) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Func,
            name: $name,
            init: $init,
            handler: $handler,
        }
    };
}

struct BridgeContext {
    uc: *mut c_void,
}

impl BridgeContext {
    fn new(uc: *mut c_void) -> Self {
        Self { uc }
    }

    fn uc(&self) -> *mut c_void {
        self.uc
    }

    fn arg<T: GuestArg>(&mut self, n: usize) -> T {
        abi::read_arg_from_context(self, n)
    }

    fn guest_arg<T, const MUT: bool>(&mut self, n: usize) -> Ptr<T, MUT> {
        self.arg(n)
    }

    fn ret<T: GuestRet>(&mut self, value: T) {
        abi::write_ret_to_context(self, value);
    }

    fn with_cstr<R>(&self, ptr: ConstPtr<u8>, f: impl FnOnce(&CStr) -> R) -> R {
        runtime::with_guest_mem(|mem| {
            let len = mem.cstr_at(ptr).len();
            let bytes = mem.bytes_at(ptr, u32::try_from(len + 1).unwrap());
            let cstr =
                CStr::from_bytes_with_nul(bytes).expect("guest C string is not NUL-terminated");
            f(cstr)
        })
    }

    fn with_two_cstr<R>(
        &self,
        first: ConstPtr<u8>,
        second: ConstPtr<u8>,
        f: impl FnOnce(&CStr, &CStr) -> R,
    ) -> R {
        runtime::with_guest_mem(|mem| {
            let first_len = mem.cstr_at(first).len();
            let second_len = mem.cstr_at(second).len();
            let first_bytes = mem.bytes_at(first, u32::try_from(first_len + 1).unwrap());
            let second_bytes = mem.bytes_at(second, u32::try_from(second_len + 1).unwrap());
            let first = CStr::from_bytes_with_nul(first_bytes)
                .expect("guest C string is not NUL-terminated");
            let second = CStr::from_bytes_with_nul(second_bytes)
                .expect("guest C string is not NUL-terminated");
            f(first, second)
        })
    }

    fn with_bytes<R>(&self, ptr: ConstPtr<u8>, len: GuestUSize, f: impl FnOnce(&[u8]) -> R) -> R {
        runtime::with_guest_mem(|mem| f(mem.bytes_at(ptr, len)))
    }

    fn with_bytes_mut<R>(
        &mut self,
        ptr: MutPtr<u8>,
        len: GuestUSize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> R {
        runtime::with_guest_mem_mut(|mem| f(mem.bytes_at_mut(ptr, len)))
    }

    fn memmove(&mut self, dst: MutPtr<c_void>, src: ConstPtr<c_void>, len: GuestUSize) {
        runtime::with_guest_mem_mut(|mem| mem.memmove(dst, src, len));
    }

    fn write_guest<T>(&mut self, ptr: MutPtr<T>, value: T)
    where
        T: SafeWrite,
    {
        runtime::with_guest_mem_mut(|mem| mem.write(ptr, value));
    }

    fn mem_read_u32(&self, addr: u32) -> u32 {
        unicorn::mem_read_u32(self.uc, addr as u64).unwrap_or(0)
    }

    fn mem_write_u32(&self, addr: u32, value: u32) {
        unicorn::mem_write_u32(self.uc, addr as u64, value).ok();
    }
}

impl RegisterContext for BridgeContext {
    fn read_reg(&mut self, reg: AbiReg) -> u32 {
        unicorn::reg_read(self.uc, abi_reg_to_unicorn(reg)).unwrap_or(0)
    }

    fn write_reg(&mut self, reg: AbiReg, value: u32) {
        if let Err(err) = unicorn::reg_write(self.uc, abi_reg_to_unicorn(reg), value) {
            log!(
                "Failed write register {reg:?}: {err:?} ({})",
                unicorn::error_text(err)
            );
        }
    }
}

impl StackMemoryContext for BridgeContext {
    fn read_stack_u32(&mut self, sp: u32, word_offset: usize) -> u32 {
        self.mem_read_u32(sp + u32::try_from(word_offset).unwrap() * 4)
    }

    fn write_stack_u32(&mut self, sp: u32, word_offset: usize, value: u32) {
        self.mem_write_u32(sp + u32::try_from(word_offset).unwrap() * 4, value);
    }
}

fn abi_reg_to_unicorn(reg: AbiReg) -> RegisterARM {
    match reg {
        AbiReg::R0 => RegisterARM::R0,
        AbiReg::R1 => RegisterARM::R1,
        AbiReg::R2 => RegisterARM::R2,
        AbiReg::R3 => RegisterARM::R3,
        AbiReg::R4 => RegisterARM::R4,
        AbiReg::R5 => RegisterARM::R5,
        AbiReg::R6 => RegisterARM::R6,
        AbiReg::R7 => RegisterARM::R7,
        AbiReg::R8 => RegisterARM::R8,
        AbiReg::R9 => RegisterARM::R9,
        AbiReg::R10 => RegisterARM::R10,
        AbiReg::R11 => RegisterARM::R11,
        AbiReg::R12 => RegisterARM::R12,
        AbiReg::SP => RegisterARM::SP,
        AbiReg::LR => RegisterARM::LR,
        AbiReg::PC => RegisterARM::PC,
    }
}

fn run_code(ctx: &mut BridgeContext, mut start_addr: u32, stop_addr: u32, is_thumb: bool) {
    ctx.write_reg(AbiReg::LR, stop_addr);
    if is_thumb {
        start_addr |= 1;
    }
    if let Err(err) = unicorn::emu_start(ctx.uc(), start_addr as u64, stop_addr as u64, 0, 0) {
        log!(
            "Failed on uc_emu_start() with error returned: {err:?} ({})",
            unicorn::error_text(err)
        );
        std::process::exit(1);
    }
}

fn dispatch_bridge_svc(uc: *mut c_void, svc: u32, entry: usize) {
    let entry = unsafe { *(entry as *const BridgeEntry) };
    let Some(handler) = entry.handler else {
        log!("!!! {}() Not yet implemented function !!!", entry.name);
        std::process::exit(1);
    };

    log!("[SVC] -> {} pos=0x{:X} type=func", entry.name, entry.pos);
    let mut ctx = BridgeContext::new(uc);
    handler(&entry, &mut ctx);
    let ret = ctx.arg::<u32>(0);
    let pc = ctx.read_reg(AbiReg::PC);
    let lr = ctx.read_reg(AbiReg::LR);
    log!(
        "[SVC] <- {} ret=0x{ret:08X} pc=0x{pc:08X} lr=0x{lr:08X}",
        entry.name
    );
}

fn hooks_init(uc: *mut c_void, map: &'static [BridgeEntry], table_size: u32) -> *mut c_void {
    let func_count = map
        .iter()
        .filter(|obj| obj.type_ == BridgeMapType::Func)
        .count() as u32;
    let ptr = compat::malloc_ext(table_size + func_count * 8);
    let start_address = runtime::to_mrp_mem_addr(ptr);
    let mut stub_address = start_address + table_size;

    for obj in map {
        let addr = start_address + obj.pos;
        match obj.type_ {
            BridgeMapType::Data => {
                if let Some(init) = obj.init {
                    init(obj, uc, addr);
                }
            }
            BridgeMapType::Func => {
                if let Some(init) = obj.init {
                    init(obj, uc, addr);
                }
                let svc = syscall::link_host_function(
                    uc,
                    stub_address,
                    obj.name,
                    dispatch_bridge_svc,
                    obj as *const BridgeEntry as usize,
                );
                unicorn::mem_write_u32(uc, addr as u64, stub_address).ok();
                log!("[SVC] linked {} pos=0x{:X} svc=#{svc}", obj.name, obj.pos);
                stub_address += 8;
            }
        }
    }
    ptr
}

fn br_mr_c_function_new(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let p_f = ctx.arg::<u32>(0);
    let p_len = ctx.arg::<u32>(1);
    log!("ext call _mr_c_function_new(0x{p_f:X}[{p_f}], 0x{p_len:X}[{p_len}])");
    compat::dump_reg(ctx.uc());

    let mr_c_function_p = compat::malloc_ext(p_len) as *mut MrCFunctionP;
    unsafe {
        MR_EXT_HELPER_ADDR = p_f;
        MR_C_FUNCTION_P = mr_c_function_p;
        ptr::write_bytes(mr_c_function_p as *mut u8, 0, p_len as usize);
    }

    let v = runtime::to_mrp_mem_addr(mr_c_function_p as *mut c_void);
    ctx.mem_write_u32(CODE_ADDRESS + 4, v);
    ctx.ret(MR_SUCCESS as u32);
}

fn br_mr_malloc(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let len = ctx.arg::<u32>(0);
    ctx.ret(compat::malloc_ext_guest(len));
}

fn br_mr_free(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let p = ctx.guest_arg::<c_void, true>(0);
    compat::free_ext_guest(p);
}

fn br_memcpy(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let dst = ctx.guest_arg::<c_void, true>(0);
    let src = ctx.guest_arg::<c_void, false>(1);
    let n = ctx.arg::<u32>(2);
    ctx.memmove(dst, src, n);
    ctx.ret(dst.to_bits());
}

fn br_memset(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let dst = ctx.guest_arg::<u8, true>(0);
    let value = ctx.arg::<u32>(1);
    let n = ctx.arg::<u32>(2);
    ctx.with_bytes_mut(dst, n, |bytes| bytes.fill(value as u8));
    ctx.ret(dst.to_bits());
}

fn br_mr_draw_bitmap(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let bmp = ctx.guest_arg::<u16, false>(0);
    let x = ctx.arg::<u32>(1) as c_int;
    let y = ctx.arg::<u32>(2) as c_int;
    let w = ctx.arg::<u32>(3) as c_int;
    let h = ctx.arg::<u32>(4) as c_int;
    let pixel_count = (w as u32).saturating_mul(h as u32);
    ctx.with_bytes(bmp.cast::<u8>(), pixel_count.saturating_mul(2), |bytes| {
        let pixels = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        app::draw_bitmap(&pixels, x, y, w, h);
    });
}

fn br_mr_open(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let filename = ctx.guest_arg::<u8, false>(0);
    let mode = ctx.arg::<u32>(1);
    let ret = ctx.with_cstr(filename, |filename| file::open_cstr(filename, mode));
    ctx.ret(ret as u32);
}

fn br_mr_close(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let f = ctx.arg::<u32>(0) as c_int;
    ctx.ret(file::close(f) as u32);
}

fn br_mr_write(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let f = ctx.arg::<u32>(0) as c_int;
    let p = ctx.guest_arg::<u8, false>(1);
    let l = ctx.arg::<u32>(2);
    let ret = ctx.with_bytes(p, l, |bytes| file::write_from(f, bytes));
    ctx.ret(ret as u32);
}

fn br_mr_read(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let f = ctx.arg::<u32>(0) as c_int;
    let p = ctx.guest_arg::<u8, true>(1);
    let l = ctx.arg::<u32>(2);
    let ret = ctx.with_bytes_mut(p, l, |bytes| file::read_into(f, bytes));
    ctx.ret(ret as u32);
}

fn br_mr_seek(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let f = ctx.arg::<u32>(0) as c_int;
    let pos = ctx.arg::<u32>(1) as c_int;
    let method = ctx.arg::<u32>(2) as c_int;
    ctx.ret(file::seek(f, pos, method) as u32);
}

fn br_mr_get_len(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let filename = ctx.guest_arg::<u8, false>(0);
    let ret = ctx.with_cstr(filename, file::get_len_cstr);
    ctx.ret(ret as u32);
}

fn br_mr_remove(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let filename = ctx.guest_arg::<u8, false>(0);
    let ret = ctx.with_cstr(filename, file::remove_cstr);
    ctx.ret(ret as u32);
}

fn br_mr_rename(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let oldname = ctx.guest_arg::<u8, false>(0);
    let newname = ctx.guest_arg::<u8, false>(1);
    let ret = ctx.with_two_cstr(oldname, newname, file::rename_cstr);
    ctx.ret(ret as u32);
}

fn br_mr_mkdir(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let name = ctx.guest_arg::<u8, false>(0);
    let ret = ctx.with_cstr(name, file::mkdir_cstr);
    ctx.ret(ret as u32);
}

fn br_mr_rmdir(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let name = ctx.guest_arg::<u8, false>(0);
    let ret = ctx.with_cstr(name, file::rmdir_cstr);
    ctx.ret(ret as u32);
}

fn br_get_uptime_ms_init(_o: &BridgeEntry, uc: *mut c_void, addr: u32) {
    unsafe {
        UPTIME_MS = compat::get_uptime_ms() as u64;
    }
    unicorn::mem_write_u32(uc, addr as u64, addr).ok();
}

fn br_get_uptime_ms(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let uptime_ms = unsafe { UPTIME_MS };
    let ret = (compat::get_uptime_ms() as u64).wrapping_sub(uptime_ms) as u32;
    ctx.ret(ret);
}

fn br_log(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let msg = ctx.guest_arg::<u8, false>(0);
    if !msg.is_null() {
        let text = ctx.with_cstr(msg, |msg| msg.to_string_lossy().into_owned());
        log!("{text}");
    }
}

fn br_mem_get(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let mem_base = ctx.arg::<u32>(0);
    let mem_len = ctx.arg::<u32>(1);
    let len = 1024 * 1024 * 4u32;
    let buffer = compat::malloc_ext_guest(len).to_bits();
    log!(
        "br_mem_get base=0x{buffer:X} len={len}({} kb) =================",
        len / 1024
    );
    ctx.mem_write_u32(mem_base, buffer);
    ctx.mem_write_u32(mem_len, len);
    ctx.ret(MR_SUCCESS as u32);
}

fn br_mem_free(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let mem = ctx.guest_arg::<c_void, true>(0);
    compat::free_ext_guest(mem);
    ctx.ret(MR_SUCCESS as u32);
}

fn br_timer_stop(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    ctx.ret(app::timer_stop() as u32);
}

fn br_timer_start(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let t = ctx.arg::<u32>(0);
    ctx.ret(app::timer_start(t as c_ushort) as u32);
}

fn br_test(_o: &BridgeEntry, _ctx: &mut BridgeContext) {}

fn br_exit(_o: &BridgeEntry, _ctx: &mut BridgeContext) {
    log!("mythroad exit.\n");
    std::process::exit(0);
}

fn br_srand(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let seed = ctx.arg::<u32>(0);
    unsafe {
        libc::srand(seed);
    }
}

fn br_rand(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    ctx.ret(unsafe { libc::rand() } as u32);
}

fn br_sleep(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let ms = ctx.arg::<u32>(0);
    unsafe {
        libc::usleep(ms.saturating_mul(1000));
    }
    ctx.ret(MR_SUCCESS as u32);
}

fn br_info(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let filename = ctx.guest_arg::<u8, false>(0);
    let ret = ctx.with_cstr(filename, file::info_cstr);
    ctx.ret(ret as u32);
}

fn br_opendir(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let name = ctx.guest_arg::<u8, false>(0);
    let ret = ctx.with_cstr(name, file::opendir_cstr);
    ctx.ret(ret as u32);
}

fn br_readdir_init(_o: &BridgeEntry, uc: *mut c_void, addr: u32) {
    let shared_mem = compat::malloc_ext_guest(READDIR_SHARED_MEM_SIZE as u32).to_bits();
    unsafe {
        READDIR_SHARED_MEM = shared_mem;
    }
    runtime::with_guest_mem_mut(|mem| {
        mem.bytes_at_mut(
            MutPtr::<u8>::from_bits(shared_mem),
            READDIR_SHARED_MEM_SIZE as u32,
        )
        .fill(0);
    });
    unicorn::mem_write_u32(uc, addr as u64, addr).ok();
}

fn br_readdir(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let f = ctx.arg::<u32>(0) as c_int;
    let Some(name) = file::readdir_name(f) else {
        ctx.ret(0);
        return;
    };
    let shared_mem = unsafe { READDIR_SHARED_MEM };
    if shared_mem == 0 {
        ctx.ret(0);
        return;
    }
    let len = name.len().min(READDIR_SHARED_MEM_SIZE - 1);
    runtime::with_guest_mem_mut(|mem| {
        let bytes = mem.bytes_at_mut(
            MutPtr::<u8>::from_bits(shared_mem),
            READDIR_SHARED_MEM_SIZE as u32,
        );
        bytes.fill(0);
        bytes[..len].copy_from_slice(&name[..len]);
    });
    ctx.ret(shared_mem);
}

fn br_closedir(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let f = ctx.arg::<u32>(0) as c_int;
    ctx.ret(file::closedir(f) as u32);
}

fn br_get_datetime(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let datetime = ctx.guest_arg::<c_void, true>(0);
    let Some(now) = compat::current_datetime() else {
        ctx.ret(MR_FAILED as u32);
        return;
    };
    let base = datetime.to_bits();
    ctx.write_guest(MutPtr::<u16>::from_bits(base), now.year);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 2), now.month);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 3), now.day);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 4), now.hour);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 5), now.minute);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 6), now.second);
    ctx.ret(MR_SUCCESS as u32);
}

fn br_mr_init_network(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let cb = ctx.arg::<u32>(0);
    let mode = ctx.guest_arg::<u8, false>(1);
    let user_data = ctx.arg::<u32>(2);
    let ret = ctx.with_cstr(mode, |mode| {
        network::init_network_cstr(
            ctx.uc(),
            cb as usize as *mut c_void,
            mode,
            user_data as usize as *mut c_void,
        )
    });
    ctx.ret(ret as u32);
}

fn br_mr_socket(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let type_ = ctx.arg::<u32>(0) as c_int;
    let protocol = ctx.arg::<u32>(1) as c_int;
    ctx.ret(network::socket(type_, protocol) as u32);
}

fn br_mr_connect(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let s = ctx.arg::<u32>(0) as c_int;
    let ip = ctx.arg::<u32>(1) as c_int;
    let port = ctx.arg::<u32>(2) as c_ushort;
    let type_ = ctx.arg::<u32>(3) as c_int;
    ctx.ret(network::connect(s, ip, port, type_) as u32);
}

fn br_mr_close_socket(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let s = ctx.arg::<u32>(0) as c_int;
    ctx.ret(network::close_socket(s) as u32);
}

fn br_mr_close_network(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    ctx.ret(network::close_network() as u32);
}

fn br_mr_get_host_by_name(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let name = ctx.guest_arg::<u8, false>(0);
    let cb = ctx.arg::<u32>(1);
    let user_data = ctx.arg::<u32>(2);
    let ret = ctx.with_cstr(name, |name| {
        network::get_host_by_name_cstr(
            ctx.uc(),
            name,
            cb as usize as *mut c_void,
            user_data as usize as *mut c_void,
        )
    });
    ctx.ret(ret as u32);
}

fn br_mr_sendto(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let s = ctx.arg::<u32>(0) as c_int;
    let buf = ctx.guest_arg::<u8, false>(1);
    let len = ctx.arg::<u32>(2) as c_int;
    let ip = ctx.arg::<u32>(3) as c_int;
    let port = ctx.arg::<u32>(4) as c_ushort;
    let ret = ctx.with_bytes(buf, len as u32, |buf| network::send_to(s, buf, ip, port));
    ctx.ret(ret as u32);
}

fn br_mr_send(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let s = ctx.arg::<u32>(0) as c_int;
    let buf = ctx.guest_arg::<u8, false>(1);
    let len = ctx.arg::<u32>(2) as c_int;
    let ret = ctx.with_bytes(buf, len as u32, |buf| network::send(s, buf));
    ctx.ret(ret as u32);
}

fn br_mr_recvfrom(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let s = ctx.arg::<u32>(0) as c_int;
    let buf = ctx.guest_arg::<u8, true>(1);
    let len = ctx.arg::<u32>(2) as c_int;
    let ip = ctx.guest_arg::<c_int, true>(3);
    let port = ctx.guest_arg::<c_ushort, true>(4);
    let mut ip_value = 0;
    let mut port_value = 0;
    let ret = ctx.with_bytes_mut(buf, len as u32, |buf| {
        network::recv_from(s, buf, &mut ip_value, &mut port_value)
    });
    ctx.write_guest(ip, ip_value);
    ctx.write_guest(port, port_value);
    ctx.ret(ret as u32);
}

fn br_mr_recv(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let s = ctx.arg::<u32>(0) as c_int;
    let buf = ctx.guest_arg::<u8, true>(1);
    let len = ctx.arg::<u32>(2) as c_int;
    let ret = ctx.with_bytes_mut(buf, len as u32, |buf| network::recv(s, buf));
    ctx.ret(ret as u32);
}

fn br_mr_get_socket_state(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let s = ctx.arg::<u32>(0) as c_int;
    ctx.ret(network::socket_state(s) as u32);
}

fn br_mr_play_sound(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let type_ = ctx.arg::<u32>(0) as c_int;
    let data = ctx.guest_arg::<u8, false>(1);
    let data_len = ctx.arg::<u32>(2);
    let loop_ = ctx.arg::<u32>(3) as c_int;
    let ret = ctx.with_bytes(data, data_len, |data| {
        app::play_sound_bytes(type_, data, loop_ != 0)
    });
    ctx.ret(ret as u32);
}

fn br_mr_stop_sound(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let type_ = ctx.arg::<u32>(0) as c_int;
    ctx.ret(app::stop_sound(type_) as u32);
}

fn br_mr_start_shake(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    ctx.ret(MR_SUCCESS as u32);
}

fn br_mr_stop_shake(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    ctx.ret(MR_SUCCESS as u32);
}

fn br_return_failed(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    ctx.ret(MR_FAILED as u32);
}

fn br_mr_edit_create(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let title = ctx.guest_arg::<u8, false>(0);
    let text = ctx.guest_arg::<u8, false>(1);
    let type_ = ctx.arg::<u32>(2) as c_int;
    let max_size = ctx.arg::<u32>(3) as c_int;
    let ret = ctx.with_two_cstr(title, text, |title, text| {
        app::edit_create_cstr(title, text, type_, max_size)
    });
    ctx.ret(ret as u32);
}

fn br_mr_edit_release(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let _edit = ctx.arg::<u32>(0) as c_int;
    ctx.ret(app::edit_release() as u32);
}

fn br_mr_edit_get_text(_o: &BridgeEntry, ctx: &mut BridgeContext) {
    let _edit = ctx.arg::<u32>(0) as c_int;
    ctx.ret(runtime::to_mrp_mem_addr(app::edit_get_text() as *mut c_void));
}

static MR_TABLE_FUNC_MAP: &[BridgeEntry] = &[
    entry!(0x0, func, "mr_malloc", Some(br_mr_malloc)),
    entry!(0x4, func, "mr_free", Some(br_mr_free)),
    entry!(0x8, func, "mr_realloc", None),
    entry!(0xC, func, "memcpy", Some(br_memcpy)),
    entry!(0x10, func, "memmove", None),
    entry!(0x14, func, "strcpy", None),
    entry!(0x18, func, "strncpy", None),
    entry!(0x1C, func, "strcat", None),
    entry!(0x20, func, "strncat", None),
    entry!(0x24, func, "memcmp", None),
    entry!(0x28, func, "strcmp", None),
    entry!(0x2C, func, "strncmp", None),
    entry!(0x30, func, "strcoll", None),
    entry!(0x34, func, "memchr", None),
    entry!(0x38, func, "memset", Some(br_memset)),
    entry!(0x3C, func, "strlen", None),
    entry!(0x40, func, "strstr", None),
    entry!(0x44, func, "sprintf", None),
    entry!(0x48, func, "atoi", None),
    entry!(0x4C, func, "strtoul", None),
    entry!(0x50, func, "rand", None),
    entry!(0x54, data, "reserve0"),
    entry!(0x58, data, "reserve1"),
    entry!(0x5C, data, "_mr_c_internal_table"),
    entry!(0x60, data, "_mr_c_port_table"),
    entry!(0x64, func, "_mr_c_function_new", Some(br_mr_c_function_new)),
    entry!(0x68, func, "mr_printf", None),
    entry!(0x6C, func, "mr_mem_get", None),
    entry!(0x70, func, "mr_mem_free", None),
    entry!(0x74, func, "mr_drawBitmap", None),
    entry!(0x78, func, "mr_getCharBitmap", None),
    entry!(0x7C, func, "mr_timerStart", None),
    entry!(0x80, func, "mr_timerStop", None),
    entry!(0x84, func, "mr_getTime", None),
    entry!(0x88, func, "mr_getDatetime", None),
    entry!(0x8C, func, "mr_getUserInfo", None),
    entry!(0x90, func, "mr_sleep", None),
    entry!(0x94, func, "mr_plat", None),
    entry!(0x98, func, "mr_platEx", None),
    entry!(0x9C, func, "mr_ferrno", None),
    entry!(0xA0, func, "mr_open", None),
    entry!(0xA4, func, "mr_close", None),
    entry!(0xA8, func, "mr_info", None),
    entry!(0xAC, func, "mr_write", None),
    entry!(0xB0, func, "mr_read", None),
    entry!(0xB4, func, "mr_seek", None),
    entry!(0xB8, func, "mr_getLen", None),
    entry!(0xBC, func, "mr_remove", None),
    entry!(0xC0, func, "mr_rename", None),
    entry!(0xC4, func, "mr_mkDir", None),
    entry!(0xC8, func, "mr_rmDir", None),
    entry!(0xCC, func, "mr_findStart", None),
    entry!(0xD0, func, "mr_findGetNext", None),
    entry!(0xD4, func, "mr_findStop", None),
    entry!(0xD8, func, "mr_exit", None),
    entry!(0xDC, func, "mr_startShake", None),
    entry!(0xE0, func, "mr_stopShake", None),
    entry!(0xE4, func, "mr_playSound", None),
    entry!(0xE8, func, "mr_stopSound", None),
    entry!(0xEC, func, "mr_sendSms", None),
    entry!(0xF0, func, "mr_call", None),
    entry!(0xF4, func, "mr_getNetworkID", None),
    entry!(0xF8, func, "mr_connectWAP", None),
    entry!(0xFC, func, "mr_menuCreate", None),
    entry!(0x100, func, "mr_menuSetItem", None),
    entry!(0x104, func, "mr_menuShow", None),
    entry!(0x108, data, "reserve"),
    entry!(0x10C, func, "mr_menuRelease", None),
    entry!(0x110, func, "mr_menuRefresh", None),
    entry!(0x114, func, "mr_dialogCreate", None),
    entry!(0x118, func, "mr_dialogRelease", None),
    entry!(0x11C, func, "mr_dialogRefresh", None),
    entry!(0x120, func, "mr_textCreate", None),
    entry!(0x124, func, "mr_textRelease", None),
    entry!(0x128, func, "mr_textRefresh", None),
    entry!(0x12C, func, "mr_editCreate", None),
    entry!(0x130, func, "mr_editRelease", None),
    entry!(0x134, func, "mr_editGetText", None),
    entry!(0x138, func, "mr_winCreate", None),
    entry!(0x13C, func, "mr_winRelease", None),
    entry!(0x140, func, "mr_getScreenInfo", None),
    entry!(0x144, func, "mr_initNetwork", None),
    entry!(0x148, func, "mr_closeNetwork", None),
    entry!(0x14C, func, "mr_getHostByName", None),
    entry!(0x150, func, "mr_socket", None),
    entry!(0x154, func, "mr_connect", None),
    entry!(0x158, func, "mr_closeSocket", None),
    entry!(0x15C, func, "mr_recv", None),
    entry!(0x160, func, "mr_recvfrom", None),
    entry!(0x164, func, "mr_send", None),
    entry!(0x168, func, "mr_sendto", None),
    entry!(0x16C, data, "mr_screenBuf"),
    entry!(0x170, data, "mr_screen_w"),
    entry!(0x174, data, "mr_screen_h"),
    entry!(0x178, data, "mr_screen_bit"),
    entry!(0x17C, data, "mr_bitmap"),
    entry!(0x180, data, "mr_tile"),
    entry!(0x184, data, "mr_map"),
    entry!(0x188, data, "mr_sound"),
    entry!(0x18C, data, "mr_sprite"),
    entry!(0x190, data, "pack_filename"),
    entry!(0x194, data, "start_filename"),
    entry!(0x198, data, "old_pack_filename"),
    entry!(0x19C, data, "old_start_filename"),
    entry!(0x1A0, data, "mr_ram_file"),
    entry!(0x1A4, data, "mr_ram_file_len"),
    entry!(0x1A8, data, "mr_soundOn"),
    entry!(0x1AC, data, "mr_shakeOn"),
    entry!(0x1B0, data, "LG_mem_base"),
    entry!(0x1B4, data, "LG_mem_len"),
    entry!(0x1B8, data, "LG_mem_end"),
    entry!(0x1BC, data, "LG_mem_left"),
    entry!(0x1C0, data, "mr_sms_cfg_buf"),
    entry!(0x1C4, func, "mr_md5_init", None),
    entry!(0x1C8, func, "mr_md5_append", None),
    entry!(0x1CC, func, "mr_md5_finish", None),
    entry!(0x1D0, func, "_mr_load_sms_cfg", None),
    entry!(0x1D4, func, "_mr_save_sms_cfg", None),
    entry!(0x1D8, func, "_DispUpEx", None),
    entry!(0x1DC, func, "_DrawPoint", None),
    entry!(0x1E0, func, "_DrawBitmap", None),
    entry!(0x1E4, func, "_DrawBitmapEx", None),
    entry!(0x1E8, func, "DrawRect", None),
    entry!(0x1EC, func, "_DrawText", None),
    entry!(0x1F0, func, "_BitmapCheck", None),
    entry!(0x1F4, func, "_mr_readFile", None),
    entry!(0x1F8, func, "mr_wstrlen", None),
    entry!(0x1FC, func, "mr_registerAPP", None),
    entry!(0x200, func, "_DrawTextEx", None),
    entry!(0x204, func, "_mr_EffSetCon", None),
    entry!(0x208, func, "_mr_TestCom", None),
    entry!(0x20C, func, "_mr_TestCom1", None),
    entry!(0x210, func, "c2u", None),
    entry!(0x214, func, "_mr_div", None),
    entry!(0x218, func, "_mr_mod", None),
    entry!(0x21C, data, "LG_mem_min"),
    entry!(0x220, data, "LG_mem_top"),
    entry!(0x224, data, "mr_updcrc"),
    entry!(0x228, data, "start_fileparameter"),
    entry!(0x22C, data, "mr_sms_return_flag"),
    entry!(0x230, data, "mr_sms_return_val"),
    entry!(0x234, data, "mr_unzip"),
    entry!(0x238, data, "mr_exit_cb"),
    entry!(0x23C, data, "mr_exit_cb_data"),
    entry!(0x240, data, "mr_entry"),
    entry!(0x244, func, "mr_platDrawChar", None),
];

static DSM_REQUIRE_FUNCS_MAP: &[BridgeEntry] = &[
    entry!(0x0, func, "test", Some(br_test)),
    entry!(0x4, func, "log", Some(br_log)),
    entry!(0x8, func, "exit", Some(br_exit)),
    entry!(0xC, func, "srand", Some(br_srand)),
    entry!(0x10, func, "rand", Some(br_rand)),
    entry!(0x14, func, "mem_get", Some(br_mem_get)),
    entry!(0x18, func, "mem_free", Some(br_mem_free)),
    entry!(0x1C, func, "timerStart", Some(br_timer_start)),
    entry!(0x20, func, "timerStop", Some(br_timer_stop)),
    entry!(
        0x24,
        func,
        "get_uptime_ms",
        Some(br_get_uptime_ms_init),
        Some(br_get_uptime_ms)
    ),
    entry!(0x28, func, "getDatetime", Some(br_get_datetime)),
    entry!(0x2C, func, "sleep", Some(br_sleep)),
    entry!(0x30, func, "open", Some(br_mr_open)),
    entry!(0x34, func, "close", Some(br_mr_close)),
    entry!(0x38, func, "read", Some(br_mr_read)),
    entry!(0x3C, func, "write", Some(br_mr_write)),
    entry!(0x40, func, "seek", Some(br_mr_seek)),
    entry!(0x44, func, "info", Some(br_info)),
    entry!(0x48, func, "remove", Some(br_mr_remove)),
    entry!(0x4C, func, "rename", Some(br_mr_rename)),
    entry!(0x50, func, "mkDir", Some(br_mr_mkdir)),
    entry!(0x54, func, "rmDir", Some(br_mr_rmdir)),
    entry!(0x58, func, "opendir", Some(br_opendir)),
    entry!(
        0x5C,
        func,
        "readdir",
        Some(br_readdir_init),
        Some(br_readdir)
    ),
    entry!(0x60, func, "closedir", Some(br_closedir)),
    entry!(0x64, func, "getLen", Some(br_mr_get_len)),
    entry!(0x68, func, "drawBitmap", Some(br_mr_draw_bitmap)),
    entry!(0x6C, func, "getHostByName", Some(br_mr_get_host_by_name)),
    entry!(0x70, func, "initNetwork", Some(br_mr_init_network)),
    entry!(0x74, func, "mr_closeNetwork", Some(br_mr_close_network)),
    entry!(0x78, func, "mr_socket", Some(br_mr_socket)),
    entry!(0x7C, func, "mr_connect", Some(br_mr_connect)),
    entry!(
        0x80,
        func,
        "mr_getSocketState",
        Some(br_mr_get_socket_state)
    ),
    entry!(0x84, func, "mr_closeSocket", Some(br_mr_close_socket)),
    entry!(0x88, func, "mr_recv", Some(br_mr_recv)),
    entry!(0x8C, func, "mr_send", Some(br_mr_send)),
    entry!(0x90, func, "mr_recvfrom", Some(br_mr_recvfrom)),
    entry!(0x94, func, "mr_sendto", Some(br_mr_sendto)),
    entry!(0x98, func, "mr_startShake", Some(br_mr_start_shake)),
    entry!(0x9C, func, "mr_stopShake", Some(br_mr_stop_shake)),
    entry!(0xA0, func, "mr_playSound", Some(br_mr_play_sound)),
    entry!(0xA4, func, "mr_stopSound", Some(br_mr_stop_sound)),
    entry!(0xA8, func, "mr_dialogCreate", Some(br_return_failed)),
    entry!(0xAC, func, "mr_dialogRelease", Some(br_return_failed)),
    entry!(0xB0, func, "mr_dialogRefresh", Some(br_return_failed)),
    entry!(0xB4, func, "mr_textCreate", Some(br_return_failed)),
    entry!(0xB8, func, "mr_textRelease", Some(br_return_failed)),
    entry!(0xBC, func, "mr_textRefresh", Some(br_return_failed)),
    entry!(0xC0, func, "mr_editCreate", Some(br_mr_edit_create)),
    entry!(0xC4, func, "mr_editRelease", Some(br_mr_edit_release)),
    entry!(0xC8, func, "mr_editGetText", Some(br_mr_edit_get_text)),
];

pub fn bridge_init(uc: *mut c_void) -> c_int {
    syscall::ensure_unicorn_svc_hook(uc);

    let len = 4 * MR_TABLE_FUNC_MAP.len() as u32;
    unsafe {
        MR_TABLE = hooks_init(uc, MR_TABLE_FUNC_MAP, len);

        DSM_REQUIRE_FUNCS = hooks_init(uc, DSM_REQUIRE_FUNCS_MAP, DSM_REQUIRE_FUNCS_SIZE);
    }
    let dsm_require_funcs = unsafe { DSM_REQUIRE_FUNCS };
    let flags_addr = runtime::to_mrp_mem_addr(dsm_require_funcs) + 0xcc;
    unicorn::mem_write_u32(uc, flags_addr as u64, FLAG_USE_UTF8_EDIT).ok();

    unsafe {
        MR_C_EVENT = compat::malloc_ext(std::mem::size_of::<Event>() as u32) as *mut Event;
        DSM_EVENT = compat::malloc_ext(std::mem::size_of::<Event>() as u32) as *mut Event;
        MR_START_DSM_PARAM = compat::malloc_ext(std::mem::size_of::<Start>() as u32) as *mut Start;
    }
    MR_SUCCESS
}

pub fn bridge_ext_init(uc: *mut c_void) -> c_int {
    let mut ctx = BridgeContext::new(uc);
    let mr_table = unsafe { MR_TABLE };
    let mut v = runtime::to_mrp_mem_addr(mr_table);
    ctx.mem_write_u32(CODE_ADDRESS, v);

    v = 1;
    ctx.write_reg(AbiReg::R0, v);
    run_code(&mut ctx, CODE_ADDRESS + 8, CODE_ADDRESS, false);

    let mr_c_function_p = unsafe { MR_C_FUNCTION_P };
    if !mr_c_function_p.is_null() {
        log!("-----> r9:@0x{:X}", unsafe {
            (*mr_c_function_p).start_of_er_rw
        });
    }
    MR_SUCCESS
}

fn bridge_mr_ext_helper(uc: *mut c_void, code: u32, input: u32, input_len: u32) -> c_int {
    let mut ctx = BridgeContext::new(uc);
    let mr_c_function_p = unsafe { MR_C_FUNCTION_P };
    let p = runtime::to_mrp_mem_addr(mr_c_function_p as *mut c_void);
    ctx.write_reg(AbiReg::R0, p);
    ctx.write_reg(AbiReg::R1, code);
    ctx.write_reg(AbiReg::R2, input);
    ctx.write_reg(AbiReg::R3, input_len);

    let helper_addr = unsafe { MR_EXT_HELPER_ADDR };
    run_code(&mut ctx, helper_addr, CODE_ADDRESS, false);
    ctx.arg::<u32>(0) as c_int
}

fn bridge_mr_event(uc: *mut c_void, code: c_int, param0: c_int, param1: c_int) -> c_int {
    let mr_c_event = unsafe { MR_C_EVENT };
    unsafe {
        (*mr_c_event).code = code;
        (*mr_c_event).p0 = param0;
        (*mr_c_event).p1 = param1;
    }
    bridge_mr_ext_helper(
        uc,
        1,
        runtime::to_mrp_mem_addr(mr_c_event as *mut c_void),
        std::mem::size_of::<Event>() as u32,
    )
}

pub fn bridge_dsm_network_cb(uc: *mut c_void, addr: u32, p0: c_int, p1: u32) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    let mut ctx = BridgeContext::new(uc);
    let r9 = ctx.read_reg(AbiReg::R9);

    let mr_c_function_p = unsafe { MR_C_FUNCTION_P };
    if !mr_c_function_p.is_null() {
        ctx.write_reg(AbiReg::R9, unsafe { (*mr_c_function_p).start_of_er_rw });
    }
    ctx.write_reg(AbiReg::R0, p0 as u32);
    ctx.write_reg(AbiReg::R1, p1);
    run_code(&mut ctx, addr, CODE_ADDRESS, false);

    ctx.write_reg(AbiReg::R9, r9);
    ctx.arg::<u32>(0) as c_int
}

pub fn bridge_dsm_mr_start_dsm(
    uc: *mut c_void,
    filename: *mut c_char,
    ext: *mut c_char,
    entry: *mut c_char,
) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();

    let start_param = unsafe { MR_START_DSM_PARAM };
    unsafe {
        (*start_param).filename = compat::copy_str_to_mrp(filename);
        (*start_param).ext = compat::copy_str_to_mrp(ext);
        (*start_param).entry = if entry.is_null() {
            0
        } else {
            compat::copy_str_to_mrp(entry)
        };
    }

    let input = runtime::to_mrp_mem_addr(start_param as *mut c_void) as c_int;
    let ret = bridge_mr_event(uc, MR_START_DSM, input, 0);

    let (filename_addr, ext_addr, entry_addr) = unsafe {
        (
            (*start_param).filename,
            (*start_param).ext,
            (*start_param).entry,
        )
    };
    compat::free_ext(runtime::get_mrp_mem_ptr(filename_addr));
    unsafe {
        (*start_param).filename = 0;
    }
    compat::free_ext(runtime::get_mrp_mem_ptr(ext_addr));
    unsafe {
        (*start_param).ext = 0;
    }

    if !entry.is_null() {
        compat::free_ext(runtime::get_mrp_mem_ptr(entry_addr));
        unsafe {
            (*start_param).entry = 0;
        }
    }
    ret
}

pub fn bridge_dsm_mr_pause_app(uc: *mut c_void) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    bridge_mr_event(uc, MR_PAUSEAPP, 0, 0)
}

pub fn bridge_dsm_mr_resume_app(uc: *mut c_void) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    bridge_mr_event(uc, MR_RESUMEAPP, 0, 0)
}

pub fn bridge_dsm_mr_timer(uc: *mut c_void) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    bridge_mr_event(uc, MR_TIMER, 0, 0)
}

pub fn bridge_dsm_mr_event(uc: *mut c_void, code: c_int, p0: c_int, p1: c_int) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    let dsm_event = unsafe { DSM_EVENT };
    unsafe {
        (*dsm_event).code = code;
        (*dsm_event).p0 = p0;
        (*dsm_event).p1 = p1;
    }
    bridge_mr_event(
        uc,
        MR_EVENT,
        runtime::to_mrp_mem_addr(dsm_event as *mut c_void) as c_int,
        0,
    )
}

pub fn bridge_dsm_init(uc: *mut c_void) -> c_int {
    let dsm_require_funcs = unsafe { DSM_REQUIRE_FUNCS };
    let ret = {
        let _guard = BRIDGE_LOCK.lock().unwrap();
        bridge_mr_event(
            uc,
            DSM_INIT,
            runtime::to_mrp_mem_addr(dsm_require_funcs) as c_int,
            0,
        )
    };
    if ret == BRIDGE_VER {
        MR_SUCCESS
    } else {
        log!("err: dsm_version got {ret} expect {BRIDGE_VER}");
        MR_FAILED
    }
}
