pub(crate) mod bridge;

use crate::environment::Environment;
use crate::mem::{ConstVoidPtr, GuestUSize, Mem, MemRegion, MutPtr, MutVoidPtr, Ptr};
use crate::unicorn;
use crate::{compat, file};
use libc::{c_char, c_int, c_void};
use std::cell::Cell;
use std::ffi::CStr;
use std::ptr;
use unicorn_engine::RegisterARM;

const MR_SUCCESS: c_int = 0;
const MR_FAILED: c_int = -1;

const CODE_ADDRESS: u32 = 0x80000;
const CODE_SIZE: u32 = 1024 * 1024;
const STACK_ADDRESS: u32 = CODE_ADDRESS + CODE_SIZE;
const STACK_SIZE: u32 = 1024 * 1024;
const MEMORY_MANAGER_ADDRESS: u32 = STACK_ADDRESS + STACK_SIZE;
const MEMORY_MANAGER_SIZE: u32 = 1024 * 1024 * 6;
const START_ADDRESS: u32 = CODE_ADDRESS;
const END_ADDRESS: u32 = MEMORY_MANAGER_ADDRESS + MEMORY_MANAGER_SIZE;
const TOTAL_MEMORY: u32 = END_ADDRESS - START_ADDRESS;

static mut MRP_MEM: *mut u8 = ptr::null_mut();
static mut LOW_MEM: *mut u8 = ptr::null_mut();
thread_local! {
    static GUEST_MEM: Cell<*mut Mem> = const { Cell::new(ptr::null_mut()) };
}

pub struct Bootstrap {
    uc: *mut c_void,
}

impl Bootstrap {
    pub fn start(env: &mut Environment) -> Result<Self, c_int> {
        env.mem = crate::environment::nullable_box::NullableBox::new(new_bootstrap_mem());
        env.syscall.initialize_process(&mut env.mem);
        env.rebuild_cpu_for_current_memory();

        let uc = init_bootstrap(env);
        if uc.is_null() {
            log!("init_bootstrap() fail.");
            return Err(MR_FAILED);
        }

        if load_code(uc) == MR_FAILED {
            log!("loadCode fail.");
            free_bootstrap(uc);
            return Err(MR_FAILED);
        }

        bridge::bridge_ext_init(uc);

        if bridge::bridge_dsm_init(uc) == MR_SUCCESS {
            log!("bridge_dsm_init success");
            compat::dump_reg(uc);

            let filename = b"dsm_gm.mrp\0";
            let ext_name = b"start.mr\0";
            let ret = bridge::bridge_dsm_mr_start_dsm(
                uc,
                filename.as_ptr() as *mut c_char,
                ext_name.as_ptr() as *mut c_char,
                ptr::null_mut(),
            );
            log!("bridge_dsm_mr_start_dsm('dsm_gm.mrp','start.mr',NULL): 0x{ret:X}");
        }

        Ok(Self { uc })
    }

    pub fn event(&mut self, code: c_int, p1: c_int, p2: c_int) -> c_int {
        bridge::bridge_dsm_mr_event(self.uc, code, p1, p2)
    }

    pub fn timer(&mut self) -> c_int {
        bridge::bridge_dsm_mr_timer(self.uc)
    }
}

impl Drop for Bootstrap {
    fn drop(&mut self) {
        free_bootstrap(self.uc);
    }
}

fn set_guest_mem(mem: *mut Mem) {
    GUEST_MEM.with(|guest_mem| {
        guest_mem.set(mem);
    });
}

pub fn with_guest_mem<R>(f: impl FnOnce(&Mem) -> R) -> R {
    GUEST_MEM.with(|guest_mem| {
        let mem = guest_mem.get();
        assert!(!mem.is_null(), "guest memory is not initialized");
        f(unsafe { &*mem })
    })
}

pub fn with_guest_mem_mut<R>(f: impl FnOnce(&mut Mem) -> R) -> R {
    GUEST_MEM.with(|guest_mem| {
        let mem = guest_mem.get();
        assert!(!mem.is_null(), "guest memory is not initialized");
        f(unsafe { &mut *mem })
    })
}

pub fn guest_alloc(size: GuestUSize) -> MutVoidPtr {
    with_guest_mem_mut(|mem| mem.alloc(size))
}

pub fn guest_calloc(size: GuestUSize) -> MutVoidPtr {
    with_guest_mem_mut(|mem| mem.calloc(size))
}

pub fn guest_realloc(ptr: MutVoidPtr, size: GuestUSize) -> MutVoidPtr {
    with_guest_mem_mut(|mem| mem.realloc(ptr, size))
}

pub fn guest_malloc_size(ptr: ConstVoidPtr) -> GuestUSize {
    with_guest_mem_mut(|mem| mem.malloc_size(ptr))
}

pub fn guest_free(ptr: MutVoidPtr) {
    with_guest_mem_mut(|mem| mem.free(ptr));
}

pub fn guest_host_ptr_mut(addr: u32, count: GuestUSize) -> *mut c_void {
    with_guest_mem_mut(|mem| {
        mem.ptr_at_mut(Ptr::<u8, true>::from_bits(addr), count)
            .cast::<c_void>()
    })
}

pub fn get_mrp_mem_ptr(addr: u32) -> *mut c_void {
    GUEST_MEM.with(|guest_mem| {
        let mem = guest_mem.get();
        if mem.is_null() {
            return ptr::null_mut();
        }
        let ptr = MutPtr::<u8>::from_bits(addr);
        unsafe { &mut *mem }
            .get_bytes_fallible_mut(ptr.cast_const().cast_void(), 1)
            .map_or(ptr::null_mut(), |bytes| bytes.as_mut_ptr().cast())
    })
}

pub fn to_mrp_mem_addr(ptr: *mut c_void) -> u32 {
    if ptr.is_null() {
        return 0;
    }
    GUEST_MEM.with(|guest_mem| {
        let mem = guest_mem.get();
        assert!(!mem.is_null(), "guest memory is not initialized");
        unsafe { &*mem }
            .host_ptr_to_guest_ptr(ptr.cast_const())
            .to_bits()
    })
}

pub fn free_bootstrap(uc: *mut c_void) -> c_int {
    unsafe {
        set_guest_mem(ptr::null_mut());
        MRP_MEM = ptr::null_mut();
        LOW_MEM = ptr::null_mut();
        unicorn::clear_owner(uc);
    }
    0
}

pub(crate) fn new_bootstrap_mem() -> Mem {
    Mem::from_regions_with_allocator_range_and_alignment(
        vec![
            MemRegion::new_owned(0, CODE_ADDRESS),
            MemRegion::new_owned(START_ADDRESS, TOTAL_MEMORY),
        ],
        MEMORY_MANAGER_ADDRESS,
        MEMORY_MANAGER_SIZE,
        8,
        8,
    )
}

pub fn init_bootstrap(env: &mut Environment) -> *mut c_void {
    let mut engine = match unicorn::new_arm() {
        Ok(engine) => engine,
        Err(err) => {
            log!(
                "Failed on uc_open() with error returned: {err:?} ({})",
                unicorn::error_text(err)
            );
            return ptr::null_mut();
        }
    };
    let uc = engine.get_handle().cast::<c_void>();

    let map_result: Result<(), ()> = (|| {
        for (base, len, ptr) in unsafe { env.mem.direct_memory_access_regions() } {
            unsafe {
                engine.mem_map_ptr(
                    base as u64,
                    len as u64,
                    unicorn_engine::unicorn_const::Prot::ALL,
                    ptr,
                )
            }
            .map_err(|err| {
                log!(
                    "Failed mem map region 0x{base:X}..0x{:X}: {err:?} ({})",
                    base + len,
                    unicorn::error_text(err)
                );
            })?;

            match base {
                0 => unsafe {
                    LOW_MEM = ptr.cast::<u8>();
                },
                START_ADDRESS => unsafe {
                    MRP_MEM = ptr.cast::<u8>();
                },
                _ => {}
            }
        }
        set_guest_mem((&mut *env.mem) as *mut Mem);

        compat::init_memory_manager(MEMORY_MANAGER_ADDRESS, MEMORY_MANAGER_SIZE);
        Ok(())
    })();

    if map_result.is_err() {
        free_bootstrap(uc);
        return ptr::null_mut();
    }

    let uc = unicorn::store_owner(engine);

    let init_result = (|| {
        if bridge::bridge_init(uc, &mut env.syscall) != MR_SUCCESS {
            log!("Failed bridge_init()");
            return Err(());
        }

        unicorn::add_mem_invalid_hook(uc, |uc, type_, address, size, value| {
            let type_name = unicorn::mem_type_name(type_);
            log!(
                ">>> Tracing mem_invalid mem_type:{type_name} at 0x{address:X}, size:0x{size:X}, value:0x{value:X}"
            );
            compat::dump_reg(uc.get_handle().cast::<c_void>());
            false
        })
        .map_err(|err| {
            log!(
                "Failed hook mem invalid: {err:?} ({})",
                unicorn::error_text(err)
            );
        })?;

        let sp = STACK_ADDRESS + STACK_SIZE;
        unicorn::reg_write(uc, RegisterARM::SP, sp).map_err(|err| {
            log!("Failed set stack: {err:?} ({})", unicorn::error_text(err));
        })?;

        Ok(())
    })();

    if init_result.is_err() {
        free_bootstrap(uc);
        return ptr::null_mut();
    }

    uc
}

pub fn load_code(uc: *mut c_void) -> c_int {
    let filename = b"mythroad/cfunction.ext\0";
    let filename = CStr::from_bytes_with_nul(filename).expect("static filename has trailing NUL");
    let data = match file::read_file_cstr(filename) {
        Ok(data) => data,
        Err(err) => {
            let cwd = std::env::current_dir()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|err| format!("<failed to read cwd: {err}>"));
            log!("loadCode failed to read cfunction.ext from cwd {cwd}: {err}");
            return MR_FAILED;
        }
    };

    let err = unicorn::mem_write(uc, CODE_ADDRESS as u64, &data);
    if let Err(err) = err {
        log!(
            "uc_mem_write code failed: {err:?} ({})",
            unicorn::error_text(err)
        );
        return MR_FAILED;
    }
    MR_SUCCESS
}
