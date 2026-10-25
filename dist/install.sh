#!/bin/bash
# Installs Burrow from source for one user, into ~/.local, and keeps it up
# to date: a daily systemd user timer runs this again as `burrow-update`,
# which builds and installs only when the branch has moved.
#
#   dist/install.sh                  install from main
#   burrow-update                    update now
#   burrow-update --branch NAME      follow another branch from now on
#
# It builds and installs whatever lands on that branch without asking, so
# only follow a branch whose pushers you trust.
set -euo pipefail

url=https://github.com/Edward-Pratt/burrow.git
data=${XDG_DATA_HOME:-$HOME/.local/share}
src=$data/burrow/src
bin=$HOME/.local/bin
# The core finds agents in lib/burrow beside the folder it runs from.
lib=$HOME/.local/lib/burrow
units=$data/systemd/user

notify() {
  # Under the timer nobody reads the output, so say it on the desktop.
  if [ ! -t 1 ] && command -v notify-send >/dev/null; then
    notify-send --app-name=Burrow --icon=utilities-terminal "$@" || true
  fi
}
trap '[ $? -eq 0 ] || notify "Burrow update failed" "See journalctl --user -u burrow-update"' EXIT

# Writes beside the destination, then renames, so a running program keeps
# its old file and nothing sees a half-written one.
put() { # mode source destination
  install -Dm"$1" "$2" "$3.new"
  mv -f "$3.new" "$3"
}

edit() { # sed-script source destination
  mkdir -p "${3%/*}"
  sed "$1" "$2" >"$3.new"
  chmod 644 "$3.new"
  mv -f "$3.new" "$3"
}

branch=
while [ $# -gt 0 ]; do
  case $1 in
    --branch)
      branch=${2:?--branch needs a branch name}
      shift 2
      ;;
    *)
      echo "usage: ${0##*/} [--branch NAME]" >&2
      exit 2
      ;;
  esac
done

mkdir -p "$data/burrow"
exec 9>"$data/burrow/update.lock"
if ! flock -n 9; then
  echo "Another Burrow update is running."
  exit 0
fi

if [ ! -d "$src/.git" ]; then
  git clone --quiet "$url" "$src"
fi
# The clone remembers which branch it follows.
branch=${branch:-$(git -C "$src" config burrow.branch || echo main)}
git check-ref-format --branch "$branch" >/dev/null
git -C "$src" config burrow.branch "$branch"
git -C "$src" fetch --quiet origin "$branch"
commit=$(git -C "$src" rev-parse FETCH_HEAD)
# Compared with what was installed, not what was checked out, so a build
# that failed is tried again next time.
if [ "$(cat "$lib/commit" 2>/dev/null)" = "$commit" ]; then
  echo "Burrow is up to date: $branch at ${commit:0:7}."
  exit 0
fi
git -C "$src" checkout --quiet --force --detach "$commit"

cd "$src"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# Names the commit, so a daemon left running from before the update looks
# out of date to the new window (ipc::BUILD).
export BURROW_BUILD=$version+g${commit:0:7}
cargo build --locked --release -p burrow-gtk -p burrowd -p burrow-cli
agents=()
for arch in x86_64 aarch64; do
  target=$arch-unknown-linux-musl
  [ -d "$(rustc --print target-libdir --target "$target" 2>/dev/null)" ] || continue
  cargo build --locked --profile agent -p burrow-agent --target "$target"
  agents+=("$arch")
done

for program in burrow-gtk burrowd burrow; do
  put 755 "target/release/$program" "$bin/$program"
done
for arch in "${agents[@]}"; do
  put 755 "target/$arch-unknown-linux-musl/agent/burrow-agent" "$lib/burrow-agent-$arch"
done
put 755 dist/install.sh "$bin/burrow-update"
# Launchers don't search ~/.local/bin, and neither does systemd.
edit "s|^Exec=burrow-gtk|Exec=$bin/burrow-gtk|" \
  dist/dev.burrow.Burrow.desktop "$data/applications/dev.burrow.Burrow.desktop"
edit 's|^ExecStart=/usr/bin/burrowd|ExecStart=%h/.local/bin/burrowd|' \
  dist/systemd/burrowd.service "$units/burrowd.service"
for unit in burrowd.socket burrow-update.service burrow-update.timer; do
  put 644 "dist/systemd/$unit" "$units/$unit"
done
printf '%s\n' "$commit" >"$lib/commit"

systemctl --user daemon-reload
systemctl --user enable --quiet --now burrow-update.timer

echo "Installed Burrow $BURROW_BUILD from $branch into ~/.local."
if [ ${#agents[@]} -eq 0 ]; then
  echo "No musl target is installed, so monitors can't put the agent on servers:"
  echo "  rustup target add x86_64-unknown-linux-musl"
fi
notify "Burrow updated to ${commit:0:7}" "Restart Burrow to use it."
