//! Direct `.mrp` runner backed by the external Mythroad MR VM bindings.
//!
//! This is the first step toward the mrpemu-style flow:
//! parse an MRP package, load a `.mr` chunk, register host functions, and run
//! the MR VM without going through `cfunction.ext`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use encoding_rs::GBK;
use mythroad::{MrCallbackContext, MrState};
use skymrp_loader::{GetMrpInfoOption, MrpFile, MrpPackage};

use crate::mythroad_host;
use crate::window::{self, DirectWindowEvent};

const DEFAULT_START_FILE: &str = "start.mr";
const DIRECT_M0_SLOT: u8 = b'K';
const M0_SLOT_COUNT: usize = 50;
const BM_COPY: i32 = 2;
const BM_TRANSPARENT: i32 = 6;
const MR_EXIT_EVENT: i32 = 8;

struct DirectServices {
    exit_requested: bool,
    m0_slots: Vec<Option<Vec<u8>>>,
    package: MrpPackage,
    native_dsm: DirectNativeDsmState,
    bitmaps: HashMap<i32, DirectBitmap>,
    sprite_frame_heights: HashMap<i32, i32>,
    timer_callback: Option<String>,
    bitmap_show_log_count: usize,
    mr_state: MrRunState,
    screen_width: i32,
    screen_height: i32,
    sound_on: bool,
    shake_on: bool,
    timer_run_without_pause: bool,
    timer_running: bool,
    in_host_event_pump: bool,
    last_host_event_pump: Option<Instant>,
}

#[derive(Default)]
struct DirectNativeDsmState {
    loaded: bool,
    initialized: bool,
    version_checked: bool,
    app_info: DirectAppInfo,
    event_count: u64,
    timer_count: u64,
    pause_count: u64,
    resume_count: u64,
    last_event: Option<DirectNativeEvent>,
}

#[derive(Default)]
struct DirectAppInfo {
    id: i32,
    version: i32,
    sid_ptr: i32,
}

#[derive(Copy, Clone)]
struct DirectNativeEvent {
    code: i32,
    param0: i32,
    param1: i32,
    param2: i32,
    payload_len: i32,
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum MrRunState {
    Run,
    Pause,
    Stop,
}

struct DirectBitmap {
    pixels: Vec<u16>,
    width: i32,
    height: i32,
    stride: i32,
}

pub fn run_mrp_file(path: &Path) -> Result<(), String> {
    log!("direct MRP runtime: loading {}", path.display());

    let package_bytes =
        fs::read(path).map_err(|err| format!("failed to read {}: {err}", path.display()))?;
    let file_root = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    let package = MrpPackage::parse(&package_bytes, Some(GetMrpInfoOption { gunzip: false }))
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

    let mut vm = MrState::new().map_err(|err| format!("failed to create MR VM: {err}"))?;
    open_core_libraries(&vm);

    let m0_slots = build_m0_slots(path, &file_root, package_bytes)?;
    let services_package = package.clone();
    mythroad_host::with_mrp_runtime(package, vm.as_ptr(), file_root, || {
        let services = Rc::new(RefCell::new(DirectServices {
            exit_requested: false,
            m0_slots,
            package: services_package,
            native_dsm: DirectNativeDsmState::default(),
            bitmaps: HashMap::new(),
            sprite_frame_heights: HashMap::new(),
            timer_callback: None,
            bitmap_show_log_count: 0,
            mr_state: MrRunState::Run,
            screen_width: 240,
            screen_height: 320,
            sound_on: true,
            shake_on: true,
            timer_run_without_pause: false,
            timer_running: false,
            in_host_event_pump: false,
            last_host_event_pump: None,
        }));
        register_host_functions(&mut vm, services.clone())?;
        set_global_string(&vm, "_mr_entry", "_dsm")?;
        set_global_string(&vm, "_mr_param", "")?;
        set_global_number(&vm, "SCR_W", 240.0)?;
        set_global_number(&vm, "SCR_H", 320.0)?;
        set_global_number(&vm, "SCREEN_WIDTH", 240.0)?;
        set_global_number(&vm, "SCREEN_HEIGHT", 320.0)?;
        set_global_sys_info(&vm)?;
        log_global_type(&vm, "sysinfo")?;

        window::init_direct_window()?;
        window::clear_framebuffer(window::rgb_to_rgb565(32, 64, 160));
        window::present_direct_frame()?;
        services.borrow_mut().bitmaps.insert(
            0,
            DirectBitmap {
                pixels: window::snapshot_framebuffer(),
                width: 240,
                height: 320,
                stride: 240,
            },
        );
        do_file_from_mrp(&vm, &entry_name)?;
        dispatch_event(&vm, 0, 20, 0)?;
        log!("direct MRP runtime: entering SDL window loop");
        window::run_direct(
            |event| match event {
                DirectWindowEvent::Event(code, param0, param1) => {
                    dispatch_event(&vm, code, param0, param1)
                }
                DirectWindowEvent::Timer => {
                    let callback = {
                        let mut services = services.borrow_mut();
                        if !services.timer_running {
                            None
                        } else {
                            services.timer_running = false;
                            services.timer_callback.clone()
                        }
                    };
                    if let Some(callback) = callback {
                        vm.call_global_numbers(&callback, &[])
                            .map_err(|err| err.to_string())
                    } else {
                        Ok(())
                    }
                }
                DirectWindowEvent::Paste(_) | DirectWindowEvent::Frame => Ok(()),
            },
            || services.borrow().exit_requested,
        )?;

        if services.borrow().exit_requested {
            log!("direct MRP runtime: app requested exit");
        }
        Ok(())
    })
}

fn build_m0_slots(
    path: &Path,
    file_root: &Path,
    package_bytes: Vec<u8>,
) -> Result<Vec<Option<Vec<u8>>>, String> {
    let mut slots = vec![None; M0_SLOT_COUNT];
    let current_slot = m0_slot_index(DIRECT_M0_SLOT).expect("DIRECT_M0_SLOT is valid");
    slots[current_slot] = Some(package_bytes);
    log!(
        "direct MRP runtime: registered {} as *{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<current>"),
        DIRECT_M0_SLOT as char
    );

    let mut candidates = fs::read_dir(file_root)
        .map_err(|err| format!("failed to scan {}: {err}", file_root.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|candidate| {
            candidate
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mrp"))
        })
        .filter(|candidate| !same_path(candidate, path))
        .collect::<Vec<_>>();
    candidates.sort();

    let mut slot = current_slot + 1;
    for candidate in candidates {
        if slot >= slots.len() {
            break;
        }
        match fs::read(&candidate) {
            Ok(bytes) => {
                log!(
                    "direct MRP runtime: registered {} as *{}",
                    candidate
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("<mrp>"),
                    (b'A' + slot as u8) as char
                );
                slots[slot] = Some(bytes);
                slot += 1;
            }
            Err(err) => {
                log!(
                    "direct MRP runtime: failed to register {}: {err}",
                    candidate.display()
                );
            }
        }
    }

    Ok(slots)
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
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
    vm.open_base_libs();
}

fn do_file_from_mrp(vm: &MrState, name: &str) -> Result<(), String> {
    vm.do_mrp_file(name)
        .map_err(|err| format!("failed to execute {name} from MRP: {err}"))?;
    if let Some(message) = mythroad_host::take_last_pcall_error() {
        return Err(format!("failed to execute {name} from MRP: {message}"));
    }
    Ok(())
}

fn dispatch_event(vm: &MrState, code: i32, param0: i32, param1: i32) -> Result<(), String> {
    log!("direct MRP runtime: dispatch dealevent({code}, {param0}, {param1})");
    vm.call_global_numbers("dealevent", &[code as f64, param0 as f64, param1 as f64])
        .map_err(|err| err.to_string())
}

fn pump_host_events_from_vm(ctx: &MrCallbackContext, services: &Rc<RefCell<DirectServices>>) {
    {
        let mut services_ref = services.borrow_mut();
        if services_ref.in_host_event_pump {
            return;
        }
        let now = Instant::now();
        if services_ref
            .last_host_event_pump
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(16))
        {
            return;
        }
        services_ref.in_host_event_pump = true;
        services_ref.last_host_event_pump = Some(now);
    }

    let (quit, events) = window::poll_direct_events();
    if quit {
        let should_dispatch_exit = {
            let mut services_ref = services.borrow_mut();
            if services_ref.exit_requested {
                false
            } else {
                services_ref.exit_requested = true;
                true
            }
        };
        if should_dispatch_exit {
            log!("direct MRP runtime: direct SDL quit requested during VM hostcall");
            if let Err(err) =
                ctx.call_global_numbers("dealevent", &[MR_EXIT_EVENT as f64, 0.0, 0.0])
            {
                log!("direct MRP runtime: hostcall quit dispatch failed: {err}");
            }
        }
    }

    for event in events {
        if let DirectWindowEvent::Event(code, param0, param1) = event {
            log!("direct MRP runtime: hostcall dispatch dealevent({code}, {param0}, {param1})");
            if let Err(err) =
                ctx.call_global_numbers("dealevent", &[code as f64, param0 as f64, param1 as f64])
            {
                log!("direct MRP runtime: hostcall event dispatch failed: {err}");
            }
        }
    }

    if window::take_direct_timer_event() {
        let callback = {
            let mut services_ref = services.borrow_mut();
            if !services_ref.timer_running {
                None
            } else {
                services_ref.timer_running = false;
                services_ref.timer_callback.clone()
            }
        };
        if let Some(callback) = callback {
            log!("direct MRP runtime: hostcall dispatch timer {callback}");
            if let Err(err) = ctx.call_global_numbers(&callback, &[]) {
                log!("direct MRP runtime: hostcall timer dispatch failed: {err}");
            }
        }
    }

    services.borrow_mut().in_host_event_pump = false;
}

fn set_global_string(vm: &MrState, name: &str, value: &str) -> Result<(), String> {
    vm.set_global_string(name, value)
        .map_err(|err| err.to_string())
}

fn set_global_number(vm: &MrState, name: &str, value: f64) -> Result<(), String> {
    vm.set_global_number(name, value)
        .map_err(|err| err.to_string())
}

fn set_global_sys_info(vm: &MrState) -> Result<(), String> {
    vm.set_global_sys_info("dsm_gm.mrp")
        .map_err(|err| err.to_string())
}

fn log_global_type(vm: &MrState, name: &str) -> Result<(), String> {
    let type_name = vm.global_type_name(name).map_err(|err| err.to_string())?;
    log!("direct MRP runtime: global {name} is {type_name}");
    Ok(())
}

fn register_host_functions(
    vm: &mut MrState,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    register_compatibility_functions(vm, services.clone())?;
    register_direct_dofile(vm)?;

    register_stub(vm, "_loadPack")?;
    register_stub(vm, "_runFile")?;
    register_stub(vm, "_rand")?;
    register_stub(vm, "_mod")?;
    register_stub(vm, "_and")?;
    register_stub(vm, "_or")?;
    register_stub(vm, "_not")?;
    register_stub(vm, "_xor")?;

    register_draw_text(vm, "_drawText")?;
    register_draw_text_ex(vm, "_drawTextEx")?;
    register_draw_rect(vm, "_drawRect")?;
    register_draw_line(vm, "_drawLine")?;
    register_log_call(vm, "_drawPoint")?;
    register_clear_screen(vm, "_clearScr")?;
    register_disp_up(vm, "_dispUpEx", services.clone())?;
    register_disp_up(vm, "_dispUp", services.clone())?;
    register_text_width(vm, "_textWidth")?;

    register_bitmap_load(vm, "_bmpLoad", services.clone())?;
    register_bitmap_show(vm, "_bmpShow", services.clone())?;
    register_log_call(vm, "_bmpShowEx")?;
    register_stub(vm, "_bmpNew")?;
    register_log_call(vm, "_bmpDraw")?;
    register_bm_get_scr(vm, "_bmpGetScr", services.clone())?;
    register_stub(vm, "_bmpInfo")?;

    let exit_services = services.clone();
    vm.register_function("_exit", move |_| {
        exit_services.borrow_mut().exit_requested = true;
        log!("direct MRP runtime: _exit()");
        Ok(0)
    })
    .map_err(|err| err.to_string())?;

    register_stub(vm, "_effSetCon")?;
    register_com(vm, "_com", services.clone())?;
    register_str_com(vm, "_strCom", services.clone())?;
    register_stub(vm, "_plat")?;
    register_stub(vm, "_platEx")?;
    register_stub(vm, "_initNet")?;
    register_stub(vm, "_closeNet")?;
    register_timer_start(vm, services.clone())?;
    register_timer_stop(vm, "_timerStop", services)?;
    Ok(())
}

fn register_direct_dofile(vm: &mut MrState) -> Result<(), String> {
    vm.register_function("dofile", |ctx| {
        let filename = ctx.to_string_lossy(1).unwrap_or_default();
        log!("direct MRP runtime: dofile({filename:?})");
        if filename.eq_ignore_ascii_case("tcpip.mr") {
            log!("direct MRP runtime: skipping tcpip.mr network module");
            ctx.push_number(0.0);
            return Ok(1);
        }

        let status = match ctx.do_mrp_file(filename.as_str()) {
            Ok(()) => 0.0,
            Err(err) => {
                log!("direct MRP runtime: dofile({filename:?}) failed: {err}");
                -1.0
            }
        };
        ctx.push_number(status);
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_str_com(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        log_direct_call(name, ctx);
        let code = number_arg(ctx, 1) as i32;
        match code {
            601 => {
                let filename = text_arg(ctx, 2);
                let data = {
                    let services_ref = services.borrow();
                    services_ref
                        .package
                        .read_file_unzipped(&filename)
                        .ok()
                        .flatten()
                };
                if let Some(data) = data {
                    log!(
                        "direct MRP runtime: {name}(601, {filename:?}) -> {} bytes",
                        data.len()
                    );
                    ctx.push_bytes(&data);
                } else {
                    log!("direct MRP runtime: {name}(601, {filename:?}) -> nil");
                    ctx.push_nil();
                }
                Ok(1)
            }
            602 => {
                let filename = text_arg(ctx, 2);
                let exists = {
                    let services_ref = services.borrow();
                    services_ref.package.file(&filename).is_some()
                };
                if exists {
                    ctx.push_number(0.0);
                } else {
                    ctx.push_nil();
                }
                Ok(1)
            }
            600 => {
                let source = lstring_arg(ctx, 2).unwrap_or_default();
                let offset = number_arg(ctx, 3).max(0.0) as usize;
                let len = number_arg(ctx, 4).max(0.0) as usize;
                if source.first() == Some(&b'*') {
                    let Some(slot_byte) = source.get(1).copied() else {
                        ctx.push_nil();
                        return Ok(1);
                    };
                    let Some(slot) = m0_slot_index(slot_byte) else {
                        ctx.push_nil();
                        return Ok(1);
                    };
                    let services_ref = services.borrow();
                    let Some(slot_data) =
                        services_ref.m0_slots.get(slot).and_then(Option::as_deref)
                    else {
                        log!(
                            "direct MRP runtime: {name}(600) source={} offset={} len={} -> nil",
                            bytes_preview(&source),
                            offset,
                            len
                        );
                        ctx.push_nil();
                        return Ok(1);
                    };
                    let data = read_package_slice(slot_data, offset, len);
                    log!(
                        "direct MRP runtime: {name}(600) source={} offset={} len={} -> {} bytes",
                        bytes_preview(&source),
                        offset,
                        len,
                        data.len()
                    );
                    ctx.push_bytes(&data);
                    return Ok(1);
                }

                let data = vec![0; len];
                ctx.push_bytes(&data);
                Ok(1)
            }
            800 => {
                // Pure Rust direct mode: treat cfunction.ext as replaced by this dispatcher.
                let native_code = number_arg(ctx, 3) as i32;
                services.borrow_mut().native_dsm.loaded = true;
                log!("direct MRP runtime: {name}(800) native dispatcher loaded code={native_code}");
                ctx.push_number(0.0);
                Ok(1)
            }
            801 => {
                let input = lstring_arg(ctx, 2).unwrap_or_default();
                let native_code = number_arg(ctx, 3) as i32;
                let ret =
                    dispatch_native_dsm_event(&mut services.borrow_mut(), native_code, &input);
                ctx.push_string("")?;
                ctx.push_number(ret as f64);
                Ok(2)
            }
            _ => {
                ctx.push_number(0.0);
                Ok(1)
            }
        }
    })
    .map_err(|err| err.to_string())
}

fn dispatch_native_dsm_event(services: &mut DirectServices, native_code: i32, input: &[u8]) -> i32 {
    if !services.native_dsm.loaded {
        // In legacy this means mr_c_function was never installed. Direct Rust mode owns
        // the native dispatcher, so keep running but make the mismatch visible.
        log!("direct MRP runtime: native DSM event code={native_code} before 800 load");
    }

    match native_code {
        0 => {
            services.native_dsm.initialized = true;
            services.mr_state = MrRunState::Run;
            log!("direct MRP runtime: native DSM init");
            0
        }
        1 => {
            services.native_dsm.event_count += 1;
            let event = parse_native_event(input);
            services.native_dsm.last_event = Some(event);
            log!(
                "direct MRP runtime: native DSM event code={} p0={} p1={} p2={} payload_len={}",
                event.code,
                event.param0,
                event.param1,
                event.param2,
                event.payload_len
            );
            0
        }
        2 => {
            services.native_dsm.timer_count += 1;
            services.timer_running = false;
            log!("direct MRP runtime: native DSM timer");
            0
        }
        4 => {
            services.native_dsm.pause_count += 1;
            services.mr_state = MrRunState::Pause;
            log!("direct MRP runtime: native DSM pause");
            0
        }
        5 => {
            services.native_dsm.resume_count += 1;
            services.mr_state = MrRunState::Run;
            log!("direct MRP runtime: native DSM resume");
            0
        }
        6 => {
            services.native_dsm.version_checked = true;
            log!(
                "direct MRP runtime: native DSM version handshake input={}",
                bytes_preview(input)
            );
            0
        }
        8 => {
            services.native_dsm.app_info = parse_app_info(input);
            log!(
                "direct MRP runtime: native DSM app info id={} version={} sid_ptr={}",
                services.native_dsm.app_info.id,
                services.native_dsm.app_info.version,
                services.native_dsm.app_info.sid_ptr
            );
            0
        }
        _ => {
            log!(
                "direct MRP runtime: native DSM event code={native_code} input_len={} ignored",
                input.len()
            );
            0
        }
    }
}

fn parse_native_event(input: &[u8]) -> DirectNativeEvent {
    DirectNativeEvent {
        code: read_i32_le(input, 0),
        param0: read_i32_le(input, 4),
        param1: read_i32_le(input, 8),
        param2: read_i32_le(input, 12),
        payload_len: read_i32_le(input, 16),
    }
}

fn parse_app_info(input: &[u8]) -> DirectAppInfo {
    DirectAppInfo {
        id: read_i32_le(input, 0),
        version: read_i32_le(input, 4),
        sid_ptr: read_i32_le(input, 8),
    }
}

fn read_i32_le(input: &[u8], offset: usize) -> i32 {
    let Some(bytes) = input.get(offset..offset + 4) else {
        return 0;
    };
    i32::from_le_bytes(bytes.try_into().expect("slice length checked"))
}

fn m0_slot_index(slot: u8) -> Option<usize> {
    let upper = slot.to_ascii_uppercase();
    if upper.is_ascii_uppercase() {
        Some((upper - b'A') as usize)
    } else {
        None
    }
}

fn read_package_slice(package_bytes: &[u8], offset: usize, len: usize) -> Vec<u8> {
    let mut data = vec![0; len];
    if offset >= package_bytes.len() {
        return data;
    }
    let available = package_bytes.len() - offset;
    let copy_len = len.min(available);
    data[..copy_len].copy_from_slice(&package_bytes[offset..offset + copy_len]);
    data
}

fn lstring_arg(ctx: &MrCallbackContext, idx: i32) -> Option<Vec<u8>> {
    ctx.bytes_lossy(idx)
}

fn bytes_preview(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn number_arg(ctx: &MrCallbackContext, idx: i32) -> f64 {
    if ctx.is_number(idx) {
        return ctx.to_number(idx);
    }

    if let Some(bytes) = lstring_arg(ctx, idx) {
        if let Ok(text) = std::str::from_utf8(&bytes) {
            if let Ok(value) = text.parse::<f64>() {
                return value;
            }
        }
    }

    ctx.to_number(idx)
}

fn text_arg(ctx: &MrCallbackContext, idx: i32) -> String {
    let Some(bytes) = lstring_arg(ctx, idx) else {
        return String::new();
    };
    if let Ok(text) = std::str::from_utf8(&bytes) {
        return text.to_string();
    }
    let (text, _, _) = GBK.decode(&bytes);
    text.into_owned()
}

fn register_compatibility_functions(
    vm: &mut MrState,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    register_stub(vm, "SaveTable")?;
    register_load_table(vm)?;
    register_get_sys_info(vm)?;
    register_stub(vm, "GetDatetime")?;
    register_stub(vm, "Call")?;
    register_stub(vm, "SendSms")?;
    register_stub(vm, "GetNetworkID")?;
    register_stub(vm, "ConnectWAP")?;
    register_stub(vm, "LoadPack")?;
    register_stub(vm, "RunFile")?;
    register_stub(vm, "c2u")?;
    register_stub(vm, "GetRand")?;
    register_stub(vm, "mod")?;

    register_draw_text(vm, "DrawText")?;
    register_draw_text_ex(vm, "DrawTextEx")?;
    register_draw_rect(vm, "DrawRect")?;
    register_draw_line(vm, "DrawLine")?;
    register_log_call(vm, "DrawPoint")?;
    register_stub(vm, "BgMusicSet")?;
    register_stub(vm, "BgMusicStart")?;
    register_stub(vm, "BgMusicStop")?;
    register_stub(vm, "SoundSet")?;
    register_stub(vm, "SoundPlay")?;
    register_stub(vm, "SoundStop")?;
    register_bitmap_load(vm, "BitmapLoad", services.clone())?;
    register_bitmap_show(vm, "BitmapShow", services.clone())?;
    register_stub(vm, "BitmapNew")?;
    register_log_call(vm, "BitmapDraw")?;
    register_bm_get_scr(vm, "BmGetScr", services.clone())?;
    register_stub(vm, "Exit")?;
    register_stub(vm, "EffSetCon")?;
    register_com(vm, "TestCom", services.clone())?;
    register_str_com(vm, "TestCom1", services.clone())?;
    register_disp_up(vm, "DispUpEx", services.clone())?;
    register_timer_start_alias(vm, "TimerStart", services.clone())?;
    register_timer_stop(vm, "TimerStop", services.clone())?;
    register_sprite_set(vm, "SpriteSet", services.clone())?;
    register_sprite_draw(vm, "SpriteDraw", services.clone())?;
    register_log_call(vm, "SpriteDrawEx")?;
    register_stub(vm, "SpriteCheck")?;
    register_clear_screen(vm, "ClearScreen")?;
    register_stub(vm, "TileSet")?;
    register_stub(vm, "TileSetRect")?;
    register_log_call(vm, "TileDraw")?;
    register_stub(vm, "GetTile")?;
    register_stub(vm, "SetTile")?;
    register_stub(vm, "TileShift")?;
    register_stub(vm, "TileLoad")?;
    Ok(())
}

fn register_get_sys_info(vm: &mut MrState) -> Result<(), String> {
    vm.register_function("GetSysInfo", |ctx| {
        log!("direct MRP runtime: GetSysInfo()");
        ctx.push_sys_info_table("dsm_gm.mrp")?;
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_load_table(vm: &mut MrState) -> Result<(), String> {
    vm.register_function("LoadTable", |ctx| {
        log!("direct MRP runtime: LoadTable()");
        ctx.push_load_table_result();
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_stub(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        log_direct_call(name, ctx);
        ctx.push_number(0.0);
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_com(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        log_direct_call(name, ctx);
        let code = number_arg(ctx, 1) as i32;
        let input1 = number_arg(ctx, 2) as i32;
        let value = match code {
            1 => {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default();
                now.as_secs_f64()
            }
            2 => {
                // Legacy stores a guest C callback pointer. Direct MR uses named globals instead.
                0.0
            }
            3 => {
                // Same as case 2; TimerStart("func") is the usable direct path.
                0.0
            }
            300 => {
                services.borrow_mut().sound_on = input1 != 0;
                0.0
            }
            301 => {
                services.borrow_mut().shake_on = input1 != 0;
                0.0
            }
            400 => {
                let millis = input1.max(0) as u64;
                std::thread::sleep(std::time::Duration::from_millis(millis));
                0.0
            }
            401 => {
                let mut services = services.borrow_mut();
                let old = services.screen_width;
                if input1 > 0 {
                    services.screen_width = input1;
                }
                old as f64
            }
            406 => {
                let mut services = services.borrow_mut();
                let old = services.screen_height;
                if input1 > 0 {
                    services.screen_height = input1;
                }
                old as f64
            }
            407 => {
                services.borrow_mut().timer_run_without_pause = input1 != 0;
                0.0
            }
            _ => 0.0,
        };
        ctx.push_number(value);
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

fn register_timer_start(
    vm: &mut MrState,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    register_timer_start_alias(vm, "_timerStart", services)
}

fn register_timer_start_alias(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let interval_ms = number_arg(ctx, 2).max(0.0) as u16;
        let callback = ctx.to_string_lossy(3).unwrap_or_default();
        log!("direct MRP runtime: {name}({interval_ms}, {callback})");
        let should_start = {
            let mut services = services.borrow_mut();
            let should_start = services.mr_state == MrRunState::Run
                || (services.timer_run_without_pause && services.mr_state == MrRunState::Pause);
            if should_start {
                services.timer_callback = if callback.is_empty() {
                    None
                } else {
                    Some(callback)
                };
                services.timer_running = true;
            }
            should_start
        };
        if !should_start {
            ctx.push_number(0.0);
            return Ok(1);
        }
        window::timer_start(interval_ms);
        ctx.push_number(0.0);
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_timer_stop(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |_| {
        services.borrow_mut().timer_running = false;
        window::timer_stop();
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_bitmap_load(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        log_direct_call(name, ctx);
        let id = number_arg(ctx, 1) as i32;
        let filename = ctx.to_string_lossy(2).unwrap_or_default();
        let width = number_arg(ctx, 5).max(0.0) as i32;
        let height = number_arg(ctx, 6).max(0.0) as i32;
        let stride = number_arg(ctx, 7).max(width as f64) as i32;
        let bitmap = {
            let services_ref = services.borrow();
            load_direct_bitmap(&services_ref.package, &filename, width, height, stride)
        };

        match bitmap {
            Some(bitmap) => {
                services.borrow_mut().bitmaps.insert(id, bitmap);
                ctx.push_number(0.0);
            }
            None => {
                log!("direct MRP runtime: failed to load bitmap {filename:?}");
                ctx.push_number(-1.0);
            }
        }
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_bitmap_show(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let id = number_arg(ctx, 1) as i32;
        let x = number_arg(ctx, 2) as i32;
        let y = number_arg(ctx, 3) as i32;
        let rop = number_arg(ctx, 4) as i32;
        let mut services_ref = services.borrow_mut();
        if services_ref.bitmap_show_log_count < 12 {
            drop(services_ref);
            log_direct_call(name, ctx);
            services_ref = services.borrow_mut();
            services_ref.bitmap_show_log_count += 1;
        }
        if x >= 240 || y >= 320 {
            return Ok(0);
        }
        if let Some(bitmap) = services_ref.bitmaps.get(&id) {
            let visible = x < 240 && y < 320 && x + bitmap.width > 0 && y + bitmap.height > 0;
            blit_bitmap(bitmap, x, y, bitmap.width, bitmap.height, rop);
            drop(services_ref);
            if visible {
                pump_host_events_from_vm(ctx, &services);
                if let Err(err) = window::present_direct_frame() {
                    log!("direct MRP runtime: present after {name} failed: {err}");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        } else {
            if id == 0 {
                return Ok(0);
            }
            if services_ref.bitmap_show_log_count < 12 {
                log!("direct MRP runtime: {name} missing bitmap id={id} x={x} y={y}");
                services_ref.bitmap_show_log_count += 1;
            }
        }
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_sprite_set(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        log_direct_call(name, ctx);
        let id = number_arg(ctx, 1) as i32;
        let frame_height = number_arg(ctx, 2).max(1.0) as i32;
        services
            .borrow_mut()
            .sprite_frame_heights
            .insert(id, frame_height);
        ctx.push_number(0.0);
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_sprite_draw(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let id = number_arg(ctx, 1) as i32;
        let frame = number_arg(ctx, 2).max(0.0) as i32;
        let x = number_arg(ctx, 3) as i32;
        let y = number_arg(ctx, 4) as i32;
        let rop = if ctx.get_top() >= 5 {
            number_arg(ctx, 5) as i32
        } else {
            BM_TRANSPARENT
        };
        let services_ref = services.borrow();
        let mut did_draw = false;
        if let Some(bitmap) = services_ref.bitmaps.get(&id) {
            let frame_height = services_ref
                .sprite_frame_heights
                .get(&id)
                .copied()
                .unwrap_or(bitmap.height);
            let sy = frame * frame_height;
            let start = (sy * bitmap.stride).max(0) as usize;
            let visible_height = frame_height.min(bitmap.height.saturating_sub(sy));
            if visible_height > 0 && start < bitmap.pixels.len() {
                blit_bitmap_slice(
                    &bitmap.pixels[start..],
                    bitmap.stride,
                    x,
                    y,
                    bitmap.width,
                    visible_height,
                    rop,
                    bitmap.pixels.first().copied().unwrap_or(0),
                );
                did_draw = true;
            }
        }
        drop(services_ref);
        if did_draw {
            pump_host_events_from_vm(ctx, &services);
            if let Err(err) = window::present_direct_frame() {
                log!("direct MRP runtime: present after {name} failed: {err}");
            }
        }
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_bm_get_scr(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let id = number_arg(ctx, 1).max(0.0) as i32;
        log!("direct MRP runtime: {name}({id})");
        services.borrow_mut().bitmaps.insert(
            id,
            DirectBitmap {
                pixels: window::snapshot_framebuffer(),
                width: 240,
                height: 320,
                stride: 240,
            },
        );
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_draw_rect(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let x = number_arg(ctx, 1) as i32;
        let y = number_arg(ctx, 2) as i32;
        let w = number_arg(ctx, 3) as i32;
        let h = number_arg(ctx, 4) as i32;
        let color = rgb_arg(ctx, 5);
        window::fill_rect_rgb565(x, y, w, h, color);
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_draw_line(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let x0 = number_arg(ctx, 1) as i32;
        let y0 = number_arg(ctx, 2) as i32;
        let x1 = number_arg(ctx, 3) as i32;
        let y1 = number_arg(ctx, 4) as i32;
        let color = rgb_arg(ctx, 5);
        window::draw_line_rgb565(x0, y0, x1, y1, color);
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_draw_text(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let text = lstring_arg(ctx, 1).unwrap_or_default();
        let x = number_arg(ctx, 2) as i32;
        let y = number_arg(ctx, 3) as i32;
        let color = rgb_arg(ctx, 4);
        let is_unicode = ctx.to_bool(7);
        window::draw_text_bytes_rgb565(&text, is_unicode, x, y, color);
        log_direct_call(name, ctx);
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_draw_text_ex(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let text = lstring_arg(ctx, 1).unwrap_or_default();
        let x = number_arg(ctx, 2) as i32;
        let y = number_arg(ctx, 3) as i32;
        let rect_x = number_arg(ctx, 4) as i32;
        let rect_y = number_arg(ctx, 5) as i32;
        let rect_w = number_arg(ctx, 6) as i32;
        let rect_h = number_arg(ctx, 7) as i32;
        let color = rgb_arg(ctx, 8);
        let flag = if ctx.get_top() >= 11 {
            number_arg(ctx, 11) as i32
        } else {
            1 | 2
        };
        let is_unicode = flag & 1 != 0;
        let auto_newline = flag & 2 != 0;
        let drawn = window::draw_text_ex_bytes_rgb565(
            &text,
            is_unicode,
            x,
            y,
            (rect_x, rect_y, rect_w, rect_h),
            color,
            auto_newline,
        );
        log_direct_call(name, ctx);
        ctx.push_number(drawn as f64);
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn register_clear_screen(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let color = rgb_arg(ctx, 1);
        window::clear_framebuffer(color);
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_disp_up(
    vm: &mut MrState,
    name: &'static str,
    services: Rc<RefCell<DirectServices>>,
) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        log_direct_call(name, ctx);
        if services.borrow().mr_state == MrRunState::Run {
            pump_host_events_from_vm(ctx, &services);
            if let Err(err) = window::present_direct_frame() {
                log!("direct MRP runtime: present after {name} failed: {err}");
            }
        }
        Ok(0)
    })
    .map_err(|err| err.to_string())
}

fn register_text_width(vm: &mut MrState, name: &'static str) -> Result<(), String> {
    vm.register_function(name, move |ctx| {
        let text = lstring_arg(ctx, 1).unwrap_or_default();
        let is_unicode = ctx.to_bool(2);
        let width = window::text_width_bytes(&text, is_unicode);
        ctx.push_number(width as f64);
        Ok(1)
    })
    .map_err(|err| err.to_string())
}

fn load_direct_bitmap(
    package: &MrpPackage,
    filename: &str,
    width: i32,
    height: i32,
    stride: i32,
) -> Option<DirectBitmap> {
    let data = package.read_file_unzipped(filename).ok().flatten()?;
    let pixels = data
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    let expected = (stride.max(width) * height).max(0) as usize;
    if expected == 0 || pixels.len() < expected {
        return None;
    }
    Some(DirectBitmap {
        pixels,
        width,
        height,
        stride: stride.max(width),
    })
}

fn blit_bitmap(bitmap: &DirectBitmap, x: i32, y: i32, w: i32, h: i32, rop: i32) {
    blit_bitmap_slice(
        &bitmap.pixels,
        bitmap.stride,
        x,
        y,
        w,
        h,
        rop,
        bitmap.pixels.first().copied().unwrap_or(0),
    );
}

fn blit_bitmap_slice(
    pixels: &[u16],
    stride: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    rop: i32,
    transparent: u16,
) {
    match rop {
        BM_TRANSPARENT => window::blit_rgb565_transparent(pixels, stride, x, y, w, h, transparent),
        BM_COPY => window::blit_rgb565(pixels, stride, x, y, w, h),
        _ => window::blit_rgb565(pixels, stride, x, y, w, h),
    }
}

fn rgb_arg(ctx: &MrCallbackContext, start_idx: i32) -> u16 {
    window::rgb_to_rgb565(
        number_arg(ctx, start_idx) as i32,
        number_arg(ctx, start_idx + 1) as i32,
        number_arg(ctx, start_idx + 2) as i32,
    )
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
