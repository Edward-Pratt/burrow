//! Monitor sessions: burrow-agent run over SSH, with what it reports
//! passed on as events and kept for about an hour.
//!
//! The agent runs on the server, so its output is checked like anything
//! else from there. A line that is too long or doesn't parse ends the
//! session, names lose control characters, and lists and numbers are kept
//! within bounds before anything is shown or stored.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_agent::{Message, PROTO, Sample, Unit};
use russh::ChannelMsg;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::agent::{self, printable, quote};
use super::{Attached, Input, Prompter};
use crate::api::{Event, MonitorHistory, Node, SessionState};
use crate::engine::Core;

/// Seconds between samples.
const INTERVAL: u64 = 2;

/// About an hour of samples.
const HISTORY: usize = 3600 / INTERVAL as usize;

/// The longest line the agent may send. A few thousand services fit.
const MAX_LINE: usize = 1 << 20;

/// The most a sample may take once cleaned, as JSON. A real one is about
/// a kilobyte; this bounds the history a server can make the daemon keep
/// at [`HISTORY`] times this, about 29 MiB, so it still fits one reply.
const MAX_SAMPLE: usize = 16 << 10;

// A full history, with the latest units (no longer than a line), fits one
// reply with room to spare.
const _: () = assert!(HISTORY * MAX_SAMPLE + MAX_LINE < crate::ipc::MAX_LINE / 2);

/// How long the agent gets to say hello, and then between lines.
const HELLO_TIMEOUT: Duration = Duration::from_secs(15);
const QUIET_TIMEOUT: Duration = Duration::from_secs(30);

/// Limits on what is kept of each sample and unit list.
const MAX_TEXT: usize = 256;
const MAX_ITEMS: usize = 256;
const MAX_UNITS: usize = 4096;

/// What a running monitor has seen.
#[derive(Default)]
pub(crate) struct Monitor {
    history: Mutex<History>,
}

#[derive(Default)]
struct History {
    systemd: bool,
    samples: VecDeque<Sample>,
    units: Option<Vec<Unit>>,
}

impl Monitor {
    pub fn history(&self) -> MonitorHistory {
        let history = self.history.lock().unwrap();
        MonitorHistory {
            systemd: history.systemd,
            samples: history.samples.iter().cloned().collect(),
            units: history.units.clone(),
        }
    }
}

/// Runs the agent until a client closes the session, the agent stops, or
/// the connection ends. Without `install`, an agent that isn't there yet
/// is an error rather than an upload.
pub(super) async fn run(
    core: &Core,
    node: &Node,
    mut input: mpsc::UnboundedReceiver<Input>,
    prompter: &Prompter,
    install: bool,
) -> Result<(), String> {
    let step = |step: String| {
        core.emit(Event::SessionChanged {
            session: prompter.session,
            node: node.id,
            state: SessionState::Connecting { step },
        })
    };
    let setup = async {
        let connection = core.connections().get(core, node, prompter, &step).await?;
        step("Checking burrow-agent".into());
        let path = agent::ensure(core, node, &connection, prompter, install).await?;
        step("Starting burrow-agent".into());
        let channel = connection
            .handle()
            .channel_open_session()
            .await
            .map_err(text)?;
        let command = format!("exec {} stream --interval {INTERVAL}", quote(&path));
        channel.exec(true, command).await.map_err(text)?;
        Ok::<_, String>((connection, channel))
    };
    let closed = async {
        while let Some(message) = input.recv().await {
            if matches!(message, Input::Close) {
                return;
            }
        }
    };
    let (connection, mut channel) = tokio::select! {
        result = setup => result?,
        () = closed => return Ok(()),
    };

    let monitor = Arc::new(Monitor::default());
    let mut reader = Reader::default();
    let mut said_hello = false;
    let mut deadline = Instant::now() + HELLO_TIMEOUT;
    loop {
        tokio::select! {
            message = channel.wait() => match message {
                Some(ChannelMsg::Data { data }) => {
                    for line in reader.push(&data)? {
                        let message = parse(&line)?;
                        deadline = Instant::now() + QUIET_TIMEOUT;
                        match (message, said_hello) {
                            (Message::Hello { proto, systemd, .. }, false) => {
                                if proto != PROTO {
                                    return Err(format!(
                                        "burrow-agent on the server speaks version {proto} \
                                         and Burrow speaks {PROTO}"
                                    ));
                                }
                                said_hello = true;
                                monitor.history.lock().unwrap().systemd = systemd;
                                core.sessions()
                                    .attach(prompter.session, Attached::Monitor(monitor.clone()));
                                core.emit(Event::SessionChanged {
                                    session: prompter.session,
                                    node: node.id,
                                    state: SessionState::Connected,
                                });
                            }
                            (Message::Hello { .. }, true) | (_, false) => {
                                return Err("burrow-agent sent something unexpected".into());
                            }
                            (Message::Sample(sample), true) => {
                                let sample = clean_sample(sample)?;
                                let mut history = monitor.history.lock().unwrap();
                                if history.samples.len() == HISTORY {
                                    history.samples.pop_front();
                                }
                                history.samples.push_back(sample.clone());
                                drop(history);
                                core.emit(Event::MonitorSample {
                                    session: prompter.session,
                                    node: node.id,
                                    sample,
                                });
                            }
                            (Message::Units { units }, true) => {
                                let units = clean_units(units);
                                let mut history = monitor.history.lock().unwrap();
                                if history.units.as_ref() != Some(&units) {
                                    history.units = Some(units.clone());
                                    drop(history);
                                    core.emit(Event::MonitorUnits {
                                        session: prompter.session,
                                        node: node.id,
                                        units,
                                    });
                                }
                            }
                        }
                    }
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => reader.stderr(&data),
                Some(ChannelMsg::Close) | Some(ChannelMsg::Eof) => {
                    return Err(reader.stopped());
                }
                // The channel went without closing: the connection ended.
                None => return Err(connection.closed().await),
                Some(_) => {}
            },
            () = tokio::time::sleep_until(deadline) => {
                let _ = channel.close().await;
                return Err(if said_hello {
                    "burrow-agent stopped reporting".into()
                } else {
                    "burrow-agent did not start".into()
                });
            }
            message = input.recv() => if matches!(message, Some(Input::Close) | None) {
                // Closing stdin is how the agent knows to stop.
                let _ = channel.eof().await;
                let _ = channel.close().await;
                return Ok(());
            },
        }
    }
}

/// Splits the agent's output into lines, and keeps the end of what it
/// wrote to stderr for when it stops.
#[derive(Default)]
struct Reader {
    partial: Vec<u8>,
    stderr: Vec<u8>,
}

impl Reader {
    fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>, String> {
        self.partial.extend_from_slice(data);
        let mut lines = Vec::new();
        while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.partial.drain(..=end).collect();
            line.pop();
            lines.push(line);
        }
        if self.partial.len() > MAX_LINE {
            return Err("burrow-agent sent a line that is too long".into());
        }
        Ok(lines)
    }

    fn stderr(&mut self, data: &[u8]) {
        const KEEP: usize = 1024;
        self.stderr.extend_from_slice(data);
        let extra = self.stderr.len().saturating_sub(KEEP);
        self.stderr.drain(..extra);
    }

    fn stopped(&self) -> String {
        let said = printable(String::from_utf8_lossy(&self.stderr).trim());
        if said.is_empty() {
            "burrow-agent stopped".into()
        } else {
            format!("burrow-agent stopped: {said}")
        }
    }
}

fn parse(line: &[u8]) -> Result<Message, String> {
    serde_json::from_slice(line)
        .map_err(|e| format!("burrow-agent sent something Burrow can't read: {e}"))
}

fn clean_text(text: &mut String) {
    *text = printable(text).chars().take(MAX_TEXT).collect();
}

/// A negative or endless number, which no stat can be, becomes 0.
fn clean_number(number: &mut f64) {
    if !number.is_finite() || *number < 0.0 {
        *number = 0.0;
    }
}

/// `sample` in bounds, or an error if it is too big to keep.
fn clean_sample(mut sample: Sample) -> Result<Sample, String> {
    clean_number(&mut sample.uptime);
    sample.load.iter_mut().for_each(clean_number);
    clean_number(&mut sample.cpu.percent);
    sample.cpu.percent = sample.cpu.percent.min(100.0);
    sample.disks.truncate(MAX_ITEMS);
    for disk in &mut sample.disks {
        clean_text(&mut disk.mount);
        clean_text(&mut disk.fs);
        disk.available = disk.available.min(disk.total);
    }
    sample.net.truncate(MAX_ITEMS);
    for interface in &mut sample.net {
        clean_text(&mut interface.name);
        clean_number(&mut interface.rx);
        clean_number(&mut interface.tx);
    }
    let memory = &mut sample.memory;
    memory.available = memory.available.min(memory.total);
    memory.swap_free = memory.swap_free.min(memory.swap_total);
    let size = serde_json::to_vec(&sample).map_or(usize::MAX, |json| json.len());
    if size > MAX_SAMPLE {
        return Err(format!(
            "burrow-agent sent a sample of {size} bytes, over the limit of {MAX_SAMPLE}"
        ));
    }
    Ok(sample)
}

fn clean_units(mut units: Vec<Unit>) -> Vec<Unit> {
    units.truncate(MAX_UNITS);
    for unit in &mut units {
        for field in [
            &mut unit.name,
            &mut unit.load,
            &mut unit.active,
            &mut unit.sub,
            &mut unit.description,
        ] {
            clean_text(field);
        }
    }
    units
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use burrow_agent::{Disk, Interface};

    #[test]
    fn reader_splits_lines_across_chunks() {
        let mut reader = Reader::default();
        assert!(reader.push(b"{\"a\"").unwrap().is_empty());
        let lines = reader.push(b":1}\n{\"b\":2}\n{").unwrap();
        assert_eq!(lines, [b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]);
        assert!(reader.push(&vec![b'x'; MAX_LINE]).is_err());
    }

    #[test]
    fn reader_keeps_the_end_of_stderr() {
        let mut reader = Reader::default();
        assert_eq!(reader.stopped(), "burrow-agent stopped");
        reader.stderr(&vec![b'a'; 5000]);
        reader.stderr(b"\x1b[31mbad\n");
        let stopped = reader.stopped();
        assert!(stopped.ends_with("[31mbad"), "{stopped}");
        assert!(stopped.len() < 1100);
    }

    #[test]
    fn samples_over_the_budget_are_refused() {
        let disk = Disk {
            mount: "é".repeat(MAX_TEXT),
            ..Disk::default()
        };
        let sample = Sample {
            disks: vec![disk; MAX_ITEMS],
            ..Sample::default()
        };
        let error = clean_sample(sample).unwrap_err();
        assert!(error.contains("over the limit"), "{error}");
    }

    #[test]
    fn samples_are_kept_in_bounds() {
        let sample = clean_sample(Sample {
            uptime: -1.0,
            cpu: burrow_agent::Cpu {
                percent: 250.0,
                cores: 4,
            },
            disks: vec![
                Disk {
                    mount: "/\x1b]0;evil\x07".into(),
                    fs: "ext4".into(),
                    total: 10,
                    available: 20,
                };
                1000
            ],
            net: vec![Interface {
                name: "x".repeat(1000),
                rx: f64::INFINITY,
                tx: 5.0,
            }],
            ..Sample::default()
        })
        .unwrap();
        assert_eq!(sample.uptime, 0.0);
        assert_eq!(sample.cpu.percent, 100.0);
        assert_eq!(sample.disks.len(), MAX_ITEMS);
        assert_eq!(sample.disks[0].mount, "/]0;evil");
        assert_eq!(sample.disks[0].available, 10);
        assert_eq!(sample.net[0].name.len(), MAX_TEXT);
        assert_eq!(sample.net[0].rx, 0.0);
    }

    #[test]
    fn unit_names_lose_control_characters() {
        let units = clean_units(vec![Unit {
            name: "a\nb.service".into(),
            ..Unit::default()
        }]);
        assert_eq!(units[0].name, "ab.service");
    }
}
