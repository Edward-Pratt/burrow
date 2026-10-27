#!/bin/bash
# Installs Burrow for one user, into ~/.local, and keeps it up to date. By
# default it downloads the build CI made of the newest main, so nothing is
# compiled here. Installed as `burrow-update`, it updates; a daily systemd
# user timer runs `burrow-update --check`, which only says on the desktop
# that an update is ready, and the window's "Check for updates…" installs
# it. Run from a terminal, it first installs what Burrow needs with
# pacman, asking for sudo once.
#
#   install.sh                       install the newest build of main
#   burrow-update                    update now
#   burrow-update --check            say whether an update is ready
#   burrow-update --stable           follow tagged releases from now on
#   burrow-update --branch NAME      build NAME from source from now on
#   burrow-update --release          go back to the builds of main
#   install.sh --stage DIR           build this checkout into DIR (CI)
#
# It installs whatever lands on main, or on the branch it follows, without
# asking, so only follow a branch whose pushers you trust.
set -euo pipefail

url=https://github.com/Edward-Pratt/burrow.git
# Where CI publishes the builds of main. Only the environment can change
# it, for testing, never a config file.
releases=${BURROW_RELEASES:-https://github.com/Edward-Pratt/burrow/releases/download/build-main}
# GitHub redirects this to the newest tagged release that isn't a
# pre-release. BURROW_STABLE_RELEASES names a release's files directly,
# for testing.
latest=https://github.com/Edward-Pratt/burrow/releases/latest
tarball=burrow-x86_64.tar.gz
data=${XDG_DATA_HOME:-$HOME/.local/share}
src=$data/burrow/src
# Says what is followed: `release` (the builds of main), `stable` (tagged
# releases), or the branch built from source.
follow_file=$data/burrow/follow
bin=$HOME/.local/bin
# The core finds agents in lib/burrow beside the folder it runs from.
lib=$HOME/.local/lib/burrow
units=$data/systemd/user

notify() {
  # Under the timer nobody reads the output, so say it on the desktop. Its
  # unit sets BURROW_NOTIFY; the window shows the output itself.
  if [ -n "${BURROW_NOTIFY:-}" ] && command -v notify-send >/dev/null; then
    notify-send --app-name=Burrow --icon=dev.burrow.Burrow "$@" || true
  fi
}
# A download that failed partway is set here, and goes.
download=
trap '[ $? -eq 0 ] || notify "Burrow update failed" "See journalctl --user -u burrow-update"; rm -rf "$download"' EXIT

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

# HTTPS only, redirects included. file:// is for testing with
# BURROW_RELEASES.
fetch() { # url destination
  curl --fail --silent --show-error --location \
    --proto '=https,file' --proto-redir '=https' --retry 2 -o "$2" "$1"
}

# True when updates download a build CI made, rather than building one.
downloads() { # follow
  [ "$1" = release ] || [ "$1" = stable ]
}

# Sets `releases` to the files of the newest tagged release, and `tag` to
# its tag.
find_stable() {
  if [ -n "${BURROW_STABLE_RELEASES:-}" ]; then
    releases=$BURROW_STABLE_RELEASES
    tag=${releases##*/}
    return
  fi
  local page
  page=$(curl --fail --silent --show-error --proto '=https' --retry 2 \
    --output /dev/null --write-out '%{redirect_url}' "$latest")
  # With no tagged release yet, GitHub sends this to the list of releases.
  if ! [[ $page =~ ^https://github\.com/Edward-Pratt/burrow/releases/tag/(v[0-9]+\.[0-9]+\.[0-9]+)$ ]]; then
    echo "Burrow has no stable release yet. burrow-update --release follows the builds of main." >&2
    exit 1
  fi
  tag=${BASH_REMATCH[1]}
  releases=https://github.com/Edward-Pratt/burrow/releases/download/$tag
}

# Only with someone at the terminal to answer sudo. Under the timer a
# missing piece fails the update, and the notification says so.
prepare() { # follow
  local tools=(curl tar sha256sum)
  if command -v pacman >/dev/null; then
    local want=(gtk4 vte4 gtksourceview5 openssh curl) missing
    if ! downloads "$1"; then
      want+=(git gcc pkgconf)
      # pacman's rust-musl and rust-aarch64-musl need its rust, which
      # rustup conflicts with, so they are only for a machine without
      # rustup.
      if ! command -v rustup >/dev/null; then
        want+=(rust rust-musl rust-aarch64-musl)
      fi
    fi
    mapfile -t missing < <(pacman -T "${want[@]}" || true)
    if [ ${#missing[@]} -gt 0 ]; then
      if ! command -v sudo >/dev/null; then
        echo "Burrow needs ${missing[*]}. As root, run:" >&2
        echo "  pacman -S --needed ${missing[*]}" >&2
        exit 1
      fi
      echo "Burrow needs ${missing[*]}. Installing with pacman:"
      sudo pacman -S --needed --noconfirm "${missing[@]}"
    fi
  fi
  if ! downloads "$1"; then
    tools=(git cargo)
    if command -v rustup >/dev/null; then
      rustup show active-toolchain >/dev/null 2>&1 || rustup default stable
      rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl
    fi
  fi
  local tool
  for tool in "${tools[@]}"; do
    if ! command -v "$tool" >/dev/null; then
      echo "Burrow needs $tool. Install it, then run this again." >&2
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

# Builds the checkout in the current folder and lays it out in `stage`
# the way a release tarball holds it, so both install the same way.
build() { # stage
  local commit version arch
  commit=$(git rev-parse HEAD)
  version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
  # The paths below are in this checkout, whatever the environment says.
  export CARGO_TARGET_DIR=target
  # Names the commit, so a daemon left running from before the update
  # looks out of date to the new window (ipc::BUILD).
  export BURROW_BUILD=$version+g${commit:0:7}
  cargo build --locked --release -p burrow-gtk -p burrowd -p burrow-cli
  mapfile -t agents < <(arches)
  for arch in "${agents[@]}"; do
    cargo build --locked --profile agent -p burrow-agent --target "$arch-unknown-linux-musl"
  done
  rm -rf "$1"
  mkdir -p "$1/bin" "$1/lib"
  cp target/release/{burrow-gtk,burrowd,burrow} "$1/bin/"
  for arch in "${agents[@]}"; do
    cp "target/$arch-unknown-linux-musl/agent/burrow-agent" "$1/lib/burrow-agent-$arch"
  done
  cp -r dist "$1/dist"
  printf '%s\n' "$commit" >"$1/commit"
}

# Installs what `build` laid out, or what a release tarball held.
install_stage() { # stage
  local stage=$1 program arch unit broken
  # A build from a newer Arch than this one names libraries this one
  # lacks, and would not start. The old install stays until it would.
  for program in burrow-gtk burrowd burrow; do
    broken=$(ldd "$stage/bin/$program" 2>&1 | grep -E 'not found' || true)
    if [ -n "$broken" ]; then
      echo "This build of Burrow doesn't run here, so the installed one stays:" >&2
      echo "$broken" >&2
      echo "Update the system (sudo pacman -Syu), then run burrow-update again." >&2
      exit 1
    fi
  done

  for program in burrow-gtk burrowd burrow; do
    put 755 "$stage/bin/$program" "$bin/$program"
  done
  for arch in x86_64 aarch64; do
    if [ -f "$stage/lib/burrow-agent-$arch" ]; then
      put 755 "$stage/lib/burrow-agent-$arch" "$lib/burrow-agent-$arch"
    fi
  done
  put 755 "$stage/dist/install.sh" "$bin/burrow-update"
  put 644 "$stage/dist/icons/dev.burrow.Burrow.svg" "$data/icons/hicolor/scalable/apps/dev.burrow.Burrow.svg"
  # GTK trusts an icon cache that another installer left here, which hides
  # icons added after it. Without one, it reads the folders themselves.
  if [ -f "$data/icons/hicolor/icon-theme.cache" ]; then
    gtk4-update-icon-cache --quiet --force --ignore-theme-index "$data/icons/hicolor"
  fi
  # Launchers don't search ~/.local/bin, and neither does systemd.
  edit "s|^Exec=burrow-gtk|Exec=$bin/burrow-gtk|" \
    "$stage/dist/dev.burrow.Burrow.desktop" "$data/applications/dev.burrow.Burrow.desktop"
  edit 's|^ExecStart=/usr/bin/burrowd|ExecStart=%h/.local/bin/burrowd|' \
    "$stage/dist/systemd/burrowd.service" "$units/burrowd.service"
  for unit in burrowd.socket burrow-update.service burrow-update.timer; do
    put 644 "$stage/dist/systemd/$unit" "$units/$unit"
  done
  cp "$stage/commit" "$lib/commit.new"
  mv -f "$lib/commit.new" "$lib/commit"

  if systemctl --user daemon-reload 2>/dev/null; then
    systemctl --user enable --quiet --now burrow-update.timer
  else
    echo "There is no systemd user manager, so Burrow won't check for updates by itself. Run burrow-update to update."
  fi
}

follow=
check=no
stage_to=
while [ $# -gt 0 ]; do
  case $1 in
    --branch)
      follow=${2:?--branch needs a branch name}
      shift 2
      ;;
    --release)
      follow=release
      shift
      ;;
    --stable)
      follow=stable
      shift
      ;;
    --check)
      check=yes
      shift
      ;;
    --stage)
      stage_to=${2:?--stage needs a folder}
      shift 2
      ;;
    *)
      echo "usage: ${0##*/} [--check] [--branch NAME | --release | --stable]" >&2
      exit 2
      ;;
  esac
done

if [ -n "$stage_to" ]; then
  stage_to=$(realpath -m "$stage_to")
  cd "$(dirname "$0")/.."
  build "$stage_to"
  exit 0
fi

follow=${follow:-$(cat "$follow_file" 2>/dev/null || echo release)}
# The script from before release builds runs this one as `--branch main`,
# and meant the newest main, which the builds now are. Only it re-runs
# this script without a follow file. This can go once installs updated.
if [ ! -f "$follow_file" ] && [ -n "${BURROW_INSTALL_FROM:-}" ] && [ "$follow" = main ]; then
  follow=release
fi
if ! downloads "$follow"; then
  git check-ref-format --branch "$follow" >/dev/null
fi
if [ -t 0 ] && [ "$check" = no ]; then
  prepare "$follow"
fi

mkdir -p "$data/burrow"
exec 9>"$data/burrow/update.lock"
if ! flock -n 9; then
  echo "Another Burrow update is running."
  # A check under the timer isn't a failure to notify about.
  [ "$check" = yes ] && exit 0
  exit 1
fi

# The newest commit of what is followed.
tag=
if downloads "$follow"; then
  [ "$follow" = release ] || find_stable
  available=$(fetch "$releases/commit" -)
  if ! [[ $available =~ ^[0-9a-f]{40}$ ]]; then
    echo "The release doesn't name a commit." >&2
    exit 1
  fi
else
  if [ ! -d "$src/.git" ]; then
    # Run from a checkout, clone the way it was cloned, which works here,
    # as SSH may where HTTPS has no credentials.
    from=$url
    checkout=$(cd "$(dirname "$0")/.." && pwd)
    if [ -f "$checkout/crates/burrowd/Cargo.toml" ]; then
      from=$(git -C "$checkout" remote get-url origin 2>/dev/null || echo "$url")
    fi
    git clone --quiet "$from" "$src"
  fi
  git -C "$src" fetch --quiet origin "$follow"
  available=$(git -C "$src" rev-parse FETCH_HEAD)
fi
installed=$(cat "$lib/commit" 2>/dev/null || echo none)
# Compared with what was installed, so an update that failed is tried
# again next time. Built from source, a musl target added since then needs
# a build too, for its agent.
current=$([ "$installed" = "$available" ] && echo yes || echo no)
if ! downloads "$follow"; then
  for arch in $(arches); do
    [ -f "$lib/burrow-agent-$arch" ] || current=no
  done
fi

if [ "$check" = yes ]; then
  # The window reads these lines.
  echo "follow $follow"
  echo "installed $installed"
  echo "available $available"
  [ -z "$tag" ] || echo "tag $tag"
  # Once per commit, or the timer would say it every day.
  if [ "$current" = no ] && [ "$(cat "$data/burrow/notified" 2>/dev/null)" != "$available" ]; then
    notify "A Burrow update is ready" "Choose Check for updates… in Burrow's menu to install it."
    printf '%s\n' "$available" >"$data/burrow/notified"
  fi
  exit 0
fi
if [ "$current" = yes ]; then
  echo "Burrow is up to date: ${tag:-${available:0:7}}."
  exit 0
fi
printf '%s\n' "$follow" >"$follow_file"

if downloads "$follow"; then
  stage=$data/burrow/stage
  # Kept after a failed install, so trying again doesn't download again.
  if [ "$(cat "$stage/commit" 2>/dev/null)" != "$available" ]; then
    echo "Downloading Burrow ${tag:-${available:0:7}}…"
    download=$(mktemp -d "$data/burrow/download.XXXXXX")
    fetch "$releases/SHA256SUMS" "$download/SHA256SUMS"
    fetch "$releases/$tarball" "$download/$tarball"
    # Only the tarball's own line, so a sum file listing more still works.
    grep -E "^[0-9a-f]{64}  $tarball\$" "$download/SHA256SUMS" >"$download/sum" || {
      echo "The release has no checksum for $tarball." >&2
      exit 1
    }
    if ! (cd "$download" && sha256sum --check --quiet --strict sum); then
      # CI replaces the files one at a time, so this can be a release
      # caught halfway.
      echo "The download doesn't match its checksum. Try again in a few minutes." >&2
      exit 1
    fi
    mkdir "$download/unpacked"
    tar -xzf "$download/$tarball" -C "$download/unpacked" --no-same-owner --no-same-permissions
    if [ "$(cat "$download/unpacked/burrow/commit" 2>/dev/null)" != "$available" ]; then
      echo "The release changed while it downloaded. Try again in a few minutes." >&2
      exit 1
    fi
    rm -rf "$stage"
    mv "$download/unpacked/burrow" "$stage"
    rm -rf "$download"
  fi
  from_script=$stage/dist/install.sh
else
  git -C "$src" checkout --quiet --force --detach "$available"
  from_script=$src/dist/install.sh
fi

# The steps below come from the commit being installed rather than from the
# copy running now, so a new step takes effect in the update that adds it.
# It inherits the lock, and takes it again when it reopens the lock file.
if [ "${BURROW_INSTALL_FROM:-}" != "$available" ]; then
  case $follow in
    release) args=(--release) ;;
    stable) args=(--stable) ;;
    *) args=(--branch "$follow") ;;
  esac
  BURROW_INSTALL_FROM=$available exec "$from_script" "${args[@]}"
fi

if ! downloads "$follow"; then
  echo "Building Burrow ${available:0:7} from $follow…"
  (cd "$src" && build "$src/target/stage")
  stage=$src/target/stage
fi
install_stage "$stage"
# Only needed until an install succeeds. Bash still reads this script from
# the deleted file.
if downloads "$follow"; then
  rm -rf "$stage"
fi

echo "Installed Burrow ${tag:-${available:0:7}} into ~/.local. Restart Burrow to use it."
# Says what is still missing at run time, such as a Secret Service.
if [ -t 1 ]; then
  "$bin/burrow" doctor || true
fi
notify "Burrow updated to ${tag:-${available:0:7}}" "Restart Burrow to use it."
