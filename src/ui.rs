//! The popup: a command line, a Run button, and a real terminal for the output.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::gdk::{Key, ModifierType};
use gtk::glib::{self, Propagation};
use gtk4 as gtk;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use vte4::prelude::*;

use crate::runner::{Outcome, extra_env, request_exit, shell_argv};

const WIDTH: i32 = 760;
const HEIGHT: i32 = 440;
const SCROLLBACK_LINES: i64 = 10_000;
/// A second Ctrl+W / Ctrl+Q within this window kills a running command and closes.
const CLOSE_CONFIRM: Duration = Duration::from_secs(3);
/// How long to wait for VTE to draw a finished command's last output.
const MARKER_WAIT: Duration = Duration::from_secs(5);
const IDLE_HINT: &str = "Enter / Alt+R run  ·  Esc close  ·  Ctrl+Shift+C/V copy/paste output";
const REQUEST_HINT: &str =
    "Requested command: check it (edit if needed), Enter to run  ·  Esc to decline";

const CSS: &str = "
/* GTK4 draws no CSS border on an undecorated toplevel, so the frame goes on the content.
   @accent_bg_color isn't defined by GTK 4.18's own theme; this older name is, and
   it holds the same accent blue. */
.runbox-frame { border: 1px solid @theme_selected_bg_color; }
.runbox-bar { padding: 6px; }
.runbox-status { padding: 2px 8px 4px 8px; font-size: smaller; opacity: 0.75; }
.runbox-status.failed { color: #e06c75; opacity: 1; }
.runbox-status.requested { color: #e5c07b; opacity: 1; font-weight: bold; }
";

#[derive(Default)]
struct State {
    running: Option<Pid>,
    close_armed_at: Option<Instant>,
    request: Option<Request>,
}

/// A `runbox --request` caller waiting for the command it put in the box.
struct Request {
    caller: gio::ApplicationCommandLine,
    /// The "$ command" line the run printed first. None until the command is actually run.
    header: Option<String>,
}

impl Request {
    /// Send the caller its result. Its `runbox --request` process exits with `code`.
    fn answer(self, output: &str, code: u8, note: &str) {
        let output = output.trim_end();
        if !output.is_empty() {
            self.caller.print_literal(&format!("{output}\n"));
        }
        self.caller.printerr_literal(&format!("runbox: {note}\n"));
        self.caller.set_exit_code(glib::ExitCode::from(code));
        self.caller.done();
    }
}

/// Every widget the handlers touch, plus the run state, behind one cheap clone.
#[derive(Clone)]
pub struct RunBox {
    window: gtk::ApplicationWindow,
    entry: gtk::Entry,
    run_button: gtk::Button,
    terminal: vte4::Terminal,
    status: gtk::Label,
    state: Rc<RefCell<State>>,
}

impl RunBox {
    pub fn new(app: &gtk::Application) -> Self {
        install_css();

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("Runbox")
            .decorated(false)
            .default_width(WIDTH)
            .default_height(HEIGHT)
            .build();
        // Add to GTK's own classes. Replacing them (the builder's css_classes) drops
        // "background", and the window then paints no background of its own.
        window.add_css_class("runbox");

        let entry = gtk::Entry::builder()
            .hexpand(true)
            .placeholder_text("Type or paste a command")
            .build();
        let run_button = gtk::Button::with_mnemonic("_Run");
        run_button.add_css_class("suggested-action");

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.add_css_class("runbox-bar");
        bar.append(&entry);
        bar.append(&run_button);

        let terminal = vte4::Terminal::new();
        terminal.set_vexpand(true);
        terminal.set_hexpand(true);
        terminal.set_scrollback_lines(SCROLLBACK_LINES);
        terminal.set_input_enabled(false);

        let scroller = gtk::ScrolledWindow::builder()
            .child(&terminal)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let status = gtk::Label::builder()
            .label(IDLE_HINT)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["runbox-status"])
            .build();

        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.add_css_class("runbox-frame");
        layout.append(&bar);
        layout.append(&scroller);
        layout.append(&status);
        window.set_child(Some(&layout));

        let runbox = RunBox {
            window,
            entry,
            run_button,
            terminal,
            status,
            state: Rc::default(),
        };
        runbox.connect_signals();
        runbox
    }

    pub fn present(&self) {
        self.window.present();
        if !self.is_running() {
            self.entry.grab_focus();
        }
    }

    /// Put a command in the box for the user to review. It never runs without their Enter.
    pub fn request(
        &self,
        command: &str,
        caller: gio::ApplicationCommandLine,
    ) -> Result<(), &'static str> {
        {
            let state = self.state.borrow();
            if state.running.is_some() {
                return Err("a command is already running in runbox");
            }
            if state.request.is_some() {
                return Err("another request is already waiting in runbox");
            }
        }
        self.state.borrow_mut().request = Some(Request {
            caller,
            header: None,
        });
        self.entry.set_text(command);
        self.entry.grab_focus();
        self.entry.set_position(-1);
        self.status.remove_css_class("failed");
        self.status.add_css_class("requested");
        self.status.set_label(REQUEST_HINT);
        Ok(())
    }

    fn connect_signals(&self) {
        let this = self.clone();
        self.entry.connect_activate(move |_| this.run());

        let this = self.clone();
        self.run_button.connect_clicked(move |_| this.run());

        let this = self.clone();
        self.terminal.connect_child_exited(move |_, status| {
            this.finished(Outcome::from_wait_status(status))
        });

        // Closing with a request still open answers it, so the caller never hangs.
        let this = self.clone();
        self.window.connect_close_request(move |_| {
            if let Some(request) = this.state.borrow_mut().request.take() {
                let note = if request.header.is_some() {
                    "window closed before the command finished"
                } else {
                    "declined"
                };
                request.answer("", request_exit::NOT_RUN, note);
            }
            Propagation::Proceed
        });

        // Capture phase: these shortcuts win before the entry or terminal see the key.
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        let this = self.clone();
        keys.connect_key_pressed(move |_, key, _, mods| this.on_key(key, mods));
        self.window.add_controller(keys);
    }

    fn on_key(&self, key: Key, mods: ModifierType) -> Propagation {
        let ctrl = mods.contains(ModifierType::CONTROL_MASK);
        let shift = mods.contains(ModifierType::SHIFT_MASK);
        let alt = mods.contains(ModifierType::ALT_MASK);

        match key {
            // Ctrl+W / Ctrl+Q always close; a running command needs a second press.
            Key::w | Key::q | Key::W | Key::Q if ctrl && !shift => {
                self.request_close();
                Propagation::Stop
            }
            // Esc closes only when idle; while running it belongs to the program (less, vim, ...).
            Key::Escape if !self.is_running() => {
                self.window.close();
                Propagation::Stop
            }
            Key::Return | Key::KP_Enter if ctrl && !self.is_running() => {
                self.run();
                Propagation::Stop
            }
            Key::r | Key::R if alt && !self.is_running() => {
                self.run();
                Propagation::Stop
            }
            Key::C if ctrl && shift => {
                self.terminal.copy_clipboard_format(vte4::Format::Text);
                Propagation::Stop
            }
            Key::V if ctrl && shift && self.is_running() => {
                self.terminal.paste_clipboard();
                Propagation::Stop
            }
            _ => Propagation::Proceed,
        }
    }

    fn is_running(&self) -> bool {
        self.state.borrow().running.is_some()
    }

    fn run(&self) {
        if self.is_running() {
            return;
        }
        let command = self.entry.text().trim().to_string();
        if command.is_empty() {
            return;
        }
        if let Some(request) = self.state.borrow_mut().request.as_mut() {
            request.header = Some(header_line(&command));
        }

        self.terminal
            .feed(format!("\x1b[1;36m$ {}\x1b[0m\r\n", command.replace('\n', "\r\n")).as_bytes());
        self.set_busy(
            true,
            &format!("Running: {command}   (Ctrl+C interrupt · Ctrl+W twice kill)"),
        );

        let argv_owned = shell_argv(&command);
        let argv: Vec<&str> = argv_owned.iter().map(String::as_str).collect();
        let env_owned = extra_env();
        let env: Vec<&str> = env_owned.iter().map(String::as_str).collect();
        let home = glib::home_dir();

        let this = self.clone();
        self.terminal.spawn_async(
            vte4::PtyFlags::DEFAULT,
            home.to_str(),
            &argv,
            &env,
            glib::SpawnFlags::DEFAULT,
            || {},
            -1,
            None::<&gtk::gio::Cancellable>,
            move |result| match result {
                Ok(pid) => this.state.borrow_mut().running = Some(Pid::from_raw(pid.0)),
                Err(err) => {
                    this.terminal
                        .feed(format!("\x1b[31mcould not start: {err}\x1b[0m\r\n").as_bytes());
                    this.set_busy(false, &format!("could not start: {err}"));
                    this.status.add_css_class("failed");
                }
            },
        );
    }

    fn finished(&self, outcome: Outcome) {
        let colour = if outcome.succeeded() { "32" } else { "31" };
        // Output that didn't end in a newline (a prompt, "^C") would run into the result line.
        let (column, _) = self.terminal.cursor_position();
        let line_break = if column > 0 { "\r\n" } else { "" };
        self.terminal.feed(
            format!(
                "{line_break}\x1b[{colour}m[{}]\x1b[0m\r\n\r\n",
                outcome.describe()
            )
            .as_bytes(),
        );
        self.answer_request(&outcome);
        {
            let mut state = self.state.borrow_mut();
            state.running = None;
            state.close_armed_at = None;
        }
        self.set_busy(false, &format!("{}  ·  {IDLE_HINT}", outcome.describe()));
        if outcome.succeeded() {
            self.status.remove_css_class("failed");
        } else {
            self.status.add_css_class("failed");
        }
        self.dump_for_tests();
    }

    /// Hand a requested run's output (everything from its "$ command" line on,
    /// scrollback included) and exit code back to the caller.
    ///
    /// VTE draws a command's last output a little after the child exits. The
    /// "[exit N]" line is fed after all of it, so once that line is on screen the
    /// output above it is complete. Poll for it rather than guess a delay.
    fn answer_request(&self, outcome: &Outcome) {
        let header = self
            .state
            .borrow()
            .request
            .as_ref()
            .and_then(|r| r.header.clone());
        let Some(header) = header else {
            return;
        };
        let Some(request) = self.state.borrow_mut().request.take() else {
            return;
        };
        self.status.remove_css_class("requested");

        let marker = format!("[{}]", outcome.describe());
        let code = outcome.shell_exit_code();
        let note = outcome.describe();
        let terminal = self.terminal.clone();
        let started_at = Instant::now();
        let mut request = Some(request);
        glib::timeout_add_local(Duration::from_millis(20), move || {
            let buffer = buffer_text(&terminal);
            let run = last_run(&buffer, &header, &marker);
            if run.is_none() && started_at.elapsed() < MARKER_WAIT {
                return glib::ControlFlow::Continue;
            }
            if let Some(request) = request.take() {
                match run {
                    Some(output) => request.answer(output, code, &note),
                    None => request.answer(
                        &buffer,
                        code,
                        &format!("{note} (couldn't find the run's end; whole window sent)"),
                    ),
                }
            }
            glib::ControlFlow::Break
        });
    }

    /// While a command runs, keys go to the terminal so prompts can be answered.
    fn set_busy(&self, busy: bool, status: &str) {
        self.status.remove_css_class("failed");
        self.status.set_label(status);
        self.entry.set_sensitive(!busy);
        self.run_button.set_sensitive(!busy);
        self.terminal.set_input_enabled(busy);
        if busy {
            self.terminal.grab_focus();
        } else {
            self.entry.grab_focus();
            self.entry.select_region(0, -1);
        }
    }

    fn request_close(&self) {
        let Some(pid) = self.state.borrow().running else {
            self.window.close();
            return;
        };
        let armed = self
            .state
            .borrow()
            .close_armed_at
            .is_some_and(|at| at.elapsed() < CLOSE_CONFIRM);
        if armed {
            // The child leads its own session on the PTY, so its pid is the group id.
            let _ = killpg(pid, Signal::SIGHUP);
            self.window.close();
        } else {
            self.state.borrow_mut().close_armed_at = Some(Instant::now());
            self.status.add_css_class("failed");
            self.status.set_label(
                "Command still running. Press Ctrl+W again within 3 s to kill it and close.",
            );
        }
    }

    /// For the GUI test only: write what the terminal shows after each command.
    /// VTE draws fed text on its next update, so the snapshot waits a moment.
    fn dump_for_tests(&self) {
        let Ok(path) = std::env::var("RUNBOX_DUMP") else {
            return;
        };
        let terminal = self.terminal.clone();
        glib::timeout_add_local_once(Duration::from_millis(150), move || {
            if let Some(text) = terminal.text_format(vte4::Format::Text) {
                let _ = std::fs::write(path, text.as_str());
            }
        });
    }
}

/// The first line a run prints, as it reads back from the terminal.
fn header_line(command: &str) -> String {
    format!("$ {}", command.lines().next().unwrap_or(""))
        .trim_end()
        .to_string()
}

/// Everything VTE holds, scrollback included, as plain text.
fn buffer_text(terminal: &vte4::Terminal) -> String {
    let stream = gtk::gio::MemoryOutputStream::new_resizable();
    if terminal
        .write_contents_sync(
            &stream,
            vte4::WriteFlags::Default,
            None::<&gtk::gio::Cancellable>,
        )
        .is_err()
        || stream.close(None::<&gtk::gio::Cancellable>).is_err()
    {
        return String::new();
    }
    String::from_utf8_lossy(&stream.steal_as_bytes()).into_owned()
}

/// The newest run in `buffer` that starts with `header` and ends with a line
/// ending in `marker`, without the marker line. None if it isn't all drawn yet.
fn last_run<'a>(buffer: &'a str, header: &str, marker: &str) -> Option<&'a str> {
    let mut start = None;
    let mut offset = 0;
    for line in buffer.split_inclusive('\n') {
        if line.trim_end() == header {
            start = Some(offset);
        }
        offset += line.len();
    }
    let start = start?;
    let run = &buffer[start..];
    let mut offset = 0;
    for line in run.split_inclusive('\n') {
        if line.trim_end().ends_with(marker) {
            let end_of_output = offset + line.trim_end().len() - marker.len();
            return Some(&run[..end_of_output]);
        }
        offset += line.len();
    }
    None
}

fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUFFER: &str = "$ echo hi\nhi\n[exit 0]\n\n$ printf 'a? '; read a\na? y\n[exit 0]\n\n$ echo hi\nhi again\n[exit 3]\n\n";

    #[test]
    fn finds_the_newest_run_of_a_repeated_command() {
        assert_eq!(
            last_run(BUFFER, "$ echo hi", "[exit 3]"),
            Some("$ echo hi\nhi again\n")
        );
    }

    #[test]
    fn waits_until_the_marker_is_drawn() {
        assert_eq!(last_run("$ echo hi\nhi\n", "$ echo hi", "[exit 0]"), None);
    }

    #[test]
    fn marker_on_the_same_line_as_unterminated_output() {
        assert_eq!(
            last_run(
                "$ sleep 9\n^C[killed by signal 2]\n",
                "$ sleep 9",
                "[killed by signal 2]"
            ),
            Some("$ sleep 9\n^C")
        );
    }

    #[test]
    fn header_uses_the_first_line_of_a_pasted_script() {
        assert_eq!(header_line("echo a\necho b"), "$ echo a");
        assert_eq!(header_line("ls  "), "$ ls");
    }
}
