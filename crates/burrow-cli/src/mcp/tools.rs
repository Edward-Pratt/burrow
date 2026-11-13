//! The tools the bridge offers, as the assistant sees them.

use serde_json::{Value, json};

pub(super) fn tools() -> Value {
    let node = json!({"type": "string", "description": "The node's name or id, from list_nodes"});
    let tool = |name: &str, description: &str, properties: Value, required: &[&str]| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": {"type": "object", "properties": properties, "required": required},
        })
    };
    json!([
        tool(
            "list_nodes",
            "List the user's SSH hosts, tunnels and databases, with their settings and what you may do on each (`allowed`).",
            json!({}),
            &[],
        ),
        tool(
            "run_command",
            "Run a shell command on an SSH host, as `ssh host command` does, and return its exit status, stdout and stderr. Needs exec.",
            json!({
                "node": node,
                "command": {"type": "string"},
                "timeout_seconds": {"type": "integer", "description": "Default 60, at most 3600"},
            }),
            &["node", "command"],
        ),
        tool(
            "query",
            "Run SQL on a Postgres node. Needs sql-read, which runs one statement read-only; sql-write allows writing.",
            json!({
                "node": node,
                "sql": {"type": "string"},
                "max_rows": {"type": "integer", "description": "Rows kept per statement, default 200"},
            }),
            &["node", "sql"],
        ),
        tool(
            "list_schema",
            "List a Postgres database's schemas, tables and columns. Needs sql-read.",
            json!({"node": node}),
            &["node"],
        ),
        tool(
            "list_files",
            "List a folder on an SSH host. Needs files-read.",
            json!({"node": node, "path": {"type": "string", "description": "Empty for the home folder"}}),
            &["node"],
        ),
        tool(
            "read_file",
            "Read a file on an SSH host. Needs files-read.",
            json!({
                "node": node,
                "path": {"type": "string"},
                "max_bytes": {"type": "integer", "description": "Default 1 MiB"},
            }),
            &["node", "path"],
        ),
        tool(
            "write_file",
            "Write a text file on an SSH host. Needs files-write.",
            json!({
                "node": node,
                "path": {"type": "string"},
                "content": {"type": "string"},
                "overwrite": {"type": "boolean", "description": "Replace an existing file"},
            }),
            &["node", "path", "content"],
        ),
        tool(
            "make_dir",
            "Create a folder on an SSH host. Needs files-write.",
            json!({"node": node, "path": {"type": "string"}}),
            &["node", "path"],
        ),
        tool(
            "rename_path",
            "Rename or move a file or folder on an SSH host. Needs files-write.",
            json!({"node": node, "from": {"type": "string"}, "to": {"type": "string"}}),
            &["node", "from", "to"],
        ),
        tool(
            "remove_path",
            "Delete a file on an SSH host, or a folder with everything in it. Needs files-write.",
            json!({"node": node, "path": {"type": "string"}}),
            &["node", "path"],
        ),
        tool(
            "start_tunnel",
            "Start a tunnel and wait until it is up. Needs tunnel.",
            json!({"node": node}),
            &["node"],
        ),
        tool(
            "monitor",
            "Show an SSH host's CPU, memory, disks, network, load and systemd services, with a summary of the last hour while the monitor has been open. For a host Burrow monitors in the background, also the alerts that hold now (failed services, full disks, low memory, not answering). Needs monitor-read, and burrow-agent installed on the host from Burrow.",
            json!({
                "node": node,
                "units": {"type": "string", "enum": ["failed", "all"], "description": "Which services to list, default failed"},
            }),
            &["node"],
        ),
        tool(
            "stop_tunnel",
            "Stop a running tunnel. Needs tunnel.",
            json!({"node": node}),
            &["node"],
        ),
    ])
}
