#!/usr/bin/env bash
set -euo pipefail

NAME="${NAME:-sulla-golden}"
IMAGE="${IMAGE:-images:ubuntu/24.04}"
INSTALL_RUST="${INSTALL_RUST:-1}"

host_user="$(id -un)"
host_uid="$(id -u)"
host_gid="$(id -g)"
host_home="$HOME"

log() { printf '==> %s\n' "$*"; }
warn() { printf 'warning: %s\n' "$*" >&2; }
die() { printf 'error: %s\n' "$*" >&2; exit 1; }

in_root() { incus exec "$NAME" -- "$@"; }
in_user() {
    incus exec "$NAME" --user "$host_uid" --group "$host_gid" \
        --env HOME="$host_home" --env USER="$host_user" --cwd "$host_home" -- "$@"
}

command -v incus >/dev/null || die "incus is not on PATH"

missing=()
grep -qx "root:${host_uid}:1" /etc/subuid 2>/dev/null || missing+=("root:${host_uid}:1 >> /etc/subuid")
grep -qx "root:${host_gid}:1" /etc/subgid 2>/dev/null || missing+=("root:${host_gid}:1 >> /etc/subgid")
if ((${#missing[@]})); then
    printf 'raw.idmap needs the host uid/gid delegated to root. Run:\n\n' >&2
    for m in "${missing[@]}"; do
        printf "  echo '%s' | sudo tee -a %s\n" "${m%% >> *}" "${m##* >> }" >&2
    done
    printf '  sudo systemctl restart incus\n\nthen re-run this script.\n' >&2
    exit 1
fi

config=(
    security.nesting=true
    security.syscalls.intercept.mknod=true
    security.syscalls.intercept.setxattr=true
    raw.idmap="both ${host_uid} ${host_gid}"
)

if incus info "$NAME" >/dev/null 2>&1; then
    log "reusing existing instance $NAME"
    if [[ "$(incus list "^${NAME}\$" -c s -f csv)" == RUNNING ]]; then
        incus stop "$NAME"
    fi
    for c in "${config[@]}"; do
        incus config set "$NAME" "$c"
    done
    incus start "$NAME"
else
    log "launching $NAME from $IMAGE"
    launch_args=()
    for c in "${config[@]}"; do
        launch_args+=(-c "$c")
    done
    incus launch "$IMAGE" "$NAME" "${launch_args[@]}"
fi

log "waiting for network"
for i in $(seq 1 60); do
    if in_root getent hosts deb.debian.org >/dev/null 2>&1; then
        break
    fi
    ((i == 60)) && die "no network in $NAME after 60s"
    sleep 1
done

log "installing packages"
in_root env DEBIAN_FRONTEND=noninteractive bash -s <<'INNER'
set -euo pipefail
apt-get update -q
apt-get install -yq ca-certificates curl git build-essential sudo docker.io docker-compose-v2
if ! command -v node >/dev/null || [ "$(node -p 'process.versions.node.split(".")[0]')" -lt 22 ]; then
    curl -fsSL https://deb.nodesource.com/setup_22.x | bash -
    apt-get install -yq nodejs
fi
systemctl enable --now docker
INNER

log "creating user $host_user ($host_uid:$host_gid) with home $host_home"
in_root bash -s -- "$host_user" "$host_uid" "$host_gid" "$host_home" <<'INNER'
set -euo pipefail
user="$1" uid="$2" gid="$3" home="$4"

if ! getent group "$gid" >/dev/null; then
    if getent group "$user" >/dev/null; then
        groupmod -g "$gid" "$user"
    else
        groupadd -g "$gid" "$user"
    fi
fi

other="$(getent passwd "$uid" | cut -d: -f1 || true)"
if [ -n "$other" ] && [ "$other" != "$user" ]; then
    userdel -r "$other" 2>/dev/null || userdel "$other"
fi

mkdir -p "$(dirname "$home")"
if id "$user" >/dev/null 2>&1; then
    usermod -u "$uid" -g "$gid" -d "$home" -s /bin/bash "$user"
else
    useradd -u "$uid" -g "$gid" -d "$home" -m -s /bin/bash "$user"
fi
mkdir -p "$home"
chown "$uid:$gid" "$home"
usermod -aG docker "$user"

echo "$user ALL=(ALL) NOPASSWD:ALL" > "/etc/sudoers.d/sulla-$user"
chmod 440 "/etc/sudoers.d/sulla-$user"
INNER

bd_version=""
if command -v bd >/dev/null; then
    bd_version="$(bd --version | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -n1 || true)"
fi
if [[ -z "$bd_version" ]]; then
    bd_version="$(curl -fsSL https://api.github.com/repos/gastownhall/beads/releases/latest \
        | grep -oE '"tag_name": *"v[^"]+"' | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' || true)"
fi
if [[ -n "$bd_version" ]]; then
    log "installing bd $bd_version"
    if ! in_root bash -s -- "$bd_version" <<'INNER'
set -euo pipefail
v="$1"
arch="$(dpkg --print-architecture)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/bd.tgz" "https://github.com/gastownhall/beads/releases/download/v${v}/beads_${v}_linux_${arch}.tar.gz"
tar -xzf "$tmp/bd.tgz" -C "$tmp" bd
install -m 0755 "$tmp/bd" /usr/local/bin/bd
bd --version
INNER
    then
        warn "failed to install bd $bd_version, install it in $NAME manually"
    fi
else
    warn "could not determine a bd version, install bd in $NAME manually"
fi

log "installing agent CLIs"
in_root npm i -g @github/copilot @anthropic-ai/claude-code ||
    warn "npm install of agent CLIs failed, install them in $NAME manually"

if [[ "$INSTALL_RUST" == 1 ]]; then
    if in_user test -x "$host_home/.cargo/bin/rustup"; then
        log "rustup already installed, updating"
        in_user "$host_home/.cargo/bin/rustup" update || warn "rustup update failed"
    else
        log "installing rustup for $host_user"
        in_user bash -o pipefail -c "curl -fsSL https://sh.rustup.rs | sh -s -- -y"
    fi
fi

log "verifying docker"
in_root docker run --rm hello-world >/dev/null || die "docker run hello-world failed in $NAME"
in_root docker image rm hello-world >/dev/null 2>&1 || true

log "stopping $NAME"
incus stop "$NAME"
log "golden container $NAME is ready"
