//! Direct `.mrp` runner backed by the external Mythroad MR VM bindings.
//!
//! This is the first step toward the mrpemu-style flow:
//! parse an MRP package, load a `.mr` chunk, register host functions, and run
//! the MR VM without going through `cfunction.ext`.

use std::cell::RefCell;
use std::ffi::CString;
use std::path::Path;
use std::rc::Rc;

use mythroad::{ffi, MrCallbackContext, MrState};
use skymrp_loader::{GetMrpInfoOption, MrpFile, MrpPackage};

use crate::mythroad_host;

const DEFAULT_START_FILE: &str = "start.mr";

#[derive(Default)]
struct DirectServices {
    exit_requested: bool,
}

pub fn run_mrp_file(path: &Path) -> Result<(), String> {
    log!("direct MRP runtime: loading {}", path.display());

    let package = MrpPackage::from_file(path, Some(GetMrpInfoOption { gunzip: false }))
        .map_err(|err| format!("failed to parse MRP {}: {err}", path.display()))?;
    log_package_summary(&package);

    let entry = select_entry_file(&package)
        .ok_or_else(|| format!("{} contains no .mr entry file", path.display()))?;
    let entry_name = entry.filename.clone();
    let entry_size = package
        .read_file_unzipped(&entry_name)
        .map_err(|err| format!("failed to read {entry_name} from {}: {err}", path.display()))?
        .ok_or_else(|| format!("{entry_name} disappeared from {}", path.display()))?
        .len();

    log!(
        "direct MRP runtime: selected entry {} ({} bytes)",
        entry_name,
        entry_size
    );

    mythroad_host::with_mrp_package(package, || {
        let mut vm = MrState::new().map_err(|err| format!("failed to create MR VM: {err}"))?;
        open_core_libraries(&vm);

        let services = Rc::new(RefCell::new(DirectServices::default()));
        register_host_functions(&mut vm, services.clone())?;
        set_global_string(&vm, "_mr_entry", "_dsm")?;
        set_global_string(&vm, "_mr_param", "")?;

        do_file_from_mrp(&vm, &entry_name)?;

        if services.borrow().exit_requested {
            log!("direct MRP runtime: app requested exit");
        }
        Ok(())
    })
}

fn log_package_summary(package: &MrpPackage) {
    let header = package.header();
    log!(
        "direct MRP runtime: package {} / {} files={}",
        header.internal_name,
        header.show_name,
        header.files.len()
    );
    for file in package.files().iter().take(16) {
        log!(
            "direct MRP runtime: file {} pos=0x{:X} size={}",
            file.filename,
            file.position,
            file.size
        );
    }
}

fn select_entry_file(package: &MrpPackage) -> Option<&MrpFile> {
    package.file(DEFAULT_START_FILE).or_else(|| {
        package
            .files()
            .iter()
            .find(|file| file.filename.ends_with(".mr"))
    })
}

fn open_core_libraries(vm: &MrState) {
    unsafe {
        ffi::mrp_open_base(vm.as_ptr());
        ffi::mrp_open_string(vm.as_ptr());
        ffi::mrp_open_table(vm.as_ptr());
        ffi::mrp_open_file(vm.as_ptr());
        ffi::mrp_settop(vm.as_ptr(), 0);
    }
}

fn do_file_from_mrp(vm: &MrState, name: &str) -> Result<(), String> {
    let name = CString::new(name).map_err(|err| err.to_string())?;
    let status = unsafe { ffi::mrp_dofile(vm.as_ptr(), name.as_ptr()) };
    if status == 0 {
        return Ok(());
    }

    let message = vm
        .to_string_lossy(-1)
        .unwrap_or_else(|| "unknown MR VM error".to_string());
    unsafe {
        ffi::mrp_pop(vm.as_ptr(), 1);
    }
    Err(format!(
        "failed to execute {} from MRP: {message}",
        name.to_string_lossy()
    ))
}

fn set_global_string(vm: &MrState, name: &str, value: &str) -> Result<(), String> {
    let name = CString::new(name).map_err(|err| err.to_string())?;
    vm.push_string(value).map_err(|err| err.to_string())?;
    unsafe {
        ffi::mrp_setglobal(vm.as_ptr(), name.as_ptr());
    }
    Ok(())
}

fn register_host_functions(
    vm: &mut MrState,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    register_stub(vm, "_loadPack")?;
    register_stub(vm, "_runFile")?;
    register_stub(vm, "_rand")?;
    register_stub(vm, "_mod")?;
    register_stub(vm, "_and")?;
    register_stub(vm, "_or")?;
    register_stub(vm, "_not")?;
    register_stub(vm, "_xor")?;

    register_log_call(vm, "_drawText")?;
    register_log_call(vm, "_drawTextEx")?;
    register_log_call(vm, "_drawRect")?;
    register_log_call(vm, "_drawLine")?;
    register_log_call(vm, "_drawPoint")?;
    register_log_call(vm, "_clearScr")?;
    register_log_call(vm, "_dispUpEx")?;
    register_log_call(vm, "_dispUp")?;
    register_stub(vm, "_textWidth")?;

    register_stub(vm, "_bmpLoad")?;
    register_log_call(vm, "_bmpShow")?;
    register_log_call(vm, "_bmpShowEx")?;
    register_stub(vm, "_bmpNew")?;
    register_log_call(vm, "_bmpDraw")?;
    register_stub(vm, "_bmpGetScr")?;
    register_stub(vm, "_bmpInfo")?;

    let exit_services = services.clone();
    vm.register_function("_exit", move |_| {
        exit_services.borrow_mut().exit_requested = true;
        log!("direct MRP runtime: _exit()");
        Ok(0)
    })
    .map_err(|err| err.to_string())?;

    register_stub(vm, "_effSetCon")?;
    register_stub(vm, "_com")?;
    register_stub(vm, "_strCom")?;
    register_stub(vm, "_plat")?;
    register_stub(vm, "_platEx")?;
    register_stub(vm, "_initNet")?;
    register_stub(vm, "_closeNet")?;
    register_timer_start(vm)?;
    register_stub(vm, "_timerStop")?;
    Ok(())
}

fn register_stub(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        log_direct_call(name, ctx);
        ctx.push_number(0.0);
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_log_call(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        log_direct_call(name, ctx);
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_timer_start(vm: &mut MrState) -> Result<(), String> {
    vm.register_function("_timerStart", |ctx| {
        let interval_ms = ctx.to_number(1) as u32;
        let callback = ctx.to_string_lossy(3).unwrap_or_default();
        log!("direct MRP runtime: _timerStart({interval_ms}, {callback})");
        ctx.push_number(0.0);
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn log_direct_call(name: &str, ctx: &MrCallbackContext) {
    let mut args = Vec::new();
    for idx in 1..=ctx.get_top() {
        if let Some(value) = ctx.to_string_lossy(idx) {
            args.push(format!("{idx}:\"{value}\""));
        } else {
            args.push(format!("{idx}:{}", ctx.to_number(idx)));
        }
    }
    log!("direct MRP runtime: {name}({})", args.join(", "));
}
