Nuage DB Editor: the settings, applications and tokens of a Nuage DB server, in a window.
https://github.com/maxlestage/Nuage

Open it: double-click nuage-editor. GNOME Files does not start a program on a double-click:
right-click it, then Run as a Program. It needs a desktop (X11 or Wayland) with OpenGL, which
desktops have.

In the applications menu, for you only: in the editor, About, then "Add the editor to the
applications menu". It points at the editor where it is: if you move it, press it again.

The same in a terminal, the editor kept in ~/.local/bin:

    mkdir -p ~/.local/bin ~/.local/share/applications ~/.local/share/icons/hicolor/256x256/apps
    cp nuage-editor ~/.local/bin/
    sed "s|^Exec=.*|Exec=\"$HOME/.local/bin/nuage-editor\"|" io.github.maxlestage.nuage-editor.desktop \
        > ~/.local/share/applications/io.github.maxlestage.nuage-editor.desktop
    cp io.github.maxlestage.nuage-editor.png ~/.local/share/icons/hicolor/256x256/apps/

---

Nuage DB Éditeur : les réglages, les applications et les jetons d'un serveur Nuage DB, dans une
fenêtre. Ouvrez-le d'un double-clic sur nuage-editor (dans GNOME Fichiers : clic droit, Exécuter
comme un programme). Pour l'avoir dans le menu des applications : dans l'éditeur, À propos, puis
« Ajouter l'éditeur au menu des applications » ; ou les commandes ci-dessus.

Nuage DB Editor: los ajustes, las aplicaciones y los tokens de un servidor Nuage DB, en una
ventana. Ábrelo con doble clic en nuage-editor (en Archivos de GNOME: clic derecho, Ejecutar como
un programa). Para tenerlo en el menú de aplicaciones: en el editor, Acerca de y luego «Añadir el
editor al menú de aplicaciones»; o las órdenes de arriba.
