use crate::mem::{ConstVoidPtr, GuestUSize, Mem, MemRegion, MutPtr, MutVoidPtr, Ptr};
use crate::unicorn;
use crate::{bridge, compat, file};
use libc::{c_char, c_int, c_void};
use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr;
use std::ptr::NonNull;
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
static mut UC: *mut c_void = ptr::null_mut();

thread_local! {
    static GUEST_MEM: RefCell<Option<Mem>> = const { RefCell::new(None) };
}

fn set_guest_mem(mem: Option<Mem>) {
    GUEST_MEM.with(|guest_mem| {
        *guest_mem.borrow_mut() = mem;
    });
}

unsafe fn rebuild_guest_mem() -> Result<(), ()> {
    if MRP_MEM.is_null() {
        return Err(());
    }

    let mut regions = Vec::new();
    if !LOW_MEM.is_null() {
        regions.push(MemRegion::new_borrowed(
            0,
            NonNull::new(LOW_MEM).unwrap(),
            CODE_ADDRESS,
        ));
    }
    regions.push(MemRegion::new_borrowed(
        START_ADDRESS,
        NonNull::new(MRP_MEM).unwrap(),
        TOTAL_MEMORY,
    ));

    set_guest_mem(Some(Mem::from_regions_with_allocator_range_and_alignment(
        regions,
        MEMORY_MANAGER_ADDRESS,
        MEMORY_MANAGER_SIZE,
        8,
        8,
    )));
    Ok(())
}

pub fn with_guest_mem<R>(f: impl FnOnce(&Mem) -> R) -> R {
    GUEST_MEM.with(|guest_mem| {
        let guest_mem = guest_mem.borrow();
        let mem = guest_mem.as_ref().expect("guest memory is not initialized");
        f(mem)
    })
}

pub fn with_guest_mem_mut<R>(f: impl FnOnce(&mut Mem) -> R) -> R {
    GUEST_MEM.with(|guest_mem| {
        let mut guest_mem = guest_mem.borrow_mut();
        let mem = guest_mem.as_mut().expect("guest memory is not initialized");
        f(mem)
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

#[no_mangle]
pub extern "C" fn getMrpMemPtr(addr: u32) -> *mut c_void {
    GUEST_MEM.with(|guest_mem| {
        let mut guest_mem = guest_mem.borrow_mut();
        let Some(mem) = guest_mem.as_mut() else {
            return ptr::null_mut();
        };
        let ptr = MutPtr::<u8>::from_bits(addr);
        mem.get_bytes_fallible_mut(ptr.cast_const().cast_void(), 1)
            .map_or(ptr::null_mut(), |bytes| bytes.as_mut_ptr().cast())
    })
}

#[no_mangle]
pub extern "C" fn toMrpMemAddr(ptr: *mut c_void) -> u32 {
    if ptr.is_null() {
        return 0;
    }
    GUEST_MEM.with(|guest_mem| {
        let guest_mem = guest_mem.borrow();
        let mem = guest_mem.as_ref().expect("guest memory is not initialized");
        mem.host_ptr_to_guest_ptr(ptr.cast_const()).to_bits()
    })
}

#[no_mangle]
pub unsafe extern "C" fn free_runtime(uc: *mut c_void) -> c_int {
    set_guest_mem(None);
    if !MRP_MEM.is_null() {
        libc::free(MRP_MEM as *mut c_void);
        MRP_MEM = ptr::null_mut();
    }
    if !LOW_MEM.is_null() {
        libc::free(LOW_MEM as *mut c_void);
        LOW_MEM = ptr::null_mut();
    }
    unicorn::clear_owner(uc);
    if UC == uc {
        UC = ptr::null_mut();
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn init_runtime() -> *mut c_void {
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

    let map_result = (|| {
        MRP_MEM = libc::malloc(TOTAL_MEMORY as usize) as *mut u8;
        if MRP_MEM.is_null() {
            log!("Failed malloc mrp memory");
            return Err(());
        }

        engine
            .mem_map_ptr(
                START_ADDRESS as u64,
                TOTAL_MEMORY as u64,
                unicorn_engine::unicorn_const::Prot::ALL,
                MRP_MEM as *mut c_void,
            )
            .map_err(|err| {
                log!("Failed mem map: {err:?} ({})", unicorn::error_text(err));
            })?;

        rebuild_guest_mem().map_err(|_| {
            log!("Failed init guest memory map");
        })?;

        compat::initMemoryManager(MEMORY_MANAGER_ADDRESS, MEMORY_MANAGER_SIZE);
        Ok(())
    })();

    if map_result.is_err() {
        free_runtime(uc);
        return ptr::null_mut();
    }

    let uc = unicorn::store_owner(engine);

    let init_result = (|| {
        if bridge::bridge_init(uc) != MR_SUCCESS {
            log!("Failed bridge_init()");
            return Err(());
        }

        LOW_MEM = libc::malloc(CODE_ADDRESS as usize) as *mut u8;
        if LOW_MEM.is_null() {
            log!("Failed malloc low memory");
            return Err(());
        }
        ptr::write_bytes(LOW_MEM, 0, CODE_ADDRESS as usize);
        unicorn::mem_map_ptr(uc, 0, CODE_ADDRESS as u64, LOW_MEM as *mut c_void).map_err(
            |err| {
                log!("Failed low mem map: {err:?} ({})", unicorn::error_text(err));
            },
        )?;

        rebuild_guest_mem().map_err(|_| {
            log!("Failed update guest memory map");
        })?;

        unicorn::add_mem_invalid_hook(uc, |uc, type_, address, size, value| {
            let type_name = unicorn::mem_type_name(type_);
            log!(
                ">>> Tracing mem_invalid mem_type:{type_name} at 0x{address:X}, size:0x{size:X}, value:0x{value:X}"
            );
            compat::dumpREG(uc.get_handle().cast::<c_void>());
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
        free_runtime(uc);
        return ptr::null_mut();
    }

    uc
}

#[no_mangle]
pub extern "C" fn event(code: c_int, p1: c_int, p2: c_int) -> c_int {
    unsafe {
        if !UC.is_null() {
            return bridge::bridge_dsm_mr_event(UC, code, p1, p2);
        }
    }
    MR_FAILED
}

#[no_mangle]
pub extern "C" fn timer() -> c_int {
    unsafe {
        if !UC.is_null() {
            return bridge::bridge_dsm_mr_timer(UC);
        }
    }
    MR_FAILED
}

#[no_mangle]
pub unsafe extern "C" fn loadCode() -> c_int {
    let filename = b"mythroad/cfunction.ext\0";
    let filename = CStr::from_bytes_with_nul_unchecked(filename);
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

    let err = unicorn::mem_write(UC, CODE_ADDRESS as u64, &data);
    if let Err(err) = err {
        log!(
            "uc_mem_write code failed: {err:?} ({})",
            unicorn::error_text(err)
        );
        return MR_FAILED;
    }
    MR_SUCCESS
}

#[no_mangle]
pub extern "C" fn start_runtime() -> c_int {
    unsafe {
        UC = init_runtime();
        if UC.is_null() {
            log!("init_runtime() fail.");
            return MR_FAILED;
        }

        if loadCode() == MR_FAILED {
            log!("loadCode fail.");
            return MR_FAILED;
        }

        bridge::bridge_ext_init(UC);

        if bridge::bridge_dsm_init(UC) == MR_SUCCESS {
            log!("bridge_dsm_init success");
            compat::dumpREG(UC);

            let filename = b"dsm_gm.mrp\0";
            let ext_name = b"start.mr\0";
            let ret = bridge::bridge_dsm_mr_start_dsm(
                UC,
                filename.as_ptr() as *mut c_char,
                ext_name.as_ptr() as *mut c_char,
                ptr::null_mut(),
            );
            log!("bridge_dsm_mr_start_dsm('dsm_gm.mrp','start.mr',NULL): 0x{ret:X}");
        }

        MR_SUCCESS
    }
}
