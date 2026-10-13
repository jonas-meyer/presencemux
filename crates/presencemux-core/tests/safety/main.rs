//! The safety rules of the controller, with one module for each rule.

mod boot;
mod device_fault;
mod effects;
mod exceeded;
mod host;
mod lapsed_lease;
mod lost;
mod observation;
mod policy;
mod request;
mod resume;
mod stream_timeout;
mod support;
