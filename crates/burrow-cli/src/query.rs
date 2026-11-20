//! `burrow query`: SQL on a database node, printed as `COPY` would.

use std::io::Write;
use std::process::ExitCode;

use burrow_api::Client;
use burrow_core::{Command, Response, StatementResult};

use crate::nodes::find;
use crate::sessions::{answer_prompts, connected, session};
use crate::text::clean;
use crate::{Result, unexpected};

pub async fn run(
    client: &Client,
    node: &str,
    sql: String,
    write: bool,
    limit: Option<u32>,
    json: bool,
) -> Result<ExitCode> {
    let node = find(client, node).await?;
    let mut events = client.subscribe();
    let prompts = answer_prompts(client, node.id);
    let session = session(client, Command::OpenDatabase { node: node.id }).await?;
    let ready = connected(client, &mut events, session).await;
    prompts.abort();
    ready?;
    let result = async {
        if write {
            let command = Command::SetReadOnly {
                session,
                read_only: false,
            };
            client.execute(command).await?;
        }
        let command = Command::RunQuery {
            session,
            sql,
            limit,
        };
        client.execute(command).await
    }
    .await;
    let _ = client.execute(Command::CloseSession { session }).await;
    let result = match result? {
        Response::Query { result } => result,
        other => return Err(unexpected(&other)),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let mut out = std::io::stdout().lock();
        for statement in &result.statements {
            print_statement(&mut out, statement)?;
        }
    }
    if result.stopped {
        eprintln!("burrow: stopped at the row limit; use --limit for more");
    }
    match result.error {
        Some(error) => {
            eprintln!("ERROR: {}", clean(&error.message));
            for (label, text) in [("DETAIL", &error.detail), ("HINT", &error.hint)] {
                if let Some(text) = text {
                    eprintln!("{label}: {}", clean(text));
                }
            }
            Ok(ExitCode::FAILURE)
        }
        None => Ok(ExitCode::SUCCESS),
    }
}

/// Rows as tab-separated values, as `COPY … TO STDOUT` writes them:
/// NULL is `\N`, and tabs, newlines and backslashes in values are escaped.
fn print_statement(out: &mut impl Write, statement: &StatementResult) -> Result<()> {
    if statement.columns.is_empty() {
        eprintln!("{} rows", statement.count);
        return Ok(());
    }
    let header: Vec<String> = statement.columns.iter().map(|c| field(Some(c))).collect();
    writeln!(out, "{}", header.join("\t"))?;
    for row in &statement.rows {
        let row: Vec<String> = row.iter().map(|v| field(v.as_deref())).collect();
        writeln!(out, "{}", row.join("\t"))?;
    }
    if statement.truncated {
        eprintln!(
            "burrow: only the first {} rows are shown",
            statement.rows.len()
        );
    }
    Ok(())
}

fn field(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "\\N".to_string();
    };
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => out.extend(c.escape_unicode()),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_print_as_copy_does() {
        assert_eq!(field(None), "\\N");
        assert_eq!(field(Some("a\tb\nc\\d")), "a\\tb\\nc\\\\d");
        assert_eq!(field(Some("\u{1b}[31m")), "\\u{1b}[31m");
    }
}
