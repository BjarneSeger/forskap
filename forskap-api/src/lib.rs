//! API for interacting with forskapd
#![allow(non_camel_case_types)]
// The generated code takes a method's arguments one by one.
#![allow(clippy::too_many_arguments)]

include!(concat!(env!("OUT_DIR"), "/org.thehoster.forskapd.rs"));

/// Raw varlink interface description, suitable for the daemon's
/// `org.varlink.service.GetInterfaceDescription` reply.
pub const VARLINK_INTERFACE_DESCRIPTION: &str =
    include_str!("../varlink/org.thehoster.forskapd.varlink");
