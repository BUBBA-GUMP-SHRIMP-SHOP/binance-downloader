#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'HELP'
Usage: scripts/install-service.sh [OPTIONS]

Install and start a systemd user service reading ARCHIVE/~incoming-job.
  -d, --destination PATH  Archive root (same default as the downloader)
  -j, --jobs NUMBER       Parallel downloads, 1-256 (default: 20)
      --binary PATH       Use this executable instead of building a release binary
      --no-start          Install without enabling or starting the service
  -h, --help              Show this help

Run as your normal user, without sudo. Requires Linux, Bash, Cargo (unless
--binary is supplied), and a running systemd user manager.
HELP
}

# The installed copy also serves as the FIFO runner. Opening read/write keeps
# stdin alive between separate writers; a poison pill still ends the process.
if [[ ${1-} == --run ]]; then
    [[ $# == 4 ]] || exit 2
    binary=$2
    archive=$3
    jobs=$4
    fifo="$archive/~incoming-job"
    [[ -p $fifo && ! -L $fifo ]] || { echo "Expected named pipe: $fifo" >&2; exit 1; }
    exec 3<> "$fifo"
    exec "$binary" --destination "$archive" --jobs "$jobs" --quiet <&3 3>&-
fi

archive="${ARCHIVE_BASE_PATH:-${XDG_DATA_HOME:-$HOME/.local/share}}/binance-archive"
jobs=20
binary=''
start=true
while (( $# )); do
    case $1 in
        -d|--destination|-j|--jobs|--binary)
            (( $# >= 2 )) || { echo "Missing value for $1" >&2; exit 2; }
            case $1 in
                -d|--destination) archive=$2 ;;
                -j|--jobs) jobs=$2 ;;
                --binary) binary=$2 ;;
            esac
            shift 2 ;;
        --no-start) start=false; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done
[[ $jobs =~ ^[0-9]{1,3}$ ]] && (( 10#$jobs >= 1 && 10#$jobs <= 256 )) || {
    echo 'Jobs must be an integer from 1 to 256' >&2; exit 2;
}
jobs=$((10#$jobs))
[[ -n $archive ]] || { echo 'Destination must not be empty' >&2; exit 2; }
command -v systemctl >/dev/null
systemctl --user show-environment >/dev/null
source_script=$(realpath -- "${BASH_SOURCE[0]}")
if [[ -z $binary ]]; then
    project=$(dirname -- "$(dirname -- "$source_script")")
    cargo build --release --locked --manifest-path "$project/Cargo.toml"
    binary="$project/target/release/binance-downloader"
fi
binary=$(realpath -e -- "$binary")
[[ -f $binary && -x $binary ]] || { echo "Not an executable: $binary" >&2; exit 2; }
archive=$(realpath -m -- "$archive")
service_dir="${XDG_DATA_HOME:-$HOME/.local/share}/binance-downloader-service"
unit_dir="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
# systemd unit values must stay on one physical line.
for value in "$archive" "$service_dir" "$binary"; do
    [[ $value != *$'\n'* && $value != *$'\r'* ]] || { echo 'Paths must not contain newlines' >&2; exit 2; }
done
fifo="$archive/~incoming-job"
mkdir -p -- "$archive" "$service_dir" "$unit_dir"
if [[ -L $fifo || ( -e $fifo && ! -p $fifo ) ]]; then
    echo "Refusing to replace non-FIFO path: $fifo" >&2
    exit 1
fi
if [[ ! -p $fifo ]]; then
    mkfifo -m 600 -- "$fifo"
fi

# Quote ExecStart arguments for systemd, including literal $ and % characters.
unit_quote() {
    local value=$1
    value=${value//\\/\\\\}
    value=${value//\"/\\\"}
    value=${value//%/%%}
    value=${value//\$/\$\$}
    printf '"%s"' "$value"
}

# Stop an existing instance before replacing its executable.
if systemctl --user is-active --quiet binance-downloader.service; then
    systemctl --user stop binance-downloader.service
fi
install -m 755 -- "$binary" "$service_dir/binance-downloader"
if [[ $source_script != "$service_dir/runner.sh" ]]; then
    install -m 755 -- "$source_script" "$service_dir/runner.sh"
fi
{
    cat <<'UNIT'
[Unit]
Description=Binance archive downloader from a named pipe

[Service]
Type=simple
UNIT
    printf 'ExecStart=/usr/bin/bash %s --run %s %s %s\n' \
        "$(unit_quote "$service_dir/runner.sh")" \
        "$(unit_quote "$service_dir/binance-downloader")" \
        "$(unit_quote "$archive")" "$jobs"
    cat <<'UNIT'
Restart=no
TimeoutStopSec=60
UMask=0077
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=default.target
UNIT
} > "$unit_dir/binance-downloader.service"
systemctl --user daemon-reload
if $start; then
    systemctl --user enable --now binance-downloader.service
fi
printf 'Installed binance-downloader.service\nNamed pipe: %s\n' "$fifo"
printf 'Send URLs: printf '\''%%s\\n'\'' '\''URL'\'' > %q\n' "$fifo"
printf 'Logs: journalctl --user -u binance-downloader.service -f\n'
