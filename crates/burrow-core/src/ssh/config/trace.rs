//! Which files and `Host` lines `ssh -v -G` used for an alias.

use std::path::{Path, PathBuf};

use super::ssh_g;

/// What `ssh -v -G` reports about reading the config for one alias.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Trace {
    /// Every file it read, in order, including ones whose options did not
    /// apply because their `Include` sat inside a `Host` block.
    pub read: Vec<PathBuf>,
    /// Every `Host` pattern whose options applied. `Match` blocks are not
    /// reported at this level.
    pub applied: Vec<Applied>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Applied {
    pub file: PathBuf,
    pub line: u32,
    pub pattern: String,
}

impl Trace {
    /// The `Host` line that names `alias` itself. A wildcard such as `*`
    /// or `*.corp` doesn't count.
    pub fn naming(&self, alias: &str) -> Option<&Applied> {
        self.applied.iter().find(|a| a.pattern == alias)
    }
}

/// Runs `ssh -v -G` for `alias` and reads which files and `Host` lines it
/// used. This is OpenSSH's debug output rather than a stable format, so
/// output with no trace in it at all is an error, never "nothing matched".
pub(crate) async fn trace(alias: &str, config_file: Option<&Path>) -> Result<Trace, String> {
    let output = ssh_g(alias, config_file, true).await?;
    parse_trace(&String::from_utf8_lossy(&output.stderr)).ok_or_else(|| {
        format!("ssh -v printed nothing about reading the OpenSSH config for {alias}")
    })
}

/// `None` when no file was read, which means the output isn't what this
/// expects.
fn parse_trace(stderr: &str) -> Option<Trace> {
    let mut trace = Trace::default();
    for line in stderr.lines() {
        let Some(line) = line.strip_prefix("debug1: ") else {
            continue;
        };
        if let Some(file) = line.strip_prefix("Reading configuration data ") {
            trace.read.push(PathBuf::from(file));
        } else if let Some((place, pattern)) = line.split_once(": Applying options for ")
            && let Some((file, number)) = place.rsplit_once(" line ")
            && let Ok(line) = number.parse()
        {
            trace.applied.push(Applied {
                file: PathBuf::from(file),
                line,
                pattern: pattern.to_string(),
            });
        }
    }
    (!trace.read.is_empty()).then_some(trace)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_name_the_files_read_and_the_lines_applied() {
        let stderr = "\
debug1: OpenSSH_10.6p1, OpenSSL 3.6.5 29 Sep 2026
debug1: Reading configuration data /home/u/.ssh/config
debug1: Reading configuration data /home/u/.ssh/config.d/burrow.conf
debug1: /home/u/.ssh/config.d/burrow.conf line 4: Applying options for web
debug1: /home/u/.ssh/config line 2: Applying options for *.corp
debug1: /home/u/.ssh/config line 9: Applying options for *
Pseudo-terminal will not be allocated because stdin is not a terminal.
";
        let trace = parse_trace(stderr).unwrap();
        assert_eq!(
            trace.read,
            [
                PathBuf::from("/home/u/.ssh/config"),
                PathBuf::from("/home/u/.ssh/config.d/burrow.conf")
            ]
        );
        assert_eq!(trace.applied.len(), 3);
        let web = trace.naming("web").unwrap();
        assert_eq!(web.file, Path::new("/home/u/.ssh/config.d/burrow.conf"));
        assert_eq!(web.line, 4);
        // Wildcards apply to many hosts, so they don't make an alias taken.
        assert!(trace.naming("db.corp").is_none());
        assert!(trace.naming("*").is_some_and(|a| a.line == 9));
    }

    #[test]
    fn a_file_name_with_line_in_it_still_parses() {
        let stderr = "debug1: Reading configuration data /a line b/config\n\
                      debug1: /a line b/config line 3: Applying options for web\n";
        let web = parse_trace(stderr).unwrap().naming("web").cloned().unwrap();
        assert_eq!(web.file, Path::new("/a line b/config"));
        assert_eq!(web.line, 3);
    }

    #[test]
    fn output_without_a_trace_is_refused() {
        assert_eq!(parse_trace(""), None);
        assert_eq!(parse_trace("debug1: OpenSSH_10.6p1\n"), None);
        // An applied line alone, with no file read, isn't trusted either.
        assert_eq!(
            parse_trace("debug1: /c line 1: Applying options for web\n"),
            None
        );
    }

    /// Pins the debug format to the installed OpenSSH, so a change there
    /// fails here instead of making every alias look free.
    #[tokio::test]
    async fn trace_reads_real_ssh_output() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = dir.path().join("config");
        std::fs::write(&config, "Host other\n  Port 2\nHost web db\n  Port 3\n").unwrap();
        let trace = trace("web", Some(&config)).await.unwrap();
        assert_eq!(trace.read, std::slice::from_ref(&config));
        let web = trace.naming("web").unwrap();
        assert_eq!((web.file.as_path(), web.line), (config.as_path(), 3));
        assert!(trace.naming("other").is_none());
    }
}
