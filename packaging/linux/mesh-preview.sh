#!/usr/bin/env bash
# The mesh preview build of this fork: WordCraft with co-editing over the Macula mesh, under its
# own identity (io.macula.wordcraft-mesh, package and binary wordcraft-mesh) so it installs beside
# an upstream WordCraft and never passes as one. Arguments go to package.sh (e.g. --formats).
#
# Source paths are remapped, so the binaries name no build machine's directories.
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export WORDCRAFT_APP_ID=io.macula.wordcraft-mesh
export WORDCRAFT_APP_NAME="WordCraft (mesh preview)"
export WORDCRAFT_PKG=wordcraft-mesh
export WORDCRAFT_MAINTAINER="${WORDCRAFT_MAINTAINER:-Macula <raf.lefever@erlef.org>}"
export WORDCRAFT_VENDOR=Macula
export WORDCRAFT_HOMEPAGE=https://github.com/rgfaber/wordcraft
ROOT="$(cd "$HERE/../.." && pwd)"
CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$CARGO_HOME=cargo --remap-path-prefix=$ROOT=wordcraft --remap-path-prefix=$HOME=home"
exec "$HERE/package.sh" "$@"
