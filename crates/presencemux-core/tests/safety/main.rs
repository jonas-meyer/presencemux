//! The safety rules of the controller, with one module for each rule.

mod boot;
mod device_fault;
mod effects;
mod host;
mod lapsed_lease;
mod observation;
mod policy;
mod request;
mod support;
