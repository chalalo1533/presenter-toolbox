#!/bin/sh
# Builds the .deb package for Debian/Ubuntu from an already compiled binary.
#
#   ./packaging/linux/build-deb.sh [binary] [output directory]
#
# Defaults to target/release/presenter_toolbox and dist/. Needs only dpkg-deb,
# which every Debian or Ubuntu machine already has.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${1:-$root/target/release/presenter_toolbox}
out=${2:-$root/dist}

[ -x "$bin" ] || { echo "No encuentro el binario: $bin" >&2; exit 1; }

# DEB_VERSION lets the release workflow use the tag instead of what Cargo.toml says.
# Debian versions have to start with a digit, so anything else (a branch name from a
# manual run, say) is ignored in favour of what Cargo.toml says.
version=$(sed -n 's/^version *= *"\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
case ${DEB_VERSION:-} in
    [0-9]*) version=$DEB_VERSION ;;
    ?*) echo "Aviso: ignoro DEB_VERSION='$DEB_VERSION' (no empieza por un dígito); uso $version" >&2 ;;
esac
arch=$(dpkg --print-architecture)

# The oldest glibc this binary can run against, read from the binary itself, so the
# package is honest about it whichever distribution built it.
glibc=$(objdump -T "$bin" 2>/dev/null \
    | sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' \
    | sort -V | tail -1)
[ -n "$glibc" ] || glibc=2.31

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
pkg=$work/presenter-toolbox

install -D -m 0755 "$bin" "$pkg/usr/bin/presenter_toolbox"
install -D -m 0644 "$root/packaging/linux/presenter-toolbox.desktop" \
    "$pkg/usr/share/applications/presenter-toolbox.desktop"

# Icons: the app draws them itself, at every size the hicolor theme looks for.
icons=$work/icons
"$bin" --write-png "$icons"
for size in 16 24 32 48 64 128 256; do
    install -D -m 0644 "$icons/$size.png" \
        "$pkg/usr/share/icons/hicolor/${size}x${size}/apps/presenter-toolbox.png"
done

install -D -m 0644 "$root/packaging/linux/LEEME.txt" \
    "$pkg/usr/share/doc/presenter-toolbox/LEEME.txt"

cat > "$work/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: Presenter Toolbox
Source: https://github.com/chalalo1533/presenter-toolbox

Files: *
Copyright: 2026 Gonzalo Perez Correa
License: MIT
 Permission is hereby granted, free of charge, to any person obtaining a copy
 of this software and associated documentation files (the "Software"), to deal
 in the Software without restriction, including without limitation the rights
 to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 copies of the Software, and to permit persons to whom the Software is
 furnished to do so, subject to the following conditions:
 .
 The above copyright notice and this permission notice shall be included in
 all copies or substantial portions of the Software.
 .
 THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 SOFTWARE.
EOF
install -D -m 0644 "$work/copyright" "$pkg/usr/share/doc/presenter-toolbox/copyright"

printf 'presenter-toolbox (%s) stable; urgency=low\n\n  * Ver las notas de la versión en\n    https://github.com/chalalo1533/presenter-toolbox/releases\n\n -- Gonzalo Perez Correa <chalalo@outlook.com>  %s\n' \
    "$version" "$(date -R)" > "$work/changelog"
gzip -9n -c "$work/changelog" > "$work/changelog.gz"
install -D -m 0644 "$work/changelog.gz" \
    "$pkg/usr/share/doc/presenter-toolbox/changelog.Debian.gz"

size_kb=$(du -ks "$pkg" | cut -f1)

# The libraries the program opens by name while it runs (it links almost nothing at
# load time), each mapped to the Debian package that ships it.
install -D -m 0644 /dev/stdin "$pkg/DEBIAN/control" <<EOF
Package: presenter-toolbox
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Maintainer: Gonzalo Perez Correa <chalalo@outlook.com>
Installed-Size: $size_kb
Depends: libc6 (>= $glibc), libgcc-s1 (>= 3.0), libx11-6, libx11-xcb1, libxcb1,
 libxcb-xkb1, libxcursor1, libxi6, libxkbcommon0, libxkbcommon-x11-0, libegl1,
 libfontconfig1
Recommends: xdg-desktop-portal, libwayland-client0, libwayland-egl1
Homepage: https://github.com/chalalo1533/presenter-toolbox
Description: Zoom, dibujo y foco en pantalla para presentaciones
 Presenter Toolbox amplia y congela la pantalla, deja dibujar encima, resalta el
 puntero, muestra los clics y las teclas que pulsas, recorta zonas al portapapeles
 y graba GIF, todo con atajos de teclado globales.
 .
 En sesiones X11 funciona completo. En sesiones Wayland (el escritorio
 predeterminado de Ubuntu) registra sus atajos en GNOME y captura la pantalla por
 el portal del escritorio; los efectos que necesitan capas sobre todo el escritorio
 no estan disponibles y se indican en la propia ventana de opciones.
EOF

mkdir -p "$out"
deb="$out/presenter-toolbox_${version}_${arch}.deb"
dpkg-deb --root-owner-group --build "$pkg" "$deb" >/dev/null
echo "$deb"
