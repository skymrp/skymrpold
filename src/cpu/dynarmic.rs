use super::{CpuBackend, CpuError, CpuState, VAddr};
use crate::mem::{ConstPtr, GuestUSize, Mem, MutPtr, Ptr, SafeRead, SafeWrite};

// Import functions from C++
use skymrp_dynarmic_wrapper::*;

pub type CpuContext = skymrp_DynarmicContext;

fn skymrp_cpu_read_impl<T: SafeRead + Default>(
    mem: *mut skymrp_Mem,
    addr: VAddr,
    error: *mut bool,
) -> T {
    // If a panic occurs (probably due to a null-pointer access), we can't let
    // it keep unwinding as it will hit non-Rust stack frames (dynarmic).
    // Instead we catch the unwind and then tell the C++ code a problem occurred
    // so it can immediately halt CPU execution and then panic itself, now
    // with only Rust stack frames to worry about and with CPU state information
    // available that's useful for debugging.
    //
    // TODO: Disable this in debug mode? This relies on dynarmic's
    // check_halt_on_memory_access option which surely has a significant
    // performance impact.
    //
    // I'm not sure if this actually is unwind-safe, but considering
    // the emulator will crash anyway, maybe this is okay.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mem = unsafe { &mut *mem.cast::<Mem>() };
        let ptr: ConstPtr<T> = Ptr::from_bits(addr);
        mem.read(ptr)
    }));
    unsafe {
        error.write(res.is_err());
    }
    res.unwrap_or_default()
}

fn skymrp_cpu_write_impl<T: SafeWrite>(mem: *mut skymrp_Mem, addr: VAddr, value: T) -> bool {
    // See comments above about catch_unwind
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mem = unsafe { &mut *mem.cast::<Mem>() };
        let ptr: MutPtr<T> = Ptr::from_bits(addr);
        mem.write(ptr, value)
    }));
    res.is_err()
}

// Export functions for use by C++
#[no_mangle]
extern "C" fn skymrp_cpu_read_u8(mem: *mut skymrp_Mem, addr: VAddr, error: *mut bool) -> u8 {
    skymrp_cpu_read_impl(mem, addr, error)
}
#[no_mangle]
extern "C" fn skymrp_cpu_read_u16(mem: *mut skymrp_Mem, addr: VAddr, error: *mut bool) -> u16 {
    skymrp_cpu_read_impl(mem, addr, error)
}
#[no_mangle]
extern "C" fn skymrp_cpu_read_u32(mem: *mut skymrp_Mem, addr: VAddr, error: *mut bool) -> u32 {
    skymrp_cpu_read_impl(mem, addr, error)
}
#[no_mangle]
extern "C" fn skymrp_cpu_read_u64(mem: *mut skymrp_Mem, addr: VAddr, error: *mut bool) -> u64 {
    skymrp_cpu_read_impl(mem, addr, error)
}
#[no_mangle]
extern "C" fn skymrp_cpu_write_u8(mem: *mut skymrp_Mem, addr: VAddr, value: u8) -> bool {
    skymrp_cpu_write_impl(mem, addr, value)
}
#[no_mangle]
extern "C" fn skymrp_cpu_write_u16(mem: *mut skymrp_Mem, addr: VAddr, value: u16) -> bool {
    skymrp_cpu_write_impl(mem, addr, value)
}
#[no_mangle]
extern "C" fn skymrp_cpu_write_u32(mem: *mut skymrp_Mem, addr: VAddr, value: u32) -> bool {
    skymrp_cpu_write_impl(mem, addr, value)
}
#[no_mangle]
extern "C" fn skymrp_cpu_write_u64(mem: *mut skymrp_Mem, addr: VAddr, value: u64) -> bool {
    skymrp_cpu_write_impl(mem, addr, value)
}

pub struct Dynarmic {
    dynarmic_wrapper: *mut skymrp_DynarmicWrapper,
    /// Copy of the direct memory access pointer used to check it has not
    /// changed. If this is null, direct memory access is not in use.
    direct_memory_access_ptr: *const std::ffi::c_void,
}

impl Drop for Dynarmic {
    fn drop(&mut self) {
        unsafe { skymrp_DynarmicWrapper_delete(self.dynarmic_wrapper) }
    }
}

impl Dynarmic {
    /// Construct a new CPU instance. If a mutable reference to a [Mem] instance
    /// is provided, direct memory access is enabled, and the CPU instance
    /// becomes bound to that [Mem] instance (subsequent calls must use the same
    /// one).
    pub fn new(direct_memory_access: Option<&mut Mem>) -> Dynarmic {
        // Null page count is in pages rather than bytes. Mem ensures it is
        // page aligned.
        let null_page_count: usize = direct_memory_access
            .as_ref()
            .map_or(0, |mem| mem.null_segment_size() / 0x1000)
            .try_into()
            .unwrap();
        // Safety: the direct memory access pointer will be retained directly by
        // the dynarmic wrapper and indirectly by cached JIT code, so we must
        // ensure we only execute the CPU while holding a &mut on the Mem object
        // to which that pointer belongs.
        let direct_memory_access_ptr = direct_memory_access
            .map_or(std::ptr::null_mut(), |mem| unsafe {
                mem.direct_memory_access_ptr()
            });
        let dynarmic_wrapper =
            unsafe { skymrp_DynarmicWrapper_new(direct_memory_access_ptr, null_page_count) };
        Dynarmic {
            dynarmic_wrapper,
            direct_memory_access_ptr,
        }
    }

    pub fn regs(&self) -> &[u32; 16] {
        unsafe {
            let ptr = skymrp_DynarmicWrapper_regs_const(self.dynarmic_wrapper);
            &*(ptr as *const [u32; 16])
        }
    }

    pub fn regs_mut(&mut self) -> &mut [u32; 16] {
        unsafe {
            let ptr = skymrp_DynarmicWrapper_regs_mut(self.dynarmic_wrapper);
            &mut *(ptr as *mut [u32; 16])
        }
    }

    pub fn cpsr(&self) -> u32 {
        unsafe { skymrp_DynarmicWrapper_cpsr(self.dynarmic_wrapper) }
    }

    pub fn set_cpsr(&mut self, cpsr: u32) {
        unsafe { skymrp_DynarmicWrapper_set_cpsr(self.dynarmic_wrapper, cpsr) }
    }

    /// Swap the current state of the CPU (registers etc) with the state stored
    /// in the context object.
    pub fn swap_context(&mut self, context: &mut CpuContext) {
        unsafe { skymrp_DynarmicWrapper_swap_context(self.dynarmic_wrapper, context) }
    }

    /// Clear dynarmic's instruction cache for some range of addresses.
    /// This is useful when host-side code patches guest code.
    pub fn invalidate_cache_range(&mut self, base: VAddr, size: GuestUSize) {
        unsafe { skymrp_DynarmicWrapper_invalidate_cache_range(self.dynarmic_wrapper, base, size) }
    }

    /// Start CPU execution.
    ///
    /// If `ticks` is [Some], it is used as an abstract time limit. The value
    /// will be reduced proportionately with the amount of ticks expended.
    ///
    /// If `ticks` is [None], the CPU executes only a single instruction. This
    /// is also known as "stepping".
    ///
    /// This will return either because the CPU ran out of time, or because
    /// something else happened which requires attention from the host.
    #[must_use]
    pub fn run_or_step(&mut self, mem: &mut Mem, ticks: Option<&mut u64>) -> CpuState {
        // See ::new() for why this is done.
        if !self.direct_memory_access_ptr.is_null() {
            assert!(self.direct_memory_access_ptr == unsafe { mem.direct_memory_access_ptr() });
        }

        let res = unsafe {
            skymrp_DynarmicWrapper_run_or_step(
                self.dynarmic_wrapper,
                mem as *mut Mem as *mut skymrp_Mem,
                ticks,
            )
        };
        match res {
            -1 => CpuState::Normal,
            -2 => CpuState::Error(CpuError::MemoryError),
            -3 => CpuState::Error(CpuError::UndefinedInstruction),
            -4 => CpuState::Error(CpuError::Breakpoint),
            _ if res < -4 => panic!("Unexpected CPU execution result"),
            svc => CpuState::Svc(svc as u32),
        }
    }
}

impl CpuBackend for Dynarmic {
    fn regs(&self) -> &[u32; 16] {
        Dynarmic::regs(self)
    }

    fn regs_mut(&mut self) -> &mut [u32; 16] {
        Dynarmic::regs_mut(self)
    }

    fn cpsr(&self) -> u32 {
        Dynarmic::cpsr(self)
    }

    fn set_cpsr(&mut self, cpsr: u32) {
        Dynarmic::set_cpsr(self, cpsr);
    }

    fn invalidate_cache_range(&mut self, base: VAddr, size: GuestUSize) {
        Dynarmic::invalidate_cache_range(self, base, size);
    }

    fn run_or_step(&mut self, mem: &mut Mem, ticks: Option<&mut u64>) -> CpuState {
        Dynarmic::run_or_step(self, mem, ticks)
    }
}
