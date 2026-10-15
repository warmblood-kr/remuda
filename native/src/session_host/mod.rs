//! Inactive foundations for issue #713's stable session host.
//!
//! These state machines do not launch hosts, authenticate connections, or
//! preserve sessions yet. Runtime integration must retain one host-owned
//! `core::Session`, including complete logical input acts and delayed writes.
#![allow(dead_code)]
mod control;
mod ledger;
mod subscription;
mod wire;
