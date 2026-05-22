use std::collections::HashMap;
use std::{cell::RefCell, rc::Rc};

use crate::{
    abi::{CallFromGuest, GuestFunction},
    mem::{Mem, MutPtr},
};

pub type HostFunction = &'static dyn CallFromGuest;

#[derive(Clone, Copy)]
enum LinkedHostCall {
    Typed(&'static str, HostFunction),
}

#[derive(Default)]
struct SyscallState {
    linked_host_functions: Vec<LinkedHostCall>,
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

fn write_return_to_host_routine_at(mem: &mut Mem, addr: u32, svc: u32) -> GuestFunction {
    let ptr = MutPtr::<u32>::from_bits(addr);
    mem.write(ptr + 0, encode_a32_svc(svc));
    mem.write(ptr + 1, encode_a32_trap());
    let ptr = GuestFunction::from_addr_with_thumb_bit(addr);
    assert!(!ptr.is_thumb());
    ptr
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

    pub fn initialize_process_at(
        &mut self,
        mem: &mut Mem,
        return_to_host_addr: u32,
        thread_exit_addr: u32,
    ) {
        self.return_to_host_routine = Some(write_return_to_host_routine_at(
            mem,
            return_to_host_addr,
            Self::SVC_RETURN_TO_HOST,
        ));
        self.thread_exit_routine = Some(write_return_to_host_routine_at(
            mem,
            thread_exit_addr,
            Self::SVC_THREAD_EXIT,
        ));
    }

    pub fn return_to_host_routine(&self) -> GuestFunction {
        self.return_to_host_routine.unwrap()
    }

    pub fn thread_exit_routine(&self) -> GuestFunction {
        self.thread_exit_routine.unwrap()
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
    /// dispatch shape.
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
                let LinkedHostCall::Typed(name, function) = linked;
                log_dbg!("Call to typed host function: {}", name);
                Some(function)
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
