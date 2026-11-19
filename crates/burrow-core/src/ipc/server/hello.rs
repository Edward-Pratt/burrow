//! A client's `initialize`: the API version it speaks and its role.

use serde_json::Value;

use super::{Conn, Failure};
use crate::api::API_VERSION;
use crate::ipc::events::forward_events;
use crate::ipc::{BUILD, Initialize, SERVER_ERROR, ServerInfo};

impl Conn {
    pub(super) fn initialize(&self, hello: Initialize) -> Result<Value, Failure> {
        if hello.api != API_VERSION {
            return Err((
                SERVER_ERROR,
                format!(
                    "this client needs API {}, but burrowd {BUILD} has API {API_VERSION}",
                    hello.api
                ),
            ));
        }
        if self.role.set(hello.role).is_err() {
            return Err((SERVER_ERROR, "already initialized".to_string()));
        }
        tokio::spawn(forward_events(
            self.core.subscribe(),
            self.control.clone(),
            hello.role,
            self.owned.clone(),
            self.alive.clone(),
        ));
        let info = ServerInfo {
            api: API_VERSION,
            build: BUILD.to_string(),
            addons_dir: self.core.addons_dir(),
        };
        Ok(serde_json::to_value(info).unwrap_or_default())
    }
}
