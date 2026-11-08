#!/usr/bin/env bash
# Real X11 end-to-end test for runbox: a private Xvfb display, Cinnamon's window
# manager (muffin), and xdotool sending real key presses and clicks.
#
#   tests/gui_xvfb.sh [path/to/runbox]      (default: target/release/runbox)
#
# RUNBOX_STARTUP_BUDGET_MS sets the startup-time limit (default 400).
#
# Everything runs inside its own D-Bus session, so it never touches the desktop
# you're logged into. Exit 0 = all checks passed, 1 = a check failed,
# 77 = SKIPPED because a tool is missing (that is NOT a pass).
set -uo pipefail

if [[ "${1:-}" != "--inner" ]]; then
    BIN=$(realpath "${1:-target/release/runbox}")
    for tool in Xvfb xdotool xprop xwd wmctrl muffin dbus-run-session gio desktop-file-validate; do
        command -v "$tool" >/dev/null || { echo "SKIPPED: $tool not installed"; exit 77; }
    done
    [[ -x "$BIN" ]] || { echo "FAIL: no binary at $BIN (cargo build --release first)"; exit 1; }
    SCRATCH=${TMPDIR:-/tmp}
    [[ -d "$SCRATCH" ]] || SCRATCH=/tmp
    WORK=$(mktemp -d "$SCRATCH/runbox-gui.XXXXXX")
    DISP=":$((90 + RANDOM % 9))"
    Xvfb "$DISP" -screen 0 1600x1000x24 -nolisten tcp >"$WORK/xvfb.log" 2>&1 &
    XPID=$!
    sleep 0.5
    # muffin refuses to start unless the session says it's X11 (CI has no session).
    DISPLAY="$DISP" XDG_SESSION_TYPE=x11 GDK_BACKEND=x11 GTK_USE_PORTAL=0 \
        dbus-run-session -- "$0" --inner "$BIN" "$WORK"
    rc=$?
    kill "$XPID" 2>/dev/null; wait "$XPID" 2>/dev/null
    rm -rf "$WORK"
    exit $rc
fi

BIN=$2
WORK=$3
REPO=$(cd "$(dirname "$0")/.." && pwd)
APP_ID=io.github.ajonesfl2600.runbox
OLD_APP_ID=dev.ajones.runbox
DUMP="$WORK/dump.txt"
PASS=0
FAIL=0

ok()   { echo "PASS  $1"; PASS=$((PASS + 1)); }
bad()  {
    echo "FAIL  $1"; FAIL=$((FAIL + 1))
    echo "      terminal showed:"; grep -v '^\s*$' "$DUMP" 2>/dev/null | tail -6 | sed 's/^/      | /'
    if [[ -n "${REQ_OUT:-}" && -e "$REQ_OUT" ]]; then
        echo "      caller got (rc=${RC:-?}, $(wc -l <"$REQ_OUT") lines):"
        head -4 "$REQ_OUT" | cat -A | cut -c1-100 | sed 's/^/      > /'
        echo "      caller stderr: $(cat "$REQ_ERR" 2>/dev/null)"
    fi
}
check() { if eval "$2"; then ok "$1"; else bad "$1"; fi; }

# Install into a throwaway home, twice to prove a rerun is safe. Every runbox
# started below finds its icon there. The space in the path is on purpose: the
# installed .desktop file has to quote it.
TEST_HOME="$WORK/home dir"
INSTALLED_DESKTOP="$TEST_HOME/.local/share/applications/$APP_ID.desktop"
export XDG_DATA_HOME="$TEST_HOME/.local/share"
install_twice() {
    for _ in 1 2; do
        HOME="$TEST_HOME" "$REPO/install.sh" "$BIN" >>"$WORK/install.log" 2>&1 || return 1
    done
}
check "install.sh runs twice cleanly" 'install_twice'
check "install.sh put the binary, icon and .desktop in the home" \
    '[[ -x "$TEST_HOME/.local/bin/runbox" \
        && -f "$XDG_DATA_HOME/icons/hicolor/scalable/apps/$APP_ID.svg" \
        && -f "$XDG_DATA_HOME/applications/$APP_ID.desktop" ]]'

# The installed .desktop runs the installed binary by its full path, not "runbox"
# off PATH. Every other line is the repo's file as it is.
check "installed .desktop has Exec= set to the installed binary" \
    'grep -qxF "Exec=\"$TEST_HOME/.local/bin/runbox\"" "$INSTALLED_DESKTOP"'
check "installed .desktop matches the repo's file apart from Exec=" \
    'diff <(grep -v "^Exec=" "$REPO/data/$APP_ID.desktop") <(grep -v "^Exec=" "$INSTALLED_DESKTOP") >/dev/null'
check "installed .desktop passes desktop-file-validate" \
    'desktop-file-validate "$INSTALLED_DESKTOP" >>"$WORK/install.log" 2>&1'

# Upgrade from the old app ID: install.sh removes the old .desktop and icon, so the
# menu doesn't show two entries. Another app's files in the same folders stay put.
UP_HOME="$WORK/home-upgrade"
UP_DATA="$UP_HOME/share"
upgrade_install() {
    mkdir -p "$UP_DATA/applications" "$UP_DATA/icons/hicolor/scalable/apps" || return 1
    printf '[Desktop Entry]\nType=Application\nName=Runbox\nExec=runbox\n' >"$UP_DATA/applications/$OLD_APP_ID.desktop"
    printf '<svg xmlns="http://www.w3.org/2000/svg"/>\n' >"$UP_DATA/icons/hicolor/scalable/apps/$OLD_APP_ID.svg"
    printf '[Desktop Entry]\nType=Application\nName=Other\nExec=other\n' >"$UP_DATA/applications/other.app.desktop"
    printf '<svg xmlns="http://www.w3.org/2000/svg"/>\n' >"$UP_DATA/icons/hicolor/scalable/apps/other.app.svg"
    HOME="$UP_HOME" XDG_DATA_HOME="$UP_DATA" "$REPO/install.sh" "$BIN" >>"$WORK/install.log" 2>&1
}
check "install.sh over an old install runs cleanly" 'upgrade_install'
check "install.sh removed the old $OLD_APP_ID .desktop and icon" \
    '[[ ! -e "$UP_DATA/applications/$OLD_APP_ID.desktop" && ! -e "$UP_DATA/icons/hicolor/scalable/apps/$OLD_APP_ID.svg" ]]'
check "install.sh put the new $APP_ID .desktop and icon in place of them" \
    '[[ -f "$UP_DATA/applications/$APP_ID.desktop" ]] && cmp -s "$REPO/data/icons/hicolor/scalable/apps/$APP_ID.svg" "$UP_DATA/icons/hicolor/scalable/apps/$APP_ID.svg"'
check "install.sh left another app's .desktop and icon alone" \
    '[[ -f "$UP_DATA/applications/other.app.desktop" && -f "$UP_DATA/icons/hicolor/scalable/apps/other.app.svg" ]]'
check "only one Runbox entry in the upgraded menu" \
    '[[ "$(grep -lx "Name=Runbox" "$UP_DATA/applications/"*.desktop | wc -l)" == 1 ]]'

# A home with every character the Exec= quoting rules care about still gives a valid file.
ODD_HOME="$WORK/odd \"home\" \$x \`y\` 100% \\z"
check "install.sh quotes an awkward home path in Exec= (still valid)" \
    'mkdir -p "$ODD_HOME" && HOME="$ODD_HOME" XDG_DATA_HOME="$ODD_HOME/share" "$REPO/install.sh" "$BIN" >>"$WORK/install.log" 2>&1 \
        && desktop-file-validate "$ODD_HOME/share/applications/$APP_ID.desktop" >>"$WORK/install.log" 2>&1'

# A relative binary path means the caller's folder, not the repo.
REL_HOME="$WORK/home-rel"
mkdir -p "$WORK/elsewhere/build"
cp "$BIN" "$WORK/elsewhere/build/runbox"
check "install.sh run from another folder takes a relative path from there" \
    '(cd "$WORK/elsewhere" && HOME="$REL_HOME" XDG_DATA_HOME="$REL_HOME/share" "$REPO/install.sh" build/runbox >>"$WORK/install.log" 2>&1) \
        && cmp -s "$BIN" "$REL_HOME/.local/bin/runbox"'
NONE_HOME="$WORK/home-none"
(cd "$WORK/elsewhere" && HOME="$NONE_HOME" XDG_DATA_HOME="$NONE_HOME/share" "$REPO/install.sh" build/missing >"$WORK/missing.out" 2>&1)
MISSING_RC=$?
check "install.sh refuses a missing binary (exit 1, installs nothing)" \
    '[[ "$MISSING_RC" == 1 && ! -e "$NONE_HOME" ]] && grep -qF "$WORK/elsewhere/build/missing" "$WORK/missing.out"'

muffin --replace >"$WORK/muffin.log" 2>&1 &
MUFFIN=$!
for _ in $(seq 50); do wmctrl -m >/dev/null 2>&1 && break; sleep 0.1; done

APP_PID=""
WID=""

launch() {
    rm -f "$DUMP"
    local t0 t1
    t0=$(date +%s%N)
    RUNBOX_DUMP="$DUMP" SHELL=/bin/bash "$BIN" >"$WORK/app.log" 2>&1 &
    APP_PID=$!
    WID=""
    for _ in $(seq 500); do
        WID=$(xdotool search --onlyvisible --name '^Runbox$' 2>/dev/null | head -1)
        [[ -n "$WID" ]] && break
        sleep 0.01
    done
    t1=$(date +%s%N)
    STARTUP_MS=$(( (t1 - t0) / 1000000 ))
    sleep 0.3
}

window_count() { xdotool search --onlyvisible --name '^Runbox$' 2>/dev/null | wc -l; }
app_alive() { kill -0 "$APP_PID" 2>/dev/null; }

# Wait for the dump (written when each command ends) to contain a pattern.
wait_for() {
    local pattern=$1 limit=${2:-50}
    for _ in $(seq "$limit"); do
        grep -qF -- "$pattern" "$DUMP" 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

# "R G B" of one pixel in an xwd dump. The header is big-endian; the pixels are
# 32-bit little-endian (B G R x), which is what Xvfb at depth 24 writes.
xwd_pixel() {
    local file=$1 x=$2 y=$3 h
    read -r -a h < <(od -An -v -tu4 --endian=big -N100 "$file" | tr -s ' \n' ' ')
    (( h[7] == 0 && h[11] == 32 )) || { echo "unexpected xwd format"; return 1; }
    local offset=$(( h[0] + h[19] * 12 + y * h[12] + x * 4 ))
    od -An -tu1 -j "$offset" -N3 "$file" | awk '{ print $3, $2, $1 }'
}

# The border must be colour: black, white and grey backgrounds all have r = g = b.
is_colour() {
    local r g b
    read -r r g b <<<"$1"
    local hi=$(( r > g ? (r > b ? r : b) : (g > b ? g : b) ))
    local lo=$(( r < g ? (r < b ? r : b) : (g < b ? g : b) ))
    (( hi - lo > 60 ))
}

type_cmd() { rm -f "$DUMP"; xdotool type --delay 8 -- "$1"; }

# ---------------------------------------------------------------------------
# Arguments that must answer and exit without ever starting a window.
timeout 5 "$BIN" --help >/dev/null 2>&1; HELP_RC=$?
timeout 5 "$BIN" --bogus >/dev/null 2>&1; BOGUS_RC=$?
timeout 5 "$BIN" --request >/dev/null 2>&1; NOARG_RC=$?
check "--help exits 0 without a window" '[[ "$HELP_RC" == 0 && "$(window_count)" == 0 ]]'
check "bad arguments exit 2 without a window" '[[ "$BOGUS_RC" == 2 && "$NOARG_RC" == 2 && "$(window_count)" == 0 ]]'

# ---------------------------------------------------------------------------
launch
check "window appears (startup ${STARTUP_MS} ms)" '[[ -n "$WID" ]]'
# A shared CI box is slower and noisier than a desktop, so CI may raise the budget.
STARTUP_BUDGET_MS=${RUNBOX_STARTUP_BUDGET_MS:-400}
check "startup under ${STARTUP_BUDGET_MS} ms (${STARTUP_MS} ms)" '(( STARTUP_MS < STARTUP_BUDGET_MS ))'
check "window is borderless (frame extents 0)" \
    '[[ "$(xprop -id "$WID" _NET_FRAME_EXTENTS 2>/dev/null)" =~ "= 0, 0, 0, 0" || "$(xprop -id "$WID" _NET_FRAME_EXTENTS 2>/dev/null)" == *"not found"* ]]'
check "window has keyboard focus on open" '[[ "$(xdotool getactivewindow 2>/dev/null)" == "$WID" ]]'

# A 1px accent border on all four edges, so the window shows on black and on white.
eval "$(xdotool getwindowgeometry --shell "$WID")"
# The first frame can land a little after the window maps; until then it reads black.
for _ in $(seq 30); do
    xwd -silent -id "$WID" -out "$WORK/window.xwd"
    TOP=$(xwd_pixel "$WORK/window.xwd" $((WIDTH / 2)) 0)
    [[ "$TOP" != "0 0 0" ]] && break
    sleep 0.1
done
BOTTOM=$(xwd_pixel "$WORK/window.xwd" $((WIDTH / 2)) $((HEIGHT - 1)))
LEFT=$(xwd_pixel "$WORK/window.xwd" 0 $((HEIGHT / 2)))
RIGHT=$(xwd_pixel "$WORK/window.xwd" $((WIDTH - 1)) $((HEIGHT / 2)))
INSIDE=$(xwd_pixel "$WORK/window.xwd" $((WIDTH / 2)) 1)
echo "      edge pixels: top=$TOP bottom=$BOTTOM left=$LEFT right=$RIGHT, 1px in=$INSIDE"
check "border: all four edges are one colour" '[[ "$TOP" == "$BOTTOM" && "$TOP" == "$LEFT" && "$TOP" == "$RIGHT" ]]'
check "border: edge colour is not black, white or grey" 'is_colour "$TOP"'
check "border: exactly 1px (the next pixel in is not the border)" '[[ -n "$INSIDE" && "$INSIDE" != "$TOP" ]]'

# Inside the border, in the bar's padding above the entry, nothing but the window's
# own background is drawn. Without GTK's "background" class that spot is 0 0 0.
# The same background shows in the bottom-left corner, under the status line.
# GTK fades the first frame in, so give the paint up to 3 s to settle on one colour.
for _ in $(seq 30); do
    xwd -silent -id "$WID" -out "$WORK/background.xwd"
    BAR_BG=$(xwd_pixel "$WORK/background.xwd" $((WIDTH / 2)) 3)
    CORNER_BG=$(xwd_pixel "$WORK/background.xwd" 2 $((HEIGHT - 2)))
    [[ "$BAR_BG" == "$CORNER_BG" ]] && break
    sleep 0.1
done
echo "      background pixels: bar=$BAR_BG bottom-left=$CORNER_BG"
check "window background: bar area is painted, not 0 0 0" '[[ -n "$BAR_BG" && "$BAR_BG" != "0 0 0" ]]'
check "window background: bar area matches the window corner (both the background)" \
    '[[ "$BAR_BG" == "$CORNER_BG" ]] && ! is_colour "$BAR_BG"'

check "window carries an icon (_NET_WM_ICON)" \
    '[[ "$(xprop -id "$WID" _NET_WM_ICON 2>/dev/null)" == *"Icon ("* ]]'
check "WM_CLASS matches the .desktop StartupWMClass" \
    'xprop -id "$WID" WM_CLASS | grep -qF "\"$(sed -n "s/^StartupWMClass=//p" "$REPO/data/$APP_ID.desktop")\""'

# Output shows in the window
type_cmd "echo hello-runbox"; xdotool key Return
check "Enter runs the command and output shows" 'wait_for "hello-runbox" && grep -qF "[exit 0]" "$DUMP"'

# apt-style [Y/n] prompt answered in the popup
type_cmd "printf 'Continue? [Y/n] '; read a; echo got:\$a"; xdotool key Return
sleep 0.8
xdotool type --delay 8 "y"; xdotool key Return
check "a [Y/n] prompt is answered in the window" 'wait_for "got:y"'

# sudo-style hidden password prompt
type_cmd "read -s -p 'Password: ' p; echo; echo len:\${#p}"; xdotool key Return
sleep 0.8
xdotool type --delay 8 "hunter2"; xdotool key Return
check "hidden password prompt works (len 7)" 'wait_for "len:7"'
check "typed password is not echoed" '! grep -qF "hunter2" "$DUMP"'

# Ctrl+C interrupts the running command
type_cmd "sleep 30; echo should-not-print"; xdotool key Return
sleep 0.8
xdotool key ctrl+c
check "Ctrl+C interrupts the command" 'wait_for "[killed by signal 2]" 30 && ! grep -qxF "should-not-print" "$DUMP"'

# A failing command reports its exit code
type_cmd "exit 3"; xdotool key Return
check "non-zero exit is reported" 'wait_for "[exit 3]"'

# Other ways to run
type_cmd "echo via-alt-r"; xdotool key alt+r
check "Alt+R runs" 'wait_for "via-alt-r"'
type_cmd "echo via-ctrl-enter"; xdotool key ctrl+Return
check "Ctrl+Enter runs" 'wait_for "via-ctrl-enter"'

type_cmd "echo via-click"
eval "$(xdotool getwindowgeometry --shell "$WID")"
xdotool mousemove $((X + WIDTH - 32)) $((Y + 22)) sleep 0.2 click 1
check "clicking the Run button runs" 'wait_for "via-click"'

# Super+R again: the running instance comes back, no second window
"$BIN" >/dev/null 2>&1 &
SECOND=$!
sleep 1.5
check "second launch reuses the window (1 window)" '[[ "$(window_count)" == 1 ]]'
check "second launch process exits" '! kill -0 "$SECOND" 2>/dev/null'

# Esc while a command runs goes to the program, not the window
type_cmd "sleep 2"; xdotool key Return
sleep 0.5
xdotool key Escape
sleep 0.3
check "Esc while running does not close" 'app_alive && [[ "$(window_count)" == 1 ]]'
wait_for "[exit 0]" 40 || true

# Ctrl+W while running needs a second press, and kills the command
type_cmd "sleep 31.7"; xdotool key Return
sleep 0.8
xdotool key ctrl+w
sleep 0.4
check "first Ctrl+W while running keeps the window" 'app_alive'
xdotool key ctrl+w
sleep 1
check "second Ctrl+W closes the window" '! app_alive'
check "second Ctrl+W kills the running command" '! pgrep -f "sleep 31.7" >/dev/null'

# Esc when idle closes
launch
xdotool key Escape
sleep 0.8
check "Esc when idle closes" '! app_alive'

# Ctrl+Q when idle closes
launch
xdotool key ctrl+q
sleep 0.8
check "Ctrl+Q when idle closes" '! app_alive'

# ---------------------------------------------------------------------------
# runbox --request: a caller puts a command in the box; it runs only on Enter.
REQ_OUT="$WORK/req.out"; REQ_ERR="$WORK/req.err"

request_bg() {
    rm -f "$REQ_OUT" "$REQ_ERR"
    RUNBOX_DUMP="$DUMP" SHELL=/bin/bash "$BIN" --request "$1" >"$REQ_OUT" 2>"$REQ_ERR" &
    RPID=$!
    for _ in $(seq 300); do
        [[ -n "$(xdotool search --onlyvisible --name '^Runbox$' 2>/dev/null)" ]] && break
        sleep 0.01
    done
    sleep 0.4
}

# Wait for the requesting process to exit; sets RC (999 = still waiting).
wait_rc() {
    local limit=${1:-50}
    RC=999
    for _ in $(seq "$limit"); do
        if ! kill -0 "$RPID" 2>/dev/null; then wait "$RPID"; RC=$?; return; fi
        sleep 0.1
    done
}

window_pid() { pgrep -nx runbox; }

# Cold start: no window open, the request opens one and gets the answer back.
check "no window before the first request" '[[ "$(window_count)" == 0 ]]'
request_bg "echo req-out; printf 'Name? '; read n; echo hi:\$n"
xdotool key Return
sleep 0.8
xdotool type --delay 8 "bob"; xdotool key Return
wait_rc
check "request: exit code comes back (0)" '[[ "$RC" == 0 ]]'
check "request: output comes back" 'grep -qx "req-out" "$REQ_OUT" && grep -qx "hi:bob" "$REQ_OUT"'
check "request: output starts with the command line" 'head -1 "$REQ_OUT" | grep -qF "$ echo req-out"'
check "request: window stays open afterwards" '[[ "$(window_count)" == 1 ]]'

request_bg "exit 4"; xdotool key Return; wait_rc
check "request: command's own exit code (4)" '[[ "$RC" == 4 ]]'

request_bg "read -s -p 'Password: ' p; echo; echo len:\${#p}"
xdotool key Return; sleep 0.8
xdotool type --delay 8 "hunter2"; xdotool key Return; wait_rc
check "request: hidden password prompt works" '[[ "$RC" == 0 ]] && grep -qx "len:7" "$REQ_OUT"'
check "request: password never reaches the caller" '! grep -qF hunter2 "$REQ_OUT" "$REQ_ERR"'

request_bg "seq 1 3000"; xdotool key Return; wait_rc 100
check "request: long output includes scrollback (3000 lines)" \
    '[[ "$RC" == 0 ]] && grep -qx 1 "$REQ_OUT" && grep -qx 3000 "$REQ_OUT" && [[ "$(grep -cxE "[0-9]+" "$REQ_OUT")" == 3000 ]]'

request_bg "sleep 30"; xdotool key Return; sleep 0.8; xdotool key ctrl+c; wait_rc
check "request: Ctrl+C gives 130" '[[ "$RC" == 130 ]]'

# Nothing runs without Enter: decline with Esc.
request_bg "touch $WORK/must-not-exist"
FIRST=$RPID
RUNBOX_DUMP="$DUMP" "$BIN" --request "echo second" >"$WORK/busy.out" 2>"$WORK/busy.err"
BUSY_RC=$?
check "request: a second request while one waits is refused (75)" '[[ "$BUSY_RC" == 75 ]] && grep -q busy "$WORK/busy.err"'
RPID=$FIRST
xdotool key Escape; wait_rc
check "request: Esc declines (125)" '[[ "$RC" == 125 ]] && grep -q declined "$REQ_ERR"'
check "request: declined command did not run" '[[ ! -e "$WORK/must-not-exist" ]]'
check "request: Esc closed the window" '[[ "$(window_count)" == 0 ]]'

# Closing mid-run answers the caller instead of leaving it hanging.
request_bg "sleep 31.3"; xdotool key Return; sleep 0.8
xdotool key ctrl+w; sleep 0.3; xdotool key ctrl+w; wait_rc
check "request: Ctrl+W twice mid-run answers 125" '[[ "$RC" == 125 ]] && grep -q "closed before" "$REQ_ERR"'
check "request: and the command was killed" '! pgrep -f "sleep 31.3" >/dev/null'

# ---------------------------------------------------------------------------
# Launch the way the menu does: through the INSTALLED .desktop file, with a PATH
# that doesn't include ~/.local/bin.
check "no window before the .desktop launch" '[[ "$(window_count)" == 0 ]]'
env PATH=/usr/bin:/bin gio launch "$INSTALLED_DESKTOP" >"$WORK/gio.log" 2>&1
GIO_RC=$?
WID=""
for _ in $(seq 500); do
    WID=$(xdotool search --onlyvisible --name '^Runbox$' 2>/dev/null | head -1)
    [[ -n "$WID" ]] && break
    sleep 0.01
done
sleep 0.3
DESKTOP_PID=$(pgrep -nx runbox)
check "gio launch of the installed .desktop opens a runbox window" '[[ "$GIO_RC" == 0 && -n "$WID" ]]'
check ".desktop launch runs the installed binary" \
    '[[ -n "$DESKTOP_PID" && "$(readlink "/proc/$DESKTOP_PID/exe")" == "$TEST_HOME/.local/bin/runbox" ]]'
check ".desktop-launched window carries the icon (_NET_WM_ICON)" \
    '[[ -n "$WID" && "$(xprop -id "$WID" _NET_WM_ICON 2>/dev/null)" == *"Icon ("* ]]'
[[ -n "$WID" ]] && xdotool windowactivate --sync "$WID" key Escape 2>/dev/null
sleep 0.8
check ".desktop-launched window closes on Esc" '[[ "$(window_count)" == 0 ]]'
[[ -n "$DESKTOP_PID" ]] && kill "$DESKTOP_PID" 2>/dev/null

kill "$MUFFIN" 2>/dev/null
app_alive && kill "$APP_PID"
echo "----"
echo "$PASS passed, $FAIL failed"
(( FAIL == 0 ))
