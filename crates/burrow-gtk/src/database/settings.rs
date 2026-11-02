//! A database node's settings, as the dialog reads and writes them.

use burrow_core::NodeId;

/// What the database dialog edits. Other props, such as `sslrootcert` when
/// TLS is off, are kept as they were.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub via: Option<NodeId>,
    /// A TCP host, or `None` for the server's socket.
    pub host: Option<String>,
    /// The socket's folder, when it isn't the default.
    pub socket: Option<String>,
    pub port: u16,
    pub user: String,
    /// `None` uses the user's name.
    pub database: Option<String>,
    /// `None` leaves it to the default for the route.
    pub sslmode: Option<String>,
    pub sslrootcert: Option<String>,
}

/// The `sslmode` choices offered, with what each one is called.
pub(super) const SSLMODES: [(Option<&str>, &str); 5] = [
    (None, "Default for the route"),
    (Some("disable"), "Off"),
    (Some("require"), "Encrypt, but don't check the server"),
    (
        Some("verify-ca"),
        "Check the CA only (needs a root certificate)",
    ),
    (Some("verify-full"), "Check the server"),
];

impl Settings {
    /// Reads what it can from a database's props, falling back to the
    /// dialog's defaults.
    pub fn from_props(props: &toml::Table) -> Settings {
        let string = |key: &str| {
            props
                .get(key)
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        Settings {
            via: string("via").and_then(|s| s.parse().ok()).map(NodeId),
            host: string("host"),
            socket: string("socket"),
            port: props
                .get("port")
                .and_then(|v| v.as_integer())
                .and_then(|n| u16::try_from(n).ok())
                .filter(|&n| n != 0)
                .unwrap_or(5432),
            user: string("user").unwrap_or_default(),
            database: string("database"),
            sslmode: string("sslmode"),
            sslrootcert: string("sslrootcert"),
        }
    }

    /// `base` with these settings written over it.
    pub fn apply(&self, base: &toml::Table) -> toml::Table {
        let mut props = base.clone();
        let mut set = |key: &str, value: Option<toml::Value>| match value {
            Some(value) => {
                props.insert(key.into(), value);
            }
            None => {
                props.remove(key);
            }
        };
        set("via", self.via.map(|id| id.to_string().into()));
        set("host", self.host.clone().map(Into::into));
        set("socket", self.socket.clone().map(Into::into));
        set("port", Some(i64::from(self.port).into()));
        set("user", Some(self.user.clone().into()));
        set("database", self.database.clone().map(Into::into));
        set("sslmode", self.sslmode.clone().map(Into::into));
        set("sslrootcert", self.sslrootcert.clone().map(Into::into));
        props
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_read_back_what_they_wrote_and_keep_other_props() {
        let mut base = toml::Table::new();
        base.insert("extra".into(), "kept".into());
        base.insert("host".into(), "old".into());
        let settings = Settings {
            via: Some(NodeId::new()),
            host: None,
            socket: Some("/tmp/pg".into()),
            port: 5433,
            user: "app".into(),
            database: None,
            sslmode: None,
            sslrootcert: None,
        };
        let props = settings.apply(&base);
        assert_eq!(props["extra"].as_str(), Some("kept"));
        assert!(!props.contains_key("host"), "a socket drops the old host");
        assert!(!props.contains_key("database"));
        assert_eq!(Settings::from_props(&props), settings);
    }
}
