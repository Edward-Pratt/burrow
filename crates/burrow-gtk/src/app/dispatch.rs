//! The window's message router: one arm per [`AppMsg`], the work done by the module that owns it.

use std::time::Instant;

use burrow_core::{Command, Event, TerminalRun};
use relm4::{ComponentSender, gtk};

use crate::database::{self};
use crate::desktop::{self};
use crate::files::{self};
use crate::monitor::{self};
use crate::session;

use super::daemon::{execute, load};
use super::events::Loaded;
use super::message::AppMsg;
use super::nodes::selected;
use super::settings_dialog::show_settings;
use super::status::ErrorLog;
use super::tabs::{TabKey, set_tab_title, switch_tab};
use super::window::{check_for_updates, open_folder};
use super::{App, Widgets};

impl App {
    /// Does what `message` asks. The window is redrawn after it, in
    /// `update_with_view`.
    pub(super) fn dispatch(
        &mut self,
        widgets: &mut Widgets,
        message: AppMsg,
        sender: ComponentSender<Self>,
        window: &gtk::ApplicationWindow,
    ) {
        match message {
            AppMsg::Loaded {
                nodes,
                problems,
                addons,
                sessions,
                autostart,
                monitoring,
                alerts,
            } => {
                let loaded = Loaded {
                    nodes,
                    problems,
                    addons,
                    sessions,
                    autostart,
                    monitoring,
                    alerts,
                };
                self.loaded(loaded, widgets, window, &sender);
            }
            AppMsg::CoreEvent(
                event @ (Event::SessionChanged { .. } | Event::PromptRequested { .. }),
            ) => self.session_event(event, window, &sender),
            AppMsg::CoreEvent(event) => self.apply_event(event, widgets, &sender),
            AppMsg::SessionsListed(sessions) => {
                self.looking_up = false;
                self.take_sessions(sessions, window, &sender);
            }
            AppMsg::Resync => load(&self.core, &sender),
            AppMsg::Disconnected => {
                self.reconnecting = true;
                // It would read as why reconnecting fails. The list keeps it.
                self.error = None;
            }
            AppMsg::Reconnected(client) => self.reconnected(client, window, &sender),
            AppMsg::Failed(error) => self.fail(error),

            AppMsg::RowActivated(id) => self.activate(id, widgets, &sender),
            AppMsg::ContextMenu { node, x, y } => self.context_menu(node, x, y, widgets, &sender),
            AppMsg::New { kind, parent } => {
                let parent = parent.unwrap_or_else(|| self.selection_folder(widgets));
                self.creating = Some((kind.kind().to_string(), parent, Instant::now()));
                self.new_node(kind, parent, window, &sender);
            }
            AppMsg::Open(id) => self.open_tab(id, widgets, &sender),
            AppMsg::Rename(id) => self.rename(id, window, &sender),
            AppMsg::Delete(id) => self.delete(id, window, &sender),
            AppMsg::MoveNode { id, parent } => {
                execute(&self.core, &sender, Command::MoveNode { id, parent });
            }
            AppMsg::MoveTo(id) => self.move_to(id, window, &sender),
            AppMsg::Colour(id) => self.colour(id, window, &sender),
            AppMsg::RenameSelected => {
                if let Some(id) = selected(widgets) {
                    self.rename(id, window, &sender);
                }
            }
            AppMsg::DeleteSelected => {
                if let Some(id) = selected(widgets) {
                    self.delete(id, window, &sender);
                }
            }
            AppMsg::FocusSidebar => self.focus_sidebar(widgets),
            AppMsg::CloseTab(key) => self.close_tab(key, widgets),
            AppMsg::CloseCurrentTab => self.close_current_tab(widgets),
            AppMsg::SwitchTab(step) => switch_tab(&widgets.notebook, step),
            AppMsg::NewTerminal(shell) => self.open_terminal(shell, None, widgets, &sender),
            AppMsg::TerminalTitle { id, title } => {
                if let Some(tab) = self.tabs.get(&TabKey::Terminal(id)) {
                    set_tab_title(&tab.title, &title);
                }
            }
            AppMsg::TerminalExited(id) => self.close_tab(TabKey::Terminal(id), widgets),
            AppMsg::SessionStarted { tab, session } => {
                self.session_started(tab, session, window, &sender)
            }
            AppMsg::SessionFailed { tab, error } => self.session_failed(tab, &error),
            AppMsg::SessionOutput { session, data } => self.session_output(session, &data),
            AppMsg::SessionOutputEnded { session } => self.session_output_ended(session),
            AppMsg::RemoteInput { tab, data } => self.remote_input(tab, data, &sender),
            AppMsg::RemoteResize { tab, cols, rows } => {
                self.remote_resize(tab, cols, rows, &sender)
            }
            AppMsg::RemoteTitle { tab, title } => self.remote_title(tab, &title),
            AppMsg::NewShell(id) => self.new_shell(id, None, widgets, &sender),
            AppMsg::OpenFiles(id) => self.open_files(id, widgets, &sender),
            AppMsg::Files {
                node,
                msg: files::Msg::Session(session::Msg::Started(session)),
            } => self.files_started(node, session, window, &sender),
            AppMsg::Files { node, msg } => self.files_msg(node, msg, window, &sender),
            AppMsg::OpenMonitor(id) => self.open_monitor(id, widgets, &sender),
            AppMsg::Monitor {
                node,
                msg: monitor::Msg::Session(session::Msg::Started(session)),
            } => self.monitor_started(node, session, window, &sender),
            AppMsg::Monitor {
                node,
                msg: monitor::Msg::SetBackground(on),
            } => self.set_monitoring(node, on, window, &sender),
            AppMsg::Monitor { node, msg } => self.monitor_msg(node, msg, &sender),
            AppMsg::MonitoringFailed(error) => {
                self.fail(error);
                self.refresh_monitors();
            }
            AppMsg::StartTunnel(node) => {
                execute(&self.core, &sender, Command::StartTunnel { node });
            }
            AppMsg::StopTunnel(node) => self.stop_tunnel(node, &sender),
            AppMsg::RestartTunnel(node) => self.restart_tunnel(node, &sender),
            AppMsg::EditTunnel(node) => self.edit_tunnel(Some(node), None, window, &sender),
            AppMsg::Database {
                node,
                msg: database::Msg::Session(session::Msg::Started(session)),
            } => self.database_started(node, session, window, &sender),
            AppMsg::Database { node, msg } => self.database_msg(node, msg, window, &sender),
            AppMsg::EditDatabase(node) => self.edit_database(Some(node), None, window, &sender),
            AppMsg::Desktop {
                node,
                msg: desktop::Msg::Session(session::Msg::Started(session)),
            } => self.desktop_started(node, session, window, &sender),
            AppMsg::Desktop { node, msg } => self.desktop_msg(node, msg),
            AppMsg::EditDesktop(node) => self.edit_desktop(Some(node), None, window, &sender),
            AppMsg::EditGrant(node) => self.load_grant(node, &sender),
            AppMsg::GrantLoaded { node, grant } => self.edit_grant(node, grant, window, &sender),
            AppMsg::SetAutostart { node, enabled } => self.set_autostart(node, enabled, &sender),
            AppMsg::AutostartFailed { node, error } => {
                self.fail(error);
                self.refresh_tunnel(node, &sender);
            }
            AppMsg::LoadView(id) => self.load_view(id, &sender),
            AppMsg::ViewLoaded { node, result } => self.show_view(node, result, &sender),
            AppMsg::Invoke {
                addon,
                action,
                node,
                values,
            } => self.invoke(addon, action, node, values, &sender),
            AppMsg::Notice(message) => self.show_notice(message, &sender),
            AppMsg::ClearNotice(id) => {
                if id == self.notice_id {
                    self.notice = None;
                }
            }
            AppMsg::ClearErrors => {
                self.errors = ErrorLog::default();
                widgets.errors_button.popdown();
            }
            AppMsg::CloseRequested => {
                // Where the shells are now, which no other save sees.
                self.save_tabs(widgets);
                self.close_window(window, None);
            }
            AppMsg::TabsMoved => self.save_tabs(widgets),
            AppMsg::Control {
                node,
                service,
                action,
            } => self.confirm_control(node, service, action, window, &sender),
            AppMsg::ControlNow {
                node,
                service,
                action,
            } => self.control(node, service, action, &sender),
            AppMsg::Controlled {
                node,
                service,
                action,
                result,
            } => self.controlled(node, service, action, result, widgets, &sender),
            AppMsg::ShowLogs { node, service } => {
                let run = TerminalRun::Logs { service };
                self.new_shell(node, Some(run), widgets, &sender);
            }
            AppMsg::ManageAddons => self.manage_addons(window, &sender),
            AppMsg::ReviewAddon(id) => self.review_addon(id, window, &sender),
            AppMsg::RevokeAddon(id) => {
                execute(&self.core, &sender, Command::RevokeAddon { id });
            }
            AppMsg::OpenAddonsFolder => open_folder(window, &self.core.info().addons_dir),
            AppMsg::ShowPalette => self.show_palette(window, &sender),
            AppMsg::ToggleSidebar => self.toggle_sidebar(widgets),
            AppMsg::ShowSettings => show_settings(window, &self.settings, &sender),
            AppMsg::CheckForUpdates => check_for_updates(window, &sender),
            AppMsg::Restart(program) => {
                self.save_tabs(widgets);
                self.close_window(window, Some(program));
            }
            AppMsg::SetTheme(theme) => {
                self.settings.theme = theme;
                self.apply_theme();
                self.save_settings();
            }
            AppMsg::SetTerminalFont(font) => self.set_terminal_font(font),
            AppMsg::ReloadConfig => execute(&self.core, &sender, Command::ReloadConfig),
            AppMsg::OpenConfigFolder => open_folder(window, &self.config_dir),
        }
    }
}
