use crate::bridge;
use libc::{c_char, c_int, c_ushort, c_void};
use std::collections::HashMap;
use std::ffi::CStr;
use std::io;
use std::net::{Ipv4Addr, ToSocketAddrs};
use std::os::fd::RawFd;
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

const MR_SUCCESS: c_int = 0;
const MR_FAILED: c_int = -1;
const MR_WAITING: c_int = 2;

const MR_SOCK_STREAM: c_int = 0;
const MR_IPPROTO_TCP: c_int = 0;

const MR_SOCKET_NONBLOCK: c_int = 1;

const CMWAP_PROXY_IP: c_int = 0x0A0000AC;

#[cfg(unix)]
const SHUTDOWN_BIDIRECTIONAL: c_int = libc::SHUT_RDWR;

struct NetworkState {
    is_cmwap: bool,
    next_socket: c_int,
    sockets: HashMap<c_int, Arc<Mutex<SocketEntry>>>,
}

struct SocketEntry {
    fd: RawFd,
    send_counter: u32,
    real_state: c_int,
    state: c_int,
    closed: bool,
    connect_thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
struct NetworkCallback {
    uc: usize,
    addr: u32,
    user_data: u32,
}

unsafe impl Send for NetworkCallback {}

static NETWORK: OnceLock<Mutex<NetworkState>> = OnceLock::new();

fn network() -> &'static Mutex<NetworkState> {
    NETWORK.get_or_init(|| {
        Mutex::new(NetworkState {
            is_cmwap: false,
            next_socket: 0,
            sockets: HashMap::new(),
        })
    })
}

fn socket_entry(handle: c_int) -> Option<Arc<Mutex<SocketEntry>>> {
    network().lock().ok()?.sockets.get(&handle).cloned()
}

fn close_entry(entry: Arc<Mutex<SocketEntry>>) -> c_int {
    let (fd, connect_thread) = {
        let mut entry = entry.lock().unwrap();
        if entry.closed {
            return MR_SUCCESS;
        }
        entry.closed = true;
        unsafe {
            libc::shutdown(entry.fd, SHUTDOWN_BIDIRECTIONAL);
        }
        (entry.fd, entry.connect_thread.take())
    };

    if let Some(connect_thread) = connect_thread {
        let _ = connect_thread.join();
    }

    if unsafe { libc::close(fd) } == 0 {
        MR_SUCCESS
    } else {
        MR_FAILED
    }
}

fn sockaddr_from_host_order(ip: c_int, port: c_ushort) -> libc::sockaddr_in {
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        addr.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }
    addr.sin_family = libc::AF_INET as libc::sa_family_t;
    addr.sin_port = port.to_be();
    addr.sin_addr = libc::in_addr {
        s_addr: (ip as u32).to_be(),
    };
    addr
}

fn connect_sync_fd(fd: RawFd, ip: c_int, port: c_ushort) -> c_int {
    let addr = sockaddr_from_host_order(ip, port);
    let printable = Ipv4Addr::from(ip as u32);
    log!("my_connect('{printable}', {port})");

    let ret = unsafe {
        libc::connect(
            fd,
            (&addr as *const libc::sockaddr_in).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if ret == 0 {
        log!("my_connect(0x{:X}) suc", ip);
        MR_SUCCESS
    } else {
        log!(
            "my_connect(0x{:X}) fail: {}",
            ip,
            io::Error::last_os_error()
        );
        MR_FAILED
    }
}

fn run_connect_async(entry: Arc<Mutex<SocketEntry>>, ip: c_int, port: c_ushort) {
    let fd = entry.lock().unwrap().fd;
    let result = connect_sync_fd(fd, ip, port);
    let mut entry = entry.lock().unwrap();
    if entry.closed {
        return;
    }
    entry.real_state = result;
    if !is_cmwap() {
        entry.state = result;
    }
}

fn is_cmwap() -> bool {
    network()
        .lock()
        .map(|state| state.is_cmwap)
        .unwrap_or(false)
}

fn cstr_ref_to_string(ptr: &CStr) -> String {
    ptr.to_string_lossy().into_owned()
}

fn resolve_host_ipv4(name: &str) -> c_int {
    let addrs = match (name, 0).to_socket_addrs() {
        Ok(addrs) => addrs,
        Err(err) => {
            log!("getaddrinfo failed for '{name}': {err}");
            return MR_FAILED;
        }
    };

    for addr in addrs {
        if let std::net::SocketAddr::V4(addr) = addr {
            let ip = u32::from_be_bytes(addr.ip().octets()) as c_int;
            log!("--- IPv4 address: {}", addr.ip());
            return ip;
        }
    }
    MR_FAILED
}

fn invoke_network_callback(callback: NetworkCallback, result: c_int) {
    unsafe {
        bridge::bridge_dsm_network_cb(
            callback.uc as *mut c_void,
            callback.addr,
            result,
            callback.user_data,
        );
    }
}

fn callback_from_parts(
    uc: *mut c_void,
    cb: *mut c_void,
    user_data: *mut c_void,
) -> NetworkCallback {
    NetworkCallback {
        uc: uc as usize,
        addr: cb as usize as u32,
        user_data: user_data as usize as u32,
    }
}

fn check_fd(fd: RawFd, write: bool) -> c_int {
    let mut fds = unsafe { std::mem::zeroed::<libc::fd_set>() };
    unsafe {
        libc::FD_ZERO(&mut fds);
        libc::FD_SET(fd, &mut fds);
    }

    let mut timeout = libc::timeval {
        tv_sec: 0,
        tv_usec: 50_000,
    };
    let ret = unsafe {
        if write {
            libc::select(
                fd + 1,
                ptr::null_mut(),
                &mut fds,
                ptr::null_mut(),
                &mut timeout,
            )
        } else {
            libc::select(
                fd + 1,
                &mut fds,
                ptr::null_mut(),
                ptr::null_mut(),
                &mut timeout,
            )
        }
    };

    if ret < 0 {
        MR_FAILED
    } else if ret == 0 {
        0
    } else if unsafe { libc::FD_ISSET(fd, &fds) } {
        1
    } else {
        0
    }
}

fn read_first_line(buf: &[u8]) -> Option<String> {
    if buf.is_empty() {
        return None;
    }
    let end = buf
        .iter()
        .position(|&b| b == b'\r' || b == 0)
        .unwrap_or(buf.len());
    Some(String::from_utf8_lossy(&buf[..end]).into_owned())
}

fn parse_host_port(line: &str) -> Option<(String, c_ushort)> {
    let start = line.find("://")? + 3;
    let rest = &line[start..];
    let host_end = rest
        .find(|ch| ch == ':' || ch == '/' || ch == ' ')
        .unwrap_or(rest.len());
    if host_end == 0 {
        return None;
    }
    let host = rest[..host_end].to_owned();
    let rest = &rest[host_end..];
    let port = if let Some(port_rest) = rest.strip_prefix(':') {
        let port_end = port_rest
            .find(|ch| ch == '/' || ch == ' ')
            .unwrap_or(port_rest.len());
        port_rest[..port_end].parse::<u16>().ok()?
    } else {
        80
    };
    Some((host, port))
}

#[no_mangle]
pub extern "C" fn my_connect(s: c_int, ip: c_int, port: c_ushort, type_: c_int) -> c_int {
    let Some(entry) = socket_entry(s) else {
        return MR_FAILED;
    };

    if ip == CMWAP_PROXY_IP {
        let mut entry = entry.lock().unwrap();
        entry.state = MR_SUCCESS;
        return MR_SUCCESS;
    }

    log!(
        "my_connect() type: {}",
        if type_ == MR_SOCKET_NONBLOCK {
            "async"
        } else {
            "block"
        }
    );

    if type_ == MR_SOCKET_NONBLOCK {
        let mut entry_guard = entry.lock().unwrap();
        let thread_entry = Arc::clone(&entry);
        let connect_thread = match thread::Builder::new()
            .name("skymrp-network-connect".to_owned())
            .spawn(move || run_connect_async(thread_entry, ip, port))
        {
            Ok(thread) => thread,
            Err(err) => {
                log!("my_connect async spawn failed: {err}");
                entry_guard.state = MR_FAILED;
                entry_guard.real_state = MR_FAILED;
                return MR_FAILED;
            }
        };

        entry_guard.connect_thread = Some(connect_thread);
        MR_WAITING
    } else {
        let result = connect_sync_fd(entry.lock().unwrap().fd, ip, port);
        let mut entry = entry.lock().unwrap();
        entry.real_state = result;
        if !is_cmwap() {
            entry.state = result;
        }
        result
    }
}

#[no_mangle]
pub extern "C" fn my_getSocketState(s: c_int) -> c_int {
    let Some(entry) = socket_entry(s) else {
        return MR_FAILED;
    };
    let state = entry.lock().unwrap().state;
    log!("my_getSocketState({s}): {state}");
    state
}

#[no_mangle]
pub extern "C" fn my_socket(type_: c_int, protocol: c_int) -> c_int {
    let socket_type = if type_ == MR_SOCK_STREAM {
        libc::SOCK_STREAM
    } else {
        libc::SOCK_DGRAM
    };
    let protocol = if protocol == MR_IPPROTO_TCP {
        libc::IPPROTO_TCP
    } else {
        libc::IPPROTO_UDP
    };

    let fd = unsafe { libc::socket(libc::AF_INET, socket_type, protocol) };
    if fd < 0 {
        log!("my_socket() fail: {}", io::Error::last_os_error());
        return MR_FAILED;
    }

    let mut state = network().lock().unwrap();
    state.next_socket += 1;
    let handle = state.next_socket;
    state.sockets.insert(
        handle,
        Arc::new(Mutex::new(SocketEntry {
            fd,
            send_counter: 0,
            real_state: MR_WAITING,
            state: MR_WAITING,
            closed: false,
            connect_thread: None,
        })),
    );
    handle
}

#[no_mangle]
pub extern "C" fn my_closeSocket(s: c_int) -> c_int {
    let entry = network().lock().unwrap().sockets.remove(&s);
    match entry {
        Some(entry) => close_entry(entry),
        None => MR_FAILED,
    }
}

#[no_mangle]
pub extern "C" fn my_closeNetwork() -> c_int {
    let entries = {
        let mut state = network().lock().unwrap();
        state
            .sockets
            .drain()
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>()
    };

    let mut ret = MR_SUCCESS;
    for entry in entries {
        if close_entry(entry) != MR_SUCCESS {
            ret = MR_FAILED;
        }
    }
    ret
}

pub fn init_network_cstr(
    uc: *mut c_void,
    cb: *mut c_void,
    mode: &CStr,
    user_data: *mut c_void,
) -> c_int {
    let mode = cstr_ref_to_string(mode);
    log!("my_initNetwork(0x{:p}, '{mode}')", cb);
    network().lock().unwrap().is_cmwap = mode.to_ascii_lowercase().starts_with("cmwap");

    if cb.is_null() {
        return MR_SUCCESS;
    }

    let callback = callback_from_parts(uc, cb, user_data);
    if thread::Builder::new()
        .name("skymrp-network-init".to_owned())
        .spawn(move || {
            log!("my_initNetworkAsync(): {MR_SUCCESS}");
            invoke_network_callback(callback, MR_SUCCESS);
        })
        .is_err()
    {
        return MR_FAILED;
    }

    MR_WAITING
}

pub fn get_host_by_name_cstr(
    uc: *mut c_void,
    name: &CStr,
    cb: *mut c_void,
    user_data: *mut c_void,
) -> c_int {
    let name = cstr_ref_to_string(name);
    log!("my_getHostByName('{name}', 0x{:p})", cb);

    if cb.is_null() {
        return resolve_host_ipv4(&name);
    }

    let callback = callback_from_parts(uc, cb, user_data);
    if thread::Builder::new()
        .name("skymrp-network-dns".to_owned())
        .spawn(move || {
            let result = resolve_host_ipv4(&name);
            log!("my_getHostByNameAsync(): 0x{result:X}");
            invoke_network_callback(callback, result);
        })
        .is_err()
    {
        return MR_FAILED;
    }

    MR_WAITING
}

pub fn send_to(s: c_int, buf: &[u8], ip: c_int, port: c_ushort) -> c_int {
    let Some(entry) = socket_entry(s) else {
        return MR_FAILED;
    };
    let fd = entry.lock().unwrap().fd;
    let addr = sockaddr_from_host_order(ip, port);
    log!(
        "my_sendto(len:{}, '{}:{port}')",
        buf.len(),
        Ipv4Addr::from(ip as u32)
    );

    let ret = unsafe {
        libc::sendto(
            fd,
            buf.as_ptr().cast::<c_void>(),
            buf.len(),
            0,
            (&addr as *const libc::sockaddr_in).cast::<libc::sockaddr>(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        MR_FAILED
    } else {
        ret as c_int
    }
}

pub fn send(s: c_int, buf: &[u8]) -> c_int {
    let Some(entry) = socket_entry(s) else {
        return MR_FAILED;
    };

    let should_start_cmwap_connect = {
        let mut entry = entry.lock().unwrap();
        entry.send_counter = entry.send_counter.saturating_add(1);
        if is_cmwap() {
            if entry.real_state == MR_WAITING {
                entry.send_counter == 1
            } else if entry.real_state == MR_FAILED {
                return MR_FAILED;
            } else {
                false
            }
        } else {
            false
        }
    };

    if should_start_cmwap_connect {
        let Some(line) = read_first_line(buf) else {
            return MR_FAILED;
        };
        let Some((host, port)) = parse_host_port(&line) else {
            return MR_FAILED;
        };
        let ip = resolve_host_ipv4(&host);
        if ip == MR_FAILED {
            return MR_FAILED;
        }
        if my_connect(s, ip, port, MR_SOCKET_NONBLOCK) == MR_FAILED {
            return MR_FAILED;
        }
        return 0;
    }

    if is_cmwap() && entry.lock().unwrap().real_state == MR_WAITING {
        return 0;
    }

    let fd = entry.lock().unwrap().fd;
    match check_fd(fd, true) {
        MR_FAILED => MR_FAILED,
        0 => 0,
        _ => {
            let ret = unsafe { libc::send(fd, buf.as_ptr().cast::<c_void>(), buf.len(), 0) };
            if ret < 0 {
                MR_FAILED
            } else {
                ret as c_int
            }
        }
    }
}

pub fn recv_from(s: c_int, buf: &mut [u8], ip: &mut c_int, port: &mut c_ushort) -> c_int {
    let Some(entry) = socket_entry(s) else {
        return MR_FAILED;
    };
    let fd = entry.lock().unwrap().fd;
    match check_fd(fd, false) {
        MR_FAILED => MR_FAILED,
        0 => 0,
        _ => {
            let mut from = unsafe { std::mem::zeroed::<libc::sockaddr_in>() };
            let mut from_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let ret = unsafe {
                libc::recvfrom(
                    fd,
                    buf.as_mut_ptr().cast::<c_void>(),
                    buf.len(),
                    0,
                    (&mut from as *mut libc::sockaddr_in).cast::<libc::sockaddr>(),
                    &mut from_len,
                )
            };
            if ret < 0 {
                return MR_FAILED;
            }
            *port = u16::from_be(from.sin_port);
            *ip = u32::from_be(from.sin_addr.s_addr) as c_int;
            log!(
                "my_recvfrom(len:{}, '{}:{}')",
                buf.len(),
                Ipv4Addr::from(*ip as u32),
                *port
            );
            ret as c_int
        }
    }
}

pub fn recv(s: c_int, buf: &mut [u8]) -> c_int {
    let Some(entry) = socket_entry(s) else {
        return MR_FAILED;
    };
    let fd = entry.lock().unwrap().fd;
    match check_fd(fd, false) {
        MR_FAILED => MR_FAILED,
        0 => 0,
        _ => {
            let ret = unsafe { libc::recv(fd, buf.as_mut_ptr().cast::<c_void>(), buf.len(), 0) };
            if ret < 0 {
                MR_FAILED
            } else {
                ret as c_int
            }
        }
    }
}

#[no_mangle]
pub extern "C" fn my_initNetwork(
    uc: *mut c_void,
    cb: *mut c_void,
    mode: *const c_char,
    user_data: *mut c_void,
) -> c_int {
    if mode.is_null() {
        return MR_FAILED;
    }
    init_network_cstr(uc, cb, unsafe { CStr::from_ptr(mode) }, user_data)
}

#[no_mangle]
pub extern "C" fn my_getHostByName(
    uc: *mut c_void,
    name: *const c_char,
    cb: *mut c_void,
    user_data: *mut c_void,
) -> c_int {
    if name.is_null() {
        return MR_FAILED;
    }
    get_host_by_name_cstr(uc, unsafe { CStr::from_ptr(name) }, cb, user_data)
}

#[no_mangle]
pub extern "C" fn my_sendto(
    s: c_int,
    buf: *const c_char,
    len: c_int,
    ip: c_int,
    port: c_ushort,
) -> c_int {
    if buf.is_null() || len < 0 {
        return MR_FAILED;
    }
    let buf = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len as usize) };
    send_to(s, buf, ip, port)
}

#[no_mangle]
pub extern "C" fn my_send(s: c_int, buf: *const c_char, len: c_int) -> c_int {
    if buf.is_null() || len < 0 {
        return MR_FAILED;
    }
    let buf = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len as usize) };
    send(s, buf)
}

#[no_mangle]
pub extern "C" fn my_recvfrom(
    s: c_int,
    buf: *mut c_char,
    len: c_int,
    ip: *mut c_int,
    port: *mut c_ushort,
) -> c_int {
    if buf.is_null() || ip.is_null() || port.is_null() || len < 0 {
        return MR_FAILED;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), len as usize) };
    recv_from(s, buf, unsafe { &mut *ip }, unsafe { &mut *port })
}

#[no_mangle]
pub extern "C" fn my_recv(s: c_int, buf: *mut c_char, len: c_int) -> c_int {
    if buf.is_null() || len < 0 {
        return MR_FAILED;
    }
    let buf = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), len as usize) };
    recv(s, buf)
}
