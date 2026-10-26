//! Listing systemd services with `systemctl`.

use std::path::Path;

use burrow_agent::Unit;

use crate::run;

/// Whether systemd runs this system, the way `sd_booted()` decides.
pub fn available() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

/// Every service, or `None` if `systemctl` failed or took too long.
pub fn list() -> Option<Vec<Unit>> {
    let systemctl = run::find("systemctl")?;
    let args = [
        "list-units",
        "--type=service",
        "--all",
        "--plain",
        "--no-legend",
        "--no-pager",
        "--full",
    ];
    run::output(&systemctl, &args, None)
        .ok()
        .map(|text| parse(&text))
}

/// Parses `UNIT LOAD ACTIVE SUB DESCRIPTION` lines.
pub fn parse(text: &str) -> Vec<Unit> {
    text.lines()
        .filter_map(|line| {
            // Older versions mark failed units with a bullet even in plain
            // output.
            let line = line.trim_start().trim_start_matches(['●', '*']);
            let mut rest = line.trim_start();
            let mut field = || {
                let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                rest = tail.trim_start();
                (!word.is_empty()).then(|| word.to_string())
            };
            let unit = Unit {
                name: field()?,
                load: field()?,
                active: field()?,
                sub: field()?,
                description: String::new(),
            };
            Some(Unit {
                description: rest.trim_end().to_string(),
                ..unit
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_reads_columns() {
        let text = "  cron.service    loaded active running Regular background program processing daemon\n\
                    ● nginx.service   loaded failed failed  A high performance web server\n\
                    ghost.service     not-found inactive dead ghost.service\n\
                    short.service loaded active\n";
        let units = parse(text);
        assert_eq!(units.len(), 3);
        assert_eq!(units[0].name, "cron.service");
        assert_eq!(units[0].sub, "running");
        assert_eq!(
            units[0].description,
            "Regular background program processing daemon"
        );
        assert_eq!(units[1].name, "nginx.service");
        assert_eq!(units[1].active, "failed");
        assert_eq!(units[2].load, "not-found");
    }
}
