//! The command lines that act on a server's services and follow their
//! logs. Names reach here from clients, and before that from the server,
//! so this is the one place they are checked and quoted.

use crate::api::{Service, ServiceAction};

type Result<T> = std::result::Result<T, String>;

/// How many lines of log a logs terminal shows before following.
const LOG_LINES: u32 = 200;

/// The line that runs `action` on `service`. A systemd unit's action needs
/// root: as anyone else it goes through sudo, which may not ask for a
/// password unless `ask` is set. Without `ask` sudo's messages are in
/// English, so a missing password can be told apart.
pub(crate) fn control(service: &Service, action: ServiceAction, ask: bool) -> Result<String> {
    if !service.actions().contains(&action) {
        return Err(format!(
            "{} can't be done to {}",
            action.verb(),
            service.name()
        ));
    }
    let verb = action.verb();
    Ok(match service {
        Service::Unit { name } => {
            let name = quoted(name)?;
            let systemctl = format!("systemctl {verb} -- {name}");
            if ask {
                // Shows how it went, since the terminal stays open.
                format!(
                    "{}; systemctl status --no-pager --lines=20 -- {name}",
                    as_root(&systemctl, "sudo")
                )
            } else {
                as_root(&systemctl, "LC_ALL=C sudo -n")
            }
        }
        Service::Container { runtime, name } => {
            format!("{} {verb} -- {}", runtime_program(runtime)?, quoted(name)?)
        }
        Service::Pm2App { pm2, name } => format!("{} {verb} {}", pm2_program(pm2)?, quoted(name)?),
    })
}

/// The line that follows `service`'s log.
pub(crate) fn logs(service: &Service) -> Result<String> {
    Ok(match service {
        Service::Unit { name } => {
            format!(
                "journalctl --lines={LOG_LINES} --follow --unit={}",
                quoted(name)?
            )
        }
        Service::Container { runtime, name } => format!(
            "{} logs --follow --tail {LOG_LINES} -- {}",
            runtime_program(runtime)?,
            quoted(name)?
        ),
        Service::Pm2App { pm2, name } => format!(
            "{} logs --lines {LOG_LINES} {}",
            pm2_program(pm2)?,
            quoted(name)?
        ),
    })
}

/// Whether a failed [`control`] line failed because sudo wanted a password.
pub(crate) fn needs_password(status: Option<u32>, stderr: &str) -> bool {
    status == Some(1)
        && (stderr.contains("a password is required")
            || stderr.contains("a terminal is required to read the password"))
}

/// `command` run as root: as it is for root, through `sudo` otherwise.
fn as_root(command: &str, sudo: &str) -> String {
    format!("if [ \"$(id -u)\" -eq 0 ]; then {command}; else {sudo} {command}; fi")
}

fn runtime_program(runtime: &str) -> Result<&'static str> {
    match runtime {
        "docker" => Ok("docker"),
        "podman" => Ok("podman"),
        _ => Err(format!("{runtime:?} is not docker or podman")),
    }
}

/// `pm2` where the monitor found it, run with its own folder first on
/// `PATH`, so the Node it needs is found too.
fn pm2_program(path: &str) -> Result<String> {
    let path = path.strip_suffix('/').unwrap_or(path);
    let Some((folder, _)) = path.rsplit_once('/').filter(|_| path.starts_with('/')) else {
        return Err(format!("{path:?} is not where pm2 is"));
    };
    let folder = if folder.is_empty() { "/" } else { folder };
    Ok(format!(
        "PATH={}:\"$PATH\" {}",
        quoted(folder)?,
        quoted(path)?
    ))
}

/// `text` in single quotes, for the remote shell. Text that can't be quoted
/// that way, or that a program would take for an option, is refused.
fn quoted(text: &str) -> Result<String> {
    let fine = !text.is_empty()
        && text.len() <= 256
        && !text.starts_with('-')
        && !text.chars().any(|c| c == '\'' || c.is_control());
    if fine {
        Ok(format!("'{text}'"))
    } else {
        Err(format!(
            "{text:?} is not a name Burrow will pass to a command"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(name: &str) -> Service {
        Service::Unit { name: name.into() }
    }

    #[test]
    fn unit_actions_go_through_sudo() {
        let line = control(&unit("nginx.service"), ServiceAction::Restart, false).unwrap();
        assert_eq!(
            line,
            "if [ \"$(id -u)\" -eq 0 ]; then systemctl restart -- 'nginx.service'; \
             else LC_ALL=C sudo -n systemctl restart -- 'nginx.service'; fi"
        );
        let asked = control(&unit("a@b.service"), ServiceAction::Enable, true).unwrap();
        assert!(asked.contains("else sudo systemctl enable -- 'a@b.service'; fi"));
        assert!(asked.ends_with("systemctl status --no-pager --lines=20 -- 'a@b.service'"));
    }

    #[test]
    fn containers_and_apps_take_their_own_actions() {
        let container = Service::Container {
            runtime: "podman".into(),
            name: "web".into(),
        };
        assert_eq!(
            control(&container, ServiceAction::Stop, false).unwrap(),
            "podman stop -- 'web'"
        );
        assert!(control(&container, ServiceAction::Enable, false).is_err());
        let app = Service::Pm2App {
            pm2: "/home/me/.nvm/versions/node/v20.1.0/bin/pm2".into(),
            name: "api server".into(),
        };
        assert_eq!(
            control(&app, ServiceAction::Reload, false).unwrap(),
            "PATH='/home/me/.nvm/versions/node/v20.1.0/bin':\"$PATH\" \
             '/home/me/.nvm/versions/node/v20.1.0/bin/pm2' reload 'api server'"
        );
        assert_eq!(
            logs(&container).unwrap(),
            "podman logs --follow --tail 200 -- 'web'"
        );
        assert_eq!(
            logs(&unit("cron.service")).unwrap(),
            "journalctl --lines=200 --follow --unit='cron.service'"
        );
    }

    #[test]
    fn names_that_would_escape_are_refused() {
        for name in ["", "x'; rm -rf ~; '", "-H", "a\nb", &"x".repeat(257)] {
            assert!(
                control(&unit(name), ServiceAction::Start, false).is_err(),
                "{name:?}"
            );
        }
        let other = Service::Container {
            runtime: "sh -c".into(),
            name: "x".into(),
        };
        assert!(logs(&other).is_err());
        for pm2 in ["pm2", "relative/pm2", "/bin/pm2'"] {
            let app = Service::Pm2App {
                pm2: pm2.into(),
                name: "x".into(),
            };
            assert!(logs(&app).is_err(), "{pm2:?}");
        }
    }

    #[test]
    fn a_missing_password_is_told_apart() {
        assert!(needs_password(Some(1), "sudo: a password is required\n"));
        assert!(!needs_password(
            Some(1),
            "Failed to restart x.service: Unit not found."
        ));
        assert!(!needs_password(Some(0), "sudo: a password is required"));
    }
}
