//! The policy daemon of PresenceMux.

#![expect(
    dead_code,
    reason = "main wires these modules together in a later step"
)]

mod config;
mod event_loop;
mod status;
mod varlink;

fn main() {}
