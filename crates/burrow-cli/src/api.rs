//! `burrow call` and `burrow events`: the daemon's API as it is, in JSON.

use std::process::ExitCode;

use burrow_api::Client;
use burrow_core::Command;
use tokio::sync::broadcast::error::RecvError;

use crate::Result;

pub async fn call(client: &Client, json: &str) -> Result<ExitCode> {
    let command: Command = serde_json::from_str(json)?;
    let response = client.execute(command).await?;
    println!("{}", serde_json::to_string_pretty(&response)?);
    Ok(ExitCode::SUCCESS)
}

pub async fn events(client: &Client) -> Result<ExitCode> {
    let mut events = client.subscribe();
    loop {
        match events.recv().await {
            Ok(event) => println!("{}", serde_json::to_string(&event)?),
            Err(RecvError::Lagged(_)) => eprintln!("burrow: missed some events"),
            Err(RecvError::Closed) => return Err("lost the connection to burrowd".into()),
        }
    }
}
