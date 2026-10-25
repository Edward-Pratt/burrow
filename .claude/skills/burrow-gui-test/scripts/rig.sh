#!/bin/sh
# Headless test rig for burrow-gtk: Xvfb on :99, an optional throwaway
# sshd, and Burrow pointed at a scratch config.
#
#   rig.sh start [--sshd]   Xvfb, plus a test sshd on 127.0.0.1:2222 (host "testbox")
#   rig.sh app [--real-ssh] (re)start burrow-gtk; --real-ssh uses ~/.ssh/config and the agent
#   rig.sh shot NAME        screenshot to $BURROW_TEST_DIR/NAME.png
#   rig.sh stop             stop everything and delete the test keys
#
# Everything lives in $BURROW_TEST_DIR. Processes are tracked by pid file:
# never `pkill -f` a pattern that also matches the shell running it.
set -eu
DIR=${BURROW_TEST_DIR:?set BURROW_TEST_DIR to a scratch folder}
REPO=$(cd "$(dirname "$0")/../../../.." && pwd)
mkdir -p "$DIR/cfg/tree" "$DIR/state"

stop_pid() {
    [ -f "$1" ] && kill "$(cat "$1")" 2>/dev/null || true
    rm -f "$1"
}

case "${1:-}" in
start)
    if [ ! -f "$DIR/xvfb.pid" ]; then
        setsid Xvfb :99 -screen 0 1280x800x24 -nolisten tcp >"$DIR/xvfb.log" 2>&1 </dev/null &
        echo $! >"$DIR/xvfb.pid"
    fi
    if [ "${2:-}" = --sshd ]; then
        S="$DIR/sshd"
        mkdir -p "$S"
        [ -f "$S/host_key" ] || ssh-keygen -q -t ed25519 -N '' -f "$S/host_key"
        [ -f "$S/client_key" ] || ssh-keygen -q -t ed25519 -N '' -f "$S/client_key"
        cp "$S/client_key.pub" "$S/authorized_keys"
        chmod 600 "$S/authorized_keys"
        cat >"$S/sshd_config" <<EOF
Port 2222
ListenAddress 127.0.0.1
HostKey $S/host_key
PidFile $S/sshd.pid
AuthorizedKeysFile $S/authorized_keys
PasswordAuthentication no
KbdInteractiveAuthentication no
UsePAM no
StrictModes no
Subsystem sftp /usr/lib/ssh/sftp-server
EOF
        cat >"$S/ssh_config" <<EOF
Host testbox
  HostName 127.0.0.1
  Port 2222
  User $(id -un)
  IdentityFile $S/client_key
  IdentitiesOnly yes
  IdentityAgent none
  UserKnownHostsFile $S/known_hosts
EOF
        /usr/bin/sshd -f "$S/sshd_config" -E "$S/sshd.log"
        sleep 1
        ssh -F "$S/ssh_config" -o StrictHostKeyChecking=accept-new testbox true
        echo "sshd up: alias testbox (it logs in as you, so its home is your real home)"
    fi
    ;;
app)
    stop_pid "$DIR/app.pid"
    cargo build -q -p burrow-gtk --manifest-path "$REPO/Cargo.toml"
    if [ "${2:-}" = --real-ssh ]; then
        ssh_env="SSH_AUTH_SOCK=/run/user/$(id -u)/ssh-agent.socket"
    else
        ssh_env="BURROW_SSH_CONFIG=$DIR/sshd/ssh_config"
    fi
    # no-portals: file dialogs would otherwise open on the real desktop.
    env $ssh_env GDK_DEBUG=no-portals DISPLAY=:99 GDK_BACKEND=x11 \
        BURROW_CONFIG_DIR="$DIR/cfg" BURROW_STATE_DIR="$DIR/state" \
        setsid "$REPO/target/debug/burrow-gtk" >"$DIR/app.log" 2>&1 </dev/null &
    echo $! >"$DIR/app.pid"
    sleep 3
    ;;
shot)
    import -display :99 -window root "$DIR/${2:?name}.png"
    echo "$DIR/$2.png"
    ;;
stop)
    stop_pid "$DIR/app.pid"
    stop_pid "$DIR/sshd/sshd.pid"
    stop_pid "$DIR/xvfb.pid"
    rm -f "$DIR"/sshd/*key* "$DIR/sshd/authorized_keys"
    sleep 1 # let them exit before anything checks
    ss -ltn | grep -q ':2222 ' && echo "warning: something still listens on 2222" || true
    ;;
*)
    sed -n '2,10p' "$0"
    exit 2
    ;;
esac
