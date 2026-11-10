#!/bin/sh
# Instala Presenter Toolbox para tu usuario (sin sudo): programa, icono y entrada en el menú.
# Uso:  ./install.sh            instala
#       ./install.sh --autostart  además lo abre al iniciar sesión
#       ./install.sh --uninstall  lo quita
set -e
here=$(cd "$(dirname "$0")" && pwd)
bin="$HOME/.local/bin"
apps="$HOME/.local/share/applications"
icons="$HOME/.local/share/icons/hicolor/256x256/apps"
auto="$HOME/.config/autostart"

if [ "$1" = "--uninstall" ]; then
    rm -f "$bin/presenter_toolbox" "$apps/presenter-toolbox.desktop" \
          "$icons/presenter-toolbox.png" "$auto/presenter-toolbox.desktop"
    echo "Presenter Toolbox desinstalado. (Tu configuración sigue en ~/.config/presenter-toolbox)"
    exit 0
fi

mkdir -p "$bin" "$apps" "$icons"
install -m 755 "$here/presenter_toolbox" "$bin/presenter_toolbox"
install -m 644 "$here/presenter-toolbox.png" "$icons/presenter-toolbox.png"
sed "s|^Exec=.*|Exec=$bin/presenter_toolbox|" "$here/presenter-toolbox.desktop" > "$apps/presenter-toolbox.desktop"
if [ "$1" = "--autostart" ]; then
    mkdir -p "$auto"
    cp "$apps/presenter-toolbox.desktop" "$auto/presenter-toolbox.desktop"
    echo "Se abrirá solo al iniciar sesión."
fi
echo "Listo: búscalo en el menú como 'Presenter Toolbox' o ejecuta $bin/presenter_toolbox"
