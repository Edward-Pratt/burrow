#!/bin/bash
# Installs Burrow from source for one user, into ~/.local, and keeps it up
# to date: a daily systemd user timer runs this again as `burrow-update`,
# which builds and installs only when the branch has moved. Run from a
# terminal, it first installs what the build needs: with pacman, asking for
# sudo once, and with rustup, the musl targets the server agent builds for.
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

# Only with someone at the terminal to answer sudo. Under the timer a
# missing piece fails the build, and the notification says so.
prepare() {
  if command -v pacman >/dev/null; then
    local want=(git gcc pkgconf gtk4 vte4 gtksourceview5 openssh) missing
    # pacman's rust-musl and rust-aarch64-musl need its rust, which rustup
    # conflicts with, so they are only for a machine without rustup.
    if ! command -v rustup >/dev/null; then
      want+=(rust rust-musl rust-aarch64-musl)
    fi
    mapfile -t missing < <(pacman -T "${want[@]}" || true)
    if [ ${#missing[@]} -gt 0 ]; then
      echo "Burrow needs ${missing[*]} to build. Installing with pacman:"
      sudo pacman -S --needed --noconfirm "${missing[@]}"
    fi
  fi
  if command -v rustup >/dev/null; then
    rustup show active-toolchain >/dev/null 2>&1 || rustup default stable
    rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
  fi
  local tool
  for tool in git cargo; do
    if ! command -v "$tool" >/dev/null; then
      echo "Burrow needs $tool to build. Install it, then run this again." >&2
      exit 1
    fi
  done
}

# The servers an agent can be built for here: those whose musl target is
# installed.
arches() {
  local arch
  for arch in x86_64 aarch64; do
    if [ -d "$(rustc --print target-libdir --target "$arch-unknown-linux-musl" 2>/dev/null)" ]; then
      echo "$arch"
    fi
  done
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

if [ -t 0 ]; then
  prepare
fi

mkdir -p "$data/burrow"
exec 9>"$data/burrow/update.lock"
if ! flock -n 9; then
  echo "Another Burrow update is running."
  exit 0
fi

if [ ! -d "$src/.git" ]; then
  # Run from a checkout, clone the way it was cloned, which works here, as
  # SSH may where HTTPS has no credentials.
  from=$url
  checkout=$(cd "$(dirname "$0")/.." && pwd)
  if [ -f "$checkout/crates/burrowd/Cargo.toml" ]; then
    from=$(git -C "$checkout" remote get-url origin 2>/dev/null || echo "$url")
  fi
  git clone --quiet "$from" "$src"
fi
# The clone remembers which branch it follows.
branch=${branch:-$(git -C "$src" config burrow.branch || echo main)}
git check-ref-format --branch "$branch" >/dev/null
git -C "$src" fetch --quiet origin "$branch"
git -C "$src" config burrow.branch "$branch"
commit=$(git -C "$src" rev-parse FETCH_HEAD)
# Compared with what was installed, not what was checked out, so a build
# that failed is tried again next time. A musl target added since then
# needs a build too, for its agent.
current=$([ "$(cat "$lib/commit" 2>/dev/null)" = "$commit" ] && echo yes || echo no)
for arch in $(arches); do
  [ -f "$lib/burrow-agent-$arch" ] || current=no
done
if [ "$current" = yes ]; then
  echo "Burrow is up to date: $branch at ${commit:0:7}."
  exit 0
fi
git -C "$src" checkout --quiet --force --detach "$commit"
# The steps below come from the commit being installed rather than from the
# copy running now, so a new step takes effect in the update that adds it.
# It inherits the lock, and takes it again when it reopens the lock file.
if [ "${BURROW_INSTALL_FROM:-}" != "$commit" ]; then
  BURROW_INSTALL_FROM=$commit exec "$src/dist/install.sh" --branch "$branch"
fi

cd "$src"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
# Names the commit, so a daemon left running from before the update looks
# out of date to the new window (ipc::BUILD).
export BURROW_BUILD=$version+g${commit:0:7}
cargo build --locked --release -p burrow-gtk -p burrowd -p burrow-cli
mapfile -t agents < <(arches)
for arch in "${agents[@]}"; do
  cargo build --locked --profile agent -p burrow-agent --target "$arch-unknown-linux-musl"
done

for program in burrow-gtk burrowd burrow; do
  put 755 "target/release/$program" "$bin/$program"
done
for arch in "${agents[@]}"; do
  put 755 "target/$arch-unknown-linux-musl/agent/burrow-agent" "$lib/burrow-agent-$arch"
done
put 755 dist/install.sh "$bin/burrow-update"
put 644 dist/icons/dev.burrow.Burrow.svg "$data/icons/hicolor/scalable/apps/dev.burrow.Burrow.svg"
# GTK trusts an icon cache that another installer left here, which hides
# icons added after it. Without one, it reads the folders themselves.
if [ -f "$data/icons/hicolor/icon-theme.cache" ]; then
  gtk4-update-icon-cache --quiet --force --ignore-theme-index "$data/icons/hicolor"
fi
# Launchers don't search ~/.local/bin, and neither does systemd.
edit "s|^Exec=burrow-gtk|Exec=$bin/burrow-gtk|" \
  dist/dev.burrow.Burrow.desktop "$data/applications/dev.burrow.Burrow.desktop"
edit 's|^ExecStart=/usr/bin/burrowd|ExecStart=%h/.local/bin/burrowd|' \
  dist/systemd/burrowd.service "$units/burrowd.service"
for unit in burrowd.socket burrow-update.service burrow-update.timer; do
  put 644 "dist/systemd/$unit" "$units/$unit"
done
printf '%s\n' "$commit" >"$lib/commit"

if systemctl --user daemon-reload 2>/dev/null; then
  systemctl --user enable --quiet --now burrow-update.timer
else
  echo "There is no systemd user manager, so Burrow won't update by itself. Run burrow-update to update."
fi

echo "Installed Burrow $BURROW_BUILD from $branch into ~/.local."
# Says what is still missing at run time, such as a Secret Service.
if [ -t 1 ]; then
  "$bin/burrow" doctor || true
fi
notify "Burrow updated to ${commit:0:7}" "Restart Burrow to use it."
