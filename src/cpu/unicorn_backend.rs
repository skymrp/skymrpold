use super::{Cpu, CpuBackend, CpuError, CpuState};
use crate::abi::GuestFunction;
use crate::mem::{GuestUSize, Mem};
use std::cell::Cell;
use std::ffi::c_void;
use std::rc::Rc;
use unicorn_engine::{
    unicorn_const::{uc_error, Arch, HookType, Mode, Prot},
    RegisterARM, Unicorn,
};

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
    mapped_memory: Option<MappedMemory>,
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
            mapped_memory: None,
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
        let base = mem.direct_memory_access_base();
        let len = mem.direct_memory_access_len();
        let ptr = unsafe { mem.direct_memory_access_ptr() } as *const c_void;

        if let Some(mapped) = self.mapped_memory {
            assert_eq!(mapped.base, base, "guest memory base changed");
            assert_eq!(mapped.len, len, "guest memory length changed");
            assert_eq!(mapped.ptr, ptr, "guest memory backing pointer changed");
            return;
        }

        assert!(base.is_multiple_of(0x1000));
        assert!(len.is_multiple_of(0x1000));

        unsafe {
            self.uc
                .mem_map_ptr(base as u64, len as u64, Prot::ALL, ptr.cast_mut())
                .expect("failed to map guest memory into Unicorn");
        }

        if mem.null_segment_size() > 0 && base == 0 {
            self.uc
                .mem_protect(0, mem.null_segment_size() as u64, Prot::NONE)
                .expect("failed to protect null page in Unicorn");
        }

        self.mapped_memory = Some(MappedMemory { base, len, ptr });
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
        let end = base.checked_add(size).expect("cache invalidation range overflow");
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
        uc.emu_stop().expect("failed to stop Unicorn after interrupt");
    })?;

    let mem_state = pending_state.clone();
    uc.add_mem_hook(HookType::MEM_INVALID, 1, 0, move |_uc, _ty, _addr, _size, _value| {
        mem_state.set(Some(CpuState::Error(CpuError::MemoryError)));
        false
    })?;

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
