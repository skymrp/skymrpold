//! CPU emulation.
//!
//! Implemented using the C++ library dynarmic, which is a dynamic recompiler.

use crate::abi::GuestFunction;
use crate::mem::{ConstPtr, GuestUSize, Mem, MutPtr, Ptr, SafeRead, SafeWrite};

use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;
use unicorn_engine::{
    unicorn_const::{uc_error, Arch, HookType, Mode, Prot},
    RegisterARM, Unicorn,
};

// Import functions from C++
use skymrp_dynarmic_wrapper::*;

type VAddr = u32;
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

pub struct Cpu {
    dynarmic_wrapper: *mut skymrp_DynarmicWrapper,
    /// Copy of the direct memory access pointer used to check it has not
    /// changed. If this is null, direct memory access is not in use.
    direct_memory_access_ptr: *const std::ffi::c_void,
}

impl Drop for Cpu {
    fn drop(&mut self) {
        unsafe { skymrp_DynarmicWrapper_delete(self.dynarmic_wrapper) }
    }
}

/// Why CPU execution ended.
#[derive(Debug)]
pub enum CpuState {
    /// Execution halted due to using up all remaining ticks (normal execution)
    /// or after the single instruction was executed (step execution).
    Normal,
    /// SVC instruction encountered.
    Svc(u32),
    /// An error was encountered.
    Error(CpuError),
}

/// A reason that can cause CPU execution to be interrupted.
#[derive(Debug, Clone, PartialEq)]
pub enum CpuError {
    /// Memory error during execution (probably a null page access).
    MemoryError,
    /// Undefined instruction (perhaps from a GDB software breakpoint).
    UndefinedInstruction,
    /// Breakpoint (`bkpt` instruction).
    Breakpoint,
}

pub trait CpuBackend {
    fn regs(&self) -> &[u32; 16];
    fn regs_mut(&mut self) -> &mut [u32; 16];
    fn cpsr(&self) -> u32;
    fn set_cpsr(&mut self, cpsr: u32);
    fn branch(&mut self, new_pc: GuestFunction);
    fn branch_with_link(
        &mut self,
        new_pc: GuestFunction,
        new_lr: GuestFunction,
    ) -> (GuestFunction, GuestFunction);
    fn invalidate_cache_range(&mut self, base: VAddr, size: GuestUSize);
    fn run_or_step(&mut self, mem: &mut Mem, ticks: Option<&mut u64>) -> CpuState;
}

impl Cpu {
    /// The register number of the stack pointer.
    pub const SP: usize = 13;
    /// The register number of the link register.
    #[allow(unused)]
    pub const LR: usize = 14;
    /// The register number of the program counter.
    pub const PC: usize = 15;

    /// When this bit is set in CPSR, the CPU is in Thumb mode.
    pub const CPSR_THUMB: u32 = 0x00000020;

    /// When this bit is set in CPSR, the CPU is in user mode.
    pub const CPSR_USER_MODE: u32 = 0x00000010;

    /// Construct a new CPU instance. If a mutable reference to a [Mem] instance
    /// is provided, direct memory access is enabled, and the CPU instance
    /// becomes bound to that [Mem] instance (subsequent calls must use the same
    /// one).
    pub fn new(direct_memory_access: Option<&mut Mem>) -> Cpu {
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
        Cpu {
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

    /// Dump the registers of the current cpu to the log output.
    /// Silently ignores panics.
    pub fn dump_regs(&self) {
        let regs = self.regs();
        Self::echo_regs(regs);
    }

    pub fn echo_regs(regs: &[u32; 16]) {
        // Silently ignore panics so it's safe to use in contexts where we
        // can't panic.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for row in 0..4 {
                use std::fmt::Write;
                let mut line = String::new();
                for col in 0..4 {
                    let reg_idx = row * 4 + col;
                    match reg_idx {
                        Self::SP => write!(&mut line, "\t SP: "),
                        Self::LR => write!(&mut line, "\t LR: "),
                        Self::PC => write!(&mut line, "\t PC: "),
                        _ if reg_idx <= 9 => write!(&mut line, "\t R{reg_idx}: "),
                        _ => write!(&mut line, "\tR{reg_idx}: "),
                    }
                    .unwrap();
                    write!(&mut line, "{:#010x}", regs[reg_idx]).unwrap();
                }
                echo!("{}", line);
            }
        }));
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

    /// Get PC with the Thumb bit appropriately set.
    pub fn pc_with_thumb_bit(&self) -> GuestFunction {
        let pc = self.regs()[Self::PC];
        let thumb = (self.cpsr() & Self::CPSR_THUMB) == Self::CPSR_THUMB;
        GuestFunction::from_addr_and_thumb_flag(pc, thumb)
    }

    /// Set PC and the Thumb flag for executing a guest function. Note that this
    /// does not touch LR.
    pub fn branch(&mut self, new_pc: GuestFunction) {
        self.regs_mut()[Self::PC] = new_pc.addr_without_thumb_bit();
        let cpsr_without_thumb = self.cpsr() & (!Self::CPSR_THUMB);
        self.set_cpsr(cpsr_without_thumb | ((new_pc.is_thumb() as u32) * Self::CPSR_THUMB))
    }

    /// Set the PC and Thumb flag (like [Self::branch]), but also set the LR,
    /// and return the original PC and LR.
    pub fn branch_with_link(
        &mut self,
        new_pc: GuestFunction,
        new_lr: GuestFunction,
    ) -> (GuestFunction, GuestFunction) {
        let old_pc = self.pc_with_thumb_bit();
        let old_lr = GuestFunction::from_addr_with_thumb_bit(self.regs()[Self::LR]);
        self.branch(new_pc);
        self.regs_mut()[Self::LR] = new_lr.addr_with_thumb_bit();
        (old_pc, old_lr)
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

impl CpuBackend for Cpu {
    fn regs(&self) -> &[u32; 16] {
        Cpu::regs(self)
    }

    fn regs_mut(&mut self) -> &mut [u32; 16] {
        Cpu::regs_mut(self)
    }

    fn cpsr(&self) -> u32 {
        Cpu::cpsr(self)
    }

    fn set_cpsr(&mut self, cpsr: u32) {
        Cpu::set_cpsr(self, cpsr);
    }

    fn branch(&mut self, new_pc: GuestFunction) {
        Cpu::branch(self, new_pc);
    }

    fn branch_with_link(
        &mut self,
        new_pc: GuestFunction,
        new_lr: GuestFunction,
    ) -> (GuestFunction, GuestFunction) {
        Cpu::branch_with_link(self, new_pc, new_lr)
    }

    fn invalidate_cache_range(&mut self, base: VAddr, size: GuestUSize) {
        Cpu::invalidate_cache_range(self, base, size);
    }

    fn run_or_step(&mut self, mem: &mut Mem, ticks: Option<&mut u64>) -> CpuState {
        Cpu::run_or_step(self, mem, ticks)
    }
}

type Uc = Unicorn<'static, ()>;

#[derive(Clone, Copy)]
struct MappedMemory {
    base: u32,
    len: GuestUSize,
    ptr: *const c_void,
}

pub struct UnicornCpu {
    uc: Uc,
    regs: [u32; 16],
    cpsr: u32,
    pending_state: Rc<Cell<Option<CpuState>>>,
    mapped_memory: Vec<MappedMemory>,
}

impl UnicornCpu {
    pub fn new(direct_memory_access: Option<&mut Mem>) -> UnicornCpu {
        let mut uc = Unicorn::new(Arch::ARM, Mode::ARM).expect("failed to create Unicorn ARM CPU");
        let pending_state = Rc::new(Cell::new(None));

        install_exception_hooks(&mut uc, pending_state.clone())
            .expect("failed to install Unicorn CPU hooks");

        let mut cpu = UnicornCpu {
            uc,
            regs: [0; 16],
            cpsr: Cpu::CPSR_USER_MODE,
            pending_state,
            mapped_memory: Vec::new(),
        };

        if let Some(mem) = direct_memory_access {
            cpu.ensure_memory_mapped(mem);
        }

        cpu
    }

    fn is_thumb(&self) -> bool {
        (self.cpsr & Cpu::CPSR_THUMB) != 0
    }

    fn ensure_memory_mapped(&mut self, mem: &mut Mem) {
        let regions = unsafe { mem.direct_memory_access_regions() };

        if !self.mapped_memory.is_empty() {
            assert_eq!(
                self.mapped_memory.len(),
                regions.len(),
                "guest memory region count changed"
            );
            for (mapped, (base, len, ptr)) in self.mapped_memory.iter().zip(regions) {
                assert_eq!(mapped.base, base, "guest memory base changed");
                assert_eq!(mapped.len, len, "guest memory length changed");
                assert_eq!(
                    mapped.ptr,
                    ptr.cast_const(),
                    "guest memory backing pointer changed"
                );
            }
            return;
        }

        for (base, len, ptr) in regions {
            assert!(base.is_multiple_of(0x1000));
            assert!(len.is_multiple_of(0x1000));

            unsafe {
                self.uc
                    .mem_map_ptr(base as u64, len as u64, Prot::ALL, ptr)
                    .expect("failed to map guest memory into Unicorn");
            }

            self.mapped_memory.push(MappedMemory {
                base,
                len,
                ptr: ptr.cast_const(),
            });
        }

        if mem.null_segment_size() > 0
            && self
                .mapped_memory
                .iter()
                .any(|region| region.base == 0 && region.len >= mem.null_segment_size())
        {
            self.uc
                .mem_protect(0, mem.null_segment_size() as u64, Prot::NONE)
                .expect("failed to protect null page in Unicorn");
        }
    }

    fn sync_regs_to_unicorn(&mut self) {
        for (idx, reg) in ARM_REGS.iter().enumerate() {
            self.uc
                .reg_write(*reg, self.regs[idx] as u64)
                .expect("failed to write Unicorn register");
        }
        self.uc
            .reg_write(RegisterARM::CPSR, self.cpsr as u64)
            .expect("failed to write Unicorn CPSR");
    }

    fn sync_regs_from_unicorn(&mut self) {
        for (idx, reg) in ARM_REGS.iter().enumerate() {
            self.regs[idx] = self
                .uc
                .reg_read(*reg)
                .expect("failed to read Unicorn register") as u32;
        }
        self.cpsr = self
            .uc
            .reg_read(RegisterARM::CPSR)
            .expect("failed to read Unicorn CPSR") as u32;
    }
}

impl CpuBackend for UnicornCpu {
    fn regs(&self) -> &[u32; 16] {
        &self.regs
    }

    fn regs_mut(&mut self) -> &mut [u32; 16] {
        &mut self.regs
    }

    fn cpsr(&self) -> u32 {
        self.cpsr
    }

    fn set_cpsr(&mut self, cpsr: u32) {
        self.cpsr = cpsr;
    }

    fn branch(&mut self, new_pc: GuestFunction) {
        self.regs[Cpu::PC] = new_pc.addr_without_thumb_bit();
        let cpsr_without_thumb = self.cpsr & (!Cpu::CPSR_THUMB);
        self.cpsr = cpsr_without_thumb | ((new_pc.is_thumb() as u32) * Cpu::CPSR_THUMB);
    }

    fn branch_with_link(
        &mut self,
        new_pc: GuestFunction,
        new_lr: GuestFunction,
    ) -> (GuestFunction, GuestFunction) {
        let old_pc = {
            let thumb = (self.cpsr & Cpu::CPSR_THUMB) == Cpu::CPSR_THUMB;
            GuestFunction::from_addr_and_thumb_flag(self.regs[Cpu::PC], thumb)
        };
        let old_lr = GuestFunction::from_addr_with_thumb_bit(self.regs[Cpu::LR]);
        self.branch(new_pc);
        self.regs[Cpu::LR] = new_lr.addr_with_thumb_bit();
        (old_pc, old_lr)
    }

    fn invalidate_cache_range(&mut self, base: u32, size: GuestUSize) {
        let end = base
            .checked_add(size)
            .expect("cache invalidation range overflow");
        self.uc
            .ctl_remove_cache(base as u64, end as u64)
            .expect("failed to invalidate Unicorn translation cache");
    }

    fn run_or_step(&mut self, mem: &mut Mem, ticks: Option<&mut u64>) -> CpuState {
        self.ensure_memory_mapped(mem);
        self.sync_regs_to_unicorn();
        self.pending_state.set(None);

        let count = match ticks.as_deref() {
            Some(0) => return CpuState::Normal,
            Some(remaining) => (*remaining).try_into().unwrap_or(usize::MAX),
            None => 1,
        };

        let begin = self.regs[Cpu::PC] as u64 | u64::from(self.is_thumb());
        let result = self.uc.emu_start(begin, 0, 0, count);

        self.sync_regs_from_unicorn();

        if let Some(remaining) = ticks {
            *remaining = 0;
        }

        if let Some(state) = self.pending_state.take() {
            return state;
        }

        match result {
            Ok(()) => CpuState::Normal,
            Err(uc_error::READ_UNMAPPED)
            | Err(uc_error::WRITE_UNMAPPED)
            | Err(uc_error::FETCH_UNMAPPED)
            | Err(uc_error::READ_PROT)
            | Err(uc_error::WRITE_PROT)
            | Err(uc_error::FETCH_PROT) => CpuState::Error(CpuError::MemoryError),
            Err(uc_error::INSN_INVALID) => CpuState::Error(CpuError::UndefinedInstruction),
            Err(uc_error::EXCEPTION) => CpuState::Error(CpuError::Breakpoint),
            Err(err) => panic!("Unexpected Unicorn CPU execution result: {err:?}"),
        }
    }
}

const ARM_REGS: [RegisterARM; 16] = [
    RegisterARM::R0,
    RegisterARM::R1,
    RegisterARM::R2,
    RegisterARM::R3,
    RegisterARM::R4,
    RegisterARM::R5,
    RegisterARM::R6,
    RegisterARM::R7,
    RegisterARM::R8,
    RegisterARM::R9,
    RegisterARM::R10,
    RegisterARM::R11,
    RegisterARM::R12,
    RegisterARM::SP,
    RegisterARM::LR,
    RegisterARM::PC,
];

fn install_exception_hooks(
    uc: &mut Uc,
    pending_state: Rc<Cell<Option<CpuState>>>,
) -> Result<(), uc_error> {
    let svc_state = pending_state.clone();
    uc.add_intr_hook(move |uc, _intno| {
        let state = decode_svc(uc)
            .map(CpuState::Svc)
            .unwrap_or(CpuState::Error(CpuError::Breakpoint));
        svc_state.set(Some(state));
        uc.emu_stop()
            .expect("failed to stop Unicorn after interrupt");
    })?;

    let mem_state = pending_state.clone();
    uc.add_mem_hook(
        HookType::MEM_INVALID,
        1,
        0,
        move |_uc, _ty, _addr, _size, _value| {
            mem_state.set(Some(CpuState::Error(CpuError::MemoryError)));
            false
        },
    )?;

    let invalid_state = pending_state;
    uc.add_insn_invalid_hook(move |uc| {
        invalid_state.set(Some(CpuState::Error(CpuError::UndefinedInstruction)));
        uc.emu_stop()
            .expect("failed to stop Unicorn after invalid instruction");
        false
    })?;

    Ok(())
}

fn decode_svc(uc: &mut Unicorn<'_, ()>) -> Option<u32> {
    let cpsr = uc.reg_read(RegisterARM::CPSR).ok()? as u32;
    let pc = uc.reg_read(RegisterARM::PC).ok()?;

    if (cpsr & Cpu::CPSR_THUMB) != 0 {
        let svc_addr = pc.checked_sub(2)?;
        let mut bytes = [0u8; 2];
        uc.mem_read(svc_addr, &mut bytes).ok()?;
        let instruction = u16::from_le_bytes(bytes);
        if instruction & 0xff00 == 0xdf00 {
            Some((instruction & 0x00ff) as u32)
        } else {
            None
        }
    } else {
        let svc_addr = pc.checked_sub(4)?;
        let mut bytes = [0u8; 4];
        uc.mem_read(svc_addr, &mut bytes).ok()?;
        let instruction = u32::from_le_bytes(bytes);
        if instruction & 0xff00_0000 == 0xef00_0000 {
            Some(instruction & 0x00ff_ffff)
        } else {
            None
        }
    }
}
