//! Inactive foundations for issue #713's stable session host.
//!
//! These state machines do not launch hosts, authenticate connections, or
//! preserve sessions yet. Runtime integration must retain one host-owned
//! `core::Session`, including complete logical input acts and delayed writes.
#![allow(dead_code)]
mod channel;
#[cfg(unix)]
mod child;
mod control;
mod coordinator;
mod ledger;
#[cfg(unix)]
mod owned;
#[cfg(unix)]
mod reaper;
mod result;
mod strings;
mod subscription;
mod terminal;
#[cfg(unix)]
mod unix;
mod wire;
