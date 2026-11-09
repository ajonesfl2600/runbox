//! The popup: a command box, Run and Exit buttons, and a real terminal for the output.

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
const IDLE_HINT: &str =
    "Enter / Alt+R run  ·  Shift+Enter new line  ·  Esc close  ·  Ctrl+Shift+C/V copy/paste output";
const PLACEHOLDER: &str = "Type or paste a command";
/// The command box starts this many lines tall, grows with its text up to
/// INPUT_MAX_LINES, then scrolls.
const INPUT_MIN_LINES: i32 = 3;
const INPUT_MAX_LINES: i32 = 6;
/// Space between the command box's frame and its text, in px.
const INPUT_MARGIN: i32 = 6;
/// Used only if Pango can't say how tall a line of the font is.
const FALLBACK_LINE_PX: i32 = 18;
/// Run and Exit are the same width, so the pair lines up and stays put.
const BUTTON_WIDTH: i32 = 72;
const REQUEST_HINT: &str =
    "Requested command: check it (edit if needed), Enter to run  ·  Esc to decline";

const CSS: &str = "
/* GTK4 draws no CSS border on an undecorated toplevel, so the frame goes on the content.
   @accent_bg_color isn't defined by GTK 4.18's own theme; this older name is, and
   it holds the same accent blue. */
.runbox-frame { border: 1px solid @theme_selected_bg_color; }
.runbox-bar { padding: 6px; }
.runbox-placeholder { opacity: 0.55; }
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
    input: gtk::TextView,
    run_button: gtk::Button,
    exit_button: gtk::Button,
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

        // A TextView, not an Entry, so a long command wraps and all of it shows.
        // Wrapping is only how it's drawn: the buffer holds exactly what was typed.
        let input = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .accepts_tab(false)
            .top_margin(INPUT_MARGIN)
            .bottom_margin(INPUT_MARGIN)
            .left_margin(INPUT_MARGIN)
            .right_margin(INPUT_MARGIN)
            .build();
        let line_px = line_height(&input);
        let input_scroller = gtk::ScrolledWindow::builder()
            .child(&input)
            .hexpand(true)
            .has_frame(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .min_content_height(INPUT_MIN_LINES * line_px + 2 * INPUT_MARGIN)
            .max_content_height(INPUT_MAX_LINES * line_px + 2 * INPUT_MARGIN)
            .build();
        // A GTK4 TextView has no placeholder, so a label sits on top while it's empty.
        // It takes no clicks, so a click on it lands in the box.
        let placeholder = gtk::Label::builder()
            .label(PLACEHOLDER)
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_start(INPUT_MARGIN + 1)
            .margin_top(INPUT_MARGIN + 1)
            .can_target(false)
            .css_classes(["runbox-placeholder"])
            .build();
        let input_area = gtk::Overlay::builder().child(&input_scroller).build();
        input_area.add_overlay(&placeholder);
        input
            .buffer()
            .connect_changed(move |buffer| placeholder.set_visible(buffer.char_count() == 0));

        let run_button = gtk::Button::builder()
            .label("_Run")
            .use_underline(true)
            .valign(gtk::Align::Start)
            .width_request(BUTTON_WIDTH)
            .tooltip_text("Run the command (Enter, Ctrl+Enter, Alt+R)")
            .build();
        run_button.add_css_class("suggested-action");
        // focus_on_click off: clicking Exit mid-run leaves keyboard focus on the
        // terminal, so the program keeps getting its keys.
        let exit_button = gtk::Button::builder()
            .label("_Exit")
            .use_underline(true)
            .valign(gtk::Align::Start)
            .width_request(BUTTON_WIDTH)
            .focus_on_click(false)
            .tooltip_text(
                "Close (Esc, Ctrl+Q, Alt+E). While a command runs, press twice within 3 s to kill it and close",
            )
            .build();

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.add_css_class("runbox-bar");
        bar.append(&input_area);
        bar.append(&run_button);
        bar.append(&exit_button);
        // The window has no title bar, so the bar is what you drag to move it.
        // Widgets that take a drag themselves (the command box selects text, the
        // buttons click) keep it. The space around them moves the window.
        let bar_handle = gtk::WindowHandle::builder().child(&bar).build();

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
        // The status line is a second handle, a wide strip along the bottom edge.
        let status_handle = gtk::WindowHandle::builder().child(&status).build();

        let layout = gtk::Box::new(gtk::Orientation::Vertical, 0);
        layout.add_css_class("runbox-frame");
        layout.append(&bar_handle);
        layout.append(&scroller);
        layout.append(&status_handle);
        window.set_child(Some(&layout));

        let runbox = RunBox {
            window,
            input,
            run_button,
            exit_button,
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
            self.input.grab_focus();
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
        let buffer = self.input.buffer();
        buffer.set_text(command);
        buffer.place_cursor(&buffer.end_iter());
        self.input.grab_focus();
        self.status.remove_css_class("failed");
        self.status.add_css_class("requested");
        self.status.set_label(REQUEST_HINT);
        Ok(())
    }

    fn connect_signals(&self) {
        let this = self.clone();
        self.run_button.connect_clicked(move |_| this.run());

        // The same path as Ctrl+Q: closes when idle, needs a second press mid-run.
        let this = self.clone();
        self.exit_button
            .connect_clicked(move |_| this.request_close());

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
            // In the command box, Enter runs and Shift+Enter falls through to the
            // TextView, which puts in a newline.
            Key::Return | Key::KP_Enter | Key::ISO_Enter
                if !shift && !alt && !self.is_running() && self.input.has_focus() =>
            {
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
        let Some(command) = command_from_input(&self.input_text()) else {
            return;
        };
        self.dump_input_for_tests();
        if let Some(request) = self.state.borrow_mut().request.as_mut() {
            request.header = Some(header_line(&command));
        }

        self.terminal.feed(echo_line(&command).as_bytes());
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
        self.input.set_sensitive(!busy);
        self.run_button.set_sensitive(!busy);
        self.terminal.set_input_enabled(busy);
        if busy {
            self.terminal.grab_focus();
        } else {
            self.input.grab_focus();
            let buffer = self.input.buffer();
            buffer.select_range(&buffer.start_iter(), &buffer.end_iter());
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
                "Command still running. Press Exit or Ctrl+W again within 3 s to kill it and close.",
            );
        }
    }

    /// Everything in the command box, exactly as typed or prefilled.
    fn input_text(&self) -> String {
        let buffer = self.input.buffer();
        buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string()
    }

    /// For the GUI test only: how the command box laid out the command it is about
    /// to run, written to `$RUNBOX_DUMP.input`. `lines` counts drawn lines, so it is
    /// over 1 when the text wrapped. `overflow` is how far the text runs past the
    /// box sideways (0 when nothing hides off the right edge). `vscroll` is how far
    /// it runs past the bottom (over 0 once the box stopped growing and scrolls).
    /// `box_height` is the visible box.
    fn dump_input_for_tests(&self) {
        let Ok(path) = std::env::var("RUNBOX_DUMP") else {
            return;
        };
        let mut iter = self.input.buffer().start_iter();
        let mut lines = 1;
        while self.input.forward_display_line(&mut iter) {
            lines += 1;
        }
        let past_edge = |adj: Option<gtk::Adjustment>| {
            adj.map_or(0.0, |a| (a.upper() - a.page_size()).max(0.0))
        };
        let overflow = past_edge(self.input.hadjustment());
        let vscroll = past_edge(self.input.vadjustment());
        let box_height = self.input.parent().map_or(0, |p| p.height());
        let report = format!(
            "lines={lines}\noverflow={overflow:.0}\nvscroll={vscroll:.0}\nbox_height={box_height}\nwindow_width={}\n",
            self.window.width()
        );
        let _ = std::fs::write(format!("{path}.input"), report);
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

/// The command a press of Run runs, from what is in the box. Only whitespace at
/// the two ends is dropped. Nothing is added: the box wraps long lines on screen,
/// but wrapping never puts a newline into the text. None for an empty box.
fn command_from_input(text: &str) -> Option<String> {
    let command = text.trim();
    (!command.is_empty()).then(|| command.to_string())
}

/// The "$ command" line fed to the terminal before a run. The terminal needs
/// "\r\n" to start a line at column 0. Every other byte is the command's own.
fn echo_line(command: &str) -> String {
    format!("\x1b[1;36m$ {}\x1b[0m\r\n", command.replace('\n', "\r\n"))
}

/// Height of one line of the command box's font, in px.
fn line_height(view: &gtk::TextView) -> i32 {
    let metrics = view.pango_context().metrics(None, None);
    let mut px = metrics.height();
    if px <= 0 {
        px = metrics.ascent() + metrics.descent();
    }
    let px = px / gtk::pango::SCALE;
    if px > 0 { px } else { FALLBACK_LINE_PX }
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

    #[test]
    fn input_runs_as_typed() {
        assert_eq!(command_from_input("ls -la"), Some("ls -la".to_string()));
        // Inner spacing and quoting are the command's own business.
        assert_eq!(
            command_from_input("echo 'a  b'   \"c\""),
            Some("echo 'a  b'   \"c\"".to_string())
        );
    }

    #[test]
    fn long_input_gets_no_newlines() {
        // A 300-character one-liner, the kind the box wraps on screen.
        let words: Vec<String> = (0..50).map(|i| format!("w{i:03}")).collect();
        let command = format!("echo {}", words.join(" "));
        let command = format!("{command}{}", "x".repeat(300 - command.len()));
        assert_eq!(command.len(), 300);
        let run = command_from_input(&command).unwrap();
        assert_eq!(run.as_bytes(), command.as_bytes());
        assert!(!run.contains('\n'));

        // One long word with no spaces, the WordChar case.
        let word = "a".repeat(500);
        assert_eq!(command_from_input(&word).unwrap(), word);
    }

    #[test]
    fn shift_enter_newlines_are_kept() {
        assert_eq!(
            command_from_input("echo one\necho two"),
            Some("echo one\necho two".to_string())
        );
        // Blank lines inside a pasted script stay too.
        assert_eq!(
            command_from_input("echo one\n\n  echo two"),
            Some("echo one\n\n  echo two".to_string())
        );
    }

    #[test]
    fn only_the_ends_are_trimmed() {
        assert_eq!(
            command_from_input("  \n echo hi \n\t"),
            Some("echo hi".to_string())
        );
        assert_eq!(
            command_from_input("é ü 日本 ✓"),
            Some("é ü 日本 ✓".to_string())
        );
    }

    #[test]
    fn empty_input_runs_nothing() {
        assert_eq!(command_from_input(""), None);
        assert_eq!(command_from_input("   "), None);
        assert_eq!(command_from_input("\n\t \n"), None);
    }

    #[test]
    fn echo_line_is_the_command_byte_for_byte() {
        let command = format!("echo {}", "y".repeat(295));
        let line = echo_line(&command);
        assert_eq!(line, format!("\x1b[1;36m$ {command}\x1b[0m\r\n"));
        // Strip the colour codes and the line ending: what's left is "$ " + command.
        let shown = line
            .strip_prefix("\x1b[1;36m$ ")
            .and_then(|l| l.strip_suffix("\x1b[0m\r\n"))
            .unwrap();
        assert_eq!(shown.as_bytes(), command.as_bytes());
    }

    #[test]
    fn echo_line_starts_each_typed_line_at_column_zero() {
        assert_eq!(
            echo_line("echo a\necho b"),
            "\x1b[1;36m$ echo a\r\necho b\x1b[0m\r\n"
        );
        // A bare "\r" passes through untouched, like any other byte.
        assert_eq!(echo_line("a\rb"), "\x1b[1;36m$ a\rb\x1b[0m\r\n");
    }
}
