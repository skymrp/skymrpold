use crate::{audio, compat, paths, Environment};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::pixels::PixelFormatEnum;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Mutex;
use std::time::Duration;

#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum DeviceFamily {
    iPhone,
    iPad,
}

impl TryFrom<&str> for DeviceFamily {
    type Error = ();

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "iphone" => Ok(DeviceFamily::iPhone),
            "ipad" => Ok(DeviceFamily::iPad),
            _ => Err(()),
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum DeviceOrientation {
    Portrait,
    LandscapeLeft,
    LandscapeRight,
}

struct EditPtr(pub *mut c_char);
unsafe impl Send for EditPtr {}
unsafe impl Sync for EditPtr {}

lazy_static::lazy_static! {
    static ref FRAME_BUFFER: Mutex<Vec<u16>> = Mutex::new(vec![0; 240 * 320]);
    static ref TIMER_TARGET: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    static ref EDIT_MODE: Mutex<bool> = Mutex::new(false);
    static ref EDIT_MAX_SIZE: Mutex<i32> = Mutex::new(0);
    static ref HOLD_EDIT_TEXT_PTR: Mutex<EditPtr> = Mutex::new(EditPtr(std::ptr::null_mut()));
}

const MR_KEY_0: c_int = 0;
const MR_KEY_1: c_int = 1;
const MR_KEY_2: c_int = 2;
const MR_KEY_3: c_int = 3;
const MR_KEY_4: c_int = 4;
const MR_KEY_5: c_int = 5;
const MR_KEY_6: c_int = 6;
const MR_KEY_7: c_int = 7;
const MR_KEY_8: c_int = 8;
const MR_KEY_9: c_int = 9;
const MR_KEY_STAR: c_int = 10;
const MR_KEY_POUND: c_int = 11;
const MR_KEY_UP: c_int = 12;
const MR_KEY_DOWN: c_int = 13;
const MR_KEY_LEFT: c_int = 14;
const MR_KEY_RIGHT: c_int = 15;
const MR_KEY_POWER: c_int = 16;
const MR_KEY_SOFTLEFT: c_int = 17;
const MR_KEY_SOFTRIGHT: c_int = 18;
const MR_KEY_SEND: c_int = 19;
const MR_KEY_SELECT: c_int = 20;

const MR_KEY_PRESS: c_int = 0;
const MR_KEY_RELEASE: c_int = 1;
const MR_MOUSE_DOWN: c_int = 2;
const MR_MOUSE_UP: c_int = 3;
const MR_DIALOG_EVENT: c_int = 6;
const MR_MOUSE_MOVE: c_int = 12;

#[no_mangle]
pub extern "C" fn guiDrawBitmap(bmp: *const u16, x: c_int, y: c_int, w: c_int, h: c_int) {
    if bmp.is_null() {
        return;
    }
    let bmp = unsafe { std::slice::from_raw_parts(bmp, (240 * 320) as usize) };
    draw_bitmap(bmp, x, y, w, h);
}

pub fn draw_bitmap(bmp: &[u16], x: c_int, y: c_int, w: c_int, h: c_int) {
    let mut fb = FRAME_BUFFER.lock().unwrap();

    for j in 0..h {
        for i in 0..w {
            let xx = x + i;
            let yy = y + j;
            if xx >= 0 && xx < 240 && yy >= 0 && yy < 320 {
                let idx = (yy * 240 + xx) as usize;
                if let Some(&pixel) = bmp.get(idx) {
                    fb[idx] = pixel;
                }
            }
        }
    }
}

pub fn timer_start(t: u16) -> c_int {
    let mut timer_target = TIMER_TARGET.lock().unwrap();
    *timer_target = Some(std::time::Instant::now() + std::time::Duration::from_millis(t as u64));
    0
}

pub fn timer_stop() -> c_int {
    let mut timer_target = TIMER_TARGET.lock().unwrap();
    *timer_target = None;
    0
}

#[no_mangle]
pub extern "C" fn play_sound(
    type_: c_int,
    data: *const c_void,
    data_len: u32,
    loop_: c_int,
) -> c_int {
    if data.is_null() && data_len != 0 {
        log!("mr_playSound failed: null data pointer with length {data_len}");
        return -1;
    }

    let data = unsafe { std::slice::from_raw_parts(data as *const u8, data_len as usize) };
    play_sound_bytes(type_, data, loop_ != 0)
}

pub fn play_sound_bytes(type_: c_int, data: &[u8], loop_: bool) -> c_int {
    audio::play_sound_from_guest(type_, data, loop_)
}

pub fn stop_sound(type_: c_int) -> c_int {
    audio::stop_sound_from_guest(type_)
}

#[no_mangle]
pub extern "C" fn editCreate(
    title: *const c_char,
    text: *const c_char,
    type_: c_int,
    max_size: c_int,
) -> c_int {
    if title.is_null() || text.is_null() {
        return -1;
    }
    edit_create_cstr(
        unsafe { CStr::from_ptr(title) },
        unsafe { CStr::from_ptr(text) },
        type_,
        max_size,
    )
}

pub fn edit_create_cstr(_title: &CStr, _text: &CStr, _type_: c_int, max_size: c_int) -> c_int {
    *EDIT_MODE.lock().unwrap() = true;
    *EDIT_MAX_SIZE.lock().unwrap() = max_size;
    log!("编辑内容已复制到剪贴板，按ctrl+v输入内容，按ctrl+z取消");
    1234
}

#[no_mangle]
pub extern "C" fn editRelease(_edit: c_int) -> c_int {
    edit_release()
}

pub fn edit_release() -> c_int {
    *EDIT_MODE.lock().unwrap() = false;
    let mut ptr = HOLD_EDIT_TEXT_PTR.lock().unwrap();
    if !ptr.0.is_null() {
        compat::free_ext(ptr.0 as *mut c_void);
        ptr.0 = std::ptr::null_mut();
    }
    0
}

#[no_mangle]
pub extern "C" fn editGetText(_edit: c_int) -> *mut c_char {
    edit_get_text()
}

pub fn edit_get_text() -> *mut c_char {
    HOLD_EDIT_TEXT_PTR.lock().unwrap().0
}

fn saveEditText(s: &str) {
    let max_size = *EDIT_MAX_SIZE.lock().unwrap();
    let mut count = 0;
    let mut byte_len = 0;
    for c in s.chars() {
        if count >= max_size {
            break;
        }
        count += 1;
        byte_len += c.len_utf8();
    }
    let truncated = &s[..byte_len];

    let mut ptr = HOLD_EDIT_TEXT_PTR.lock().unwrap();
    if !ptr.0.is_null() {
        compat::free_ext(ptr.0 as *mut c_void);
        ptr.0 = std::ptr::null_mut();
    }

    let c_str = std::ffi::CString::new(truncated).unwrap();
    let bytes = c_str.as_bytes_with_nul();
    let allocated = compat::malloc_ext(bytes.len() as u32);
    if !allocated.is_null() {
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), allocated as *mut u8, bytes.len());
        }
        ptr.0 = allocated as *mut c_char;
    }
}

fn keycode_to_mr(k: Keycode) -> Option<c_int> {
    match k {
        Keycode::Num0 | Keycode::Kp0 => Some(MR_KEY_0),
        Keycode::Num1 | Keycode::Kp1 => Some(MR_KEY_1),
        Keycode::Num2 | Keycode::Kp2 => Some(MR_KEY_2),
        Keycode::Num3 | Keycode::Kp3 => Some(MR_KEY_3),
        Keycode::Num4 | Keycode::Kp4 => Some(MR_KEY_4),
        Keycode::Num5 | Keycode::Kp5 => Some(MR_KEY_5),
        Keycode::Num6 | Keycode::Kp6 => Some(MR_KEY_6),
        Keycode::Num7 | Keycode::Kp7 => Some(MR_KEY_7),
        Keycode::Num8 | Keycode::Kp8 => Some(MR_KEY_8),
        Keycode::Num9 | Keycode::Kp9 => Some(MR_KEY_9),
        Keycode::Return | Keycode::KpEnter => Some(MR_KEY_SELECT),
        Keycode::Equals => Some(MR_KEY_POUND),
        Keycode::Minus => Some(MR_KEY_STAR),
        Keycode::W | Keycode::Up => Some(MR_KEY_UP),
        Keycode::S | Keycode::Down => Some(MR_KEY_DOWN),
        Keycode::A | Keycode::Left => Some(MR_KEY_LEFT),
        Keycode::D | Keycode::Right => Some(MR_KEY_RIGHT),
        Keycode::Q | Keycode::LeftBracket => Some(MR_KEY_SOFTLEFT),
        Keycode::E | Keycode::RightBracket => Some(MR_KEY_SOFTRIGHT),
        Keycode::Tab => Some(MR_KEY_SEND),
        Keycode::Escape => Some(MR_KEY_POWER),
        _ => None,
    }
}

fn show_missing_system_files_message(
    mythroad_dir: &std::path::Path,
    missing: &[std::path::PathBuf],
) {
    let missing = missing
        .iter()
        .map(|path| format!("  - {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let message = format!(
        "Missing required system files.\n\nPlease add these files to:\n{}\n\n{}\n\nSet {} to use another directory.",
        mythroad_dir.display(),
        missing,
        paths::MYTHROAD_DIR_ENV,
    );

    let _ = sdl2::messagebox::show_simple_message_box(
        sdl2::messagebox::MessageBoxFlag::ERROR,
        "Missing system files",
        &message,
        None,
    );
}

pub fn run() -> Result<(), String> {
    log!("Starting bootstrap from Rust...");

    let sdl_context = sdl2::init()?;
    let mythroad_dir = paths::ensure_mythroad_dir()?;
    let missing = paths::missing_required_system_files();
    if !missing.is_empty() {
        show_missing_system_files_message(&mythroad_dir, &missing);
        return Err(format!(
            "missing required system files in {}",
            mythroad_dir.display()
        ));
    }

    let mut env = Environment::new(crate::options::Options::default())?;
    env.start()?;

    let video_subsystem = sdl_context.video()?;

    let window = video_subsystem
        .window("SKYMRP", 240, 320)
        .position_centered()
        .build()
        .map_err(|e| e.to_string())?;

    let mut canvas = window.into_canvas().build().map_err(|e| e.to_string())?;

    // Create a texture to act as the framebuffer
    let texture_creator = canvas.texture_creator();
    let mut texture = texture_creator
        .create_texture_streaming(PixelFormatEnum::RGB565, 240, 320)
        .map_err(|e| e.to_string())?;

    let mut event_pump = sdl_context.event_pump()?;

    'running: loop {
        let is_edit_mode = *EDIT_MODE.lock().unwrap();

        for ev in event_pump.poll_iter() {
            if is_edit_mode {
                match ev {
                    Event::KeyDown {
                        keycode: Some(Keycode::Z),
                        keymod,
                        ..
                    } if keymod.contains(sdl2::keyboard::Mod::LCTRLMOD)
                        || keymod.contains(sdl2::keyboard::Mod::RCTRLMOD) =>
                    {
                        env.event(MR_DIALOG_EVENT, 1, 0);
                        log!("取消输入");
                    }
                    Event::KeyDown {
                        keycode: Some(Keycode::V),
                        keymod,
                        ..
                    } if keymod.contains(sdl2::keyboard::Mod::LCTRLMOD)
                        || keymod.contains(sdl2::keyboard::Mod::RCTRLMOD) =>
                    {
                        if let Ok(text) = canvas.window().subsystem().clipboard().clipboard_text() {
                            saveEditText(&text);
                        }
                        env.event(MR_DIALOG_EVENT, 0, 0);
                    }
                    Event::MouseButtonDown { .. } => {
                        log!("ctrl+v输入内容，ctrl+z取消输入");
                    }
                    Event::Quit { .. } => break 'running,
                    _ => {}
                }
                continue;
            }

            match ev {
                Event::Quit { .. } => {
                    break 'running;
                }
                Event::KeyDown {
                    keycode: Some(k), ..
                } => {
                    if let Some(mr_key) = keycode_to_mr(k) {
                        env.event(MR_KEY_PRESS, mr_key, 0);
                    }
                }
                Event::KeyUp {
                    keycode: Some(k), ..
                } => {
                    if let Some(mr_key) = keycode_to_mr(k) {
                        env.event(MR_KEY_RELEASE, mr_key, 0);
                    }
                }
                Event::MouseButtonDown { x, y, .. } => {
                    env.event(MR_MOUSE_DOWN, x, y);
                }
                Event::MouseButtonUp { x, y, .. } => {
                    env.event(MR_MOUSE_UP, x, y);
                }
                Event::MouseMotion {
                    x, y, mousestate, ..
                } => {
                    if mousestate.left() || mousestate.right() || mousestate.middle() {
                        env.event(MR_MOUSE_MOVE, x, y);
                    }
                }
                _ => {}
            }
        }

        let should_trigger_timer = {
            let mut timer_target = TIMER_TARGET.lock().unwrap();
            if let Some(target) = *timer_target {
                if std::time::Instant::now() >= target {
                    *timer_target = None;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };

        if should_trigger_timer {
            env.timer();
        }

        {
            let fb = FRAME_BUFFER.lock().unwrap();
            let fb_u8: &[u8] =
                unsafe { std::slice::from_raw_parts(fb.as_ptr() as *const u8, fb.len() * 2) };
            texture.update(None, fb_u8, 240 * 2).unwrap();
        }

        canvas.clear();
        canvas.copy(&texture, None, None)?;
        canvas.present();

        std::thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}
