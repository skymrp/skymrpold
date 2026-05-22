//! CPU emulation.
//!
//! The public CPU interface is [CpuBackend]. Individual backends can use
//! Dynarmic, Unicorn, or another engine internally.

mod dynarmic;
mod unicorn;

use crate::abi::GuestFunction;
use crate::mem::{GuestUSize, Mem};

pub use dynarmic::{CpuContext, Dynarmic};
pub use unicorn::Unicorn;

pub(super) type VAddr = u32;

/// ARM CPU register indices and CPSR flags shared by all backends.
pub struct Cpu;

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

    /// Get PC with the Thumb bit appropriately set.
    fn pc_with_thumb_bit(&self) -> GuestFunction {
        let pc = self.regs()[Cpu::PC];
        let thumb = (self.cpsr() & Cpu::CPSR_THUMB) == Cpu::CPSR_THUMB;
        GuestFunction::from_addr_and_thumb_flag(pc, thumb)
    }

    /// Set PC and the Thumb flag for executing a guest function. Note that this
    /// does not touch LR.
    fn branch(&mut self, new_pc: GuestFunction) {
        self.regs_mut()[Cpu::PC] = new_pc.addr_without_thumb_bit();
        let cpsr_without_thumb = self.cpsr() & (!Cpu::CPSR_THUMB);
        self.set_cpsr(cpsr_without_thumb | ((new_pc.is_thumb() as u32) * Cpu::CPSR_THUMB));
    }

    /// Set the PC and Thumb flag (like [Self::branch]), but also set the LR,
    /// and return the original PC and LR.
    fn branch_with_link(
        &mut self,
        new_pc: GuestFunction,
        new_lr: GuestFunction,
    ) -> (GuestFunction, GuestFunction) {
        let old_pc = self.pc_with_thumb_bit();
        let old_lr = GuestFunction::from_addr_with_thumb_bit(self.regs()[Cpu::LR]);
        self.branch(new_pc);
        self.regs_mut()[Cpu::LR] = new_lr.addr_with_thumb_bit();
        (old_pc, old_lr)
    }

    /// Dump the registers of the current CPU to the log output.
    fn dump_regs(&self) {
        echo_regs(self.regs());
    }

    fn invalidate_cache_range(&mut self, base: VAddr, size: GuestUSize);
    fn run_or_step(&mut self, mem: &mut Mem, ticks: Option<&mut u64>) -> CpuState;
}

pub fn echo_regs(regs: &[u32; 16]) {
    // Silently ignore panics so it's safe to use in contexts where we can't
    // panic.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for row in 0..4 {
            use std::fmt::Write;
            let mut line = String::new();
            for col in 0..4 {
                let reg_idx = row * 4 + col;
                match reg_idx {
                    Cpu::SP => write!(&mut line, "	 SP: "),
                    Cpu::LR => write!(&mut line, "	 LR: "),
                    Cpu::PC => write!(&mut line, "	 PC: "),
                    _ if reg_idx <= 9 => write!(&mut line, "	 R{reg_idx}: "),
                    _ => write!(&mut line, "	R{reg_idx}: "),
                }
                .unwrap();
                write!(&mut line, "{:#010x}", regs[reg_idx]).unwrap();
            }
            echo!("{}", line);
        }
    }));
}
