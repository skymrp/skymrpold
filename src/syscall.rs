use libc::c_void;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use unicorn_engine::RegisterARM;

use crate::{
    abi::{CallFromGuest, GuestFunction},
    mem::{Mem, MutPtr},
    unicorn,
};

pub type HostFunction = &'static dyn CallFromGuest;
pub type LinkedHostFunction = fn(*mut c_void, u32, usize);

#[derive(Clone, Copy)]
struct LinkedHostCall {
    name: &'static str,
    dispatch: LinkedHostFunction,
    user_data: usize,
}

static LINKED_HOST_FUNCTIONS: OnceLock<Mutex<HashMap<u32, LinkedHostCall>>> = OnceLock::new();
static INSTALLED_SVC_HOOKS: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();

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

fn linked_host_functions() -> &'static Mutex<HashMap<u32, LinkedHostCall>> {
    LINKED_HOST_FUNCTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn installed_svc_hooks() -> &'static Mutex<HashSet<usize>> {
    INSTALLED_SVC_HOOKS.get_or_init(|| Mutex::new(HashSet::new()))
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

fn dispatch_svc(uc: *mut c_void, svc: u32) {
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

    let linked = linked_host_functions().lock().unwrap().get(&svc).copied();
    let Some(linked) = linked else {
        log!("!!! unknown SVC #{svc} !!!");
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

fn hook_svc(uc: *mut c_void) {
    let Some(svc) = decode_svc(uc) else {
        log!("!!! failed to decode SVC !!!");
        return;
    };
    dispatch_svc(uc, svc);
}

pub fn ensure_unicorn_svc_hook(uc: *mut c_void) {
    let handle = uc as usize;
    {
        let hooks = installed_svc_hooks().lock().unwrap();
        if hooks.contains(&handle) {
            return;
        }
    }

    if let Err(err) = unicorn::add_intr_hook(uc, |engine, _intno| {
        hook_svc(engine.get_handle().cast::<c_void>());
    }) {
        log!("add SVC hook err {err:?} ({})", unicorn::error_text(err));
        std::process::exit(1);
    }

    installed_svc_hooks().lock().unwrap().insert(handle);
}

pub fn link_host_function(
    uc: *mut c_void,
    stub_addr: u32,
    name: &'static str,
    dispatch: LinkedHostFunction,
    user_data: usize,
) -> u32 {
    let mut functions = linked_host_functions().lock().unwrap();
    let svc = Syscall::SVC_LINKED_FUNCTIONS_BASE + functions.len() as u32;
    write_linked_host_function_stub(uc, stub_addr, svc);
    if functions
        .insert(
            svc,
            LinkedHostCall {
                name,
                dispatch,
                user_data,
            },
        )
        .is_some()
    {
        log!("linked host function insert failed SVC #{svc} exists.");
        std::process::exit(1);
    }
    svc
}

pub struct Syscall {
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
            return_to_host_routine: None,
            thread_exit_routine: None,
            non_lazy_host_functions: HashMap::new(),
        }
    }

    pub fn return_to_host_routine(&self) -> GuestFunction {
        self.return_to_host_routine.unwrap()
    }

    pub fn thread_exit_routine(&self) -> GuestFunction {
        self.thread_exit_routine.unwrap()
    }
}
