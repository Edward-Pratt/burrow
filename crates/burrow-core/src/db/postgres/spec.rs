//! A database node's props, read and checked before anything is dialed:
//! the route, the user and database, the TLS settings, and the target a
//! saved password is pinned to.

use std::path::PathBuf;

use crate::api::{Node, NodeId};
use crate::net::tls::Verify;
use crate::net::{self, Route, Target};

use super::Result;

const DEFAULT_PORT: u16 = 5432;

/// Where Arch, Fedora and Debian (through `/var/run`) put the server's
/// socket.
const DEFAULT_SOCKET_DIR: &str = "/run/postgresql";

/// What a database node's props describe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Spec {
    pub route: Route,
    pub user: String,
    pub database: String,
    pub sslmode: String,
    pub tls: Option<TlsSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TlsSpec {
    pub verify: Verify,
    pub root_cert: Option<PathBuf>,
    /// The name the certificate is checked against: the host as dialed.
    pub name: String,
}

impl Spec {
    /// Reads and checks a database node's props:
    ///
    /// ```toml
    /// via = "<id of an ssh-host node>"   # optional
    /// host = "db.internal"               # TCP; as the SSH host sees it
    /// socket = "/run/postgresql"         # or a socket directory
    /// port = 5432                        # optional
    /// user = "app"
    /// database = "app"                   # optional; the user's name
    /// sslmode = "verify-full"            # optional
    /// sslrootcert = "/path/to/ca.pem"    # optional
    /// ```
    ///
    /// Without `host` the server is reached through its socket. `sslmode`
    /// is `disable`, `require`, `verify-ca` or `verify-full`. It defaults to
    /// `verify-full` for a server reached straight over the network, and to
    /// `disable` through an SSH host or a socket, which are already private.
    pub fn from_node(node: &Node) -> Result<Spec> {
        let props = &node.props;
        let string = |key: &str| -> Result<Option<&str>> {
            match props.get(key) {
                None => Ok(None),
                Some(value) => value
                    .as_str()
                    .map(Some)
                    .ok_or_else(|| format!("{key} must be text")),
            }
        };
        let port = match props.get("port") {
            None => DEFAULT_PORT,
            Some(value) => value
                .as_integer()
                .and_then(|n| u16::try_from(n).ok())
                .filter(|&n| n != 0)
                .ok_or("port must be a port number from 1 to 65535")?,
        };
        let via = string("via")?
            .map(|via| {
                via.parse()
                    .map(NodeId)
                    .map_err(|_| "via must be the id of an SSH host".to_string())
            })
            .transpose()?;
        let target = match (string("host")?, string("socket")?) {
            (Some(_), Some(_)) => return Err("set host or socket, not both".into()),
            (Some(host), None) => Target::tcp("host", host, port)?,
            (None, socket) => {
                let dir = PathBuf::from(socket.unwrap_or(DEFAULT_SOCKET_DIR));
                net::check_path("socket", &dir)?;
                Target::unix(dir.join(format!(".s.PGSQL.{port}")))?
            }
        };
        let user = string("user")?.ok_or("user is missing")?;
        check_name("user", user)?;
        let database = string("database")?.unwrap_or(user);
        check_name("database", database)?;

        let direct_tcp = via.is_none() && matches!(target, Target::Tcp { .. });
        let sslmode =
            string("sslmode")?.unwrap_or(if direct_tcp { "verify-full" } else { "disable" });
        let verify = match sslmode {
            "disable" => None,
            "require" => Some(Verify::Nothing),
            "verify-ca" => Some(Verify::Ca),
            "verify-full" => Some(Verify::Full),
            other => {
                return Err(format!(
                    "sslmode {other:?} is not disable, require, verify-ca or verify-full"
                ));
            }
        };
        let tls = match (verify, &target) {
            (None, _) => None,
            (Some(_), Target::Unix(_)) => {
                return Err("TLS isn't used over a socket: set sslmode = \"disable\"".into());
            }
            (Some(verify), Target::Tcp { host, .. }) => {
                let root_cert = string("sslrootcert")?.map(PathBuf::from);
                if let Some(path) = &root_cert {
                    net::check_path("sslrootcert", path)?;
                }
                Some(TlsSpec {
                    verify,
                    root_cert,
                    name: host.clone(),
                })
            }
        };
        Ok(Spec {
            route: Route { via, target },
            user: user.to_string(),
            database: database.to_string(),
            sslmode: sslmode.to_string(),
            tls,
        })
    }

    /// Where a password goes: everything that decides who could read it.
    /// `host` is the SSH host it goes through. Each part is quoted, so no
    /// user or database name can make one target read as another.
    pub(super) fn password_target(&self, host: Option<&Node>) -> String {
        let mut target = format!(
            "{:?}@{:?}/{:?}",
            self.user,
            self.route.target.to_string(),
            self.database
        );
        if let Some(host) = host {
            let alias = crate::ssh::alias(host);
            target += &format!(" through {alias:?} ({})", host.id);
        }
        target += &format!(", sslmode={:?}", self.sslmode);
        if let Some(path) = self.tls.as_ref().and_then(|t| t.root_cert.as_ref()) {
            target += &format!(", sslrootcert={:?}", path.display().to_string());
        }
        target
    }
}

/// A user or database name: Postgres allows nearly anything, but never a
/// NUL, and control characters only make trouble in prompts.
fn check_name(field: &str, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
        return Err(format!(
            "{field} {name:?} must be 1 to 63 bytes without control characters"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::NodeId;

    fn node(kind: &str, props: &str) -> Node {
        Node {
            id: NodeId::new(),
            parent: None,
            kind: kind.into(),
            name: "db".into(),
            color: None,
            props: toml::from_str(props).unwrap(),
            secrets: Default::default(),
        }
    }

    fn spec(props: &str) -> Result<Spec> {
        Spec::from_node(&node("postgres", props))
    }

    #[test]
    fn a_host_is_reached_over_tcp_with_full_verification() {
        let spec = spec("host = \"db.example\"\nuser = \"app\"").unwrap();
        assert_eq!(spec.route.via, None);
        assert_eq!(spec.route.target.to_string(), "db.example:5432");
        assert_eq!(spec.database, "app");
        assert_eq!(spec.sslmode, "verify-full");
        assert_eq!(spec.tls.unwrap().verify, Verify::Full);
    }

    #[test]
    fn without_a_host_the_socket_is_named_from_its_directory_and_port() {
        let spec = spec("user = \"me\"\nport = 5433").unwrap();
        assert_eq!(
            spec.route.target,
            Target::Unix("/run/postgresql/.s.PGSQL.5433".into())
        );
        assert_eq!(spec.tls, None);
        let spec = self::spec("user = \"me\"\nsocket = \"/tmp/pg\"").unwrap();
        assert_eq!(
            spec.route.target,
            Target::Unix("/tmp/pg/.s.PGSQL.5432".into())
        );
    }

    #[test]
    fn through_an_ssh_host_tls_is_off_unless_asked_for() {
        let via = "via = \"6f1c2a4e-0000-4000-8000-000000000000\"\nuser = \"app\"";
        let spec = spec(&format!("{via}\nhost = \"localhost\"")).unwrap();
        assert!(spec.route.via.is_some());
        assert_eq!(spec.tls, None);
        let spec = self::spec(&format!(
            "{via}\nhost = \"db\"\nsslmode = \"verify-ca\"\nsslrootcert = \"/ca.pem\""
        ))
        .unwrap();
        assert_eq!(spec.tls.unwrap().verify, Verify::Ca);
    }

    #[test]
    fn bad_props_are_refused_before_anything_is_dialed() {
        for (props, why) in [
            ("host = \"db\"", "user is missing"),
            ("host = \"db\"\nsocket = \"/run\"\nuser = \"a\"", "not both"),
            (
                "host = \"-oProxyCommand=x\"\nuser = \"a\"",
                "not a host name",
            ),
            ("socket = \"run/pg\"\nuser = \"a\"", "absolute"),
            ("socket = \"/run/../home\"\nuser = \"a\"", "\"..\""),
            ("user = \"a\"\nport = 0", "port must be"),
            ("user = \"a\"\nsslmode = \"require\"", "over a socket"),
            (
                "host = \"db\"\nuser = \"a\"\nsslmode = \"prefer\"",
                "is not disable",
            ),
            ("host = \"db\"\nuser = \"a\\nb\"", "control characters"),
            (
                "host = \"db\"\nuser = \"a\"\nvia = \"web\"",
                "id of an SSH host",
            ),
            (
                "host = \"db\"\nuser = \"a\"\nsslrootcert = \"ca.pem\"",
                "not an absolute path",
            ),
        ] {
            let error = spec(props).unwrap_err();
            assert!(error.contains(why), "{props:?}: {error}");
        }
    }

    #[test]
    fn the_password_target_covers_route_and_tls() {
        let direct = spec("host = \"db\"\nuser = \"app\"").unwrap();
        assert_eq!(
            direct.password_target(None),
            r#""app"@"db:5432"/"app", sslmode="verify-full""#
        );
        let plain = spec("host = \"db\"\nuser = \"app\"\nsslmode = \"disable\"").unwrap();
        assert_ne!(plain.password_target(None), direct.password_target(None));
        let host = node("ssh-host", "alias = \"prod-web\"");
        let through = plain.password_target(Some(&host));
        assert!(
            through.contains(&format!("through \"prod-web\" ({})", host.id)),
            "{through}"
        );
    }

    #[test]
    fn a_name_cant_make_one_password_target_read_as_another() {
        // Unquoted, both would read
        // `app@db:5432/app, sslmode=verify-full, sslrootcert=/a, sslmode=require`.
        let honest =
            spec("host = \"db\"\nuser = \"app\"\nsslrootcert = \"/a, sslmode=require\"").unwrap();
        let forged = spec(
            "host = \"db\"\nuser = \"app\"\nsslmode = \"require\"\n\
             database = \"app, sslmode=verify-full, sslrootcert=/a\"",
        )
        .unwrap();
        assert_ne!(honest.password_target(None), forged.password_target(None));
    }
}
