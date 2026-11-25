#!/usr/bin/env bash
set -euo pipefail
umask 0022

# Package Lune into Linux distribution formats.
#
# Usage:
#   ./scripts/package.sh <target>
#
# Targets:
#   release         Build the binary only
#   linux-tar       Build dist/lune-<version>-linux-<arch>.tar.gz
#   linux-tar-gz    Alias for linux-tar
#   linux-targz     Alias for linux-tar
#   linux-deb       Build dist/lune_<version>_<deb-arch>.deb
#   linux-rpm       Build dist/lune-<version>-1.<rpm-arch>.rpm
#   linux-arch      Build dist/lune-<version>-1-<arch>.pkg.tar.zst
#   linux-appimage  Build dist/lune-<version>-<arch>.AppImage
#   linux-all       Build all Linux package formats
#
# Environment variables:
#   PROFILE              Cargo profile: "release" (default) or "debug"
#   TARGET_TRIPLE        Optional Rust target triple (defaults to host)
#   DIST_DIR             Output directory (default: dist)
#   STRIP                "1" (default for release) or "0" to skip stripping
#   CARGO_FLAGS          Extra flags passed to `cargo build` (e.g. "--locked --offline")
#   APPIMAGETOOL         Path to appimagetool binary (default: lookup in PATH)
#   APPIMAGE_RUNTIME     Optional path to a static AppImage type-2 runtime binary
#   ALLOW_MISSING_TOOLS  Set to "1" with linux-all to skip targets missing external tools

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DIST_DIR="${DIST_DIR:-${ROOT_DIR}/dist}"
PROFILE="${PROFILE:-release}"
TARGET_TRIPLE="${TARGET_TRIPLE:-}"
CARGO_FLAGS="${CARGO_FLAGS:-}"
ALLOW_MISSING_TOOLS="${ALLOW_MISSING_TOOLS:-0}"
PKG_RELEASE="${PKG_RELEASE:-1}"

if [[ -z "${STRIP+x}" ]]; then
    if [[ "${PROFILE}" == "release" ]]; then
        STRIP=1
    else
        STRIP=0
    fi
fi

PKG_NAME="lune"
PKG_SUMMARY="GPU-accelerated Wayland terminal emulator"
PKG_LICENSE="MIT"
PKG_URL="https://github.com/lune-terminal/lune"

read_version() {
    awk -F'"' '/^version[[:space:]]*=/ { print $2; exit }' "${ROOT_DIR}/app/Cargo.toml"
}

PKG_VERSION="$(read_version)"
if [[ -z "${PKG_VERSION}" ]]; then
    echo "error: could not read version from app/Cargo.toml" >&2
    exit 1
fi

detect_arch() {
    local raw
    if [[ -n "${TARGET_TRIPLE}" ]]; then
        raw="${TARGET_TRIPLE%%-*}"
    else
        raw="$(uname -m)"
    fi
    case "${raw}" in
        x86_64|amd64)
            ARCH="x86_64"
            DEB_ARCH="amd64"
            RPM_ARCH="x86_64"
            ARCH_PKG_ARCH="x86_64"
            APPIMAGE_ARCH="x86_64"
            ;;
        aarch64|arm64)
            ARCH="aarch64"
            DEB_ARCH="arm64"
            RPM_ARCH="aarch64"
            ARCH_PKG_ARCH="aarch64"
            APPIMAGE_ARCH="aarch64"
            ;;
        *)
            ARCH="${raw}"
            DEB_ARCH="${raw}"
            RPM_ARCH="${raw}"
            ARCH_PKG_ARCH="${raw}"
            APPIMAGE_ARCH="${raw}"
            ;;
    esac
}

detect_arch

WORK_DIR="$(mktemp -d "${TMPDIR:-/tmp}/lune-package.XXXXXX")"
cleanup() {
    rm -rf "${WORK_DIR}"
}
trap cleanup EXIT

STAGED_BIN=""
BUILT_ARTIFACTS=()
SKIPPED_TARGETS=()

log() {
    printf '[package] %s\n' "$*"
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

write_checksum() {
    local file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        (cd "$(dirname "${file}")" && sha256sum "$(basename "${file}")" > "$(basename "${file}").sha256")
    fi
}

ensure_binary() {
    if [[ -n "${STAGED_BIN}" && -x "${STAGED_BIN}" ]]; then
        return 0
    fi

    local cargo_args=(build --bin "${PKG_NAME}")
    if [[ "${PROFILE}" == "release" ]]; then
        cargo_args+=(--release)
    elif [[ "${PROFILE}" != "debug" ]]; then
        cargo_args+=(--profile "${PROFILE}")
    fi
    if [[ -n "${TARGET_TRIPLE}" ]]; then
        cargo_args+=(--target "${TARGET_TRIPLE}")
    fi
    if [[ -n "${CARGO_FLAGS}" ]]; then
        # Split whitespace-separated user flags intentionally.
        read -r -a extra_flags <<< "${CARGO_FLAGS}"
        cargo_args+=("${extra_flags[@]}")
    fi

    log "Building ${PKG_NAME} (${PROFILE}, ${ARCH})..."
    (cd "${ROOT_DIR}" && cargo "${cargo_args[@]}")

    local bin_path="${ROOT_DIR}/target"
    if [[ -n "${TARGET_TRIPLE}" ]]; then
        bin_path+="/${TARGET_TRIPLE}"
    fi
    bin_path+="/${PROFILE}/${PKG_NAME}"
    [[ -x "${bin_path}" ]] || die "built binary not found at ${bin_path}"

    STAGED_BIN="${WORK_DIR}/${PKG_NAME}"
    cp "${bin_path}" "${STAGED_BIN}"
    chmod 0755 "${STAGED_BIN}"

    if [[ "${STRIP}" == "1" ]] && command -v strip >/dev/null 2>&1; then
        log "Stripping staged binary..."
        strip --strip-unneeded "${STAGED_BIN}" 2>/dev/null || strip "${STAGED_BIN}" || true
    fi
}

populate_usr_tree() {
    local prefix_root="$1"
    install -Dm0755 "${STAGED_BIN}" "${prefix_root}/bin/${PKG_NAME}"
    install -Dm0644 "${ROOT_DIR}/assets/lune.desktop" "${prefix_root}/share/applications/lune.desktop"
    install -Dm0644 "${ROOT_DIR}/assets/lune.svg" "${prefix_root}/share/icons/hicolor/scalable/apps/lune.svg"
}

build_tar_gz() {
    ensure_binary
    mkdir -p "${DIST_DIR}"

    local stem="${PKG_NAME}-${PKG_VERSION}-linux-${ARCH}"
    local stage="${WORK_DIR}/tar/${stem}"
    local output="${DIST_DIR}/${stem}.tar.gz"

    populate_usr_tree "${stage}"
    cat > "${stage}/install.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
umask 0022
PREFIX="${PREFIX:-/usr/local}"
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
install -Dm0755 "${DIR}/bin/lune" "${PREFIX}/bin/lune"
install -Dm0644 "${DIR}/share/applications/lune.desktop" "${PREFIX}/share/applications/lune.desktop"
install -Dm0644 "${DIR}/share/icons/hicolor/scalable/apps/lune.svg" "${PREFIX}/share/icons/hicolor/scalable/apps/lune.svg"
echo "Installed lune to ${PREFIX}"
EOF
    chmod 0755 "${stage}/install.sh"

    log "Creating ${output}..."
    tar -C "${WORK_DIR}/tar" --owner=0 --group=0 -cf - "${stem}" | gzip -n -9 > "${output}"
    write_checksum "${output}"
    BUILT_ARTIFACTS+=("${output}")
}

build_deb() {
    ensure_binary
    mkdir -p "${DIST_DIR}"

    local stage="${WORK_DIR}/deb"
    local output="${DIST_DIR}/${PKG_NAME}_${PKG_VERSION}_${DEB_ARCH}.deb"

    populate_usr_tree "${stage}/usr"
    mkdir -p "${stage}/DEBIAN"
    chmod 0755 "${stage}" "${stage}/DEBIAN"

    local installed_size
    installed_size="$(du -sk "${stage}/usr" | awk '{print $1}')"

    cat > "${stage}/DEBIAN/control" <<EOF
Package: ${PKG_NAME}
Version: ${PKG_VERSION}
Section: x11
Priority: optional
Architecture: ${DEB_ARCH}
Maintainer: Lune Developers
Installed-Size: ${installed_size}
Depends: libc6, libgcc-s1, libwayland-client0, libxkbcommon0, libfontconfig1
Recommends: libvulkan1
Homepage: ${PKG_URL}
Description: ${PKG_SUMMARY}
 Lune is a GPU-accelerated Wayland terminal emulator written in Rust.
EOF
    chmod 0644 "${stage}/DEBIAN/control"

    log "Creating ${output}..."
    if command -v dpkg-deb >/dev/null 2>&1; then
        dpkg-deb --root-owner-group --build "${stage}" "${output}" >/dev/null
    elif command -v ar >/dev/null 2>&1; then
        local deb_work="${WORK_DIR}/deb-ar"
        mkdir -p "${deb_work}"
        printf '2.0\n' > "${deb_work}/debian-binary"
        tar -C "${stage}/DEBIAN" --owner=0 --group=0 -cf - . | gzip -n -9 > "${deb_work}/control.tar.gz"
        tar -C "${stage}" --owner=0 --group=0 -cf - usr | gzip -n -9 > "${deb_work}/data.tar.gz"
        rm -f "${output}"
        (cd "${deb_work}" && ar rcs "${output}" debian-binary control.tar.gz data.tar.gz)
    else
        die "linux-deb requires either 'dpkg-deb' or 'ar'"
    fi
    write_checksum "${output}"
    BUILT_ARTIFACTS+=("${output}")
}

build_rpm() {
    if ! command -v rpmbuild >/dev/null 2>&1; then
        if [[ "${ALLOW_MISSING_TOOLS}" == "1" ]]; then
            log "Skipping linux-rpm (rpmbuild not found)"
            SKIPPED_TARGETS+=("linux-rpm (missing rpmbuild)")
            return 0
        fi
        die "linux-rpm requires 'rpmbuild'"
    fi

    ensure_binary
    mkdir -p "${DIST_DIR}"

    local topdir="${WORK_DIR}/rpmbuild"
    mkdir -p "${topdir}"/{BUILD,BUILDROOT,RPMS,SOURCES,SPECS,SRPMS,tmp}

    cp "${STAGED_BIN}" "${topdir}/SOURCES/${PKG_NAME}"
    cp "${ROOT_DIR}/assets/lune.desktop" "${topdir}/SOURCES/lune.desktop"
    cp "${ROOT_DIR}/assets/lune.svg" "${topdir}/SOURCES/lune.svg"

    local rpm_requires=""
    case "${RPM_ARCH}" in
        x86_64|aarch64)
            rpm_requires=$'Requires:       libwayland-client.so.0()(64bit)\nRequires:       libxkbcommon.so.0()(64bit)\nRequires:       libfontconfig.so.1()(64bit)\nRecommends:     libvulkan.so.1()(64bit)'
            ;;
    esac

    local spec="${topdir}/SPECS/${PKG_NAME}.spec"
    cat > "${spec}" <<EOF
%define debug_package %{nil}
%define _build_id_links none

Name:           ${PKG_NAME}
Version:        ${PKG_VERSION}
Release:        ${PKG_RELEASE}
Summary:        ${PKG_SUMMARY}
License:        ${PKG_LICENSE}
URL:            ${PKG_URL}
BuildArch:      ${RPM_ARCH}
${rpm_requires}

%description
Lune is a GPU-accelerated Wayland terminal emulator written in Rust.

%install
install -Dm0755 %{_sourcedir}/${PKG_NAME} %{buildroot}%{_bindir}/${PKG_NAME}
install -Dm0644 %{_sourcedir}/lune.desktop %{buildroot}%{_datadir}/applications/lune.desktop
install -Dm0644 %{_sourcedir}/lune.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/lune.svg

%files
%attr(0755,root,root) %{_bindir}/${PKG_NAME}
%attr(0644,root,root) %{_datadir}/applications/lune.desktop
%attr(0644,root,root) %{_datadir}/icons/hicolor/scalable/apps/lune.svg
EOF

    log "Creating RPM package (${RPM_ARCH})..."
    rpmbuild --quiet \
        --define "_topdir ${topdir}" \
        --define "_tmppath ${topdir}/tmp" \
        --define "source_date_epoch_from_changelog 0" \
        --target "${RPM_ARCH}" \
        -bb "${spec}"

    local built_rpm
    built_rpm="$(find "${topdir}/RPMS" -type f -name '*.rpm' | head -n 1)"
    [[ -f "${built_rpm}" ]] || die "rpmbuild did not produce an .rpm file"

    local output="${DIST_DIR}/${PKG_NAME}-${PKG_VERSION}-${PKG_RELEASE}.${RPM_ARCH}.rpm"
    cp "${built_rpm}" "${output}"
    write_checksum "${output}"
    BUILT_ARTIFACTS+=("${output}")
}

build_arch() {
    if ! command -v bsdtar >/dev/null 2>&1 || ! command -v zstd >/dev/null 2>&1; then
        if [[ "${ALLOW_MISSING_TOOLS}" == "1" ]]; then
            log "Skipping linux-arch (bsdtar and zstd required)"
            SKIPPED_TARGETS+=("linux-arch (missing bsdtar/zstd)")
            return 0
        fi
        die "linux-arch requires 'bsdtar' and 'zstd'"
    fi

    ensure_binary
    mkdir -p "${DIST_DIR}"

    local stage="${WORK_DIR}/arch"
    local output="${DIST_DIR}/${PKG_NAME}-${PKG_VERSION}-${PKG_RELEASE}-${ARCH_PKG_ARCH}.pkg.tar.zst"

    populate_usr_tree "${stage}/usr"

    local pkg_size build_date
    pkg_size="$(du -sb "${stage}/usr" | awk '{print $1}')"
    build_date="${SOURCE_DATE_EPOCH:-$(date +%s)}"

    cat > "${stage}/.PKGINFO" <<EOF
pkgname = ${PKG_NAME}
pkgbase = ${PKG_NAME}
pkgver = ${PKG_VERSION}-${PKG_RELEASE}
pkgdesc = ${PKG_SUMMARY}
url = ${PKG_URL}
builddate = ${build_date}
packager = Unknown Packager
size = ${pkg_size}
arch = ${ARCH_PKG_ARCH}
license = ${PKG_LICENSE}
depend = glibc
depend = gcc-libs
depend = wayland
depend = libxkbcommon
depend = fontconfig
optdepend = vulkan-icd-loader: Vulkan GPU rendering
EOF

    log "Creating ${output}..."
    (
        cd "${stage}"
        # Generate pacman-compatible .MTREE metadata before archiving.
        LANG=C bsdtar -czf .MTREE --format=mtree \
            --options='!all,use-set,type,uid,gid,mode,time,size,md5,sha256,link' \
            --uid 0 --gid 0 \
            .PKGINFO usr
        LANG=C bsdtar -cf - --uid 0 --gid 0 .PKGINFO .MTREE usr \
            | zstd -q -T0 -19 -o "${output}"
    )
    write_checksum "${output}"
    BUILT_ARTIFACTS+=("${output}")
}

build_appimage() {
    local tool="${APPIMAGETOOL:-}"
    if [[ -z "${tool}" ]] && command -v appimagetool >/dev/null 2>&1; then
        tool="$(command -v appimagetool)"
    fi
    local runtime="${APPIMAGE_RUNTIME:-}"
    local has_runtime=0
    if [[ -n "${runtime}" && -f "${runtime}" ]] && command -v mksquashfs >/dev/null 2>&1; then
        has_runtime=1
    fi

    if [[ -z "${tool}" && "${has_runtime}" != "1" ]]; then
        if [[ "${ALLOW_MISSING_TOOLS}" == "1" ]]; then
            log "Skipping linux-appimage (appimagetool or APPIMAGE_RUNTIME + mksquashfs not found)"
            SKIPPED_TARGETS+=("linux-appimage (missing appimagetool)")
            return 0
        fi
        die "linux-appimage requires 'appimagetool' in PATH (or APPIMAGETOOL=/path/to/appimagetool, or APPIMAGE_RUNTIME=/path/to/runtime with mksquashfs)"
    fi

    ensure_binary
    mkdir -p "${DIST_DIR}"

    local appdir="${WORK_DIR}/lune.AppDir"
    local output="${DIST_DIR}/${PKG_NAME}-${PKG_VERSION}-${APPIMAGE_ARCH}.AppImage"

    populate_usr_tree "${appdir}/usr"
    cp "${ROOT_DIR}/assets/lune.desktop" "${appdir}/lune.desktop"
    cp "${ROOT_DIR}/assets/lune.svg" "${appdir}/lune.svg"
    ln -sf "lune.svg" "${appdir}/.DirIcon"

    cat > "${appdir}/AppRun" <<'EOF'
#!/usr/bin/env sh
HERE="$(dirname "$(readlink -f "$0")")"
exec "${HERE}/usr/bin/lune" "$@"
EOF
    chmod 0755 "${appdir}/AppRun"

    log "Creating ${output}..."
    if [[ -n "${tool}" ]]; then
        ARCH="${APPIMAGE_ARCH}" "${tool}" --no-appstream "${appdir}" "${output}"
    else
        local squashfs="${WORK_DIR}/lune.squashfs"
        mksquashfs "${appdir}" "${squashfs}" -root-owned -noappend -comp zstd -quiet
        cat "${runtime}" "${squashfs}" > "${output}"
    fi
    chmod 0755 "${output}"
    write_checksum "${output}"
    BUILT_ARTIFACTS+=("${output}")
}

usage() {
    sed -n '3,26p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

main() {
    local target="${1:-release}"
    case "${target}" in
        -h|--help|help)
            usage
            return 0
            ;;
        release)
            ensure_binary
            log "Built binary: ${ROOT_DIR}/target/${TARGET_TRIPLE:+${TARGET_TRIPLE}/}${PROFILE}/${PKG_NAME}"
            return 0
            ;;
        linux-tar|linux-tar-gz|linux-targz|tar.gz|tar)
            build_tar_gz
            ;;
        linux-deb|deb)
            build_deb
            ;;
        linux-rpm|rpm)
            build_rpm
            ;;
        linux-arch|arch)
            build_arch
            ;;
        linux-appimage|appimage)
            build_appimage
            ;;
        linux-all|all)
            build_tar_gz
            build_deb
            build_rpm
            build_arch
            build_appimage
            ;;
        *)
            die "unknown build target '${target}' (run './scripts/package.sh --help' for targets)"
            ;;
    esac

    if [[ "${#BUILT_ARTIFACTS[@]}" -gt 0 ]]; then
        log "Artifacts written to ${DIST_DIR}:"
        for artifact in "${BUILT_ARTIFACTS[@]}"; do
            printf '  %s\n' "${artifact}"
        done
    fi
    if [[ "${#SKIPPED_TARGETS[@]}" -gt 0 ]]; then
        log "Skipped targets:"
        for skipped in "${SKIPPED_TARGETS[@]}"; do
            printf '  %s\n' "${skipped}"
        done
    fi
}

main "$@"
