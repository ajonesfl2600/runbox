# runbox

A hotkey popup for Linux that runs one shell command in a real terminal, and lets an AI agent or script hand you a command to approve before it runs.

![demo](docs/demo.gif)

## What it does

Press Super+R and a small borderless window opens. Type or paste a command and press Enter. The output shows in a real terminal inside the popup, so `sudo` password prompts, `apt` `[Y/n]` questions, colors, progress bars and Ctrl+C all work. When you're done, Esc closes it.

That's the human side. The other side is for programs.

Any AI coding agent or script that can run a shell command can call `runbox --request "<cmd>"`. The command shows up in the popup for you to read. Nothing runs until you press Enter. You can edit it first, or press Esc to say no. The caller waits, then gets back what the run printed and the command's exit code.

So an agent that hits `sudo apt install ...` doesn't need your password or root. It asks, you look, you decide.

## Quick start

```
sudo apt install libgtk-4-dev libvte-2.91-gtk4-dev
cargo build --release
./install.sh
~/.local/bin/runbox
```

Then bind it to a key (see [Install](#install)).

## Keys

| Key | What it does |
|---|---|
| Enter, Ctrl+Enter, Alt+R, or click **Run** | Run the command |
| Shift+Enter | Start a new line in the command box |
| (while a command runs) | Type answers to its prompts |
| Ctrl+C | Stop the running command |
| Esc | Close when nothing is running (while a command runs, Esc goes to the program) |
| Ctrl+Q, Ctrl+W, Alt+E, or click **Exit** | Close. While a command runs, press it twice within 3 seconds to kill the command and close |
| Ctrl+Shift+C / Ctrl+Shift+V | Copy output / paste into a running command |

The command box wraps a long command so you can read all of it before you run it. It starts 3 lines tall, grows to 6, then scrolls. Wrapping only changes how it looks. What runs is exactly what you typed.

The window has no title bar. To move it, drag the empty space around the command box and buttons, or the status line at the bottom. Dragging in the command box or the terminal selects text, as usual.

Only one window runs at a time. Pressing Super+R again brings back the open one.

## The `--request` contract

```
runbox --request "sudo apt update"
```

1. If runbox isn't open, this opens it.
2. The command goes into the popup with a yellow note asking you to check it.
3. It runs only when you press Enter (or Ctrl+Enter, Alt+R, or click Run). You can edit it first.
4. The caller blocks until the run ends or you close the window.
5. On stdout the caller gets the run's text, starting with the `$ command` line. Scrollback is included, up to 10,000 lines. Anything typed at a hidden prompt, like a `sudo` password, never shows up. A short status line (`runbox: exit 0`, `runbox: declined`) goes to stderr.

Exit codes:

| Code | Meaning |
|---|---|
| the command's own code | It ran. A command killed by a signal gives 128 + the signal, so Ctrl+C gives 130, like bash |
| `125` | Declined with Esc or Exit, or the window closed before the command finished |
| `75` | Busy: another request is waiting or a command is running. Try again later |
| `2` | Bad arguments |

Here's a script that asks before it does anything as root:

```bash
#!/usr/bin/env bash
out=$(runbox --request "sudo apt-get install -y ripgrep")
rc=$?
case $rc in
  0)   echo "installed" ;;
  125) echo "the user said no" ;;
  75)  echo "runbox is busy, try again later" ;;
  *)   echo "failed with exit $rc"; printf '%s\n' "$out" ;;
esac
```

An agent works the same way. Have it run `runbox --request "<cmd>"` through its normal shell tool and read the output and exit code. The call blocks until you act, so give that tool a long enough timeout.

A few things to know. The command runs in your login shell (`$SHELL -lc`), starting in your home folder, not in the caller's folder. If you edit the command, the edited one is what runs, and the first line of the output shows it. The output is plain text with no color codes.

## Install

Build from source. The Rust version is pinned in `rust-toolchain.toml`, so `rustup` picks it up.

```
sudo apt install libgtk-4-dev libvte-2.91-gtk4-dev
cargo build --release
./install.sh
```

`install.sh` needs no sudo and is safe to run again. It puts:

- the binary in `~/.local/bin/runbox`
- the icon in `~/.local/share/icons/hicolor/scalable/apps/`
- `io.github.ajonesfl2600.runbox.desktop` in `~/.local/share/applications/`

The installed `.desktop` file runs the binary by its full path, so the app menu works even if your session's PATH doesn't include `~/.local/bin`. To install a binary from somewhere else, pass its path: `./install.sh ../build/runbox`.

### Bind Super+R on Cinnamon

Set the shortcut's name, command and keys first. Add it to `custom-list` last, because Cinnamon only reloads when that list changes. Pick a `customN` name that isn't taken yet (`gsettings get org.cinnamon.desktop.keybindings custom-list` shows the ones in use).

```
P=org.cinnamon.desktop.keybindings.custom-keybinding:/org/cinnamon/desktop/keybindings/custom-keybindings/custom5/
gsettings set $P name 'Runbox'
gsettings set $P command "$HOME/.local/bin/runbox"
gsettings set $P binding "['<Super>r']"
gsettings set org.cinnamon.desktop.keybindings custom-list "[..., 'custom5']"
```

In the last line, replace `...` with the names that are already in the list.

On other desktops, point any custom shortcut at `~/.local/bin/runbox`.

## Requirements

- GTK 4.18 or newer and VTE 0.80 or newer. Debian 13 ships both.
- A D-Bus session bus (any normal desktop login has one).
- Built and tested on Cinnamon under X11. It should work on other X11 desktops, but I haven't tested them. Wayland is untested too.

## Security notes

runbox is an approval step. It is not a sandbox.

- Once you press Enter, the command runs as you, with your full environment. It can do anything you can.
- The only check is you reading the command. A long one-liner, a `curl ... | sh`, or a base64 blob can hide what it does. If you can't tell what a command does, press Esc.
- The caller gets back everything the run printed. If the command prints a secret, the caller sees it. Hidden prompt input, like a `sudo` password, isn't printed, so it doesn't go back.
- Any program running as your user can send a request. That doesn't give it anything new, since such a program can already run commands as you. What runbox adds is a way for it to ask for things it can't do alone, like `sudo`, with you in the loop.
- Nothing runs without a key press or click in the window. The GUI test checks that a declined command never runs.

## How it's tested

- `cargo test` runs the unit tests.
- `tests/gui_xvfb.sh` is an end-to-end test on a real X11 display. It starts a private Xvfb server, runs muffin (Cinnamon's window manager) and drives runbox with xdotool: real key presses and real clicks. It answers a `[Y/n]` prompt, types a hidden password and checks it's never shown or sent back, presses Ctrl+C, checks every way to run and close, and runs the whole `--request` flow, including decline, busy and a 3,000-line output. It clicks Exit idle and mid-run, wraps a 300-character command and checks it runs byte for byte, and drags the window by its bar and its status line, comparing the window's position before and after. It also checks that a drag in the command box or the terminal selects text and leaves the window where it was. It also installs into a throwaway home, launches through the installed `.desktop` file, and checks the window's icon, its 1px border and its background from a screenshot.
- The GUI test needs `xvfb`, `xdotool`, `x11-utils`, `x11-apps`, `wmctrl`, `muffin`, `dbus`, `desktop-file-utils` and `libglib2.0-bin`. If one is missing it exits 77, which means skipped. A skip is not a pass.
- CI runs formatting, clippy, the unit tests, a release build and the GUI test on every pull request, inside a Debian 13 container.

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT), at your option.

Unless you explicitly state otherwise, any contribution you submit for inclusion in runbox is dual licensed as above, without any additional terms or conditions.

## Contributing

Bug reports and ideas are welcome as [issues](https://github.com/ajonesfl2600/runbox/issues). Pull requests are welcome too: a good one gets applied with you credited as its author, and it lands in the next release. See the [wiki](https://github.com/ajonesfl2600/runbox/wiki) for how to build and test.
