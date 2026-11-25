set shell := ["bash", "-euo", "pipefail", "-c"]

[private]
default:
    @just --list

build-linux-rpm:
    @./scripts/package.sh linux-rpm

build-linux-deb:
    @./scripts/package.sh linux-deb

build-linux-arch:
    @./scripts/package.sh linux-arch

build-linux-tar:
    @./scripts/package.sh linux-tar

build-linux-appimage:
    @./scripts/package.sh linux-appimage

build-linux-all:
    @./scripts/package.sh linux-all
