#!/usr/bin/env bash
# Remove a per-user Newskite install made by packaging/install-local.sh.
#
# Removes only the files install-local.sh created. Your queue, settings, history and
# downloaded files (~/.local/share/newskite and the download folder) are left alone.
# Default-handler associations pointing at Newskite are removed from mimeapps.list.
#
# Usage: packaging/uninstall-local.sh [options]
#   --bin-dir DIR    where the binary was installed     (default: ~/.local/bin)
#   --data-dir DIR   XDG data home it was installed to  (default: ${XDG_DATA_HOME:-~/.local/share})
#   -h, --help       show this help

set -euo pipefail

readonly APP_ID="net.innerpulse.Newskite"
readonly BIN_NAME="newskite"
readonly ICON_SIZES=(16x16 24x24 32x32 48x48 64x64 128x128 256x256 512x512)

bin_dir="${HOME}/.local/bin"
data_dir="${XDG_DATA_HOME:-${HOME}/.local/share}"
config_dir="${XDG_CONFIG_HOME:-${HOME}/.config}"

usage_error() { echo "uninstall-local: $*" >&2; echo "Try --help." >&2; exit 2; }
show_help() { sed -n '2,/^$/{s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}"; }
have() { command -v "$1" >/dev/null 2>&1; }
remove() {
    if [[ -e "$1" || -L "$1" ]]; then
        rm -f -- "$1"
        echo "uninstall-local: removed $1"
    fi
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --bin-dir) [[ $# -ge 2 ]] || usage_error "--bin-dir needs a directory"; bin_dir="$2"; shift 2 ;;
        --data-dir) [[ $# -ge 2 ]] || usage_error "--data-dir needs a directory"; data_dir="$2"; shift 2 ;;
        -h|--help) show_help; exit 0 ;;
        *) usage_error "unknown option: $1" ;;
    esac
done

remove "${bin_dir}/${BIN_NAME}"
remove "${data_dir}/applications/${APP_ID}.desktop"
remove "${data_dir}/metainfo/${APP_ID}.metainfo.xml"
remove "${data_dir}/mime/packages/newskite-nzb.xml"
remove "${data_dir}/icons/hicolor/scalable/apps/${APP_ID}.svg"
for size in "${ICON_SIZES[@]}"; do
    remove "${data_dir}/icons/hicolor/${size}/apps/${APP_ID}.png"
done

# Drop "<mime>=net.innerpulse.Newskite.desktop" associations that xdg-mime wrote. Other
# handlers listed on the same line are kept.
mimeapps="${config_dir}/mimeapps.list"
if [[ -f "${mimeapps}" ]] && grep -q "${APP_ID}\.desktop" "${mimeapps}"; then
    tmp="$(mktemp "${mimeapps}.XXXXXX")"
    sed -e "s/${APP_ID//./\\.}\.desktop;\{0,1\}//g" -e '/^[^=[]*=$/d' "${mimeapps}" > "${tmp}"
    chmod --reference="${mimeapps}" "${tmp}" 2>/dev/null || true
    mv -f "${tmp}" "${mimeapps}"
    echo "uninstall-local: removed Newskite associations from ${mimeapps}"
fi

if have update-desktop-database && [[ -d "${data_dir}/applications" ]]; then
    update-desktop-database -q "${data_dir}/applications" || true
fi
if have update-mime-database && [[ -d "${data_dir}/mime/packages" ]]; then
    update-mime-database "${data_dir}/mime" || true
fi
touch "${data_dir}/icons/hicolor" 2>/dev/null || true

echo "uninstall-local: done. Your data in ${data_dir}/newskite was kept."
