//! Listing containers with Docker or Podman, and apps with PM2.

use std::path::{Path, PathBuf};

use burrow_agent::{Container, Containers, Pm2, Pm2App};
use serde_json::Value;

use crate::run;

/// One container a line. Docker and Podman both take this template, and
/// container names can't hold a tab.
const FORMAT: &str = "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}";

/// The containers of the first of Docker and Podman that is installed.
pub fn containers() -> Option<Containers> {
    let (runtime, program) = ["docker", "podman"]
        .into_iter()
        .find_map(|runtime| Some((runtime, run::find(runtime)?)))?;
    let args = ["ps", "--all", "--no-trunc", "--format", FORMAT];
    Some(match run::output(&program, &args, None) {
        Ok(text) => Containers {
            runtime: runtime.into(),
            error: None,
            list: parse_containers(&text),
        },
        Err(error) => Containers {
            runtime: runtime.into(),
            error: Some(error),
            list: Vec::new(),
        },
    })
}

fn parse_containers(text: &str) -> Vec<Container> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let mut field = || fields.next().map(str::to_string);
            let id: String = field()?.chars().take(12).collect();
            Some(Container {
                id,
                name: field()?,
                image: field()?,
                state: field()?.to_ascii_lowercase(),
                status: field()?,
            })
        })
        .collect()
}

/// PM2's apps, when PM2 runs for this user. `pm2 jlist` starts PM2's
/// daemon when it isn't running, and the agent changes nothing, so it
/// only asks a daemon that is already there.
pub fn pm2() -> Option<Pm2> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let pm2_home = std::env::var_os("PM2_HOME").map_or_else(|| home.join(".pm2"), PathBuf::from);
    if !daemon_running(&pm2_home.join("pm2.pid")) {
        return None;
    }
    let Some(program) = find_pm2(&home) else {
        return Some(Pm2 {
            error: Some("PM2 is running, but its pm2 command wasn't found".into()),
            apps: Vec::new(),
        });
    };
    // pm2 is a Node script, and Node installed with nvm or Volta is only on
    // the PATH of a login shell. It sits beside pm2.
    let path = program.parent().map(|dir| {
        let rest = std::env::var("PATH").unwrap_or_default();
        format!("{}:{rest}", dir.display())
    });
    Some(match run::output(&program, &["jlist"], path.as_deref()) {
        Ok(text) => match parse_pm2(&text) {
            Some(apps) => Pm2 { error: None, apps },
            None => Pm2 {
                error: Some("pm2 jlist printed no list".into()),
                apps: Vec::new(),
            },
        },
        Err(error) => Pm2 {
            error: Some(error),
            apps: Vec::new(),
        },
    })
}

fn daemon_running(pid_file: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(pid_file)
        .ok()
        .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
        .filter(|&pid| pid > 0)
    else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    alive || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// `pm2` on `PATH`, or where Node version managers and npm put it.
fn find_pm2(home: &Path) -> Option<PathBuf> {
    if let Some(found) = run::find("pm2") {
        return Some(found);
    }
    let mut candidates = vec![
        home.join(".volta/bin/pm2"),
        home.join(".npm-global/bin/pm2"),
        home.join(".local/bin/pm2"),
        home.join("n/bin/pm2"),
        PathBuf::from("/usr/local/bin/pm2"),
    ];
    // The newest Node nvm has, by its version folder's name.
    if let Ok(versions) = std::fs::read_dir(home.join(".nvm/versions/node")) {
        let mut nodes: Vec<PathBuf> = versions.flatten().map(|entry| entry.path()).collect();
        nodes.sort_by_key(|path| version_key(path));
        candidates.extend(nodes.into_iter().rev().map(|node| node.join("bin/pm2")));
    }
    candidates.into_iter().find(|path| run::is_program(path))
}

/// `v20.11.1` as numbers, so v20 sorts after v9.
fn version_key(path: &Path) -> Vec<u64> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    name.trim_start_matches('v')
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect()
}

/// The apps in what `pm2 jlist` printed. PM2 can print warnings first, on
/// lines that may start with `[` too, so the list is the first line from
/// which a JSON array parses.
fn parse_pm2(text: &str) -> Option<Vec<Pm2App>> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim_start().starts_with('[') {
            let rest = &text[offset..];
            let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
            if let Some(Ok(Value::Array(apps))) = values.next() {
                return Some(apps.iter().map(pm2_app).collect());
            }
        }
        offset += line.len();
    }
    None
}

fn pm2_app(app: &Value) -> Pm2App {
    let env = &app["pm2_env"];
    let number = |value: &Value| value.as_u64().unwrap_or(0);
    let text = |value: &Value| value.as_str().unwrap_or_default().to_string();
    Pm2App {
        id: number(&app["pm_id"]),
        name: text(&app["name"]),
        status: text(&env["status"]),
        cpu: app["monit"]["cpu"].as_f64().unwrap_or(0.0),
        memory: number(&app["monit"]["memory"]),
        restarts: number(&env["restart_time"]),
        since: number(&env["pm_uptime"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_are_read_from_the_template() {
        let text = "0123456789abcdef0123\tweb\tnginx:1.27\trunning\tUp 3 hours\n\
                    fedcba9876543210\tjob\talpine\tExited\tExited (1) 2 minutes ago\n\
                    short line\n";
        let list = parse_containers(text);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "0123456789ab");
        assert_eq!(list[0].name, "web");
        assert_eq!(list[0].status, "Up 3 hours");
        assert_eq!(list[1].state, "exited");
        assert_eq!(list[1].status, "Exited (1) 2 minutes ago");
    }

    #[test]
    fn pm2_apps_keep_only_what_is_shown() {
        let text = r#"[PM2][WARN] Current process list is not synchronized
>>>> In-memory PM2 is out-of-date
[{"pid":42,"name":"api","pm_id":0,"monit":{"memory":52428800,"cpu":1.5},
  "pm2_env":{"status":"online","restart_time":3,"pm_uptime":1700000000000,
  "env":{"DATABASE_PASSWORD":"secret"}}},
 {"name":"worker","pm_id":1,"pm2_env":{"status":"errored"}}]
"#;
        let apps = parse_pm2(text).unwrap();
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[0].name, "api");
        assert_eq!(apps[0].memory, 52428800);
        assert_eq!(apps[0].restarts, 3);
        assert_eq!(apps[0].since, 1700000000000);
        assert_eq!(apps[1].status, "errored");
        let json = serde_json::to_string(&apps).unwrap();
        assert!(!json.contains("secret"));
        assert_eq!(parse_pm2("no list here"), None);
    }

    #[test]
    fn newer_node_versions_sort_later() {
        let mut paths = ["v9.11.2", "v20.11.1", "v18.0.0"].map(PathBuf::from);
        paths.sort_by_key(|path| version_key(path));
        assert_eq!(
            paths.map(|p| p.display().to_string()),
            ["v9.11.2", "v18.0.0", "v20.11.1"]
        );
    }

    #[test]
    fn a_missing_or_dead_pid_is_not_a_daemon() {
        assert!(!daemon_running(Path::new("/nonexistent/pm2.pid")));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pm2.pid");
        std::fs::write(&file, std::process::id().to_string()).unwrap();
        assert!(daemon_running(&file));
        std::fs::write(&file, "0").unwrap();
        assert!(!daemon_running(&file));
    }
}
