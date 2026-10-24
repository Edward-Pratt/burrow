//! An example Burrow addon: plain-text notes in the sidebar.
//!
//! It speaks the addon protocol by hand, one JSON-RPC 2.0 message per line
//! on stdin and stdout, to show that an addon needs nothing from Burrow but
//! a JSON library. See `docs/API.md` for the protocol.
//!
//! It is also the fixture for the addon integration tests, which is what
//! the undeclared `crash`, `flood`, `env`, `trespass`, `redraw`, `touch`
//! and `events` actions are for.
//! Burrow only lets those through from buttons in a note's own view, and
//! the view never shows them.

use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

const KIND: &str = "note";

struct Addon {
    lines: io::Lines<io::StdinLock<'static>>,
    next_id: u64,
    /// The `type` of every event received, for the tests.
    events: Vec<String>,
}

fn main() {
    let mut addon = Addon {
        lines: io::stdin().lock().lines(),
        next_id: 0,
        events: Vec::new(),
    };
    // Burrow closes stdin when it is done with us.
    while let Some(message) = addon.receive() {
        let Some(method) = message["method"].as_str() else {
            continue;
        };
        let params = &message["params"];
        let result = match method {
            "initialize" => Ok(json!({})),
            "open_view" => Ok(view(&params["node"])),
            "invoke" => addon.invoke(params),
            // `event` and `resync` notifications: nothing to keep up to date.
            _ => continue,
        };
        if let Some(id) = message.get("id") {
            send(&match result {
                Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                Err(error) => json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32000, "message": error},
                }),
            });
        }
    }
}

impl Addon {
    fn receive(&mut self) -> Option<Value> {
        loop {
            let line = self.lines.next()?.ok()?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message["method"] == "event" {
                let kind = message["params"]["type"].as_str().unwrap_or_default();
                self.events.push(kind.to_string());
            }
            return Some(message);
        }
    }

    /// Sends a command to Burrow and waits for its answer.
    fn execute(&mut self, command: Value) -> Result<Value, String> {
        self.request("execute", command)
    }

    /// Sends a request and waits for its answer. Notifications that arrive
    /// meanwhile are events, which this addon can skip.
    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        loop {
            let message = self.receive().ok_or("Burrow went away")?;
            if message.get("method").is_none() && message["id"] == id {
                return match message.get("error") {
                    Some(error) => Err(error["message"].as_str().unwrap_or("error").to_string()),
                    None => Ok(message["result"].clone()),
                };
            }
        }
    }

    fn invoke(&mut self, params: &Value) -> Result<Value, String> {
        let node = &params["node"];
        let message = match params["action"].as_str().unwrap_or_default() {
            "new-note" => {
                self.execute(json!({"type": "create_node", "kind": KIND, "name": "New note"}))?;
                "Added \"New note\" to the sidebar".to_string()
            }
            "count" => {
                let nodes = self.execute(json!({"type": "list_nodes"}))?;
                let count = nodes["nodes"].as_array().map_or(0, |nodes| {
                    nodes.iter().filter(|n| n["kind"] == KIND).count()
                });
                format!("{count} note{}", if count == 1 { "" } else { "s" })
            }
            "describe" => format!(
                "{} connects to the alias {}",
                node["name"].as_str().unwrap_or_default(),
                node["props"]["alias"]
                    .as_str()
                    .or(node["name"].as_str())
                    .unwrap_or_default()
            ),
            "save" => {
                let text = params["values"]["text"].as_str().unwrap_or_default();
                let mut props = node["props"].clone();
                props["text"] = json!(text);
                let updated = self.execute(json!({
                    "type": "update_node",
                    "id": node["id"],
                    "patch": {"props": props},
                }))?;
                let node = &updated["node"];
                send(&json!({
                    "jsonrpc": "2.0",
                    "method": "view_changed",
                    "params": {"node": node["id"], "view": view(node)},
                }));
                "Saved".to_string()
            }
            "crash" => std::process::exit(3),
            "flood" => {
                let line = "x".repeat(2 << 20);
                let mut out = io::stdout().lock();
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
                "Flooded".to_string()
            }
            "env" => std::env::var("SSH_AUTH_SOCK").unwrap_or_default(),
            "touch" => {
                std::fs::write("scratch", "written by the notes addon\n")
                    .map_err(|e| e.to_string())?;
                std::env::current_dir()
                    .map_err(|e| e.to_string())?
                    .display()
                    .to_string()
            }
            "events" => self.events.join(","),
            "redraw" => {
                // As a request rather than a notification, to see the answer.
                let target = params["values"]["target"].as_str().unwrap_or_default();
                let view = json!({"blocks": []});
                self.request("view_changed", json!({"node": target, "view": view}))?;
                "Redrawn".to_string()
            }
            "trespass" => {
                let target = params["values"]["target"].as_str().unwrap_or_default();
                self.execute(json!({
                    "type": "update_node",
                    "id": target,
                    "patch": {"name": "taken"},
                }))?;
                "Renamed".to_string()
            }
            other => return Err(format!("unknown action {other:?}")),
        };
        Ok(json!({ "message": message }))
    }
}

fn view(node: &Value) -> Value {
    let text = node["props"]["text"].as_str().unwrap_or_default();
    json!({"blocks": [
        {"type": "heading", "text": node["name"]},
        {"type": "text", "text": if text.is_empty() { "This note is empty." } else { text }},
        {"type": "form", "action": "save", "submit": "Save", "fields": [
            {"name": "text", "label": "Text", "input": "multiline", "value": text},
        ]},
        {"type": "buttons", "buttons": [{"action": "count", "label": "Count notes"}]},
    ]})
}

fn send(message: &Value) {
    let mut out = io::stdout().lock();
    // A write error means Burrow is gone; the next read ends the loop.
    let _ = writeln!(out, "{message}");
    let _ = out.flush();
}
