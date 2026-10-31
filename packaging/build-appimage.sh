#!/usr/bin/env bash
# Build a Newskite AppImage.
#
# Steps:
#   1. build the release binary with cargo (skip with --binary PATH or --no-build)
#   2. assemble an AppDir: AppRun, desktop entry, icons, AppStream metadata, MIME definition
#   3. run appimagetool on the AppDir, if appimagetool is installed
#
# This script never downloads anything. When appimagetool is missing it prints how to get it
# and exits with status 3 after leaving the finished AppDir in place.
#
# The AppImage bundles only the Newskite binary. Graphics and windowing libraries (libGL,
# libEGL, libwayland-client, libxkbcommon, libX11...) are loaded from the host at run time,
# like any other desktop app. glibc is linked dynamically, so the AppImage runs on systems
# whose glibc is at least as new as the build machine's: build on the oldest distribution
# you want to support.
#
# Usage: packaging/build-appimage.sh [options]
#   --binary PATH    use this prebuilt newskite binary (implies --no-build)
#   --no-build       do not run cargo; use $CARGO_TARGET_DIR/release/newskite
#   --appdir DIR     where to assemble the AppDir   (default: <target>/appimage/Newskite.AppDir)
#   --out DIR        where to write the .AppImage   (default: <target>/appimage)
#   --appdir-only    stop after assembling the AppDir (exit 0)
#   -h, --help       show this help
#
# Exit status: 0 success, 1 build/assembly error, 2 usage error, 3 appimagetool missing.

set -euo pipefail

readonly APP_ID="net.innerpulse.Newskite"
readonly BIN_NAME="newskite"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_DIR}/target}"

binary=""
build=1
appdir=""
out_dir=""
appdir_only=0

die() { echo "build-appimage: error: $*" >&2; exit 1; }
usage_error() { echo "build-appimage: $*" >&2; echo "Try --help." >&2; exit 2; }
show_help() { sed -n '2,/^$/{s/^# \{0,1\}//;p}' "${BASH_SOURCE[0]}"; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary) [[ $# -ge 2 ]] || usage_error "--binary needs a path"; binary="$2"; build=0; shift 2 ;;
        --no-build) build=0; shift ;;
        --appdir) [[ $# -ge 2 ]] || usage_error "--appdir needs a directory"; appdir="$2"; shift 2 ;;
        --out) [[ $# -ge 2 ]] || usage_error "--out needs a directory"; out_dir="$2"; shift 2 ;;
        --appdir-only) appdir_only=1; shift ;;
        -h|--help) show_help; exit 0 ;;
        *) usage_error "unknown option: $1" ;;
    esac
done

out_dir="${out_dir:-${TARGET_DIR}/appimage}"
appdir="${appdir:-${out_dir}/Newskite.AppDir}"

# --- 1. binary ---------------------------------------------------------------------------
if [[ -z "${binary}" ]]; then
    binary="${TARGET_DIR}/release/${BIN_NAME}"
    if [[ ${build} -eq 1 ]]; then
        command -v cargo >/dev/null 2>&1 || die "cargo not found; install Rust or pass --binary"
        echo "build-appimage: building release binary"
        (cd "${REPO_DIR}" && cargo build --release -p newskite --locked)
    fi
fi
[[ -f "${binary}" && -x "${binary}" ]] || die "binary not found or not executable: ${binary}"

# --- 2. AppDir ---------------------------------------------------------------------------
# Refuse to clear anything that does not look like an AppDir we made.
case "${appdir}" in
    ""|"/"|"${HOME}"|"${REPO_DIR}") die "refusing to use ${appdir:-<empty>} as the AppDir" ;;
esac
if [[ -e "${appdir}" ]]; then
    [[ -f "${appdir}/AppRun" || -z "$(ls -A "${appdir}")" ]] \
        || die "${appdir} exists and is not an AppDir; remove it or choose another --appdir"
    rm -rf -- "${appdir}"
fi

echo "build-appimage: assembling ${appdir}"
install -d "${appdir}/usr/bin" \
    "${appdir}/usr/share/applications" \
    "${appdir}/usr/share/metainfo" \
    "${appdir}/usr/share/mime/packages" \
    "${appdir}/usr/share/icons/hicolor/scalable/apps"

install -m 755 "${binary}" "${appdir}/usr/bin/${BIN_NAME}"
install -m 644 "${SCRIPT_DIR}/${APP_ID}.desktop" "${appdir}/usr/share/applications/${APP_ID}.desktop"
install -m 644 "${SCRIPT_DIR}/${APP_ID}.metainfo.xml" "${appdir}/usr/share/metainfo/${APP_ID}.appdata.xml"
install -m 644 "${SCRIPT_DIR}/mime/newskite-nzb.xml" "${appdir}/usr/share/mime/packages/newskite-nzb.xml"
install -m 644 "${SCRIPT_DIR}/icons/${APP_ID}.svg" "${appdir}/usr/share/icons/hicolor/scalable/apps/${APP_ID}.svg"
for png in "${SCRIPT_DIR}"/icons/hicolor/*/apps/"${APP_ID}".png; do
    [[ -f "${png}" ]] || continue
    size_dir="$(basename "$(dirname "$(dirname "${png}")")")"
    install -d "${appdir}/usr/share/icons/hicolor/${size_dir}/apps"
    install -m 644 "${png}" "${appdir}/usr/share/icons/hicolor/${size_dir}/apps/${APP_ID}.png"
done

# Top-level entries appimagetool requires: desktop file, icon, .DirIcon, AppRun.
ln -s "usr/share/applications/${APP_ID}.desktop" "${appdir}/${APP_ID}.desktop"
if [[ -f "${SCRIPT_DIR}/icons/hicolor/256x256/apps/${APP_ID}.png" ]]; then
    ln -s "usr/share/icons/hicolor/256x256/apps/${APP_ID}.png" "${appdir}/${APP_ID}.png"
    ln -s "${APP_ID}.png" "${appdir}/.DirIcon"
else
    ln -s "usr/share/icons/hicolor/scalable/apps/${APP_ID}.svg" "${appdir}/${APP_ID}.svg"
    ln -s "${APP_ID}.svg" "${appdir}/.DirIcon"
fi

cat > "${appdir}/AppRun" <<'APPRUN'
#!/bin/sh
# AppImage entry point: run the bundled newskite with the original arguments.
HERE="$(dirname "$(readlink -f "$0")")"
export PATH="${HERE}/usr/bin:${PATH}"
exec "${HERE}/usr/bin/newskite" "$@"
APPRUN
chmod 755 "${appdir}/AppRun"

echo "build-appimage: AppDir ready"
if [[ ${appdir_only} -eq 1 ]]; then
    exit 0
fi

# --- 3. appimagetool ---------------------------------------------------------------------
tool="${APPIMAGETOOL:-}"
if [[ -z "${tool}" ]]; then
    for candidate in appimagetool appimagetool-x86_64.AppImage appimagetool-aarch64.AppImage; do
        if command -v "${candidate}" >/dev/null 2>&1; then
            tool="$(command -v "${candidate}")"
            break
        fi
    done
fi

if [[ -z "${tool}" || ! -x "${tool}" ]]; then
    cat >&2 <<EOF
build-appimage: appimagetool was not found, so no .AppImage was produced.
The AppDir is complete at:
    ${appdir}

To finish:
  1. Download appimagetool for your architecture from its official release page
     (https://github.com/AppImage/appimagetool/releases), for example
     appimagetool-x86_64.AppImage.
  2. chmod +x it and put it on PATH as "appimagetool", or point APPIMAGETOOL at it.
  3. Re-run: packaging/build-appimage.sh --no-build
     (or run it by hand: ARCH=$(uname -m) appimagetool "${appdir}" Newskite-$(uname -m).AppImage)
EOF
    exit 3
fi

arch="$(uname -m)"
version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' "${REPO_DIR}/Cargo.toml" | head -n 1)"
output="${out_dir}/Newskite-${version:-dev}-${arch}.AppImage"
install -d "${out_dir}"
echo "build-appimage: running ${tool}"
ARCH="${arch}" "${tool}" "${appdir}" "${output}" || die "appimagetool failed"
echo "build-appimage: wrote ${output}"
