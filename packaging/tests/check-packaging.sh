#!/usr/bin/env bash
# Checks for the packaging files. Safe to run anywhere: everything happens in a temporary
# directory, nothing is installed for the user and nothing is downloaded.
#
#   packaging/tests/check-packaging.sh [path/to/newskite]
#
# With a binary path (or an existing $CARGO_TARGET_DIR/release/newskite) the AppImage AppDir
# is also smoke-tested by running "AppRun --version". Optional validators (desktop-file-validate,
# appstreamcli, xmllint, update-mime-database, shellcheck, gio) are used when installed and
# reported as skipped otherwise.

set -uo pipefail

readonly APP_ID="net.innerpulse.Newskite"
PKG_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "${PKG_DIR}/.." && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-${REPO_DIR}/target}"
REAL_BIN="${1:-${TARGET_DIR}/release/newskite}"

passed=0
failed=0
skipped=0
work="$(mktemp -d)"
trap 'rm -rf -- "${work}"' EXIT

ok() { passed=$((passed + 1)); echo "ok    $*"; }
bad() { failed=$((failed + 1)); echo "FAIL  $*"; }
skip() { skipped=$((skipped + 1)); echo "skip  $*"; }
have() { command -v "$1" >/dev/null 2>&1; }
check() { local name="$1"; shift; if "$@" >"${work}/out" 2>&1; then ok "${name}"; else bad "${name}"; sed 's/^/      /' "${work}/out"; fi; }

desktop="${PKG_DIR}/${APP_ID}.desktop"
metainfo="${PKG_DIR}/${APP_ID}.metainfo.xml"
mimexml="${PKG_DIR}/mime/newskite-nzb.xml"

# --- desktop entry -----------------------------------------------------------------------
if have desktop-file-validate; then check "desktop-file-validate" desktop-file-validate "${desktop}"; else skip "desktop-file-validate not installed"; fi
for want in "Exec=newskite %U" "Icon=${APP_ID}" "StartupWMClass=${APP_ID}" "Categories=Network;FileTransfer;" \
            "MimeType=application/x-nzb;x-scheme-handler/nzb;x-scheme-handler/nzblnk;"; do
    check "desktop entry has '${want}'" grep -qxF "${want}" "${desktop}"
done
check "APP_ID constant matches the desktop file name" \
    grep -qF "APP_ID: &str = \"${APP_ID}\"" "${REPO_DIR}/crates/newskite-types/src/lib.rs"

# --- AppStream / MIME XML ----------------------------------------------------------------
if have appstreamcli; then check "appstreamcli validate --no-net" appstreamcli validate --no-net "${metainfo}"; else skip "appstreamcli not installed"; fi
if have xmllint; then
    check "metainfo is well-formed XML" xmllint --noout "${metainfo}"
    check "MIME definition is well-formed XML" xmllint --noout "${mimexml}"
else
    skip "xmllint not installed"
fi
check "metainfo launchable names the desktop file" grep -qF ">${APP_ID}.desktop</launchable>" "${metainfo}"

if have update-mime-database; then
    mkdir -p "${work}/share/mime/packages"
    cp "${mimexml}" "${work}/share/mime/packages/"
    if update-mime-database "${work}/share/mime" >"${work}/out" 2>&1; then
        ok "update-mime-database accepts the definition"
        check "*.nzb glob registered" grep -q 'application/x-nzb:\*\.nzb' "${work}/share/mime/globs2"
        check "<nzb magic registered" grep -qa '<nzb' "${work}/share/mime/magic"
        if have gio; then
            printf '<?xml version="1.0"?>\n<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb"></nzb>\n' > "${work}/sample.nzb"
            cp "${work}/sample.nzb" "${work}/sample-no-extension"
            for f in sample.nzb sample-no-extension; do
                type="$(XDG_DATA_HOME="${work}/share" XDG_DATA_DIRS="${work}/share:/usr/share" \
                    gio info -a standard::content-type "${work}/${f}" 2>/dev/null | sed -n 's/.*standard::content-type: //p')"
                if [[ "${type}" == "application/x-nzb" ]]; then ok "gio detects ${f} as application/x-nzb"; else bad "gio detects ${f} as '${type}'"; fi
            done
        else
            skip "gio not installed (content-type detection)"
        fi
    else
        bad "update-mime-database rejected the definition"; sed 's/^/      /' "${work}/out"
    fi
else
    skip "update-mime-database not installed"
fi

# --- icons -------------------------------------------------------------------------------
check "scalable icon is well-formed SVG" grep -q '<svg ' "${PKG_DIR}/icons/${APP_ID}.svg"
if have xmllint; then check "scalable icon parses as XML" xmllint --noout "${PKG_DIR}/icons/${APP_ID}.svg"; fi
check "GUI window icon exists" test -f "${REPO_DIR}/assets/brand/app-icon-256.png"
check "GUI window icon matches the 256px hicolor PNG" cmp -s "${REPO_DIR}/assets/brand/app-icon-256.png" "${PKG_DIR}/icons/hicolor/256x256/apps/${APP_ID}.png"
for size in 16 24 32 48 64 128 256 512; do
    png="${PKG_DIR}/icons/hicolor/${size}x${size}/apps/${APP_ID}.png"
    if [[ ! -f "${png}" ]]; then bad "missing ${size}px PNG"; continue; fi
    # PNG IHDR: width and height are big-endian u32 at byte offsets 16 and 20.
    dims="$(od -An -tu1 -j16 -N8 "${png}" | awk '{printf "%d %d", $1*16777216+$2*65536+$3*256+$4, $5*16777216+$6*65536+$7*256+$8}')"
    if [[ "${dims}" == "${size} ${size}" ]]; then ok "${size}px PNG is ${size}x${size}"; else bad "${size}px PNG is ${dims}"; fi
done

# --- cargo-deb assets exist --------------------------------------------------------------
app_dir="${REPO_DIR}/crates/newskite-app"
missing=0
while IFS= read -r src; do
    [[ "${src}" == target/* ]] && continue
    [[ -e "${app_dir}/${src}" ]] || { bad "cargo-deb asset missing: ${src}"; missing=1; }
done < <(sed -n 's/^ *\["\([^"]*\)",.*/\1/p' "${app_dir}/Cargo.toml")
[[ ${missing} -eq 0 ]] && ok "every cargo-deb asset source exists"

# --- shell scripts -----------------------------------------------------------------------
for script in "${PKG_DIR}"/*.sh "${PKG_DIR}"/tests/*.sh; do
    check "bash -n $(basename "${script}")" bash -n "${script}"
done
if have shellcheck; then
    check "shellcheck" shellcheck "${PKG_DIR}"/*.sh "${PKG_DIR}"/tests/*.sh
else
    skip "shellcheck not installed"
fi

# --- AppImage AppDir assembly ------------------------------------------------------------
fake="${work}/fake-newskite"
printf '#!/bin/sh\necho "fake newskite $*"\n' > "${fake}"
chmod 755 "${fake}"
appdir="${work}/Newskite.AppDir"
check "build-appimage --appdir-only" "${PKG_DIR}/build-appimage.sh" --binary "${fake}" --appdir "${appdir}" --out "${work}" --appdir-only
for entry in AppRun "${APP_ID}.desktop" "${APP_ID}.png" .DirIcon usr/bin/newskite \
             "usr/share/metainfo/${APP_ID}.appdata.xml" usr/share/mime/packages/newskite-nzb.xml \
             "usr/share/icons/hicolor/scalable/apps/${APP_ID}.svg" "usr/share/icons/hicolor/16x16/apps/${APP_ID}.png"; do
    if [[ -e "${appdir}/${entry}" ]]; then ok "AppDir has ${entry}"; else bad "AppDir lacks ${entry}"; fi
done
check "AppRun is executable" test -x "${appdir}/AppRun"
check "AppRun forwards arguments" bash -c "[[ \"\$('${appdir}/AppRun' --nzb 'a b.nzb')\" == 'fake newskite --nzb a b.nzb' ]]"
check "AppRun works through a symlink" bash -c "ln -s '${appdir}/AppRun' '${work}/linked' && [[ \"\$('${work}/linked' x)\" == 'fake newskite x' ]]"
check "rebuilding over an existing AppDir works" "${PKG_DIR}/build-appimage.sh" --binary "${fake}" --appdir "${appdir}" --out "${work}" --appdir-only

mkdir -p "${work}/not-an-appdir"; touch "${work}/not-an-appdir/keep"
if "${PKG_DIR}/build-appimage.sh" --binary "${fake}" --appdir "${work}/not-an-appdir" --appdir-only >/dev/null 2>&1; then
    bad "build-appimage overwrote a directory that is not an AppDir"
else
    check "build-appimage refuses to clear a foreign directory" test -f "${work}/not-an-appdir/keep"
fi
"${PKG_DIR}/build-appimage.sh" --bogus >/dev/null 2>&1
rc=$?; if [[ ${rc} -eq 2 ]]; then ok "unknown option exits 2"; else bad "unknown option exited ${rc}"; fi
"${PKG_DIR}/build-appimage.sh" --binary "${work}/does-not-exist" --appdir "${work}/x.AppDir" --appdir-only >/dev/null 2>&1
rc=$?; if [[ ${rc} -eq 1 ]]; then ok "missing binary exits 1"; else bad "missing binary exited ${rc}"; fi

if ! have appimagetool && [[ -z "${APPIMAGETOOL:-}" ]]; then
    "${PKG_DIR}/build-appimage.sh" --binary "${fake}" --appdir "${work}/t.AppDir" --out "${work}" >"${work}/out" 2>&1
    rc=$?
    if [[ ${rc} -eq 3 ]] && grep -q "appimagetool was not found" "${work}/out" && [[ -x "${work}/t.AppDir/AppRun" ]]; then
        ok "without appimagetool: exits 3 with instructions and keeps the AppDir"
    else
        bad "without appimagetool: exit ${rc}"; sed 's/^/      /' "${work}/out"
    fi
else
    skip "appimagetool is installed; missing-tool path not exercised"
fi

if [[ -x "${REAL_BIN}" ]]; then
    real_appdir="${work}/Real.AppDir"
    if "${PKG_DIR}/build-appimage.sh" --binary "${REAL_BIN}" --appdir "${real_appdir}" --out "${work}" --appdir-only >/dev/null 2>&1; then
        check "AppRun --version runs the real binary" bash -c "'${real_appdir}/AppRun' --version | grep -q '^newskite '"
    else
        bad "AppDir assembly with the real binary"
    fi
else
    skip "no release binary at ${REAL_BIN}; real AppRun smoke test"
fi

echo
echo "packaging checks: ${passed} passed, ${failed} failed, ${skipped} skipped"
[[ ${failed} -eq 0 ]]
