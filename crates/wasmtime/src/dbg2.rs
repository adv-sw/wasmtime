//! DBG2 stub implementation for guest debugging.

use crate::{Result, StoreContextMut};
use std::net::TcpStream;

/// Starts a new GDB stub debugging session over the given TCP stream.
pub fn start_debug_session<T>(
    _ctx: &mut StoreContextMut<'_, T>,
    _stream: TcpStream,
) -> Result<()> {
    // DBG2 initialize
    
    todo!("Implement dbg2 internal initialize");
}