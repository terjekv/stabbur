#!/bin/sh
set -eu

PATH=/usr/bin:/bin:/usr/sbin:/sbin
export PATH

worker_user=_stabbur_worker
worker_group=staff
worker_gid=20
binary_target=/usr/local/bin/stabbur-server
state_dir=/usr/local/var/lib/stabbur-worker
log_dir=/usr/local/var/log/stabbur-worker
credential_target=$state_dir/credential.json
plist_target=/Library/LaunchDaemons/com.stabbur.worker.plist
autopkg_program=/Library/AutoPkg/autopkg
install_tmp=

script_dir=$(CDPATH='' cd "$(dirname "$0")" && pwd -P)
repository_root=$(dirname "$script_dir")
plist_source=$repository_root/docs/examples/launchd/com.stabbur.worker.plist
binary_source=
credential_source=
server_url=
requested_uid=auto
replace_credential=0
start_worker=0

usage() {
  cat <<'USAGE'
Usage: install-worker-macos.sh --binary PATH --server-url URL [OPTIONS]

Install or atomically upgrade the default native macOS Stabbur worker host.

Options:
  --binary PATH           Native stabbur-server executable to install (required).
  --server-url URL        HTTPS control-plane origin, or a loopback HTTP origin (required).
  --credential-file PATH  Server-issued worker credential to install owner-only.
  --replace-credential    Permit replacement by a different supplied credential.
  --uid UID               New role-account UID in 450-499; default selects the first free UID.
  --autopkg-program PATH  Worker-local AutoPkg executable path.
  --plist PATH            launchd plist template to render and install.
  --start                 Load or restart the launchd job after validation.
  -h, --help              Show this help.

The installer never downloads AutoPkg or provisions a worker identity. Without
--start it only stages the account, binary, directories, credential, and plist.
USAGE
}

die() {
  echo "install-worker-macos: $*" >&2
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

dscl_value() {
  key=$1
  dscl . -read "/Users/$worker_user" "$key" \
    | awk -v label="$key:" '$1 == label {print $2; exit}'
}

validate_credential() {
  credential_path=$1
  plutil -extract worker_id raw -expect string -o /dev/null "$credential_path" \
    || return 1
  plutil -extract token raw -expect string -o /dev/null "$credential_path" \
    || return 1
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --binary)
      [ "$#" -ge 2 ] || die "--binary requires a path"
      binary_source=$2
      shift 2
      ;;
    --server-url)
      [ "$#" -ge 2 ] || die "--server-url requires a URL"
      server_url=$2
      shift 2
      ;;
    --credential-file)
      [ "$#" -ge 2 ] || die "--credential-file requires a path"
      credential_source=$2
      shift 2
      ;;
    --replace-credential)
      replace_credential=1
      shift
      ;;
    --uid)
      [ "$#" -ge 2 ] || die "--uid requires a value"
      requested_uid=$2
      shift 2
      ;;
    --autopkg-program)
      [ "$#" -ge 2 ] || die "--autopkg-program requires a path"
      autopkg_program=$2
      shift 2
      ;;
    --plist)
      [ "$#" -ge 2 ] || die "--plist requires a path"
      plist_source=$2
      shift 2
      ;;
    --start)
      start_worker=1
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
[ -n "$server_url" ] || die "--server-url is required"
[ "$replace_credential" != 1 ] || [ -n "$credential_source" ] \
  || die "--replace-credential requires --credential-file"
[ "$(uname -s)" = Darwin ] || die "this installer requires macOS"
[ "$(id -u)" = 0 ] || die "run as root"

for program in awk chmod chown cmp dscl id install launchctl mktemp mv plutil sysadminctl; do
  command -v "$program" >/dev/null 2>&1 || die "required program is unavailable: $program"
done
if [ "$start_worker" = 1 ]; then
  for program in curl sudo; do
    command -v "$program" >/dev/null 2>&1 || die "required program is unavailable: $program"
  done
fi

case "$server_url" in
  *[[:space:]]* | *"@"* | *"?"* | *"#"*)
    die "server URL must not contain whitespace, credentials, a query, or a fragment"
    ;;
  https://?*) ;;
  http://localhost | http://localhost:* | http://127.0.0.1 | http://127.0.0.1:*) ;;
  *) die "server URL must use HTTPS, except for an explicit loopback HTTP origin" ;;
esac
url_authority=${server_url#*://}
url_authority=${url_authority%%/*}
[ -n "$url_authority" ] || die "server URL must include a host"
case "$autopkg_program" in
  /*) ;;
  *) die "AutoPkg program must be an absolute path" ;;
esac
case "$requested_uid" in
  auto) ;;
  '' | *[!0-9]*) die "worker UID must be auto or an integer in 450-499" ;;
  *)
    [ "$requested_uid" -ge 450 ] && [ "$requested_uid" -le 499 ] \
      || die "worker UID must be in Apple's role-account range 450-499"
    ;;
esac

[ -f "$binary_source" ] || die "binary is not a regular file: $binary_source"
[ -x "$binary_source" ] || die "binary is not executable: $binary_source"
[ -r "$plist_source" ] || die "launchd plist is not readable: $plist_source"
[ -f "$plist_source" ] || die "launchd plist is not a regular file: $plist_source"
plutil -lint "$plist_source" >/dev/null

if dscl . -read "/Users/$worker_user" >/dev/null 2>&1; then
  existing_uid=$(dscl_value UniqueID)
  if [ "$requested_uid" != auto ] && [ "$requested_uid" != "$existing_uid" ]; then
    die "existing $worker_user account has UID $existing_uid, not requested UID $requested_uid"
  fi
else
  allocated_uids=$(dscl . -list /Users UniqueID) \
    || die "could not enumerate local macOS account UIDs"
  if [ "$requested_uid" = auto ]; then
    requested_uid=450
    while [ "$requested_uid" -le 499 ]; do
      if ! printf '%s\n' "$allocated_uids" \
        | awk -v candidate="$requested_uid" '$2 == candidate {found = 1} END {exit found ? 0 : 1}'; then
        break
      fi
      requested_uid=$((requested_uid + 1))
    done
    [ "$requested_uid" -le 499 ] || die "no free macOS role-account UID exists in 450-499"
  else
    if printf '%s\n' "$allocated_uids" \
      | awk -v candidate="$requested_uid" '$2 == candidate {found = 1} END {exit found ? 0 : 1}'; then
      die "requested worker UID is already allocated: $requested_uid"
    fi
  fi

  sysadminctl -addUser "$worker_user" \
    -fullName "Stabbur Worker" \
    -UID "$requested_uid" \
    -GID "$worker_gid" \
    -shell /usr/bin/false \
    -home /var/empty \
    -roleAccount
  dscl . -read "/Users/$worker_user" >/dev/null 2>&1 \
    || die "sysadminctl did not create $worker_user"
fi

existing_uid=$(dscl_value UniqueID)
case "$existing_uid" in
  '' | *[!0-9]*) die "existing $worker_user account has an invalid UID" ;;
  *)
    [ "$existing_uid" -ge 450 ] && [ "$existing_uid" -le 499 ] \
      || die "existing $worker_user account is not a macOS role account"
    ;;
esac
[ "$(dscl_value PrimaryGroupID)" = "$worker_gid" ] \
  || die "existing $worker_user account has the wrong primary group"
[ "$(dscl_value NFSHomeDirectory)" = /var/empty ] \
  || die "existing $worker_user account has the wrong home directory"
case "$(dscl_value UserShell)" in
  /usr/bin/false | /bin/false) ;;
  *) die "existing $worker_user account does not have a non-login shell" ;;
esac
[ "$(id -g "$worker_user")" = "$worker_gid" ] \
  || die "existing $worker_user account does not resolve to group $worker_group"

for shared_directory in \
  /usr/local \
  /usr/local/bin \
  /usr/local/var \
  /usr/local/var/lib \
  /usr/local/var/log; do
  [ ! -L "$shared_directory" ] \
    || die "refusing symlinked installation directory: $shared_directory"
  if [ ! -d "$shared_directory" ]; then
    install -d -o root -g wheel -m 0755 "$shared_directory"
  fi
done
for directory in "$state_dir" "$log_dir"; do
  [ ! -L "$directory" ] || die "refusing symlinked worker directory: $directory"
  install -d -o "$worker_user" -g "$worker_group" -m 0700 "$directory"
done

atomic_install "$binary_source" "$binary_target" root wheel 0755

if [ -n "$credential_source" ]; then
  [ -r "$credential_source" ] || die "credential is not readable: $credential_source"
  [ -f "$credential_source" ] || die "credential is not a regular file: $credential_source"
  validate_credential "$credential_source" \
    || die "credential must contain string worker_id and token fields"
  if [ -f "$credential_target" ] \
    && ! cmp -s "$credential_source" "$credential_target" \
    && [ "$replace_credential" != 1 ]; then
    die "a different worker credential already exists; use --replace-credential to rotate it"
  fi
  atomic_install "$credential_source" "$credential_target" "$worker_user" "$worker_group" 0600
elif [ -e "$credential_target" ]; then
  [ ! -L "$credential_target" ] || die "refusing symlinked worker credential"
  [ -f "$credential_target" ] || die "worker credential is not a regular file"
  validate_credential "$credential_target" \
    || die "installed credential must contain string worker_id and token fields"
  chown "$worker_user:$worker_group" "$credential_target"
  chmod 0600 "$credential_target"
fi

if [ -L "$plist_target" ]; then
  die "refusing to replace symlink: $plist_target"
fi
if [ -e "$plist_target" ] && [ ! -f "$plist_target" ]; then
  die "launchd target is not a regular file: $plist_target"
fi
install_tmp=$(mktemp /Library/LaunchDaemons/.com.stabbur.worker.XXXXXX)
install -o root -g wheel -m 0644 "$plist_source" "$install_tmp"
plutil -replace EnvironmentVariables.STABBUR_SERVER_URL \
  -string "$server_url" "$install_tmp"
plutil -replace EnvironmentVariables.STABBUR_AUTOPKG_PROGRAM \
  -string "$autopkg_program" "$install_tmp"
plutil -lint "$install_tmp" >/dev/null
if [ -f "$plist_target" ] && cmp -s "$install_tmp" "$plist_target"; then
  rm -f "$install_tmp"
  install_tmp=
  chown root:wheel "$plist_target"
  chmod 0644 "$plist_target"
else
  mv -f "$install_tmp" "$plist_target"
  install_tmp=
fi

if [ "$start_worker" = 1 ]; then
  [ -f "$credential_target" ] || die "install a server-issued credential before using --start"
  [ -x "$autopkg_program" ] || die "prepare AutoPkg before using --start: $autopkg_program"
  sudo -u "$worker_user" "$binary_target" worker \
    --print-capabilities \
    --autopkg-program "$autopkg_program" >/dev/null
  curl --fail --silent --show-error "${server_url%/}/readyz" >/dev/null

  if launchctl print system/com.stabbur.worker >/dev/null 2>&1; then
    launchctl bootout system/com.stabbur.worker
  fi
  launchctl bootstrap system "$plist_target"
  launchctl enable system/com.stabbur.worker
  launchctl kickstart -k system/com.stabbur.worker
  launchctl print system/com.stabbur.worker >/dev/null
  echo "Stabbur worker installed and active; inspect registration on the server."
else
  echo "Stabbur worker staged; the launchd job was not started."
  echo "Prepare AutoPkg and install a credential, then rerun this installer with --start."
fi
