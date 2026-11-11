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

use crate::result::{RunResult, Status, unix_now};
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
.runbox-id { font-weight: bold; color: #e5c07b; padding: 0 4px; }
";
/// The longest id the header shows in full; the title always has all of it.
const ID_MAX_CHARS: i32 = 20;

/// What happened in a window, for whoever keeps track of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A command started.
    Running,
    /// The command ended; the window is idle again.
    Done,
    /// The window is gone.
    Closed,
}

/// Told about every `Event` in one window, with the window's request id at the
/// time (None for the plain popup). Never called while the window's own state
/// is borrowed, so it may call back into the window.
pub type Watcher = Rc<dyn Fn(Option<&str>, Event)>;

#[derive(Default)]
struct State {
    running: Option<Pid>,
    close_armed_at: Option<Instant>,
    request: Option<Request>,
    /// The request id this window shows. None for the plain popup.
    id: Option<String>,
}

/// A `runbox --request` caller waiting for the command it put in the box.
pub struct Request {
    caller: gio::ApplicationCommandLine,
    id: String,
    /// What the caller asked for. The person can edit it before it runs.
    command: String,
    json: bool,
    /// Where to write the output too, already resolved against the caller's folder.
    output_file: Option<String>,
    /// When the request arrived (unix seconds).
    opened: u64,
    /// The run as it started: its "$ command" line, the command as edited, and
    /// the time. None until the command is actually run.
    run: Option<Started>,
}

struct Started {
    header: String,
    command: String,
    at: u64,
}

impl Request {
    pub fn new(
        caller: gio::ApplicationCommandLine,
        id: String,
        command: String,
        json: bool,
        output_file: Option<String>,
        opened: u64,
    ) -> Self {
        Request {
            caller,
            id,
            command,
            json,
            output_file,
            opened,
            run: None,
        }
    }

    /// The result the caller gets. `command` is what actually ran when it ran.
    fn result(&self, output: &str, code: u8, status: Status) -> RunResult {
        let output = output.trim_end();
        let (command, started) = match &self.run {
            Some(run) => (run.command.clone(), run.at),
            None => (self.command.clone(), self.opened),
        };
        RunResult {
            id: self.id.clone(),
            command,
            exit_code: i32::from(code),
            status,
            started,
            ended: unix_now(),
            output: if output.is_empty() {
                String::new()
            } else {
                format!("{output}\n")
            },
        }
    }

    /// Send the caller its result. Its `runbox --request` process exits with `code`.
    fn answer(self, output: &str, code: u8, note: &str, status: Status) {
        let result = self.result(output, code, status);
        if let Some(path) = &self.output_file
            && let Err(err) = result.write_output_file(path)
        {
            self.caller
                .printerr_literal(&format!("runbox: can't write --output-file: {err}\n"));
        }
        if self.json {
            self.caller
                .print_literal(&format!("{}\n", result.to_json()));
        } else if !result.output.is_empty() {
            self.caller.print_literal(&result.output);
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
    id_label: gtk::Label,
    state: Rc<RefCell<State>>,
    watcher: Watcher,
}

impl RunBox {
    /// A popup. `id` makes it a request window: the id shows in its title and
    /// at the left of its bar.
    pub fn new(app: &gtk::Application, id: Option<&str>, watcher: Watcher) -> Self {
        install_css();

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title(window_title(None))
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

        // The request id, so the person can tell which window is which. Hidden in
        // the plain popup.
        let id_label = gtk::Label::builder()
            .valign(gtk::Align::Start)
            .margin_top(INPUT_MARGIN)
            .max_width_chars(ID_MAX_CHARS)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .visible(false)
            .css_classes(["runbox-id"])
            .build();

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        bar.add_css_class("runbox-bar");
        bar.append(&id_label);
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
            id_label,
            state: Rc::default(),
            watcher,
        };
        if let Some(id) = id {
            runbox.set_id(id);
        }
        runbox.connect_signals();
        runbox
    }

    /// Show `id` in the title ("Runbox [id]") and the bar.
    pub fn set_id(&self, id: &str) {
        self.state.borrow_mut().id = Some(id.to_string());
        self.window.set_title(Some(&window_title(Some(id))));
        self.id_label.set_label(&format!("[{id}]"));
        self.id_label.set_tooltip_text(Some(id));
        self.id_label.set_visible(true);
    }

    /// Open `offset` px right of and below the centred spot, so request windows
    /// don't sit exactly on top of each other. X11 only: Wayland doesn't let an
    /// app place its windows, so there the compositor decides.
    pub fn place_at_offset(&self, offset: (i32, i32)) {
        self.window.connect_map(move |window| {
            let window = window.clone();
            // After the window manager's own placement, which happens on map.
            glib::idle_add_local_once(move || move_on_x11(&window, offset));
        });
    }

    fn notify(&self, event: Event) {
        let id = self.state.borrow().id.clone();
        (self.watcher)(id.as_deref(), event);
    }

    pub fn present(&self) {
        self.window.present();
        if !self.is_running() {
            self.input.grab_focus();
        }
    }

    /// Put a command in the box for the user to review. It never runs without their Enter.
    pub fn request(&self, command: &str, request: Request) -> Result<(), &'static str> {
        {
            let state = self.state.borrow();
            if state.running.is_some() {
                return Err("a command is already running in runbox");
            }
            if state.request.is_some() {
                return Err("another request is already waiting in runbox");
            }
        }
        self.state.borrow_mut().request = Some(request);
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
            let request = this.state.borrow_mut().request.take();
            if let Some(request) = request {
                match request.run.as_ref().map(|run| run.header.clone()) {
                    // Closed mid-run: the command is killed. Send what it printed so far.
                    Some(header) => {
                        let buffer = buffer_text(&this.terminal);
                        let output = run_so_far(&buffer, &header).unwrap_or("");
                        request.answer(
                            output,
                            request_exit::NOT_RUN,
                            "window closed before the command finished",
                            Status::Killed,
                        );
                    }
                    None => request.answer("", request_exit::NOT_RUN, "declined", Status::Declined),
                }
            }
            this.notify(Event::Closed);
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
            request.run = Some(Started {
                header: header_line(&command),
                command: command.clone(),
                at: unix_now(),
            });
        }
        self.notify(Event::Running);

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
                    // Nothing ran, so the caller hears that now instead of waiting
                    // for the window to close.
                    let request = this.state.borrow_mut().request.take();
                    if let Some(request) = request {
                        this.status.remove_css_class("requested");
                        request.answer(
                            "",
                            request_exit::NOT_RUN,
                            &format!("could not start: {err}"),
                            Status::Declined,
                        );
                    }
                    this.notify(Event::Done);
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
        self.notify(Event::Done);
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
            .and_then(|r| r.run.as_ref().map(|run| run.header.clone()));
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
        let status = match outcome {
            Outcome::Exited(_) => Status::Ran,
            Outcome::Signaled(_) => Status::Killed,
        };
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
                    Some(output) => request.answer(output, code, &note, status.clone()),
                    None => request.answer(
                        &buffer,
                        code,
                        &format!("{note} (couldn't find the run's end; whole window sent)"),
                        status.clone(),
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

/// "Runbox" for the plain popup, "Runbox [id]" for a request window.
fn window_title(id: Option<&str>) -> String {
    match id {
        Some(id) => format!("Runbox [{id}]"),
        None => "Runbox".to_string(),
    }
}

/// The newest run in `buffer` that starts with `header`, up to the end of the
/// buffer. For a run cut short, which never got its "[exit N]" line.
fn run_so_far<'a>(buffer: &'a str, header: &str) -> Option<&'a str> {
    let mut start = None;
    let mut offset = 0;
    for line in buffer.split_inclusive('\n') {
        if line.trim_end() == header {
            start = Some(offset);
        }
        offset += line.len();
    }
    start.map(|start| &buffer[start..])
}

/// Move `window` to the centre of its monitor plus `offset`. Does nothing off X11.
fn move_on_x11(window: &gtk::ApplicationWindow, offset: (i32, i32)) {
    use gdk4_x11::{X11Display, X11Surface};
    let Some(surface) = window.surface() else {
        return;
    };
    let (Some(x11_surface), Some(x11_display)) = (
        surface.downcast_ref::<X11Surface>(),
        surface.display().downcast_ref::<X11Display>().cloned(),
    ) else {
        return;
    };
    let Some(monitor) = surface.display().monitor_at_surface(&surface) else {
        return;
    };
    let area = monitor.geometry();
    let (x, y) = centred_at_offset(
        (area.x(), area.y(), area.width(), area.height()),
        (window.width().max(WIDTH), window.height().max(HEIGHT)),
        offset,
    );
    // GTK 4 has no call to move a window, so ask X directly. libX11 is already
    // loaded (GDK's X11 backend uses it), so opening it is cheap.
    let Ok(xlib) = gdk4_x11::x11::xlib::Xlib::open() else {
        return;
    };
    // SAFETY: the display and window ids come from GDK for a mapped surface,
    // on GTK's own (main) thread. XMoveWindow only queues a request; GDK
    // flushes it with its own traffic.
    unsafe {
        let xdisplay = x11_display.xdisplay();
        (xlib.XMoveWindow)(xdisplay as _, x11_surface.xid(), x, y);
    }
}

/// Top-left corner that centres a `size` window on a monitor (`area` is x, y,
/// width, height), moved by `offset`, kept on the monitor.
fn centred_at_offset(
    area: (i32, i32, i32, i32),
    size: (i32, i32),
    offset: (i32, i32),
) -> (i32, i32) {
    let (ax, ay, aw, ah) = area;
    let (w, h) = size;
    let place = |start: i32, span: i32, len: i32, by: i32| {
        let centred = start + (span - len) / 2 + by;
        centred.clamp(start, (start + span - len).max(start))
    };
    (place(ax, aw, w, offset.0), place(ay, ah, h, offset.1))
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
    fn run_so_far_takes_the_newest_run_to_the_end() {
        let buffer = "$ sleep 9\n[exit 0]\n\n$ sleep 9\npartial\nmore";
        assert_eq!(
            run_so_far(buffer, "$ sleep 9"),
            Some("$ sleep 9\npartial\nmore")
        );
        // Nothing printed yet but the header line.
        assert_eq!(run_so_far("$ cat\n", "$ cat"), Some("$ cat\n"));
    }

    #[test]
    fn run_so_far_without_the_header_is_none() {
        assert_eq!(run_so_far("", "$ ls"), None);
        assert_eq!(run_so_far("$ lsblk\n", "$ ls"), None);
    }

    #[test]
    fn titles_carry_the_request_id() {
        assert_eq!(window_title(None), "Runbox");
        assert_eq!(window_title(Some("mac-wipe")), "Runbox [mac-wipe]");
        assert_eq!(window_title(Some("r1")), "Runbox [r1]");
    }

    #[test]
    fn windows_cascade_from_the_centre() {
        let screen = (0, 0, 1600, 1000);
        assert_eq!(centred_at_offset(screen, (760, 440), (0, 0)), (420, 280));
        assert_eq!(centred_at_offset(screen, (760, 440), (32, 32)), (452, 312));
        assert_eq!(centred_at_offset(screen, (760, 440), (96, 96)), (516, 376));
        // A second monitor to the right keeps its own origin.
        assert_eq!(
            centred_at_offset((1600, 0, 1920, 1080), (760, 440), (32, 32)),
            (1600 + 580 + 32, 320 + 32)
        );
    }

    #[test]
    fn windows_stay_on_a_small_monitor() {
        // An offset that would push the window off the right and bottom edges.
        assert_eq!(
            centred_at_offset((0, 0, 800, 480), (760, 440), (96, 96)),
            (40, 40)
        );
        // A window bigger than the monitor sits at its top left.
        assert_eq!(
            centred_at_offset((0, 0, 640, 400), (760, 440), (32, 32)),
            (0, 0)
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
