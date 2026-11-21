//! Running a command for a client, by the role it plays.

use super::Conn;
use crate::api::rpc::Role;
use crate::api::{Command, Response};
use crate::error::Result;

impl Conn {
    pub(super) async fn execute_as(&self, role: Role, command: Command) -> Result<Response> {
        match role {
            Role::Gui | Role::Cli | Role::Mobile => self.core.execute(command).await,
            // Nobody at the bridge can answer a session's questions. The
            // window shows a tunnel's, whoever started it.
            Role::Mcp if matches!(command, Command::StartTunnel { .. }) => {
                self.execute_mcp(command).await
            }
            Role::Mcp => crate::ssh::unattended(self.execute_mcp(command)).await,
        }
    }
}
