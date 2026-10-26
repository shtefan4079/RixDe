# RixDE

RixDE is a modern, lightweight, and responsive Wayland desktop environment written in Rust. It combines a hardware-accelerated compositor, a fluid user interface built with Slint, a native file manager, an integrated control center, and a secure display manager into a cohesive, polished desktop experience.

---

## Components

The RixDE workspace consists of modular, tightly integrated components:

- **`rix-comp`**: Wayland compositor powered by Smithay, featuring server-side decorations, fractional scaling, hardware-accelerated blur effects, and flexible multi-monitor layout management.
- **`rix-shell`**: Desktop panels, application launcher, taskbar, system tray (StatusNotifierItem), and system status popups.
- **`rix-settings`**: Central configuration hub for themes, window appearance, display outputs, audio, networking, and autostart apps.
- **`rix-files`**: Fast native file manager with tabbed browsing, archive handling (ZIP, TAR, 7z, RAR), and instant document previews.
- **`rix-capture`**: Interactive screenshot utility with markup tools, annotations, magnifier, and instant clipboard handoff.
- **`rix-dm` / `rix-greeter` / `rix-lock`**: Display manager, greeter, and screen locker integrated with Linux PAM authentication.
- **`clavis`**: Secret Service and encrypted credential storage manager.

---

## Building and Installation

### Prerequisites

Ensure you have Rust (stable 2021 edition) and essential system libraries installed:

- `wayland`, `wayland-protocols`
- `mesa`, `gbm`, `egl`, `drm`
- `xkbcommon`
- `pam`
- `pipewire`, `libpulse` (or `pipewire-pulse`)
- `systemd` / `logind`
- `fontconfig`, `freetype`

### Build

Compile the release binaries:

```bash
./scripts/rix-build-release
```

### Install

Install all binaries, desktop session entries, PAM policies, systemd services, and default themes:

```bash
sudo ./scripts/rix-install
```

To test installation in a staging directory:

```bash
./scripts/rix-install --destdir /tmp/rix-stage --prefix /usr
```

---

## License

RixDE is licensed under the **[MIT License](LICENSE-MIT)**.

Bundled third-party theme assets and fonts are distributed under their respective open-source licenses (GPL-3.0, LGPL-3.0, and SIL Open Font License 1.1). Full license texts are included in `assets/rix/themes/fairywren-dark/licenses/`.

---

## Acknowledgments & Credits

We would like to express our deepest gratitude to the open-source projects and communities that made RixDE possible:

- **KDE Community**: A very special thanks to the KDE project. The visual design, ergonomics, and aesthetic elegance of RixDE were heavily inspired by KDE Plasma, which the author deeply loves and admires.
- **FairyWren**: Special thanks to the creators and designers of the **FairyWren** theme and icon project for their beautiful artwork and theme design.
- **Smithay Project**: Sincere appreciation to the maintainers and contributors of **Smithay** for providing the incredible Rust framework and building blocks for Wayland compositors.
- **Slint**: Thanks to the **Slint** team for the expressive declarative GUI toolkit that powers our shell, file manager, and system dialogs.
