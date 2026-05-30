use crate::{audio, compat, mem::Mem, paths, Environment};
use sdl2::event::Event;
use sdl2::keyboard::Keycode;
use sdl2::pixels::PixelFormatEnum;
use sdl2::render::Canvas;
use sdl2::video::Window;
use sdl2::EventPump;
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::fs;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
    static ref FONT16: Option<Vec<u8>> = fs::read(paths::mythroad_dir().join("system/gb16.uc2")).ok();
}

thread_local! {
    static DIRECT_WINDOW: RefCell<Option<DirectWindowState>> = const { RefCell::new(None) };
}

struct DirectWindowState {
    _sdl_context: sdl2::Sdl,
    canvas: Canvas<Window>,
    event_pump: EventPump,
    last_present: Option<Instant>,
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
    blit_rgb565(bmp, 240, x, y, w, h);
}

pub fn clear_framebuffer(color: u16) {
    FRAME_BUFFER.lock().unwrap().fill(color);
}

pub fn snapshot_framebuffer() -> Vec<u16> {
    FRAME_BUFFER.lock().unwrap().clone()
}

pub fn blit_rgb565(pixels: &[u16], stride: c_int, x: c_int, y: c_int, w: c_int, h: c_int) {
    blit_rgb565_inner(pixels, stride, x, y, w, h, None);
}

pub fn blit_rgb565_transparent(
    pixels: &[u16],
    stride: c_int,
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
    transparent: u16,
) {
    blit_rgb565_inner(pixels, stride, x, y, w, h, Some(transparent));
}

fn blit_rgb565_inner(
    pixels: &[u16],
    stride: c_int,
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
    transparent: Option<u16>,
) {
    if stride <= 0 || w <= 0 || h <= 0 {
        return;
    }
    if x >= 240 || y >= 320 || x + w <= 0 || y + h <= 0 {
        return;
    }

    let mut fb = FRAME_BUFFER.lock().unwrap();
    let src_x_start = if x < 0 { -x } else { 0 };
    let src_y_start = if y < 0 { -y } else { 0 };
    let src_x_end = w.min(240 - x);
    let src_y_end = h.min(320 - y);

    for j in src_y_start..src_y_end {
        let dst_start = ((y + j) * 240 + (x + src_x_start)) as usize;
        let src_start = (j * stride + src_x_start) as usize;
        let width = (src_x_end - src_x_start) as usize;
        let Some(src_row) = pixels.get(src_start..src_start + width) else {
            continue;
        };
        let dst_row = &mut fb[dst_start..dst_start + width];
        if let Some(transparent) = transparent {
            for (dst, src) in dst_row.iter_mut().zip(src_row.iter().copied()) {
                if src != transparent {
                    *dst = src;
                }
            }
        } else {
            dst_row.copy_from_slice(src_row);
        }
    }
}

pub fn fill_rect_rgb565(x: c_int, y: c_int, w: c_int, h: c_int, color: u16) {
    if w <= 0 || h <= 0 {
        return;
    }

    let mut fb = FRAME_BUFFER.lock().unwrap();
    for yy in y.max(0)..(y + h).min(320) {
        for xx in x.max(0)..(x + w).min(240) {
            fb[(yy * 240 + xx) as usize] = color;
        }
    }
}

pub fn draw_line_rgb565(mut x0: c_int, mut y0: c_int, x1: c_int, y1: c_int, color: u16) {
    let dx = (x1 - x0).abs();
    let sx = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    let mut fb = FRAME_BUFFER.lock().unwrap();

    loop {
        if x0 >= 0 && x0 < 240 && y0 >= 0 && y0 < 320 {
            fb[(y0 * 240 + x0) as usize] = color;
        }
        if x0 == x1 && y0 == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x0 += sx;
        }
        if e2 <= dx {
            err += dx;
            y0 += sy;
        }
    }
}

pub fn draw_text_rgb565(text: &str, x: c_int, y: c_int, color: u16) {
    let chars = text.chars().collect::<Vec<_>>();
    draw_text_chars_rgb565(&chars, x, y, color, None, false);
}

pub fn draw_text_bytes_rgb565(bytes: &[u8], is_unicode: bool, x: c_int, y: c_int, color: u16) {
    let chars = decode_text_bytes(bytes, is_unicode);
    draw_text_chars_rgb565(&chars, x, y, color, None, false);
}

pub fn draw_text_ex_bytes_rgb565(
    bytes: &[u8],
    is_unicode: bool,
    x: c_int,
    y: c_int,
    rect: (c_int, c_int, c_int, c_int),
    color: u16,
    auto_newline: bool,
) -> c_int {
    let chars = decode_text_bytes(bytes, is_unicode);
    draw_text_chars_rgb565(&chars, x, y, color, Some(rect), auto_newline) as c_int
}

pub fn text_width_bytes(bytes: &[u8], is_unicode: bool) -> c_int {
    decode_text_bytes(bytes, is_unicode)
        .iter()
        .map(|ch| if ch.is_ascii() { 8 } else { 16 })
        .sum()
}

fn decode_text_bytes(bytes: &[u8], is_unicode: bool) -> Vec<char> {
    if is_unicode {
        return bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_be_bytes([chunk[0], chunk[1]]))
            .take_while(|code| *code != 0)
            .filter_map(|code| char::from_u32(code as u32))
            .collect();
    }

    let text = String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| {
        let (text, _, _) = encoding_rs::GBK.decode(bytes);
        text.into_owned()
    });
    text.chars().collect()
}

fn draw_text_chars_rgb565(
    chars: &[char],
    x: c_int,
    y: c_int,
    color: u16,
    clip: Option<(c_int, c_int, c_int, c_int)>,
    auto_newline: bool,
) -> usize {
    let mut cursor_x = x;
    let mut cursor_y = y;
    let line_height = 16;
    let mut drawn_chars = 0usize;

    for ch in chars.iter().copied().take(256) {
        let width = if ch.is_ascii() { 8 } else { 16 };
        if let Some((_, _, clip_w, clip_h)) = clip {
            if auto_newline && (cursor_x + width > x + clip_w || ch == '\n') {
                cursor_x = x;
                cursor_y += line_height + 2;
                if cursor_y > y + clip_h {
                    break;
                }
                if ch == '\n' {
                    drawn_chars += 1;
                    continue;
                }
            } else if !auto_newline && (cursor_x > x + clip_w || ch == '\n') {
                break;
            }
        } else if ch == '\n' {
            cursor_x = x;
            cursor_y += line_height;
            drawn_chars += 1;
            continue;
        }

        if !draw_gb16_char(ch, cursor_x, cursor_y, color, clip) {
            draw_fallback_char(cursor_x, cursor_y, width, line_height, color, clip);
        }
        cursor_x += width;
        drawn_chars += 1;
    }
    drawn_chars
}

fn draw_gb16_char(
    ch: char,
    x: c_int,
    y: c_int,
    color: u16,
    clip: Option<(c_int, c_int, c_int, c_int)>,
) -> bool {
    let Some(font) = FONT16.as_ref() else {
        return false;
    };
    let codepoint = ch as usize;
    let glyph_size = 32;
    let offset = codepoint.saturating_mul(glyph_size);
    let Some(glyph) = font.get(offset..offset + glyph_size) else {
        return false;
    };
    if glyph.iter().all(|byte| *byte == 0) {
        return ch == ' ';
    }

    let mut fb = FRAME_BUFFER.lock().unwrap();
    for gy in 0..16 {
        for gx in 0..16 {
            let byte = glyph[gy * 2 + gx / 8];
            if byte & (0x80 >> (gx & 7)) == 0 {
                continue;
            }
            let xx = x + gx as c_int;
            let yy = y + gy as c_int;
            if point_in_clip(xx, yy, clip) {
                fb[(yy * 240 + xx) as usize] = color;
            }
        }
    }
    true
}

fn draw_fallback_char(
    x: c_int,
    y: c_int,
    w: c_int,
    h: c_int,
    color: u16,
    clip: Option<(c_int, c_int, c_int, c_int)>,
) {
    let mut fb = FRAME_BUFFER.lock().unwrap();
    for yy in y..y + h {
        for xx in x..x + w {
            let on_border = yy == y || yy == y + h - 1 || xx == x || xx == x + w - 1;
            if on_border && point_in_clip(xx, yy, clip) {
                fb[(yy * 240 + xx) as usize] = color;
            }
        }
    }
}

fn point_in_clip(x: c_int, y: c_int, clip: Option<(c_int, c_int, c_int, c_int)>) -> bool {
    if x < 0 || x >= 240 || y < 0 || y >= 320 {
        return false;
    }
    if let Some((clip_x, clip_y, clip_w, clip_h)) = clip {
        x >= clip_x && x < clip_x + clip_w && y >= clip_y && y < clip_y + clip_h
    } else {
        true
    }
}

pub fn rgb_to_rgb565(r: c_int, g: c_int, b: c_int) -> u16 {
    let r = r.clamp(0, 255) as u16;
    let g = g.clamp(0, 255) as u16;
    let b = b.clamp(0, 255) as u16;
    ((r & 0xF8) << 8) | ((g & 0xFC) << 3) | (b >> 3)
}

pub fn init_direct_window() -> Result<(), String> {
    DIRECT_WINDOW.with(|state| {
        if state.borrow().is_some() {
            return Ok(());
        }

        let sdl_context = sdl2::init()?;
        let video_subsystem = sdl_context.video()?;
        let mut window = video_subsystem
            .window("SKYMRP", 240, 320)
            .position_centered()
            .build()
            .map_err(|e| e.to_string())?;
        window.show();
        window.raise();
        let event_pump = sdl_context.event_pump()?;
        let canvas = window.into_canvas().build().map_err(|e| e.to_string())?;
        state.replace(Some(DirectWindowState {
            _sdl_context: sdl_context,
            canvas,
            event_pump,
            last_present: None,
        }));
        Ok(())
    })
}

pub fn present_direct_frame() -> Result<(), String> {
    DIRECT_WINDOW.with(|state| {
        let mut state = state.borrow_mut();
        let Some(state) = state.as_mut() else {
            return Ok(());
        };

        let now = Instant::now();
        if state
            .last_present
            .is_some_and(|last| now.duration_since(last) < Duration::from_millis(16))
        {
            return Ok(());
        }
        state.last_present = Some(now);

        let texture_creator = state.canvas.texture_creator();
        let mut texture = texture_creator
            .create_texture_streaming(PixelFormatEnum::RGB565, 240, 320)
            .map_err(|e| e.to_string())?;

        {
            let fb = FRAME_BUFFER.lock().unwrap();
            let fb_u8: &[u8] =
                unsafe { std::slice::from_raw_parts(fb.as_ptr() as *const u8, fb.len() * 2) };
            texture
                .update(None, fb_u8, 240 * 2)
                .map_err(|e| e.to_string())?;
        }

        state.canvas.clear();
        state.canvas.copy(&texture, None, None)?;
        state.canvas.present();
        Ok(())
    })
}

pub fn timer_start(t: u16) -> c_int {
    let mut timer_target = TIMER_TARGET.lock().unwrap();
    *timer_target = Some(std::time::Instant::now() + std::time::Duration::from_millis(t as u64));
    0
}

pub fn poll_direct_events() -> (bool, Vec<DirectWindowEvent>) {
    let mut events = Vec::new();
    let mut quit = false;
    DIRECT_WINDOW.with(|state| {
        if let Some(state) = state.borrow_mut().as_mut() {
            for ev in state.event_pump.poll_iter() {
                collect_direct_event(ev, &mut events, &mut quit);
            }
        }
    });
    (quit, events)
}

pub fn take_direct_timer_event() -> bool {
    let mut timer_target = TIMER_TARGET.lock().unwrap();
    if let Some(target) = *timer_target {
        if std::time::Instant::now() >= target {
            *timer_target = None;
            return true;
        }
    }
    false
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
    log!("editRelease called without guest memory context");
    0
}

pub fn edit_release(mem: &mut Mem) -> c_int {
    *EDIT_MODE.lock().unwrap() = false;
    let mut ptr = HOLD_EDIT_TEXT_PTR.lock().unwrap();
    if !ptr.0.is_null() {
        compat::free_ext_in(mem, ptr.0 as *mut c_void);
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

fn save_edit_text(mem: &mut Mem, s: &str) {
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
        compat::free_ext_in(mem, ptr.0 as *mut c_void);
        ptr.0 = std::ptr::null_mut();
    }

    let c_str = std::ffi::CString::new(truncated).unwrap();
    let bytes = c_str.as_bytes_with_nul();
    let allocated = compat::malloc_ext_in(mem, bytes.len() as u32);
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

pub enum DirectWindowEvent {
    Event(c_int, c_int, c_int),
    Timer,
    Paste(String),
    Frame,
}

pub(crate) fn show_missing_system_files_message(
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

pub fn run(env: &mut Environment) -> Result<(), String> {
    run_loop(|event| match event {
        DirectWindowEvent::Event(code, param0, param1) => {
            env.event(code, param0, param1);
            Ok(())
        }
        DirectWindowEvent::Timer => {
            env.timer();
            Ok(())
        }
        DirectWindowEvent::Paste(text) => {
            save_edit_text(&mut env.mem, &text);
            Ok(())
        }
        DirectWindowEvent::Frame => {
            env.poll_network_callbacks();
            Ok(())
        }
    })
}

pub fn run_direct(
    mut handler: impl FnMut(DirectWindowEvent) -> Result<(), String>,
    should_exit: impl Fn() -> bool,
) -> Result<(), String> {
    if is_direct_window_initialized() {
        return run_existing_direct_loop(handler, should_exit);
    }

    run_loop(|event| match event {
        DirectWindowEvent::Frame => {
            if should_exit() {
                Err("__SKYMRP_DIRECT_EXIT__".to_string())
            } else {
                Ok(())
            }
        }
        DirectWindowEvent::Paste(_) => Ok(()),
        event => handler(event),
    })
    .or_else(|err| {
        if err == "__SKYMRP_DIRECT_EXIT__" {
            Ok(())
        } else {
            Err(err)
        }
    })
}

fn is_direct_window_initialized() -> bool {
    DIRECT_WINDOW.with(|state| state.borrow().is_some())
}

fn run_existing_direct_loop(
    mut handler: impl FnMut(DirectWindowEvent) -> Result<(), String>,
    should_exit: impl Fn() -> bool,
) -> Result<(), String> {
    'running: loop {
        if should_exit() {
            break 'running;
        }

        let (quit, events) = poll_direct_events();

        if quit {
            break 'running;
        }

        for event in events {
            handler(event)?;
        }

        if take_direct_timer_event() {
            handler(DirectWindowEvent::Timer)?;
        }

        handler(DirectWindowEvent::Frame)?;
        present_direct_frame()?;
        std::thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}

fn collect_direct_event(ev: Event, events: &mut Vec<DirectWindowEvent>, quit: &mut bool) {
    match ev {
        Event::Quit { .. } => *quit = true,
        Event::KeyDown {
            keycode: Some(k), ..
        } => {
            if let Some(mr_key) = keycode_to_mr(k) {
                events.push(DirectWindowEvent::Event(MR_KEY_PRESS, mr_key, 0));
            }
        }
        Event::KeyUp {
            keycode: Some(k), ..
        } => {
            if let Some(mr_key) = keycode_to_mr(k) {
                events.push(DirectWindowEvent::Event(MR_KEY_RELEASE, mr_key, 0));
            }
        }
        Event::MouseButtonDown { x, y, .. } => {
            events.push(DirectWindowEvent::Event(MR_MOUSE_DOWN, x, y));
        }
        Event::MouseButtonUp { x, y, .. } => {
            events.push(DirectWindowEvent::Event(MR_MOUSE_UP, x, y));
        }
        Event::MouseMotion {
            x, y, mousestate, ..
        } => {
            if mousestate.left() || mousestate.right() || mousestate.middle() {
                events.push(DirectWindowEvent::Event(MR_MOUSE_MOVE, x, y));
            }
        }
        _ => {}
    }
}

fn run_loop(
    mut handler: impl FnMut(DirectWindowEvent) -> Result<(), String>,
) -> Result<(), String> {
    let sdl_context = sdl2::init()?;
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
                        handler(DirectWindowEvent::Event(MR_DIALOG_EVENT, 1, 0))?;
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
                            handler(DirectWindowEvent::Paste(text))?;
                        }
                        handler(DirectWindowEvent::Event(MR_DIALOG_EVENT, 0, 0))?;
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
                        handler(DirectWindowEvent::Event(MR_KEY_PRESS, mr_key, 0))?;
                    }
                }
                Event::KeyUp {
                    keycode: Some(k), ..
                } => {
                    if let Some(mr_key) = keycode_to_mr(k) {
                        handler(DirectWindowEvent::Event(MR_KEY_RELEASE, mr_key, 0))?;
                    }
                }
                Event::MouseButtonDown { x, y, .. } => {
                    handler(DirectWindowEvent::Event(MR_MOUSE_DOWN, x, y))?;
                }
                Event::MouseButtonUp { x, y, .. } => {
                    handler(DirectWindowEvent::Event(MR_MOUSE_UP, x, y))?;
                }
                Event::MouseMotion {
                    x, y, mousestate, ..
                } => {
                    if mousestate.left() || mousestate.right() || mousestate.middle() {
                        handler(DirectWindowEvent::Event(MR_MOUSE_MOVE, x, y))?;
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
            handler(DirectWindowEvent::Timer)?;
        }

        handler(DirectWindowEvent::Frame)?;

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
