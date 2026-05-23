use crate::abi::{
    self, AbiReg, CallFromGuest, CallFromHost, GuestArg, GuestFunction, GuestRet, RegisterContext,
    StackMemoryContext,
};
use crate::bootstrap;
use crate::cpu::Cpu;
use crate::environment::Environment;
use crate::mem::{ConstPtr, GuestUSize, MutPtr, Ptr, SafeWrite};
use crate::{compat, file, network, window};
use libc::{c_char, c_int, c_ushort, c_void};
use std::ffi::CStr;
use std::ptr;
use std::sync::Mutex;

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

type BridgeHandler = &'static dyn BridgeCallFromGuest;
type BridgeInit = fn(&BridgeEntry, &mut Environment, u32);

#[derive(Clone, Copy)]
struct BridgeEntry {
    pos: u32,
    type_: BridgeMapType,
    name: &'static str,
    init: Option<BridgeInit>,
    handler: Option<BridgeHandler>,
}

impl CallFromGuest for BridgeEntry {
    fn call_from_guest(&self, env: &mut Environment) {
        let Some(handler) = self.handler else {
            log!("!!! {}() Not yet implemented function !!!", self.name);
            std::process::exit(1);
        };

        log!("[SVC] -> {} pos=0x{:X} type=func", self.name, self.pos);
        let mut ctx = BridgeContext::new(env);
        handler.call_from_guest(self, &mut ctx);
        let ret = ctx.arg::<u32>(0);
        let pc = ctx.read_reg(AbiReg::PC);
        let lr = ctx.read_reg(AbiReg::LR);
        log!(
            "[SVC] <- {} ret=0x{ret:08X} pc=0x{pc:08X} lr=0x{lr:08X}",
            self.name
        );
    }
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

trait BridgeCallFromGuest: Sync {
    fn call_from_guest(&self, entry: &BridgeEntry, ctx: &mut BridgeContext<'_>);
}

impl BridgeCallFromGuest for fn(&BridgeEntry, &mut BridgeContext<'_>) {
    fn call_from_guest(&self, entry: &BridgeEntry, ctx: &mut BridgeContext<'_>) {
        self(entry, ctx);
    }
}

macro_rules! impl_bridge_call_from_guest {
    ( $($p:tt => $P:ident),* ) => {
        impl<R, $($P),*> BridgeCallFromGuest for fn(&mut BridgeContext<'_>, $($P),*) -> R
        where
            R: GuestRet,
            $($P: GuestArg,)*
        {
            #[allow(unused_variables, unused_mut, clippy::unused_unit)]
            fn call_from_guest(&self, _entry: &BridgeEntry, ctx: &mut BridgeContext<'_>) {
                let args: ($($P,)*) = ($(ctx.arg::<$P>($p),)*);
                let ret = self(ctx, $(args.$p),*);
                ctx.ret(ret);
            }
        }
    };
}

impl_bridge_call_from_guest!();
impl_bridge_call_from_guest!(0 => P0);
impl_bridge_call_from_guest!(0 => P0, 1 => P1);
impl_bridge_call_from_guest!(0 => P0, 1 => P1, 2 => P2);
impl_bridge_call_from_guest!(0 => P0, 1 => P1, 2 => P2, 3 => P3);
impl_bridge_call_from_guest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4);

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
    ($pos:expr, func, $name:expr, None) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Func,
            name: $name,
            init: None,
            handler: None,
        }
    };
    ($pos:expr, func, $name:expr, Some($handler:path)) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Func,
            name: $name,
            init: None,
            handler: Some(&($handler as fn(&BridgeEntry, &mut BridgeContext<'_>)) as BridgeHandler),
        }
    };
    ($pos:expr, func, $name:expr, typed $handler:path as $ty:ty) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Func,
            name: $name,
            init: None,
            handler: Some(&($handler as $ty) as BridgeHandler),
        }
    };
    ($pos:expr, func, $name:expr, $init:expr, typed $handler:path as $ty:ty) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Func,
            name: $name,
            init: $init,
            handler: Some(&($handler as $ty) as BridgeHandler),
        }
    };
    ($pos:expr, func, $name:expr, $init:expr, None) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Func,
            name: $name,
            init: $init,
            handler: None,
        }
    };
    ($pos:expr, func, $name:expr, $init:expr, Some($handler:path)) => {
        BridgeEntry {
            pos: $pos,
            type_: BridgeMapType::Func,
            name: $name,
            init: $init,
            handler: Some(&($handler as fn(&BridgeEntry, &mut BridgeContext<'_>)) as BridgeHandler),
        }
    };
}

struct BridgeContext<'a> {
    env: &'a mut Environment,
}

impl<'a> BridgeContext<'a> {
    fn new(env: &'a mut Environment) -> Self {
        Self { env }
    }

    fn env(&self) -> &Environment {
        self.env
    }

    fn env_mut(&mut self) -> &mut Environment {
        self.env
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
        let mem = &self.env().mem;
        let len = mem.cstr_at(ptr).len();
        let bytes = mem.bytes_at(ptr, u32::try_from(len + 1).unwrap());
        let cstr = CStr::from_bytes_with_nul(bytes).expect("guest C string is not NUL-terminated");
        f(cstr)
    }

    fn with_two_cstr<R>(
        &self,
        first: ConstPtr<u8>,
        second: ConstPtr<u8>,
        f: impl FnOnce(&CStr, &CStr) -> R,
    ) -> R {
        let mem = &self.env().mem;
        let first_len = mem.cstr_at(first).len();
        let second_len = mem.cstr_at(second).len();
        let first_bytes = mem.bytes_at(first, u32::try_from(first_len + 1).unwrap());
        let second_bytes = mem.bytes_at(second, u32::try_from(second_len + 1).unwrap());
        let first =
            CStr::from_bytes_with_nul(first_bytes).expect("guest C string is not NUL-terminated");
        let second =
            CStr::from_bytes_with_nul(second_bytes).expect("guest C string is not NUL-terminated");
        f(first, second)
    }

    fn with_bytes<R>(&self, ptr: ConstPtr<u8>, len: GuestUSize, f: impl FnOnce(&[u8]) -> R) -> R {
        f(self.env().mem.bytes_at(ptr, len))
    }

    fn with_bytes_mut<R>(
        &mut self,
        ptr: MutPtr<u8>,
        len: GuestUSize,
        f: impl FnOnce(&mut [u8]) -> R,
    ) -> R {
        f(self.env_mut().mem.bytes_at_mut(ptr, len))
    }

    fn memmove(&mut self, dst: MutPtr<c_void>, src: ConstPtr<c_void>, len: GuestUSize) {
        self.env_mut().mem.memmove(dst, src, len);
    }

    fn write_guest<T>(&mut self, ptr: MutPtr<T>, value: T)
    where
        T: SafeWrite,
    {
        self.env_mut().mem.write(ptr, value);
    }

    fn mem_read_u32(&self, addr: u32) -> u32 {
        self.env().mem.read(Ptr::<u32, false>::from_bits(addr))
    }

    fn mem_write_u32(&mut self, addr: u32, value: u32) {
        self.env_mut()
            .mem
            .write(MutPtr::<u32>::from_bits(addr), value);
    }
}

impl RegisterContext for BridgeContext<'_> {
    fn read_reg(&mut self, reg: AbiReg) -> u32 {
        self.env_mut().cpu.regs()[abi_reg_index(reg)]
    }

    fn write_reg(&mut self, reg: AbiReg, value: u32) {
        self.env_mut().cpu.regs_mut()[abi_reg_index(reg)] = value;
    }
}

impl StackMemoryContext for BridgeContext<'_> {
    fn read_stack_u32(&mut self, sp: u32, word_offset: usize) -> u32 {
        self.mem_read_u32(sp + u32::try_from(word_offset).unwrap() * 4)
    }

    fn write_stack_u32(&mut self, sp: u32, word_offset: usize, value: u32) {
        self.mem_write_u32(sp + u32::try_from(word_offset).unwrap() * 4, value);
    }
}

fn abi_reg_index(reg: AbiReg) -> usize {
    match reg {
        AbiReg::R0 => 0,
        AbiReg::R1 => 1,
        AbiReg::R2 => 2,
        AbiReg::R3 => 3,
        AbiReg::R4 => 4,
        AbiReg::R5 => 5,
        AbiReg::R6 => 6,
        AbiReg::R7 => 7,
        AbiReg::R8 => 8,
        AbiReg::R9 => 9,
        AbiReg::R10 => 10,
        AbiReg::R11 => 11,
        AbiReg::R12 => 12,
        AbiReg::SP => Cpu::SP,
        AbiReg::LR => Cpu::LR,
        AbiReg::PC => Cpu::PC,
    }
}

fn run_code(env: &mut Environment, start_addr: u32, is_thumb: bool) {
    let function = GuestFunction::from_addr_and_thumb_flag(start_addr, is_thumb);
    let _: u32 = function.call_from_host(env, ());
}

fn hooks_init(env: &mut Environment, map: &'static [BridgeEntry], table_size: u32) -> *mut c_void {
    let func_count = map
        .iter()
        .filter(|obj| obj.type_ == BridgeMapType::Func)
        .count() as u32;
    let ptr = compat::malloc_ext_in(&mut env.mem, table_size + func_count * 8);
    let start_address = bootstrap::to_mrp_mem_addr(&env.mem, ptr);
    let mut stub_address = start_address + table_size;

    for obj in map {
        let addr = start_address + obj.pos;
        match obj.type_ {
            BridgeMapType::Data => {
                if let Some(init) = obj.init {
                    init(obj, env, addr);
                }
            }
            BridgeMapType::Func => {
                if let Some(init) = obj.init {
                    init(obj, env, addr);
                }
                let svc = env.syscall.link_typed_host_function(
                    &mut env.mem,
                    stub_address,
                    obj.name,
                    obj as &'static dyn CallFromGuest,
                );
                env.mem.write(MutPtr::<u32>::from_bits(addr), stub_address);
                log!("[SVC] linked {} pos=0x{:X} svc=#{svc}", obj.name, obj.pos);
                stub_address += 8;
            }
        }
    }
    ptr
}

fn br_mr_c_function_new(ctx: &mut BridgeContext, p_f: u32, p_len: u32) -> u32 {
    log!("ext call _mr_c_function_new(0x{p_f:X}[{p_f}], 0x{p_len:X}[{p_len}])");

    let mr_c_function_p = compat::malloc_ext_in(&mut ctx.env_mut().mem, p_len) as *mut MrCFunctionP;
    unsafe {
        MR_EXT_HELPER_ADDR = p_f;
        MR_C_FUNCTION_P = mr_c_function_p;
        ptr::write_bytes(mr_c_function_p as *mut u8, 0, p_len as usize);
    }

    let v = bootstrap::to_mrp_mem_addr(&ctx.env().mem, mr_c_function_p as *mut c_void);
    ctx.mem_write_u32(CODE_ADDRESS + 4, v);
    MR_SUCCESS as u32
}

fn br_mr_malloc(ctx: &mut BridgeContext, len: u32) -> MutPtr<c_void> {
    compat::malloc_ext_guest_in(&mut ctx.env_mut().mem, len)
}

fn br_mr_free(ctx: &mut BridgeContext, p: MutPtr<c_void>) {
    compat::free_ext_guest_in(&mut ctx.env_mut().mem, p);
}

fn br_memcpy(
    ctx: &mut BridgeContext,
    dst: MutPtr<c_void>,
    src: ConstPtr<c_void>,
    n: u32,
) -> MutPtr<c_void> {
    ctx.memmove(dst, src, n);
    dst
}

fn br_memset(ctx: &mut BridgeContext, dst: MutPtr<u8>, value: u32, n: u32) -> MutPtr<u8> {
    ctx.with_bytes_mut(dst, n, |bytes| bytes.fill(value as u8));
    dst
}

fn br_mr_draw_bitmap(ctx: &mut BridgeContext, bmp: ConstPtr<u16>, x: u32, y: u32, w: u32, h: u32) {
    let x = x as c_int;
    let y = y as c_int;
    let w = w as c_int;
    let h = h as c_int;
    let pixel_count = (w as u32).saturating_mul(h as u32);
    ctx.with_bytes(bmp.cast::<u8>(), pixel_count.saturating_mul(2), |bytes| {
        let pixels = bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        window::draw_bitmap(&pixels, x, y, w, h);
    });
}

fn br_mr_open(ctx: &mut BridgeContext, filename: ConstPtr<u8>, mode: u32) -> u32 {
    ctx.with_cstr(filename, |filename| file::open_cstr(filename, mode)) as u32
}

fn br_mr_close(_ctx: &mut BridgeContext, f: u32) -> u32 {
    file::close(f as c_int) as u32
}

fn br_mr_write(ctx: &mut BridgeContext, f: u32, p: ConstPtr<u8>, l: u32) -> u32 {
    ctx.with_bytes(p, l, |bytes| file::write_from(f as c_int, bytes)) as u32
}

fn br_mr_read(ctx: &mut BridgeContext, f: u32, p: MutPtr<u8>, l: u32) -> u32 {
    ctx.with_bytes_mut(p, l, |bytes| file::read_into(f as c_int, bytes)) as u32
}

fn br_mr_seek(_ctx: &mut BridgeContext, f: u32, pos: u32, method: u32) -> u32 {
    file::seek(f as c_int, pos as c_int, method as c_int) as u32
}

fn br_mr_get_len(ctx: &mut BridgeContext, filename: ConstPtr<u8>) -> u32 {
    ctx.with_cstr(filename, file::get_len_cstr) as u32
}

fn br_mr_remove(ctx: &mut BridgeContext, filename: ConstPtr<u8>) -> u32 {
    ctx.with_cstr(filename, file::remove_cstr) as u32
}

fn br_mr_rename(ctx: &mut BridgeContext, oldname: ConstPtr<u8>, newname: ConstPtr<u8>) -> u32 {
    ctx.with_two_cstr(oldname, newname, file::rename_cstr) as u32
}

fn br_mr_mkdir(ctx: &mut BridgeContext, name: ConstPtr<u8>) -> u32 {
    ctx.with_cstr(name, file::mkdir_cstr) as u32
}

fn br_mr_rmdir(ctx: &mut BridgeContext, name: ConstPtr<u8>) -> u32 {
    ctx.with_cstr(name, file::rmdir_cstr) as u32
}

fn br_get_uptime_ms_init(_o: &BridgeEntry, env: &mut Environment, addr: u32) {
    unsafe {
        UPTIME_MS = compat::get_uptime_ms() as u64;
    }
    env.mem.write(MutPtr::<u32>::from_bits(addr), addr);
}

fn br_get_uptime_ms(_ctx: &mut BridgeContext) -> u32 {
    let uptime_ms = unsafe { UPTIME_MS };
    (compat::get_uptime_ms() as u64).wrapping_sub(uptime_ms) as u32
}

fn br_log(ctx: &mut BridgeContext, msg: ConstPtr<u8>) {
    if !msg.is_null() {
        let text = ctx.with_cstr(msg, |msg| msg.to_string_lossy().into_owned());
        log!("{text}");
    }
}

fn br_mem_get(ctx: &mut BridgeContext, mem_base: MutPtr<u32>, mem_len: MutPtr<u32>) -> u32 {
    let len = 1024 * 1024 * 4u32;
    let buffer = compat::malloc_ext_guest_in(&mut ctx.env_mut().mem, len);
    log!(
        "br_mem_get base=0x{:X} len={len}({} kb) =================",
        buffer.to_bits(),
        len / 1024
    );
    ctx.write_guest(mem_base, buffer.to_bits());
    ctx.write_guest(mem_len, len);
    MR_SUCCESS as u32
}

fn br_mem_free(ctx: &mut BridgeContext, mem: MutPtr<c_void>) -> u32 {
    compat::free_ext_guest_in(&mut ctx.env_mut().mem, mem);
    MR_SUCCESS as u32
}

fn br_timer_stop(_ctx: &mut BridgeContext) -> u32 {
    window::timer_stop() as u32
}

fn br_timer_start(_ctx: &mut BridgeContext, t: u32) -> u32 {
    window::timer_start(t as c_ushort) as u32
}

fn br_test(_ctx: &mut BridgeContext) {}

fn br_exit(_ctx: &mut BridgeContext) {
    log!("mythroad exit.\n");
    std::process::exit(0);
}

fn br_srand(_ctx: &mut BridgeContext, seed: u32) {
    unsafe {
        libc::srand(seed);
    }
}

fn br_rand(_ctx: &mut BridgeContext) -> u32 {
    (unsafe { libc::rand() }) as u32
}

fn br_sleep(_ctx: &mut BridgeContext, ms: u32) -> u32 {
    unsafe {
        libc::usleep(ms.saturating_mul(1000));
    }
    MR_SUCCESS as u32
}

fn br_info(ctx: &mut BridgeContext, filename: ConstPtr<u8>) -> u32 {
    ctx.with_cstr(filename, file::info_cstr) as u32
}

fn br_opendir(ctx: &mut BridgeContext, name: ConstPtr<u8>) -> u32 {
    ctx.with_cstr(name, file::opendir_cstr) as u32
}

fn br_readdir_init(_o: &BridgeEntry, env: &mut Environment, addr: u32) {
    let shared_mem =
        compat::malloc_ext_guest_in(&mut env.mem, READDIR_SHARED_MEM_SIZE as u32).to_bits();
    unsafe {
        READDIR_SHARED_MEM = shared_mem;
    }
    env.mem
        .bytes_at_mut(
            MutPtr::<u8>::from_bits(shared_mem),
            READDIR_SHARED_MEM_SIZE as u32,
        )
        .fill(0);
    env.mem.write(MutPtr::<u32>::from_bits(addr), addr);
}

fn br_readdir(ctx: &mut BridgeContext, f: u32) -> u32 {
    let Some(name) = file::readdir_name(f as c_int) else {
        return 0;
    };
    let shared_mem = unsafe { READDIR_SHARED_MEM };
    if shared_mem == 0 {
        return 0;
    }
    let len = name.len().min(READDIR_SHARED_MEM_SIZE - 1);
    let bytes = ctx.env_mut().mem.bytes_at_mut(
        MutPtr::<u8>::from_bits(shared_mem),
        READDIR_SHARED_MEM_SIZE as u32,
    );
    bytes.fill(0);
    bytes[..len].copy_from_slice(&name[..len]);
    shared_mem
}

fn br_closedir(_ctx: &mut BridgeContext, f: u32) -> u32 {
    file::closedir(f as c_int) as u32
}

fn br_get_datetime(ctx: &mut BridgeContext, datetime: MutPtr<c_void>) -> u32 {
    let Some(now) = compat::current_datetime() else {
        return MR_FAILED as u32;
    };
    let base = datetime.to_bits();
    ctx.write_guest(MutPtr::<u16>::from_bits(base), now.year);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 2), now.month);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 3), now.day);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 4), now.hour);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 5), now.minute);
    ctx.write_guest(MutPtr::<u8>::from_bits(base + 6), now.second);
    MR_SUCCESS as u32
}

fn br_mr_init_network(ctx: &mut BridgeContext, cb: u32, mode: ConstPtr<u8>, user_data: u32) -> u32 {
    ctx.with_cstr(mode, |mode| network::init_network_cstr(cb, mode, user_data)) as u32
}

fn br_mr_socket(_ctx: &mut BridgeContext, type_: u32, protocol: u32) -> u32 {
    network::socket(type_ as c_int, protocol as c_int) as u32
}

fn br_mr_connect(_ctx: &mut BridgeContext, s: u32, ip: u32, port: u32, type_: u32) -> u32 {
    network::connect(s as c_int, ip as c_int, port as c_ushort, type_ as c_int) as u32
}

fn br_mr_close_socket(_ctx: &mut BridgeContext, s: u32) -> u32 {
    network::close_socket(s as c_int) as u32
}

fn br_mr_close_network(_ctx: &mut BridgeContext) -> u32 {
    network::close_network() as u32
}

fn br_mr_get_host_by_name(
    ctx: &mut BridgeContext,
    name: ConstPtr<u8>,
    cb: u32,
    user_data: u32,
) -> u32 {
    ctx.with_cstr(name, |name| {
        network::get_host_by_name_cstr(name, cb, user_data)
    }) as u32
}

fn br_mr_sendto(
    ctx: &mut BridgeContext,
    s: u32,
    buf: ConstPtr<u8>,
    len: u32,
    ip: u32,
    port: u32,
) -> u32 {
    ctx.with_bytes(buf, len, |buf| {
        network::send_to(s as c_int, buf, ip as c_int, port as c_ushort)
    }) as u32
}

fn br_mr_send(ctx: &mut BridgeContext, s: u32, buf: ConstPtr<u8>, len: u32) -> u32 {
    ctx.with_bytes(buf, len, |buf| network::send(s as c_int, buf)) as u32
}

fn br_mr_recvfrom(
    ctx: &mut BridgeContext,
    s: u32,
    buf: MutPtr<u8>,
    len: u32,
    ip: MutPtr<c_int>,
    port: MutPtr<c_ushort>,
) -> u32 {
    let mut ip_value = 0;
    let mut port_value = 0;
    let ret = ctx.with_bytes_mut(buf, len, |buf| {
        network::recv_from(s as c_int, buf, &mut ip_value, &mut port_value)
    });
    ctx.write_guest(ip, ip_value);
    ctx.write_guest(port, port_value);
    ret as u32
}

fn br_mr_recv(ctx: &mut BridgeContext, s: u32, buf: MutPtr<u8>, len: u32) -> u32 {
    ctx.with_bytes_mut(buf, len, |buf| network::recv(s as c_int, buf)) as u32
}

fn br_mr_get_socket_state(_ctx: &mut BridgeContext, s: u32) -> u32 {
    network::socket_state(s as c_int) as u32
}

fn br_mr_play_sound(
    ctx: &mut BridgeContext,
    type_: u32,
    data: ConstPtr<u8>,
    data_len: u32,
    loop_: u32,
) -> u32 {
    ctx.with_bytes(data, data_len, |data| {
        window::play_sound_bytes(type_ as c_int, data, loop_ != 0)
    }) as u32
}

fn br_mr_stop_sound(_ctx: &mut BridgeContext, type_: u32) -> u32 {
    window::stop_sound(type_ as c_int) as u32
}

fn br_mr_start_shake(_ctx: &mut BridgeContext) -> u32 {
    MR_SUCCESS as u32
}

fn br_mr_stop_shake(_ctx: &mut BridgeContext) -> u32 {
    MR_SUCCESS as u32
}

fn br_return_failed(_ctx: &mut BridgeContext) -> u32 {
    MR_FAILED as u32
}

fn br_mr_edit_create(
    ctx: &mut BridgeContext,
    title: ConstPtr<u8>,
    text: ConstPtr<u8>,
    type_: u32,
    max_size: u32,
) -> u32 {
    ctx.with_two_cstr(title, text, |title, text| {
        window::edit_create_cstr(title, text, type_ as c_int, max_size as c_int)
    }) as u32
}

fn br_mr_edit_release(ctx: &mut BridgeContext, _edit: u32) -> u32 {
    window::edit_release(&mut ctx.env_mut().mem) as u32
}

fn br_mr_edit_get_text(ctx: &mut BridgeContext, _edit: u32) -> u32 {
    bootstrap::to_mrp_mem_addr(&ctx.env().mem, window::edit_get_text() as *mut c_void)
}

static MR_TABLE_FUNC_MAP: &[BridgeEntry] = &[
    entry!(0x0, func, "mr_malloc", typed br_mr_malloc as fn(&mut BridgeContext, u32) -> MutPtr<c_void>),
    entry!(0x4, func, "mr_free", typed br_mr_free as fn(&mut BridgeContext, MutPtr<c_void>)),
    entry!(0x8, func, "mr_realloc", None),
    entry!(0xC, func, "memcpy", typed br_memcpy as fn(&mut BridgeContext, MutPtr<c_void>, ConstPtr<c_void>, u32) -> MutPtr<c_void>),
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
    entry!(0x38, func, "memset", typed br_memset as fn(&mut BridgeContext, MutPtr<u8>, u32, u32) -> MutPtr<u8>),
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
    entry!(0x64, func, "_mr_c_function_new", typed br_mr_c_function_new as fn(&mut BridgeContext, u32, u32) -> u32),
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
    entry!(0x0, func, "test", typed br_test as fn(&mut BridgeContext)),
    entry!(0x4, func, "log", typed br_log as fn(&mut BridgeContext, ConstPtr<u8>)),
    entry!(0x8, func, "exit", typed br_exit as fn(&mut BridgeContext)),
    entry!(0xC, func, "srand", typed br_srand as fn(&mut BridgeContext, u32)),
    entry!(0x10, func, "rand", typed br_rand as fn(&mut BridgeContext) -> u32),
    entry!(0x14, func, "mem_get", typed br_mem_get as fn(&mut BridgeContext, MutPtr<u32>, MutPtr<u32>) -> u32),
    entry!(0x18, func, "mem_free", typed br_mem_free as fn(&mut BridgeContext, MutPtr<c_void>) -> u32),
    entry!(0x1C, func, "timerStart", typed br_timer_start as fn(&mut BridgeContext, u32) -> u32),
    entry!(0x20, func, "timerStop", typed br_timer_stop as fn(&mut BridgeContext) -> u32),
    entry!(
        0x24,
        func,
        "get_uptime_ms",
        Some(br_get_uptime_ms_init),
        typed br_get_uptime_ms as fn(&mut BridgeContext) -> u32
    ),
    entry!(0x28, func, "getDatetime", typed br_get_datetime as fn(&mut BridgeContext, MutPtr<c_void>) -> u32),
    entry!(0x2C, func, "sleep", typed br_sleep as fn(&mut BridgeContext, u32) -> u32),
    entry!(0x30, func, "open", typed br_mr_open as fn(&mut BridgeContext, ConstPtr<u8>, u32) -> u32),
    entry!(0x34, func, "close", typed br_mr_close as fn(&mut BridgeContext, u32) -> u32),
    entry!(0x38, func, "read", typed br_mr_read as fn(&mut BridgeContext, u32, MutPtr<u8>, u32) -> u32),
    entry!(0x3C, func, "write", typed br_mr_write as fn(&mut BridgeContext, u32, ConstPtr<u8>, u32) -> u32),
    entry!(0x40, func, "seek", typed br_mr_seek as fn(&mut BridgeContext, u32, u32, u32) -> u32),
    entry!(0x44, func, "info", typed br_info as fn(&mut BridgeContext, ConstPtr<u8>) -> u32),
    entry!(0x48, func, "remove", typed br_mr_remove as fn(&mut BridgeContext, ConstPtr<u8>) -> u32),
    entry!(0x4C, func, "rename", typed br_mr_rename as fn(&mut BridgeContext, ConstPtr<u8>, ConstPtr<u8>) -> u32),
    entry!(0x50, func, "mkDir", typed br_mr_mkdir as fn(&mut BridgeContext, ConstPtr<u8>) -> u32),
    entry!(0x54, func, "rmDir", typed br_mr_rmdir as fn(&mut BridgeContext, ConstPtr<u8>) -> u32),
    entry!(0x58, func, "opendir", typed br_opendir as fn(&mut BridgeContext, ConstPtr<u8>) -> u32),
    entry!(
        0x5C,
        func,
        "readdir",
        Some(br_readdir_init),
        typed br_readdir as fn(&mut BridgeContext, u32) -> u32
    ),
    entry!(0x60, func, "closedir", typed br_closedir as fn(&mut BridgeContext, u32) -> u32),
    entry!(0x64, func, "getLen", typed br_mr_get_len as fn(&mut BridgeContext, ConstPtr<u8>) -> u32),
    entry!(0x68, func, "drawBitmap", typed br_mr_draw_bitmap as fn(&mut BridgeContext, ConstPtr<u16>, u32, u32, u32, u32)),
    entry!(0x6C, func, "getHostByName", typed br_mr_get_host_by_name as fn(&mut BridgeContext, ConstPtr<u8>, u32, u32) -> u32),
    entry!(0x70, func, "initNetwork", typed br_mr_init_network as fn(&mut BridgeContext, u32, ConstPtr<u8>, u32) -> u32),
    entry!(0x74, func, "mr_closeNetwork", typed br_mr_close_network as fn(&mut BridgeContext) -> u32),
    entry!(0x78, func, "mr_socket", typed br_mr_socket as fn(&mut BridgeContext, u32, u32) -> u32),
    entry!(0x7C, func, "mr_connect", typed br_mr_connect as fn(&mut BridgeContext, u32, u32, u32, u32) -> u32),
    entry!(
        0x80,
        func,
        "mr_getSocketState",
        typed br_mr_get_socket_state as fn(&mut BridgeContext, u32) -> u32
    ),
    entry!(0x84, func, "mr_closeSocket", typed br_mr_close_socket as fn(&mut BridgeContext, u32) -> u32),
    entry!(0x88, func, "mr_recv", typed br_mr_recv as fn(&mut BridgeContext, u32, MutPtr<u8>, u32) -> u32),
    entry!(0x8C, func, "mr_send", typed br_mr_send as fn(&mut BridgeContext, u32, ConstPtr<u8>, u32) -> u32),
    entry!(0x90, func, "mr_recvfrom", typed br_mr_recvfrom as fn(&mut BridgeContext, u32, MutPtr<u8>, u32, MutPtr<c_int>, MutPtr<c_ushort>) -> u32),
    entry!(0x94, func, "mr_sendto", typed br_mr_sendto as fn(&mut BridgeContext, u32, ConstPtr<u8>, u32, u32, u32) -> u32),
    entry!(0x98, func, "mr_startShake", typed br_mr_start_shake as fn(&mut BridgeContext) -> u32),
    entry!(0x9C, func, "mr_stopShake", typed br_mr_stop_shake as fn(&mut BridgeContext) -> u32),
    entry!(0xA0, func, "mr_playSound", typed br_mr_play_sound as fn(&mut BridgeContext, u32, ConstPtr<u8>, u32, u32) -> u32),
    entry!(0xA4, func, "mr_stopSound", typed br_mr_stop_sound as fn(&mut BridgeContext, u32) -> u32),
    entry!(0xA8, func, "mr_dialogCreate", typed br_return_failed as fn(&mut BridgeContext) -> u32),
    entry!(0xAC, func, "mr_dialogRelease", typed br_return_failed as fn(&mut BridgeContext) -> u32),
    entry!(0xB0, func, "mr_dialogRefresh", typed br_return_failed as fn(&mut BridgeContext) -> u32),
    entry!(0xB4, func, "mr_textCreate", typed br_return_failed as fn(&mut BridgeContext) -> u32),
    entry!(0xB8, func, "mr_textRelease", typed br_return_failed as fn(&mut BridgeContext) -> u32),
    entry!(0xBC, func, "mr_textRefresh", typed br_return_failed as fn(&mut BridgeContext) -> u32),
    entry!(0xC0, func, "mr_editCreate", typed br_mr_edit_create as fn(&mut BridgeContext, ConstPtr<u8>, ConstPtr<u8>, u32, u32) -> u32),
    entry!(0xC4, func, "mr_editRelease", typed br_mr_edit_release as fn(&mut BridgeContext, u32) -> u32),
    entry!(0xC8, func, "mr_editGetText", typed br_mr_edit_get_text as fn(&mut BridgeContext, u32) -> u32),
];

pub fn bridge_init(env: &mut Environment) -> c_int {
    let len = 4 * MR_TABLE_FUNC_MAP.len() as u32;
    unsafe {
        MR_TABLE = hooks_init(env, MR_TABLE_FUNC_MAP, len);

        DSM_REQUIRE_FUNCS = hooks_init(env, DSM_REQUIRE_FUNCS_MAP, DSM_REQUIRE_FUNCS_SIZE);
    }
    let dsm_require_funcs = unsafe { DSM_REQUIRE_FUNCS };
    let flags_addr = bootstrap::to_mrp_mem_addr(&env.mem, dsm_require_funcs) + 0xcc;
    env.mem
        .write(MutPtr::<u32>::from_bits(flags_addr), FLAG_USE_UTF8_EDIT);

    unsafe {
        MR_C_EVENT =
            compat::malloc_ext_in(&mut env.mem, std::mem::size_of::<Event>() as u32) as *mut Event;
        DSM_EVENT =
            compat::malloc_ext_in(&mut env.mem, std::mem::size_of::<Event>() as u32) as *mut Event;
        MR_START_DSM_PARAM =
            compat::malloc_ext_in(&mut env.mem, std::mem::size_of::<Start>() as u32) as *mut Start;
    }
    MR_SUCCESS
}

pub fn bridge_ext_init(env: &mut Environment) -> c_int {
    let mut ctx = BridgeContext::new(env);
    let mr_table = unsafe { MR_TABLE };
    let mut v = bootstrap::to_mrp_mem_addr(&ctx.env().mem, mr_table);
    ctx.mem_write_u32(CODE_ADDRESS, v);

    v = 1;
    ctx.write_reg(AbiReg::R0, v);
    run_code(ctx.env_mut(), CODE_ADDRESS + 8, false);

    let mr_c_function_p = unsafe { MR_C_FUNCTION_P };
    if !mr_c_function_p.is_null() {
        log!("-----> r9:@0x{:X}", unsafe {
            (*mr_c_function_p).start_of_er_rw
        });
    }
    MR_SUCCESS
}

fn bridge_mr_ext_helper(env: &mut Environment, code: u32, input: u32, input_len: u32) -> c_int {
    let mut ctx = BridgeContext::new(env);
    let mr_c_function_p = unsafe { MR_C_FUNCTION_P };
    let p = bootstrap::to_mrp_mem_addr(&ctx.env().mem, mr_c_function_p as *mut c_void);
    ctx.write_reg(AbiReg::R0, p);
    ctx.write_reg(AbiReg::R1, code);
    ctx.write_reg(AbiReg::R2, input);
    ctx.write_reg(AbiReg::R3, input_len);

    let helper_addr = unsafe { MR_EXT_HELPER_ADDR };
    run_code(ctx.env_mut(), helper_addr, false);
    ctx.arg::<u32>(0) as c_int
}

fn bridge_mr_event(env: &mut Environment, code: c_int, param0: c_int, param1: c_int) -> c_int {
    let mr_c_event = unsafe { MR_C_EVENT };
    unsafe {
        (*mr_c_event).code = code;
        (*mr_c_event).p0 = param0;
        (*mr_c_event).p1 = param1;
    }
    bridge_mr_ext_helper(
        env,
        1,
        bootstrap::to_mrp_mem_addr(&env.mem, mr_c_event as *mut c_void),
        std::mem::size_of::<Event>() as u32,
    )
}

pub fn bridge_dsm_network_cb(env: &mut Environment, callback: network::NetworkCallback) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    let mut ctx = BridgeContext::new(env);
    let r9 = ctx.read_reg(AbiReg::R9);

    let mr_c_function_p = unsafe { MR_C_FUNCTION_P };
    if !mr_c_function_p.is_null() {
        ctx.write_reg(AbiReg::R9, unsafe { (*mr_c_function_p).start_of_er_rw });
    }
    ctx.write_reg(AbiReg::R0, callback.result as u32);
    ctx.write_reg(AbiReg::R1, callback.user_data);
    run_code(ctx.env_mut(), callback.addr, false);

    ctx.write_reg(AbiReg::R9, r9);
    ctx.arg::<u32>(0) as c_int
}

pub fn bridge_drain_network_callbacks(env: &mut Environment) {
    for callback in network::drain_callbacks() {
        let _ = bridge_dsm_network_cb(env, callback);
    }
}

pub fn bridge_dsm_mr_start_dsm(
    env: &mut Environment,
    filename: *mut c_char,
    ext: *mut c_char,
    entry: *mut c_char,
) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();

    let start_param = unsafe { MR_START_DSM_PARAM };
    unsafe {
        (*start_param).filename = compat::copy_str_to_mrp_in(&mut env.mem, filename);
        (*start_param).ext = compat::copy_str_to_mrp_in(&mut env.mem, ext);
        (*start_param).entry = if entry.is_null() {
            0
        } else {
            compat::copy_str_to_mrp_in(&mut env.mem, entry)
        };
    }

    let input = bootstrap::to_mrp_mem_addr(&env.mem, start_param as *mut c_void) as c_int;
    let ret = bridge_mr_event(env, MR_START_DSM, input, 0);

    let (filename_addr, ext_addr, entry_addr) = unsafe {
        (
            (*start_param).filename,
            (*start_param).ext,
            (*start_param).entry,
        )
    };
    let filename_ptr = bootstrap::get_mrp_mem_ptr(&mut env.mem, filename_addr);
    compat::free_ext_in(&mut env.mem, filename_ptr);
    unsafe {
        (*start_param).filename = 0;
    }
    let ext_ptr = bootstrap::get_mrp_mem_ptr(&mut env.mem, ext_addr);
    compat::free_ext_in(&mut env.mem, ext_ptr);
    unsafe {
        (*start_param).ext = 0;
    }

    if !entry.is_null() {
        let entry_ptr = bootstrap::get_mrp_mem_ptr(&mut env.mem, entry_addr);
        compat::free_ext_in(&mut env.mem, entry_ptr);
        unsafe {
            (*start_param).entry = 0;
        }
    }
    ret
}

pub fn bridge_dsm_mr_pause_app(env: &mut Environment) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    bridge_mr_event(env, MR_PAUSEAPP, 0, 0)
}

pub fn bridge_dsm_mr_resume_app(env: &mut Environment) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    bridge_mr_event(env, MR_RESUMEAPP, 0, 0)
}

pub fn bridge_dsm_mr_timer(env: &mut Environment) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    bridge_mr_event(env, MR_TIMER, 0, 0)
}

pub fn bridge_dsm_mr_event(env: &mut Environment, code: c_int, p0: c_int, p1: c_int) -> c_int {
    let _guard = BRIDGE_LOCK.lock().unwrap();
    let dsm_event = unsafe { DSM_EVENT };
    unsafe {
        (*dsm_event).code = code;
        (*dsm_event).p0 = p0;
        (*dsm_event).p1 = p1;
    }
    bridge_mr_event(
        env,
        MR_EVENT,
        bootstrap::to_mrp_mem_addr(&env.mem, dsm_event as *mut c_void) as c_int,
        0,
    )
}

pub fn bridge_dsm_init(env: &mut Environment) -> c_int {
    let dsm_require_funcs = unsafe { DSM_REQUIRE_FUNCS };
    let ret = {
        let _guard = BRIDGE_LOCK.lock().unwrap();
        bridge_mr_event(
            env,
            DSM_INIT,
            bootstrap::to_mrp_mem_addr(&env.mem, dsm_require_funcs) as c_int,
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
