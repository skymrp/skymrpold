use libc::c_void;
use std::collections::{HashMap, HashSet};
use std::{cell::RefCell, rc::Rc};
use unicorn_engine::RegisterARM;

use crate::{
    abi::{CallFromGuest, GuestFunction},
    mem::{Mem, MutPtr},
    unicorn,
};

pub type HostFunction = &'static dyn CallFromGuest;
pub type LinkedHostFunction = fn(*mut c_void, u32, usize);

#[derive(Clone, Copy)]
struct LegacyHostCall {
    name: &'static str,
    dispatch: LinkedHostFunction,
    user_data: usize,
}

#[derive(Clone, Copy)]
enum LinkedHostCall {
    Legacy(LegacyHostCall),
    Typed(&'static str, HostFunction),
}

#[derive(Default)]
struct SyscallState {
    linked_host_functions: Vec<LinkedHostCall>,
    installed_svc_hooks: HashSet<usize>,
}

fn encode_a32_svc(imm: u32) -> u32 {
    assert!(imm & 0xff000000 == 0);
    imm | 0xef000000
}
fn encode_a32_ret() -> u32 {
    0xe12fff1e
}
fn encode_a32_trap() -> u32 {
    0xe7ffdefe
}

fn write_return_to_host_routine(mem: &mut Mem, svc: u32) -> GuestFunction {
    let routine = [
        encode_a32_svc(svc),
        // When a return-to-host occurs, it's the host's responsibility
        // to reset the PC to somewhere else. So something has gone
        // wrong if this is executed.
        encode_a32_trap(),
    ];
    let ptr: MutPtr<u32> = mem.alloc(4 * 2).cast();
    mem.write(ptr + 0, routine[0]);
    mem.write(ptr + 1, routine[1]);
    let ptr = GuestFunction::from_addr_with_thumb_bit(ptr.to_bits());
    assert!(!ptr.is_thumb());
    ptr
}

fn write_linked_host_function_stub(uc: *mut c_void, addr: u32, svc: u32) {
    unicorn::mem_write_u32(uc, addr as u64, encode_a32_svc(svc)).unwrap();
    unicorn::mem_write_u32(uc, (addr + 4) as u64, encode_a32_ret()).unwrap();
}

fn decode_svc(uc: *mut c_void) -> Option<u32> {
    let cpsr = match unicorn::reg_read(uc, RegisterARM::CPSR) {
        Ok(cpsr) => cpsr,
        Err(err) => {
            log!(
                "[SVC] failed to read CPSR: {err:?} ({})",
                unicorn::error_text(err)
            );
            return None;
        }
    };
    let pc = match unicorn::reg_read(uc, RegisterARM::PC) {
        Ok(pc) => pc,
        Err(err) => {
            log!(
                "[SVC] failed to read PC: {err:?} ({})",
                unicorn::error_text(err)
            );
            return None;
        }
    };
    let is_thumb = (cpsr & 0x20) != 0;
    log!("[SVC] decode start pc=0x{pc:08X} cpsr=0x{cpsr:08X} thumb={is_thumb}");
    if is_thumb {
        let Some(addr) = pc.checked_sub(2) else {
            log!("[SVC] thumb PC underflow while decoding pc=0x{pc:08X}");
            return None;
        };
        let mut bytes = [0u8; 2];
        if let Err(err) = unicorn::mem_read(uc, addr as u64, &mut bytes) {
            log!(
                "[SVC] failed to read thumb instruction at 0x{addr:08X}: {err:?} ({})",
                unicorn::error_text(err)
            );
            return None;
        }
        let instruction = u16::from_le_bytes(bytes);
        if instruction & 0xff00 == 0xdf00 {
            let svc = (instruction & 0x00ff) as u32;
            log!(
                "[SVC] decoded thumb instruction=0x{instruction:04X} addr=0x{addr:08X} svc=#{svc}"
            );
            Some(svc)
        } else {
            log!("[SVC] non-SVC thumb instruction=0x{instruction:04X} addr=0x{addr:08X}");
            None
        }
    } else {
        let Some(addr) = pc.checked_sub(4) else {
            log!("[SVC] arm PC underflow while decoding pc=0x{pc:08X}");
            return None;
        };
        let instruction = match unicorn::mem_read_u32(uc, addr as u64) {
            Ok(instruction) => instruction,
            Err(err) => {
                log!(
                    "[SVC] failed to read arm instruction at 0x{addr:08X}: {err:?} ({})",
                    unicorn::error_text(err)
                );
                return None;
            }
        };
        if instruction & 0xff00_0000 == 0xef00_0000 {
            let svc = instruction & 0x00ff_ffff;
            log!("[SVC] decoded arm instruction=0x{instruction:08X} addr=0x{addr:08X} svc=#{svc}");
            Some(svc)
        } else {
            log!("[SVC] non-SVC arm instruction=0x{instruction:08X} addr=0x{addr:08X}");
            None
        }
    }
}

fn dispatch_svc(state: &Rc<RefCell<SyscallState>>, uc: *mut c_void, svc: u32) {
    let pc = unicorn::reg_read(uc, RegisterARM::PC).unwrap_or(0);
    let lr = unicorn::reg_read(uc, RegisterARM::LR).unwrap_or(0);
    let sp = unicorn::reg_read(uc, RegisterARM::SP).unwrap_or(0);
    let r0 = unicorn::reg_read(uc, RegisterARM::R0).unwrap_or(0);
    let r1 = unicorn::reg_read(uc, RegisterARM::R1).unwrap_or(0);
    let r2 = unicorn::reg_read(uc, RegisterARM::R2).unwrap_or(0);
    let r3 = unicorn::reg_read(uc, RegisterARM::R3).unwrap_or(0);
    log!(
        "[SVC] dispatch svc=#{svc} pc=0x{pc:08X} lr=0x{lr:08X} sp=0x{sp:08X} args=[0x{r0:08X}, 0x{r1:08X}, 0x{r2:08X}, 0x{r3:08X}]"
    );

    let linked = svc
        .checked_sub(Syscall::SVC_LINKED_FUNCTIONS_BASE)
        .and_then(|idx| {
            state
                .borrow()
                .linked_host_functions
                .get(idx as usize)
                .copied()
        });
    let Some(linked) = linked else {
        log!("!!! unknown SVC #{svc} !!!");
        return;
    };

    let LinkedHostCall::Legacy(linked) = linked else {
        log!("!!! SVC #{svc} is a typed host function and must be handled by Environment !!!");
        return;
    };

    log!("[SVC] -> {}", linked.name);
    (linked.dispatch)(uc, svc, linked.user_data);
    let ret = unicorn::reg_read(uc, RegisterARM::R0).unwrap_or(0);
    let pc = unicorn::reg_read(uc, RegisterARM::PC).unwrap_or(0);
    let lr = unicorn::reg_read(uc, RegisterARM::LR).unwrap_or(0);
    log!(
        "[SVC] <- {} ret=0x{ret:08X} pc=0x{pc:08X} lr=0x{lr:08X}",
        linked.name
    );
}

fn hook_svc(state: &Rc<RefCell<SyscallState>>, uc: *mut c_void) {
    let Some(svc) = decode_svc(uc) else {
        log!("!!! failed to decode SVC !!!");
        return;
    };
    dispatch_svc(state, uc, svc);
}

pub struct Syscall {
    state: Rc<RefCell<SyscallState>>,
    return_to_host_routine: Option<GuestFunction>,
    thread_exit_routine: Option<GuestFunction>,
    non_lazy_host_functions: HashMap<&'static str, GuestFunction>,
}

impl Syscall {
    /// We reserve this SVC ID for the exit routine for spawned threads.
    pub const SVC_THREAD_EXIT: u32 = 0;
    /// We reserve this SVC ID for the special return-to-host routine.
    pub const SVC_RETURN_TO_HOST: u32 = 1;
    /// The range of SVC IDs `SVC_LINKED_FUNCTIONS_BASE..` is used to reference
    /// [Self::linked_host_functions] entries.
    pub const SVC_LINKED_FUNCTIONS_BASE: u32 = Self::SVC_RETURN_TO_HOST + 1;

    pub fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(SyscallState::default())),
            return_to_host_routine: None,
            thread_exit_routine: None,
            non_lazy_host_functions: HashMap::new(),
        }
    }

    pub fn initialize_process(&mut self, mem: &mut Mem) {
        self.return_to_host_routine =
            Some(write_return_to_host_routine(mem, Self::SVC_RETURN_TO_HOST));
        self.thread_exit_routine = Some(write_return_to_host_routine(mem, Self::SVC_THREAD_EXIT));
    }

    pub fn return_to_host_routine(&self) -> GuestFunction {
        self.return_to_host_routine.unwrap()
    }

    pub fn thread_exit_routine(&self) -> GuestFunction {
        self.thread_exit_routine.unwrap()
    }

    pub fn ensure_unicorn_svc_hook(&mut self, uc: *mut c_void) {
        let handle = uc as usize;
        if self.state.borrow().installed_svc_hooks.contains(&handle) {
            return;
        }

        let state = self.state.clone();
        if let Err(err) = unicorn::add_intr_hook(uc, move |engine, _intno| {
            hook_svc(&state, engine.get_handle().cast::<c_void>());
        }) {
            log!("add SVC hook err {err:?} ({})", unicorn::error_text(err));
            std::process::exit(1);
        }

        self.state.borrow_mut().installed_svc_hooks.insert(handle);
    }

    pub fn link_host_function(
        &mut self,
        uc: *mut c_void,
        stub_addr: u32,
        name: &'static str,
        dispatch: LinkedHostFunction,
        user_data: usize,
    ) -> u32 {
        let mut state = self.state.borrow_mut();
        let svc = Self::SVC_LINKED_FUNCTIONS_BASE + state.linked_host_functions.len() as u32;
        write_linked_host_function_stub(uc, stub_addr, svc);
        state
            .linked_host_functions
            .push(LinkedHostCall::Legacy(LegacyHostCall {
                name,
                dispatch,
                user_data,
            }));
        svc
    }

    pub fn link_typed_host_function(
        &mut self,
        mem: &mut Mem,
        stub_addr: u32,
        name: &'static str,
        function: HostFunction,
    ) -> u32 {
        let mut state = self.state.borrow_mut();
        let svc = Self::SVC_LINKED_FUNCTIONS_BASE + state.linked_host_functions.len() as u32;
        let ptr = MutPtr::<u32>::from_bits(stub_addr);
        mem.write(ptr + 0, encode_a32_svc(svc));
        mem.write(ptr + 1, encode_a32_ret());
        state
            .linked_host_functions
            .push(LinkedHostCall::Typed(name, function));
        svc
    }

    /// Return the host function for a linked SVC, matching touchHLE's dyld
    /// dispatch shape. Legacy entries are still handled by the Unicorn hook
    /// while the bootstrap path is being migrated.
    pub fn get_svc_handler(&mut self, svc_pc: u32, svc: u32) -> Option<HostFunction> {
        match svc {
            Self::SVC_THREAD_EXIT | Self::SVC_RETURN_TO_HOST => unreachable!(),
            Self::SVC_LINKED_FUNCTIONS_BASE.. => {
                let linked = svc
                    .checked_sub(Self::SVC_LINKED_FUNCTIONS_BASE)
                    .and_then(|idx| {
                        self.state
                            .borrow()
                            .linked_host_functions
                            .get(idx as usize)
                            .copied()
                    });
                let Some(linked) = linked else {
                    panic!("Unexpected SVC #{svc} at {svc_pc:#x}");
                };
                match linked {
                    LinkedHostCall::Typed(name, function) => {
                        log_dbg!("Call to typed host function: {}", name);
                        Some(function)
                    }
                    LinkedHostCall::Legacy(call) => {
                        log_dbg!(
                            "SVC #{} at {svc_pc:#x} is legacy host function: {}",
                            svc,
                            call.name
                        );
                        None
                    }
                }
            }
        }
    }

    pub fn create_guest_function(
        &mut self,
        mem: &mut Mem,
        name: &'static str,
        function: HostFunction,
    ) -> GuestFunction {
        let function_ptr = mem.alloc(8);
        let function_ptr: MutPtr<u32> = function_ptr.cast();
        self.link_typed_host_function(mem, function_ptr.to_bits(), name, function);
        GuestFunction::from_addr_with_thumb_bit(function_ptr.to_bits())
    }
}
