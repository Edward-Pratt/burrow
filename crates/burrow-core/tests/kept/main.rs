//! Kept terminals outlive the client that opened them, keep their output
//! meanwhile, and let a client attach to them again. Against a throwaway
//! `sshd`, over the socket, as the phone uses them.

mod attach;
#[path = "../common/mod.rs"]
mod common;
mod helpers;
mod lifecycle;
mod phone;
