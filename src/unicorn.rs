use libc::c_void;
use std::cell::RefCell;
use unicorn_engine::{
    unicorn_const::{uc_error, Arch, HookType, MemType, Mode, Prot},
    RegisterARM, UcHookId, Unicorn,
};

pub type Uc = Unicorn<'static, ()>;

thread_local! {
    static OWNER: RefCell<Option<Uc>> = const { RefCell::new(None) };
}

pub fn new_arm() -> Result<Uc, uc_error> {
    Unicorn::new(Arch::ARM, Mode::ARM)
}

pub fn store_owner(uc: Uc) -> *mut c_void {
    let handle = uc.get_handle().cast::<c_void>();
    OWNER.with(|owner| {
        *owner.borrow_mut() = Some(uc);
    });
    handle
}

pub fn clear_owner(handle: *mut c_void) {
    OWNER.with(|owner| {
        let mut owner = owner.borrow_mut();
        if owner
            .as_ref()
            .is_some_and(|uc| uc.get_handle().cast::<c_void>() == handle)
        {
            *owner = None;
        }
    });
}

fn with_handle_mut<R>(
    handle: *mut c_void,
    f: impl FnOnce(&mut Uc) -> Result<R, uc_error>,
) -> Result<R, uc_error> {
    if handle.is_null() {
        return Err(uc_error::HANDLE);
    }

    OWNER.with(|owner| {
        if let Ok(mut borrowed) = owner.try_borrow_mut() {
            if let Some(uc) = borrowed.as_mut() {
                if uc.get_handle().cast::<c_void>() == handle {
                    return f(uc);
                }
            }
            drop(borrowed);
        }

        // SAFETY: callers only pass handles returned by Unicorn or by this module.
        let mut uc = unsafe { Unicorn::from_handle(handle.cast())? };
        f(&mut uc)
    })
}

pub fn error_text(error: uc_error) -> String {
    error.to_string()
}

pub fn mem_type_name(mem_type: MemType) -> &'static str {
    match mem_type {
        MemType::READ => "UC_MEM_READ",
        MemType::WRITE => "UC_MEM_WRITE",
        MemType::FETCH => "UC_MEM_FETCH",
        MemType::READ_UNMAPPED => "UC_MEM_READ_UNMAPPED",
        MemType::WRITE_UNMAPPED => "UC_MEM_WRITE_UNMAPPED",
        MemType::FETCH_UNMAPPED => "UC_MEM_FETCH_UNMAPPED",
        MemType::WRITE_PROT => "UC_MEM_WRITE_PROT",
        MemType::READ_PROT => "UC_MEM_READ_PROT",
        MemType::FETCH_PROT => "UC_MEM_FETCH_PROT",
        MemType::READ_AFTER => "UC_MEM_READ_AFTER",
    }
}

pub fn mem_map_ptr(
    handle: *mut c_void,
    address: u64,
    size: u64,
    ptr: *mut c_void,
) -> Result<(), uc_error> {
    with_handle_mut(handle, |uc| {
        // SAFETY: the caller owns the backing buffer and keeps it alive while mapped.
        unsafe { uc.mem_map_ptr(address, size, Prot::ALL, ptr) }
    })
}

pub fn mem_write(handle: *mut c_void, address: u64, bytes: &[u8]) -> Result<(), uc_error> {
    with_handle_mut(handle, |uc| uc.mem_write(address, bytes))
}

pub fn mem_read(handle: *mut c_void, address: u64, bytes: &mut [u8]) -> Result<(), uc_error> {
    with_handle_mut(handle, |uc| uc.mem_read(address, bytes))
}

pub fn mem_read_u32(handle: *mut c_void, address: u64) -> Result<u32, uc_error> {
    let mut bytes = [0u8; 4];
    mem_read(handle, address, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

pub fn mem_write_u32(handle: *mut c_void, address: u64, value: u32) -> Result<(), uc_error> {
    mem_write(handle, address, &value.to_le_bytes())
}

pub fn reg_read(handle: *mut c_void, reg: RegisterARM) -> Result<u32, uc_error> {
    with_handle_mut(handle, |uc| Ok(uc.reg_read(reg)? as u32))
}

pub fn reg_write(handle: *mut c_void, reg: RegisterARM, value: u32) -> Result<(), uc_error> {
    with_handle_mut(handle, |uc| uc.reg_write(reg, value as u64))
}

pub fn emu_start(
    handle: *mut c_void,
    begin: u64,
    until: u64,
    timeout: u64,
    count: usize,
) -> Result<(), uc_error> {
    with_handle_mut(handle, |uc| uc.emu_start(begin, until, timeout, count))
}

pub fn add_code_hook<F>(
    handle: *mut c_void,
    begin: u64,
    end: u64,
    callback: F,
) -> Result<UcHookId, uc_error>
where
    F: for<'a, 'b> FnMut(&'a mut Unicorn<'b, ()>, u64, u32) + 'static,
{
    with_handle_mut(handle, |uc| uc.add_code_hook(begin, end, callback))
}

pub fn add_mem_invalid_hook<F>(handle: *mut c_void, callback: F) -> Result<UcHookId, uc_error>
where
    F: for<'a, 'b> FnMut(&'a mut Unicorn<'b, ()>, MemType, u64, usize, i64) -> bool + 'static,
{
    with_handle_mut(handle, |uc| {
        uc.add_mem_hook(HookType::MEM_INVALID, 1, 0, callback)
    })
}
