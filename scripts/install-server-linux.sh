#!/bin/sh
set -eu

PATH=/usr/sbin:/usr/bin:/sbin:/bin
export PATH

service_user=stabbur
service_group=stabbur
binary_target=/usr/local/bin/stabbur-server
data_dir=/var/lib/stabbur
unit_target=/etc/systemd/system/stabbur-server.service
install_tmp=

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
repository_root=$(dirname "$script_dir")
unit_source=$repository_root/docs/examples/systemd/stabbur-server.service
binary_source=
start_service=0

usage() {
  cat <<'USAGE'
Usage: install-server-linux.sh --binary PATH [OPTIONS]

Install or atomically upgrade the default Linux Stabbur server host.

Options:
  --binary PATH  Native stabbur-server executable to install (required).
  --unit PATH    systemd unit template to install.
  --start        Enable and start the unit, or restart it when already active.
  -h, --help     Show this help.

The installer never bootstraps an administrator or accepts a password. Without
--start it only stages the account, binary, state directory, and validated unit.
USAGE
}

die() {
  echo "install-server-linux: $*" >&2
  exit 64
}

cleanup() {
  if [ -n "$install_tmp" ]; then
    rm -f "$install_tmp"
  fi
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

atomic_install() {
  source_path=$1
  target_path=$2
  owner=$3
  group=$4
  mode=$5

  if [ -L "$target_path" ]; then
    die "refusing to replace symlink: $target_path"
  fi
  if [ -e "$target_path" ] && [ ! -f "$target_path" ]; then
    die "installation target is not a regular file: $target_path"
  fi
  if [ -f "$target_path" ] && cmp -s "$source_path" "$target_path"; then
    chown "$owner:$group" "$target_path"
    chmod "$mode" "$target_path"
    return
  fi

  target_dir=$(dirname "$target_path")
  [ -d "$target_dir" ] || die "installation directory does not exist: $target_dir"
  install_tmp=$(mktemp "$target_dir/.stabbur-install.XXXXXX")
  install -o "$owner" -g "$group" -m "$mode" "$source_path" "$install_tmp"
  mv -f "$install_tmp" "$target_path"
  install_tmp=
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary)
      [ "$#" -ge 2 ] || die "--binary requires a path"
      binary_source=$2
      shift 2
      ;;
    --unit)
      [ "$#" -ge 2 ] || die "--unit requires a path"
      unit_source=$2
      shift 2
      ;;
    --start)
      start_service=1
      shift
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[ -n "$binary_source" ] || die "--binary is required"
[ "$(uname -s)" = Linux ] || die "this installer requires Linux"
[ "$(id -u)" = 0 ] || die "run as root"

for program in awk chmod chown cmp getent groupadd id install mktemp mv \
  systemd-analyze useradd; do
  command -v "$program" >/dev/null 2>&1 || die "required program is unavailable: $program"
done
if [ "$start_service" = 1 ]; then
  for program in curl sleep systemctl; do
    command -v "$program" >/dev/null 2>&1 || die "required program is unavailable: $program"
  done
fi

[ -f "$binary_source" ] || die "binary is not a regular file: $binary_source"
[ -x "$binary_source" ] || die "binary is not executable: $binary_source"
[ -r "$unit_source" ] || die "systemd unit is not readable: $unit_source"
[ -f "$unit_source" ] || die "systemd unit is not a regular file: $unit_source"

if ! getent group "$service_group" >/dev/null; then
  groupadd --system "$service_group"
fi
service_gid=$(getent group "$service_group" | awk -F: 'NR == 1 {print $3}')
[ -n "$service_gid" ] || die "could not resolve service group: $service_group"

passwd_entry=$(getent passwd "$service_user" || true)
if [ -z "$passwd_entry" ]; then
  if [ -x /usr/sbin/nologin ]; then
    nologin_shell=/usr/sbin/nologin
  elif [ -x /sbin/nologin ]; then
    nologin_shell=/sbin/nologin
  else
    nologin_shell=/bin/false
  fi
  useradd --system \
    --gid "$service_group" \
    --home-dir "$data_dir" \
    --shell "$nologin_shell" \
    "$service_user"
  passwd_entry=$(getent passwd "$service_user")
fi

user_gid=$(id -g "$service_user")
[ "$user_gid" = "$service_gid" ] || die "existing $service_user account has the wrong primary group"
user_home=$(printf '%s\n' "$passwd_entry" | awk -F: 'NR == 1 {print $6}')
[ "$user_home" = "$data_dir" ] || die "existing $service_user account has the wrong home directory"
user_shell=$(printf '%s\n' "$passwd_entry" | awk -F: 'NR == 1 {print $7}')
case "$user_shell" in
  /usr/sbin/nologin | /sbin/nologin | /usr/bin/false | /bin/false) ;;
  *) die "existing $service_user account does not have a non-login shell" ;;
esac

if [ -L "$data_dir" ]; then
  die "refusing symlinked state directory: $data_dir"
fi
if [ -L /usr/local/bin ]; then
  die "refusing symlinked installation directory: /usr/local/bin"
fi
if [ ! -d /usr/local/bin ]; then
  install -d -o root -g root -m 0755 /usr/local/bin
fi
install -d -o "$service_user" -g "$service_group" -m 0700 "$data_dir"

atomic_install "$binary_source" "$binary_target" root root 0755
atomic_install "$unit_source" "$unit_target" root root 0644
systemd-analyze verify "$unit_target"

if [ "$start_service" = 1 ]; then
  systemctl daemon-reload
  systemctl enable stabbur-server.service
  if systemctl is-active --quiet stabbur-server.service; then
    systemctl restart stabbur-server.service
  else
    systemctl start stabbur-server.service
  fi

  readiness_attempt=0
  until curl --fail --silent http://127.0.0.1:8080/readyz >/dev/null 2>&1; do
    readiness_attempt=$((readiness_attempt + 1))
    if [ "$readiness_attempt" -ge 30 ]; then
      die "server did not become ready within 30 seconds"
    fi
    sleep 1
  done
  echo "Stabbur server installed and active; readiness check passed."
else
  echo "Stabbur server staged; the systemd unit was not started."
  echo "Bootstrap locally if needed, then rerun this installer with --start."
fi
