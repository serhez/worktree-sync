//! One module per terminal multiplexer. Each adapts the host-agnostic
//! lifecycle to what its host provides and supplies what the host lacks.

pub mod herdr;
pub mod rex;
