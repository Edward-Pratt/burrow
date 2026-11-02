//! The status bar: the current error, the errors list, notices and the counts.

use std::collections::VecDeque;
use std::time::Duration;

use relm4::gtk::glib;
use relm4::gtk::prelude::*;

use super::{App, Widgets};

/// How long a message from an addon stays in the status bar.
pub(super) const NOTICE_TIME: Duration = Duration::from_secs(6);

/// How many errors the status bar's list keeps.
pub(super) const MAX_ERRORS: usize = 50;

/// The errors the status bar has shown, oldest first, with the time each
/// arrived. Only the newest is on the status line, and a burst of them
/// would otherwise replace one another before they could be read.
#[derive(Default)]
pub(super) struct ErrorLog(pub(super) VecDeque<(String, String)>);

impl ErrorLog {
    pub(super) fn push(&mut self, time: String, message: String) {
        if self.0.len() == MAX_ERRORS {
            self.0.pop_front();
        }
        self.0.push_back((time, message));
    }

    /// One line per error, newest first.
    pub(super) fn text(&self) -> String {
        (self.0.iter().rev())
            .map(|(time, message)| format!("{time}  {message}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

use super::message::AppMsg;
use relm4::ComponentSender;
impl App {
    /// Shows `error` in the status bar until the user does something or a
    /// newer error replaces it, and keeps it in the errors list.
    pub(super) fn fail(&mut self, error: String) {
        let time = glib::DateTime::now_local()
            .and_then(|now| now.format("%H:%M:%S"))
            .map(|time| time.to_string())
            .unwrap_or_default();
        self.errors.push(time, error.clone());
        self.error = Some(error);
    }

    pub(super) fn refresh_status(&self, widgets: &Widgets) {
        let (text, is_error) = if self.reconnecting {
            // Stays up until the new connection is, with why the last try
            // failed beside it.
            let lost = "Lost the connection to burrowd. Reconnecting…";
            let text = match &self.error {
                Some(error) => format!("{lost} ({error})"),
                None => lost.to_string(),
            };
            (text, true)
        } else if let Some(error) = &self.error {
            (error.clone(), true)
        } else if let Some(notice) = &self.notice {
            (notice.clone(), false)
        } else {
            let leaves = self.nodes.values().filter(|n| !n.is_folder()).count();
            let folders = self.nodes.len() - leaves;
            let text = format!(
                "{leaves} {} · {folders} {}",
                plural(leaves, "item", "items"),
                plural(folders, "folder", "folders"),
            );
            (text, false)
        };
        widgets.status.set_label(&text);
        if is_error {
            widgets.status.add_css_class("error");
        } else {
            widgets.status.remove_css_class("error");
        }
        let errors = self.errors.0.len();
        widgets.errors_button.set_visible(errors > 0);
        widgets
            .errors_button
            .set_label(&format!("{errors} {}", plural(errors, "error", "errors")));
        widgets.errors_list.set_label(&self.errors.text());
        widgets
            .problems_button
            .set_visible(!self.problems.is_empty());
        widgets.problems_button.set_label(&format!(
            "{} config {}",
            self.problems.len(),
            plural(self.problems.len(), "problem", "problems")
        ));
        widgets.problems_list.set_label(
            &self
                .problems
                .iter()
                .map(|p| format!("{}\n    {}", p.path.display(), p.message))
                .collect::<Vec<_>>()
                .join("\n\n"),
        );
    }

    /// Shows a message from an addon in the status bar for a while.
    pub(super) fn show_notice(&mut self, message: String, sender: &ComponentSender<Self>) {
        self.notice = Some(message);
        self.notice_id += 1;
        let (id, sender) = (self.notice_id, sender.clone());
        glib::timeout_add_local_once(NOTICE_TIME, move || {
            sender.input(AppMsg::ClearNotice(id));
        });
    }
}

pub(super) fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_error_log_keeps_the_newest() {
        let mut log = ErrorLog::default();
        for n in 0..MAX_ERRORS + 10 {
            log.push(format!("t{n}"), format!("error {n}"));
        }
        assert_eq!(log.0.len(), MAX_ERRORS);
        let text = log.text();
        let first = text.lines().next().unwrap();
        assert_eq!(first, format!("t{0}  error {0}", MAX_ERRORS + 9));
        assert!(!text.contains("error 9\n") && text.ends_with("error 10"));
    }
}
