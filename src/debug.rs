use crate::{bootstrap, compat, file, unicorn};
use libc::{c_char, c_void};
use std::ffi::{CStr, CString};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use unicorn_engine::RegisterARM;

static BRK_ADDRESS: AtomicU32 = AtomicU32::new(0);
static RUN: AtomicBool = AtomicBool::new(false);

fn to_u32(s: &str) -> u32 {
    let mut value = 0u32;
    let mut shift = 0;
    for ch in s.chars().rev() {
        let digit = match ch {
            '0'..='9' => ch as u32 - '0' as u32,
            'a'..='f' => ch as u32 - 'a' as u32 + 10,
            'A'..='F' => ch as u32 - 'A' as u32 + 10,
            _ => break,
        };
        if shift >= 32 {
            break;
        }
        value |= digit << shift;
        shift += 4;
    }
    value
}

fn read_reg(uc: *mut c_void, reg: RegisterARM) -> u32 {
    unicorn::reg_read(uc, reg).unwrap_or(0)
}

fn register_from_name(name: &str) -> Option<RegisterARM> {
    match name {
        "r0" => Some(RegisterARM::R0),
        "r1" => Some(RegisterARM::R1),
        "r2" => Some(RegisterARM::R2),
        "r3" => Some(RegisterARM::R3),
        "r4" => Some(RegisterARM::R4),
        "r5" => Some(RegisterARM::R5),
        "r6" => Some(RegisterARM::R6),
        "r7" => Some(RegisterARM::R7),
        "r8" => Some(RegisterARM::R8),
        "r9" => Some(RegisterARM::R9),
        "r10" => Some(RegisterARM::R10),
        "r11" => Some(RegisterARM::R11),
        "r12" => Some(RegisterARM::R12),
        "sp" => Some(RegisterARM::SP),
        "lr" => Some(RegisterARM::LR),
        "pc" => Some(RegisterARM::PC),
        _ => None,
    }
}

fn dump_file(command: &str) {
    let mut parts = command.split(',');
    let _ = parts.next();
    let Some(filename) = parts.next() else {
        return;
    };
    let Some(addr) = parts.next() else {
        return;
    };
    let Some(len) = parts.next() else {
        return;
    };

    let Ok(filename) = CString::new(filename) else {
        return;
    };
    let addr = to_u32(addr);
    let len = to_u32(len);
    unsafe {
        file::writeFile(filename.as_ptr(), bootstrap::get_mrp_mem_ptr(addr), len);
    }
}

fn print_memory_string(uc: *mut c_void, mut addr: u32) {
    print!("==> print 0x{addr:x} memory string: ");
    loop {
        let mut value = 0u8;
        if let Err(err) = unicorn::mem_read(uc, addr as u64, std::slice::from_mut(&mut value)) {
            print!(" <read error {}>", unicorn::error_text(err));
            break;
        }
        if value == 0 {
            break;
        }
        print!("{}", value as char);
        addr = addr.wrapping_add(1);
    }
    echo!();
}

fn print_prompt(uc: *mut c_void, address: u64, size: u32) {
    let pc = read_reg(uc, RegisterARM::PC);
    let cpsr = read_reg(uc, RegisterARM::CPSR);
    let mut cpsr_str = [0 as c_char; 5];
    unsafe {
        compat::cpsr_to_str(cpsr, cpsr_str.as_mut_ptr());
    }
    let cpsr_str = unsafe { CStr::from_ptr(cpsr_str.as_ptr()) }.to_string_lossy();
    let mode = if cpsr & (1 << 5) != 0 { "THUMB" } else { "ARM" };
    print!("[PC:0x{pc:X}  {cpsr_str}   {mode}  mem:0x{address:X}, size:{size}]> ");
    let _ = io::stdout().flush();
}

fn print_help() {
    print!(
        "    reg                       - print all regs\n\
         run                       - run\n\
         brk 0x80030               - run code to 0x80030\n\
         brklr                     - run code to lr\n\
         SP=0x0027FFF0             - set SP register to 0x0027FFF0\n\
         0x00080008                - print 0x00080008 memory content\n\
         =0x80E34                  - print 0x80E34 address string content\n\
         0x00080008=0xFFFFFFFF     - set 0x00080008 memory content to 0xFFFFFFFF\n\
         dump,a.bin,0x2b3e16,0xff  - dump memory 0x2b3e16 to a.bin length is 0xff\n"
    );
}

fn execute_command(uc: *mut c_void, command: &str, pc: u32, size: u32) -> bool {
    let command = command.trim().to_ascii_lowercase();
    if command.is_empty() {
        return true;
    }

    if command == "reg" {
        unsafe {
            compat::dump_reg(uc);
        }
    } else if command.starts_with("run") {
        RUN.store(true, Ordering::Relaxed);
        return true;
    } else if command.starts_with("dump") {
        dump_file(&command);
        return true;
    } else if command.starts_with("brkn") {
        let addr = pc.wrapping_add(size);
        BRK_ADDRESS.store(addr, Ordering::Relaxed);
        log!("-------------> brkn 0x{addr:X}");
    } else if command.starts_with("brklr") {
        let addr = read_reg(uc, RegisterARM::LR);
        BRK_ADDRESS.store(addr, Ordering::Relaxed);
        log!("-------------> brklr 0x{addr:X}");
    } else if command.starts_with("brk") {
        let addr = to_u32(&command);
        BRK_ADDRESS.store(addr, Ordering::Relaxed);
        log!("-------------> brk 0x{addr:X}");
    } else if command.starts_with("=0x") {
        print_memory_string(uc, to_u32(&command));
    } else if command.starts_with("0x") {
        if let Some((left, right)) = command.split_once('=') {
            let addr = to_u32(left);
            let value = to_u32(right);
            let err = unicorn::mem_write_u32(uc, addr as u64, value);
            if let Err(err) = err {
                log!(
                    "==> Failed set memory addr: 0x{addr:x}=0x{value:x} err:{err:?} ({})",
                    unicorn::error_text(err)
                );
            } else {
                log!("==> set memory addr: 0x{addr:x}=0x{value:x}");
            }
        } else {
            let addr = to_u32(&command);
            let value = unicorn::mem_read_u32(uc, addr as u64);
            if let Err(err) = value {
                log!(
                    "==> Failed read memory addr: 0x{addr:x} err:{err:?} ({})",
                    unicorn::error_text(err)
                );
            } else {
                let mut value = value.unwrap();
                print!("==> read memory addr: 0x{addr:x}=0x{value:x}  ");
                unsafe {
                    compat::dumpMemStr(&mut value as *mut u32 as *mut c_void, 4);
                }
                echo!();
            }
        }
    } else if let Some((reg_name, value)) = command.split_once('=') {
        let reg = register_from_name(reg_name);
        let value = to_u32(value);
        if let Some(reg) = reg {
            let err = unicorn::reg_write(uc, reg, value);
            if let Err(err) = err {
                log!(
                    "==> Failed register assign {reg_name}=0x{value:x} err:{err:?} ({})",
                    unicorn::error_text(err)
                );
            } else {
                log!("==> register assign {reg_name}=0x{value:x}");
            }
        } else {
            log!("==> register '{reg_name}' invalid");
        }
    } else {
        print_help();
    }

    false
}

#[no_mangle]
pub extern "C" fn hook_code_debug(
    uc: *mut c_void,
    address: u64,
    size: u32,
    _user_data: *mut c_void,
) {
    if RUN.load(Ordering::Relaxed) {
        return;
    }

    while {
        let brk = BRK_ADDRESS.load(Ordering::Relaxed);
        brk == 0 || brk as u64 == address
    } {
        BRK_ADDRESS.store(0, Ordering::Relaxed);
        print_prompt(uc, address, size);

        let mut line = String::new();
        match io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let pc = read_reg(uc, RegisterARM::PC);
                if execute_command(uc, &line, pc, size) {
                    break;
                }
            }
        }
    }
}
