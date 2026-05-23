pub(crate) mod bridge;

use crate::environment::Environment;
use crate::mem::{GuestUSize, Mem, MemRegion, MutPtr, Ptr};
use crate::{compat, file};
use libc::{c_char, c_int, c_void};
use std::ffi::CStr;
use std::ptr;

const MR_SUCCESS: c_int = 0;
const MR_FAILED: c_int = -1;

const CODE_ADDRESS: u32 = 0x80000;
const RETURN_TO_HOST_ADDRESS: u32 = 0x1000;
const THREAD_EXIT_ADDRESS: u32 = RETURN_TO_HOST_ADDRESS + 8;
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

pub struct Bootstrap;

impl Bootstrap {
    pub fn start(env: &mut Environment) -> Result<Self, c_int> {
        env.mem = crate::environment::nullable_box::NullableBox::new(new_bootstrap_mem());
        env.syscall.initialize_process_at(
            &mut env.mem,
            RETURN_TO_HOST_ADDRESS,
            THREAD_EXIT_ADDRESS,
        );
        env.rebuild_cpu_for_current_memory();

        if init_bootstrap(env) == MR_FAILED {
            log!("init_bootstrap() fail.");
            return Err(MR_FAILED);
        }

        if load_code(env) == MR_FAILED {
            log!("loadCode fail.");
            free_bootstrap();
            return Err(MR_FAILED);
        }

        bridge::bridge_ext_init(env);

        if bridge::bridge_dsm_init(env) == MR_SUCCESS {
            log!("bridge_dsm_init success");

            let filename = b"dsm_gm.mrp\0";
            let ext_name = b"start.mr\0";
            let ret = bridge::bridge_dsm_mr_start_dsm(
                env,
                filename.as_ptr() as *mut c_char,
                ext_name.as_ptr() as *mut c_char,
                ptr::null_mut(),
            );
            log!("bridge_dsm_mr_start_dsm('dsm_gm.mrp','start.mr',NULL): 0x{ret:X}");
        }

        Ok(Self)
    }

    pub fn event(&mut self, env: &mut Environment, code: c_int, p1: c_int, p2: c_int) -> c_int {
        bridge::bridge_dsm_mr_event(env, code, p1, p2)
    }

    pub fn timer(&mut self, env: &mut Environment) -> c_int {
        bridge::bridge_dsm_mr_timer(env)
    }
}

impl Drop for Bootstrap {
    fn drop(&mut self) {
        free_bootstrap();
    }
}

pub fn guest_host_ptr_mut(mem: &mut Mem, addr: u32, count: GuestUSize) -> *mut c_void {
    mem.ptr_at_mut(Ptr::<u8, true>::from_bits(addr), count)
        .cast::<c_void>()
}

pub fn get_mrp_mem_ptr(mem: &mut Mem, addr: u32) -> *mut c_void {
    let ptr = MutPtr::<u8>::from_bits(addr);
    mem.get_bytes_fallible_mut(ptr.cast_const().cast_void(), 1)
        .map_or(ptr::null_mut(), |bytes| bytes.as_mut_ptr().cast())
}

pub fn to_mrp_mem_addr(mem: &Mem, ptr: *mut c_void) -> u32 {
    if ptr.is_null() {
        return 0;
    }
    mem.host_ptr_to_guest_ptr(ptr.cast_const()).to_bits()
}

pub fn free_bootstrap() -> c_int {
    unsafe {
        MRP_MEM = ptr::null_mut();
        LOW_MEM = ptr::null_mut();
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

pub fn init_bootstrap(env: &mut Environment) -> c_int {
    let map_result: Result<(), ()> = (|| {
        for (base, len, ptr) in unsafe { env.mem.direct_memory_access_regions() } {
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
        compat::init_memory_manager(&mut env.mem, MEMORY_MANAGER_ADDRESS, MEMORY_MANAGER_SIZE);
        Ok(())
    })();

    if map_result.is_err() {
        free_bootstrap();
        return MR_FAILED;
    }

    let init_result = (|| {
        if bridge::bridge_init(env) != MR_SUCCESS {
            log!("Failed bridge_init()");
            return Err(());
        }

        let sp = STACK_ADDRESS + STACK_SIZE;
        env.cpu.set_sp(sp);

        Ok(())
    })();

    if init_result.is_err() {
        free_bootstrap();
        return MR_FAILED;
    }

    MR_SUCCESS
}

pub fn load_code(env: &mut Environment) -> c_int {
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

    env.mem
        .bytes_at_mut(MutPtr::<u8>::from_bits(CODE_ADDRESS), data.len() as u32)
        .copy_from_slice(&data);
    MR_SUCCESS
}
