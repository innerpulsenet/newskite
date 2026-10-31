#!/usr/bin/env bash
# Install Newskite for the current user only (no root needed).
#
# Installs:
#   ~/.local/bin/newskite
#   ~/.local/share/applications/net.innerpulse.Newskite.desktop   (Exec points at the binary)
#   ~/.local/share/metainfo/net.innerpulse.Newskite.metainfo.xml
#   ~/.local/share/mime/packages/newskite-nzb.xml
#   ~/.local/share/icons/hicolor/{scalable,16x16,...,512x512}/apps/net.innerpulse.Newskite.*
# then refreshes the desktop, MIME and icon caches when the tools exist, and makes Newskite
# the default handler for application/x-nzb, nzb:// and nzblnk: links (skip with --no-default).
#
# Usage: packaging/install-local.sh [options]
#   --binary PATH    install this binary (default: $CARGO_TARGET_DIR/release/newskite,
#                    building it with cargo first if it does not exist)
#   --bin-dir DIR    where to put the binary            (default: ~/.local/bin)
#   --data-dir DIR   XDG data home to install into      (default: ${XDG_DATA_HOME:-~/.local/share})
#   --no-default     do not change the default handler for NZB files and links
#   -h, --help       show this help
#
# Undo with packaging/uninstall-local.sh (same --bin-dir/--data-dir options).
# Your queue, settings and downloads are not touched by either script.

set -euo pipefail

readonly APP_ID="net.innerpulse.Newskite"
readonly BIN_NAME="newskite"
readonly MIME_TYPES=(application/x-nzb x-scheme-handler/nzb x-scheme-handler/nzblnk)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_DIR}/target}"

binary=""
bin_dir="${HOME}/.local/bin"
data_dir="${XDG_DATA_HOME:-${HOME}/.local/share}"
set_default=1

die() { echo "install-local: error: $*" >&2; exit 1; }
usage_error() { echo "install-local: $*" >&2; echo "Try --help." >&2; exit 2; }
show_help() { sed -n '2,/^$/{s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}"; }
have() { command -v "$1" >/dev/null 2>&1; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary) [[ $# -ge 2 ]] || usage_error "--binary needs a path"; binary="$2"; shift 2 ;;
        --bin-dir) [[ $# -ge 2 ]] || usage_error "--bin-dir needs a directory"; bin_dir="$2"; shift 2 ;;
        --data-dir) [[ $# -ge 2 ]] || usage_error "--data-dir needs a directory"; data_dir="$2"; shift 2 ;;
        --no-default) set_default=0; shift ;;
        -h|--help) show_help; exit 0 ;;
        *) usage_error "unknown option: $1" ;;
    esac
done

[[ "$(id -u)" -ne 0 ]] || die "this is a per-user install; do not run it as root (use the .deb instead)"

if [[ -z "${binary}" ]]; then
    binary="${TARGET_DIR}/release/${BIN_NAME}"
    if [[ ! -x "${binary}" ]]; then
        have cargo || die "no binary at ${binary} and cargo is not installed; pass --binary"
        echo "install-local: building release binary"
        (cd "${REPO_DIR}" && cargo build --release -p newskite --locked)
    fi
fi
[[ -f "${binary}" && -x "${binary}" ]] || die "binary not found or not executable: ${binary}"

apps_dir="${data_dir}/applications"
desktop_file="${apps_dir}/${APP_ID}.desktop"
installed_bin="${bin_dir}/${BIN_NAME}"

case "${installed_bin}" in
    /*) ;;
    *) die "--bin-dir must be an absolute path" ;;
esac
case "${installed_bin}" in
    *[\"\`\$\\\|\&%]*|*$'\n'*) die "--bin-dir contains characters a desktop entry cannot hold: ${bin_dir}" ;;
esac

echo "install-local: installing ${installed_bin}"
install -d "${bin_dir}"
install -m 755 "${binary}" "${installed_bin}"

echo "install-local: installing desktop entry, metadata, MIME type and icons under ${data_dir}"
install -d "${apps_dir}" "${data_dir}/metainfo" "${data_dir}/mime/packages"
# Point Exec/TryExec at the installed binary so the launcher works even when ~/.local/bin is
# not on the PATH of the desktop session. The path is double-quoted per the Desktop Entry
# spec; characters that would need further escaping there (or in sed) are rejected up front.
sed -e "s|^Exec=${BIN_NAME} |Exec=\"${installed_bin}\" |" \
    -e "s|^TryExec=${BIN_NAME}\$|TryExec=${installed_bin}|" \
    "${SCRIPT_DIR}/${APP_ID}.desktop" > "${desktop_file}.tmp"
chmod 644 "${desktop_file}.tmp"
mv -f "${desktop_file}.tmp" "${desktop_file}"
install -m 644 "${SCRIPT_DIR}/${APP_ID}.metainfo.xml" "${data_dir}/metainfo/${APP_ID}.metainfo.xml"
install -m 644 "${SCRIPT_DIR}/mime/newskite-nzb.xml" "${data_dir}/mime/packages/newskite-nzb.xml"

icons_dir="${data_dir}/icons/hicolor"
install -d "${icons_dir}/scalable/apps"
install -m 644 "${SCRIPT_DIR}/icons/${APP_ID}.svg" "${icons_dir}/scalable/apps/${APP_ID}.svg"
for png in "${SCRIPT_DIR}"/icons/hicolor/*/apps/"${APP_ID}".png; do
    [[ -f "${png}" ]] || continue
    size_dir="$(basename "$(dirname "$(dirname "${png}")")")"
    install -d "${icons_dir}/${size_dir}/apps"
    install -m 644 "${png}" "${icons_dir}/${size_dir}/apps/${APP_ID}.png"
done

if have desktop-file-validate; then
    desktop-file-validate "${desktop_file}" || echo "install-local: warning: desktop entry did not validate" >&2
fi
if have update-desktop-database; then
    update-desktop-database -q "${apps_dir}" || echo "install-local: warning: update-desktop-database failed" >&2
fi
if have update-mime-database; then
    update-mime-database "${data_dir}/mime" || echo "install-local: warning: update-mime-database failed" >&2
fi
if have gtk-update-icon-cache && [[ -f "${icons_dir}/index.theme" ]]; then
    gtk-update-icon-cache -q -t "${icons_dir}" || true
fi
touch "${icons_dir}" 2>/dev/null || true

if [[ ${set_default} -eq 1 ]]; then
    if have xdg-mime; then
        for mime in "${MIME_TYPES[@]}"; do
            xdg-mime default "${APP_ID}.desktop" "${mime}" \
                || echo "install-local: warning: could not set default for ${mime}" >&2
        done
        echo "install-local: Newskite is now the default for NZB files and nzb:/nzblnk: links"
    else
        echo "install-local: xdg-mime not found; set the default NZB handler in system settings" >&2
    fi
fi

case ":${PATH}:" in
    *":${bin_dir}:"*) ;;
    *) echo "install-local: note: ${bin_dir} is not on PATH; add it to run 'newskite' from a terminal" ;;
esac
echo "install-local: done. Launch Newskite from the application menu or run ${installed_bin}"
