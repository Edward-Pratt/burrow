//! The core as a user service. The daemon itself lives in
//! `burrow_core::daemon`, so clients can start the same build when there is
//! no service.

fn main() {
    burrow_core::daemon::main();
}
