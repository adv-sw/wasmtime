//! C API implementation of `wasmtime_store_start_gdbstub`.
//!

use wasmtime::{Error, Store};
use std::ffi::CStr;
use std::net::{SocketAddr, TcpListener};
use std::os::raw::c_char;
use std::sync::{Arc, Mutex};
use wasmtime::AsContextMut;


/// Opaque host-data type used when the caller provides none.
/// The store_gdbstub function only needs a context pointer; the actual
/// host data lives inside the caller's `wasmtime_store_t`.
struct NoData;

/// # Safety
///
/// Called from C.  `store` must be a valid, non-null `wasmtime_store_t *`.
/// `host` must be a valid, null-terminated C string.
#[unsafe(no_mangle)]
#[cfg(feature = "gdbstub")]
pub unsafe extern "C" fn wasmtime_store_start_gdbstub(
    store: *mut crate::WasmtimeStore,
    host: *const c_char,
    port: u16,
) -> Option<Box<Error>> {
    // --- Decode arguments ------------------------------------------------
    let host_str = unsafe {
        match CStr::from_ptr(host).to_str() {
            Ok(s) => s,
            Err(e) => {
               return Some(Box::new(Error::msg(format!("invalid host string: {}", e))));
            }
        }
    };

    let addr: SocketAddr = match format!("{}:{}", host_str, port).parse() {
        Ok(a) => a,
        Err(e) => {
         return Some(Box::new(Error::msg(format!(
             "invalid address '{}:{}': {}",
             host_str,
             port,
             e
         ))));
        }
    };

    // --- Bind TCP listener -----------------------------------------------
    let listener = match TcpListener::bind(addr) {
        Ok(l) => l,
        Err(e) => {
         return Some(Box::new(Error::msg(format!(
             "gdbstub: bind failed on {}: {}",
             addr,
             e
         ))));
        }
    };

    eprintln!(
        "Debugger: Debugger listening on {}",
        listener.local_addr().unwrap()
    );
    eprintln!(
        "Debugger: In LLDB, attach with: process connect --plugin wasm connect://{}",
        listener.local_addr().unwrap()
    );

    // --- Accept one connection (blocks until debugger connects) ----------
    let (tcp_stream, remote_addr) = match listener.accept() {
        Ok(pair) => pair,
        Err(e) => {
            return Some(Box::new(Error::msg(format!(
                "gdbstub: accept failed: {}",
                e
            ))));
        }
    };
    eprintln!("Debugger: Connection from {}", remote_addr);

    // --- Install debug handler on the store ------------------------------
    //
    // Safety: `store` is guaranteed non-null and exclusively owned by the
    // caller for the duration of this call.  `wasmtime_store_t` is an
    // opaque wrapper around `wasmtime::Store<T>`.  We reach into it via
    // the same pattern used in `crate::store` (store.context_mut()).
    let store_ref: &mut crate::WasmtimeStore = unsafe { &mut *store };

    // `WasmtimeStore` exposes `as_context_mut()` returning
    // `wasmtime::StoreContextMut<'_, crate::StoreData>`.
    let mut ctx = store_ref.as_context_mut();

    // Build and start the gdbstub session in a background thread.
    // `wasmtime::gdbstub::start_debug_session` is available when the
    // `gdbstub` Cargo feature is enabled on the main `wasmtime` crate.
    // It takes ownership of the TcpStream and installs a DebugHandler on
    // the store via `Store::set_debug_handler`.
    match wasmtime::gdbstub::start_debug_session(&mut ctx, tcp_stream) {
        Ok(()) => None,
        Err(e) => Some(Box::new(Error::from(e))),
    }
}
