//! The cam worker of PresenceMux. It sends live video, a slate or blank to the
//! laptop through the UVC gadget function.

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "only the tests use this until the event loop exists"
    )
)]
mod uvc;

fn main() {}
