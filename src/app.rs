use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{
    Arc, Mutex,
    mpsc::{self, Receiver, Sender},
};
use std::thread;

use windows::Win32::Devices::Usb::{WINUSB_INTERFACE_HANDLE, WinUsb_AbortPipe};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{BeginPaint, EndPaint, InvalidateRect, PAINTSTRUCT};
use windows::Win32::System::IO::CancelIoEx;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
    GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowRect, IDC_ARROW,
    LoadCursorW, MB_ICONERROR, MSG, MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW,
    SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SetProcessDPIAware, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, WM_APP, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE,
    WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_PAINT, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, PCWSTR, w};

use crate::adb;
use crate::protocol::{Config, FPS, Frames, decode_jpeg, packet_size, publish};
use crate::renderer::Renderer;

const WM_FRAME: u32 = WM_APP + 1;
const INPUT_TOUCH_DOWN: u32 = 1;
const INPUT_TOUCH_MOVE: u32 = 2;
const INPUT_TOUCH_UP: u32 = 3;
const SOFT_SCALE: usize = 1000;

struct State {
    frames: Arc<Mutex<Frames>>,
    running: Arc<AtomicBool>,
    connection: Arc<Mutex<Option<Connection>>>,
    input: Sender<[u8; 16]>,
    pending: Arc<AtomicBool>,
    renderer: Option<Renderer>,
    touch: bool,
    numeric: bool,
    keyboard_known: bool,
}

#[derive(Clone, Copy)]
struct Connection {
    device: isize,
    interface: isize,
    input: u8,
}

fn notify(hwnd: isize, message: u32) {
    unsafe {
        let _ = PostMessageW(
            Some(HWND(hwnd as *mut c_void)),
            message,
            WPARAM(0),
            LPARAM(0),
        );
    }
}

fn abort(connection: &Arc<Mutex<Option<Connection>>>) {
    let connection = connection.lock().unwrap();
    if let Some(connection) = *connection {
        let interface = WINUSB_INTERFACE_HANDLE(connection.interface as *mut c_void);
        unsafe {
            let _ = CancelIoEx(HANDLE(connection.device as *mut c_void), None);
            let _ = WinUsb_AbortPipe(interface, connection.input);
        }
    }
}

fn send_input(input: &Sender<[u8; 16]>, action: u32, x: u16, y: u16) {
    let mut command = [0u8; 16];
    command[..8].copy_from_slice(b"CMINPUT1");
    command[8..12].copy_from_slice(&action.to_le_bytes());
    command[12..14].copy_from_slice(&x.to_le_bytes());
    command[14..16].copy_from_slice(&y.to_le_bytes());
    let _ = input.send(command);
}

fn point(hwnd: HWND, state: &State, lparam: LPARAM) -> Option<(u16, u16)> {
    let value = lparam.0 as u32;
    let x = (value as u16 as i16) as i32;
    let y = ((value >> 16) as u16 as i16) as i32;
    let (width, height) = {
        let frames = state.frames.lock().unwrap();
        (frames.width as i32, frames.height as i32)
    };
    if width <= 0 || height <= 0 {
        return None;
    }
    let mut rect = RECT::default();
    if unsafe { GetClientRect(hwnd, &mut rect) }.is_err() {
        return None;
    }
    let client_width = rect.right - rect.left;
    let client_height = rect.bottom - rect.top;
    if client_width <= 0 || client_height <= 0 {
        return None;
    }
    let scale = (client_width as f64 / height as f64).min(client_height as f64 / width as f64);
    let draw_width = height as f64 * scale;
    let draw_height = width as f64 * scale;
    let left = (client_width as f64 - draw_width) / 2.0;
    let top = (client_height as f64 - draw_height) / 2.0;
    if (x as f64) < left
        || (x as f64) >= left + draw_width
        || (y as f64) < top
        || (y as f64) >= top + draw_height
    {
        return None;
    }
    let source_x = ((y as f64 - top) / scale).clamp(0.0, (width - 1) as f64) as u16;
    let source_y =
        ((height - 1) as f64 - (x as f64 - left) / scale).clamp(0.0, (height - 1) as f64) as u16;
    Some((source_x, source_y))
}

fn scroll(state: &State, upward: bool) {
    let (width, height) = {
        let frames = state.frames.lock().unwrap();
        (frames.width, frames.height)
    };
    if width == 0 || height == 0 {
        return;
    }
    let first = if upward { width * 3 / 4 } else { width / 4 };
    let last = width - first;
    let middle = (first + last) / 2;
    let y = (height / 2) as u16;
    send_input(
        &state.input,
        INPUT_TOUCH_DOWN,
        first.min(u16::MAX as usize) as u16,
        y,
    );
    send_input(
        &state.input,
        INPUT_TOUCH_MOVE,
        middle.min(u16::MAX as usize) as u16,
        y,
    );
    send_input(
        &state.input,
        INPUT_TOUCH_UP,
        last.min(u16::MAX as usize) as u16,
        y,
    );
}

fn soft_tap(input: &Sender<[u8; 16]>, width: usize, height: usize, x: usize, y: usize) {
    if width == 0 || height == 0 {
        return;
    }
    let x = (x * width / SOFT_SCALE).min(width - 1) as u16;
    let y = (y * height / SOFT_SCALE).min(height - 1) as u16;
    send_input(input, INPUT_TOUCH_DOWN, x, y);
    send_input(input, INPUT_TOUCH_UP, x, y);
}

fn soft_layout(frames: &Frames) -> Option<bool> {
    if frames.width == 0 || frames.height == 0 {
        return None;
    }
    let x0 = frames.width * 350 / SOFT_SCALE;
    let x1 = frames.width * 472 / SOFT_SCALE;
    let y0 = frames.height * 935 / SOFT_SCALE;
    let y1 = frames.height * 968 / SOFT_SCALE;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let mut count = 0usize;
    for y in y0..y1 {
        for x in x0..x1 {
            let pixel = (y * frames.width + x) * 4;
            if frames.buffers[frames.front]
                .get(pixel..pixel + 3)
                .is_some_and(|rgb| rgb.iter().all(|value| *value > 180))
            {
                count += 1;
            }
        }
    }
    let area = (x1 - x0) * (y1 - y0);
    (count * 100 > area * 4).then_some(count * 100 < area * 11)
}

fn soft_key(state: &mut State, character: char) {
    let (width, height, layout) = {
        let frames = state.frames.lock().unwrap();
        (frames.width, frames.height, soft_layout(&frames))
    };
    if !state.keyboard_known {
        let Some(layout) = layout else {
            return;
        };
        state.numeric = layout;
        state.keyboard_known = true;
    }
    let tap = |x, y| soft_tap(&state.input, width, height, x, y);
    if character.is_ascii_alphabetic() {
        if state.numeric {
            tap(632, 63);
            state.numeric = false;
        }
        if character.is_ascii_uppercase() {
            tap(632, 958);
        }
        let character = character.to_ascii_lowercase() as u8;
        if let Some(index) = b"qwertyuiop".iter().position(|key| *key == character) {
            tap(411, 950 - index * 88);
        } else if let Some(index) = b"asdfghjkl".iter().position(|key| *key == character) {
            tap(632, 875 - index * 88);
        } else if let Some(index) = b"zxcvbnm".iter().position(|key| *key == character) {
            tap(854, 841 - index * 88);
        }
    } else if character == ' ' {
        tap(854, 199);
    }
}

fn soft_mode(state: &mut State, x: u16, y: u16) {
    let (width, height) = {
        let frames = state.frames.lock().unwrap();
        (frames.width, frames.height)
    };
    if width == 0 || height == 0 {
        return;
    }
    let x = x as usize * SOFT_SCALE / width;
    let y = y as usize * SOFT_SCALE / height;
    if (529..=736).contains(&x) && y <= 118 && state.keyboard_known {
        state.numeric = !state.numeric;
    }
}

fn stream(
    hwnd: isize,
    frames: Arc<Mutex<Frames>>,
    running: Arc<AtomicBool>,
    connection: Arc<Mutex<Option<Connection>>>,
    pending: Arc<AtomicBool>,
    input: Receiver<[u8; 16]>,
) {
    if let Err(error) = stream_inner(hwnd, &frames, &running, &connection, &pending, &input)
        && running.load(Ordering::Acquire)
    {
        let error = HSTRING::from(error);
        unsafe {
            let _ = MessageBoxW(
                Some(HWND(hwnd as *mut c_void)),
                &error,
                w!("PenCast"),
                MB_ICONERROR,
            );
        }
        notify(hwnd, WM_CLOSE);
    }
}

fn stream_inner(
    hwnd: isize,
    frames: &Arc<Mutex<Frames>>,
    running: &Arc<AtomicBool>,
    connection: &Arc<Mutex<Option<Connection>>>,
    pending: &AtomicBool,
    input: &Receiver<[u8; 16]>,
) -> Result<(), String> {
    for attempt in 0..2 {
        let serial = adb::provision(running)?;
        let usb = adb::wait_for_usb(running)?;
        *connection.lock().unwrap() = Some(Connection {
            device: usb.device.0 as isize,
            interface: usb.interface.0 as isize,
            input: usb.input,
        });
        let mut start = [0u8; 16];
        start[..8].copy_from_slice(b"CMSTART1");
        start[8..12].copy_from_slice(&FPS.to_le_bytes());
        let result = (|| {
            let mut ready = [0u8; 16];
            usb.read_exact(&mut ready)?;
            if &ready[..8] != b"CMREADY1" {
                return Err(String::from("Invalid PenCast USB handshake"));
            }
            usb.write(&start)?;
            let mut header = [0u8; 48];
            usb.read_exact(&mut header)?;
            let mut config = Config::parse(&header)?;
            native_size(HWND(hwnd as *mut c_void), &config);
            let mut maximum = config.payload;
            let mut packet = [0u8; 16];
            let mut encoded = vec![0u8; maximum];
            let mut raw = vec![0u8; config.payload];
            loop {
                if !running.load(Ordering::Acquire) {
                    return Ok(());
                }
                usb.read_exact(&mut packet)?;
                if &packet[..8] == b"CMCONFIG" {
                    let mut header = [0u8; 48];
                    header[..16].copy_from_slice(&packet);
                    usb.read_exact(&mut header[16..])?;
                    config = Config::parse(&header)?;
                    native_size(HWND(hwnd as *mut c_void), &config);
                    maximum = config.payload;
                    encoded.resize(maximum, 0);
                    raw.resize(config.payload, 0);
                    continue;
                }
                let length = packet_size(&packet, maximum)?;
                usb.read_exact(&mut encoded[..length])?;
                decode_jpeg(&encoded[..length], &config, &mut raw)?;
                publish(frames, &config, &raw);
                if !pending.swap(true, Ordering::AcqRel) {
                    notify(hwnd, WM_FRAME);
                }
                while let Ok(command) = input.try_recv() {
                    usb.write(&command)?;
                }
            }
        })();
        let mut stop = [0u8; 16];
        stop[..8].copy_from_slice(b"CMSTOP01");
        let _ = usb.write(&stop);
        usb.abort();
        *connection.lock().unwrap() = None;
        drop(usb);
        if attempt == 0
            && running.load(Ordering::Acquire)
            && matches!(&result, Err(error) if error == "Invalid PenCast USB handshake")
        {
            adb::wait_for_cleanup(&serial, running)?;
            continue;
        }
        return result;
    }
    unreachable!()
}

fn native_size(hwnd: HWND, config: &Config) {
    let mut outer = RECT::default();
    let mut client = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut outer) }.is_err()
        || unsafe { GetClientRect(hwnd, &mut client) }.is_err()
    {
        return;
    }
    let width = config.height as i32 + (outer.right - outer.left) - (client.right - client.left);
    let height = config.width as i32 + (outer.bottom - outer.top) - (client.bottom - client.top);
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width,
            height,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn paint(hwnd: HWND, state: &mut State) -> Result<(), String> {
    let frames = Arc::clone(&state.frames);
    let frames = frames.lock().unwrap();
    state
        .renderer
        .as_mut()
        .ok_or_else(|| String::from("D3D12 renderer is unavailable"))?
        .render(hwnd, &frames)
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam.0 as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        }
        return LRESULT(1);
    }
    let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State };
    if pointer.is_null() {
        return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
    }
    let state = unsafe { &mut *pointer };
    match message {
        WM_PAINT => {
            let mut structure = PAINTSTRUCT::default();
            unsafe {
                let _ = BeginPaint(hwnd, &mut structure);
                let _ = EndPaint(hwnd, &structure);
            }
            if paint(hwnd, state).is_err() {
                state.renderer = None;
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_FRAME => {
            state.pending.store(false, Ordering::Release);
            unsafe {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_KEYDOWN if wparam.0 == 0x1b => {
            state.renderer = None;
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_CHAR => {
            if let Some(character) = char::from_u32(wparam.0 as u32) {
                soft_key(state, character);
            }
            LRESULT(0)
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => LRESULT(0),
        WM_LBUTTONDOWN => {
            if let Some((x, y)) = point(hwnd, state, lparam) {
                state.touch = true;
                let width = state.frames.lock().unwrap().width;
                if width == 0
                    || !(286 * width / SOFT_SCALE..=964 * width / SOFT_SCALE)
                        .contains(&(x as usize))
                {
                    state.keyboard_known = false;
                }
                soft_mode(state, x, y);
                send_input(&state.input, INPUT_TOUCH_DOWN, x, y);
                unsafe {
                    let _ = SetCapture(hwnd);
                }
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE if state.touch => {
            if let Some((x, y)) = point(hwnd, state, lparam) {
                send_input(&state.input, INPUT_TOUCH_MOVE, x, y);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP | WM_CAPTURECHANGED => {
            if state.touch {
                let (x, y) = point(hwnd, state, lparam).unwrap_or((0, 0));
                send_input(&state.input, INPUT_TOUCH_UP, x, y);
                state.touch = false;
            }
            unsafe {
                let _ = ReleaseCapture();
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let delta = ((wparam.0 >> 16) as u16) as i16;
            if delta != 0 {
                scroll(state, delta > 0);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            state.renderer = None;
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            state.renderer = None;
            state.running.store(false, Ordering::Release);
            abort(&state.connection);
            unsafe {
                PostQuitMessage(0);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

pub(crate) fn run() {
    if let Err(error) = run_inner() {
        let error = HSTRING::from(error);
        unsafe {
            let _ = MessageBoxW(None, &error, w!("PenCast"), MB_ICONERROR);
        }
    }
}

fn run_inner() -> Result<(), String> {
    unsafe {
        let _ = SetProcessDPIAware();
    }
    let frames = Arc::new(Mutex::new(Frames::new()));
    let running = Arc::new(AtomicBool::new(true));
    let connection = Arc::new(Mutex::new(None));
    let pending = Arc::new(AtomicBool::new(false));
    let (input, receiver) = mpsc::channel();
    let mut state = Box::new(State {
        frames: Arc::clone(&frames),
        running: Arc::clone(&running),
        connection: Arc::clone(&connection),
        input,
        pending: Arc::clone(&pending),
        renderer: None,
        touch: false,
        numeric: false,
        keyboard_known: false,
    });
    let module = unsafe { GetModuleHandleW(PCWSTR::null()) }.map_err(|error| error.to_string())?;
    let instance = windows::Win32::Foundation::HINSTANCE(module.0);
    let cursor = unsafe { LoadCursorW(None, IDC_ARROW) }.map_err(|error| error.to_string())?;
    let class = WNDCLASSW {
        hCursor: cursor,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        lpszClassName: w!("PenCastWindow"),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(String::from("RegisterClassW failed"));
    }
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("PenCastWindow"),
            w!("PenCast"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1000,
            480,
            None,
            None,
            Some(instance),
            Some((&mut *state as *mut State).cast::<c_void>()),
        )
    }
    .map_err(|error| error.to_string())?;
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    let worker_frames = Arc::clone(&frames);
    let worker_running = Arc::clone(&running);
    let worker_connection = Arc::clone(&connection);
    let worker_pending = Arc::clone(&pending);
    let worker_hwnd = hwnd.0 as isize;
    let worker = thread::spawn(move || {
        stream(
            worker_hwnd,
            worker_frames,
            worker_running,
            worker_connection,
            worker_pending,
            receiver,
        )
    });
    state.renderer = Some(Renderer::new(hwnd)?);
    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    running.store(false, Ordering::Release);
    let _ = worker.join();
    drop(state);
    Ok(())
}
