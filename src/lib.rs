#![no_std]
#![allow(non_snake_case)]

#[cfg(test)]
extern crate std;

pub mod codec;
pub mod handler;
pub mod config;
pub mod broker;
pub mod client;
pub mod error;
pub mod protocol;
pub mod router;
#[path = "session/mod.rs"]
pub mod session;
pub mod tls;
pub mod topics;
pub mod transport;

#[cfg(test)]
mod host_defmt {
    #[no_mangle]
    pub unsafe fn _defmt_acquire() {}

    #[no_mangle]
    pub unsafe fn _defmt_release() {}

    #[no_mangle]
    pub unsafe fn _defmt_flush() {}

    #[no_mangle]
    pub unsafe fn _defmt_write(_bytes: &[u8]) {}

    #[no_mangle]
    pub unsafe fn _defmt_timestamp(_fmt: defmt::Formatter<'_>) {}

    #[no_mangle]
    pub unsafe fn _defmt_panic() -> ! {
        panic!("defmt panic")
    }

    #[no_mangle]
    pub static __DEFMT_MARKER_TRACE_START: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_TRACE_END: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_DEBUG_START: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_DEBUG_END: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_INFO_START: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_INFO_END: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_WARN_START: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_WARN_END: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_ERROR_START: u8 = 0;
    #[no_mangle]
    pub static __DEFMT_MARKER_ERROR_END: u8 = 0;
}
