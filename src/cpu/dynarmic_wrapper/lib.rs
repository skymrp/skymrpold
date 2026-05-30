//! This is separated out into its own package so that we can avoid rebuilding
//! dynarmic more often than necessary, and to improve build-time parallelism.

/// Opaque type from C
#[allow(non_camel_case_types)]
pub type skymrp_DynarmicWrapper = std::ffi::c_void;
/// Opaque type from Rust (this is the `Mem` type from the main crate, but
/// `c_void` is used here to avoid depending on it directly)
#[allow(non_camel_case_types)]
pub type skymrp_Mem = std::ffi::c_void;

#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub struct skymrp_DynarmicContext {
    pub regs: [u32; 16],
    pub extregs: [u32; 64],
    pub cpsr: u32,
    pub fpscr: u32,
}

impl Default for skymrp_DynarmicContext {
    fn default() -> Self {
        Self {
            regs: [0; 16],
            extregs: [0; 64],
            cpsr: 0,
            fpscr: 0,
        }
    }
}

impl skymrp_DynarmicContext {
    pub fn new() -> Self {
        Self::default()
    }
}
type VAddr = u32;

// Import functions from lib.cpp, see build.rs. Note that lib.cpp depends on
// some functions being exported from Rust, but those are in the main crate.
extern "C" {
    pub fn skymrp_DynarmicWrapper_new(
        dynamic_memory_access_ptr: *mut std::ffi::c_void,
        null_page_count: usize,
    ) -> *mut skymrp_DynarmicWrapper;
    pub fn skymrp_DynarmicWrapper_delete(cpu: *mut skymrp_DynarmicWrapper);
    pub fn skymrp_DynarmicWrapper_regs_const(cpu: *const skymrp_DynarmicWrapper) -> *const u32;
    pub fn skymrp_DynarmicWrapper_regs_mut(cpu: *mut skymrp_DynarmicWrapper) -> *mut u32;
    pub fn skymrp_DynarmicWrapper_cpsr(cpu: *const skymrp_DynarmicWrapper) -> u32;
    pub fn skymrp_DynarmicWrapper_set_cpsr(cpu: *mut skymrp_DynarmicWrapper, cpsr: u32);
    pub fn skymrp_DynarmicWrapper_swap_context(
        cpu: *mut skymrp_DynarmicWrapper,
        context: *mut skymrp_DynarmicContext,
    );
    pub fn skymrp_DynarmicWrapper_invalidate_cache_range(
        cpu: *mut skymrp_DynarmicWrapper,
        start: VAddr,
        size: u32,
    );
    pub fn skymrp_DynarmicWrapper_run_or_step(
        cpu: *mut skymrp_DynarmicWrapper,
        mem: *mut skymrp_Mem,
        ticks: Option<&mut u64>,
    ) -> i32;

}
