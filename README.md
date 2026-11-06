# powerlimit

<img src="docs/icon.png" width="96" align="right">

Native GTK4 app (Rust) to cap CPU/GPU power on a Dell XPS 15 9510 (i7-11800H + RTX 3050 Ti)
and auto-tune the limits per game so the laptop stops overheating.

![Overview](docs/overview.png)

- **Presets & sliders** for CPU sustained (PL1), CPU burst (PL2) and GPU watts, with a live temperature/power graph.
- **Auto-tune** (open a game, pick it by PID, press *Start tuning*, ~30 min), three phases:
  1. bisects the shared CPU+GPU power budget in 1 W steps up to the thermal ceiling (thermal throttling or your temperature caps);
  2. at the ceiling, moves watts toward the bottleneck (GPU-bound game: CPU to GPU, otherwise the reverse) while the weighted CPU/GPU clock score keeps improving;
  3. picks the knee: the cheapest unthrottled setting within 1.5% of the best score, so it doesn't spend heat and fan noise on the last sliver of MHz.
  It waits for temperatures to level off before measuring and discards samples while the game is idle (menus, pause).
  There is no FPS reading, so "best" means the highest sustained clocks without throttling.
- **Saved profiles** appear in the window with Apply / delete buttons; tick *Auto-apply* (or run `sudo powerlimit watch`) to switch profiles automatically when a game starts.
- Limits live in RAPL (`/sys/class/powercap`) and `nvidia-smi -pl`. They reset on reboot unless you tick *Keep CPU limits after reboot* (or use `set ... --persist`), which installs a udev rule `/etc/udev/rules.d/99-powerlimit.rules` that re-applies the CPU limits when the RAPL device appears at boot - no service. The GPU limit is not kept.
  Some laptops lock the GPU power limit, in which case only the CPU limit is applied.

Other Intel laptops with RAPL may work, but this is only tested on the XPS 15 9510. Use at your own risk.

## Look and feel

A "console" interface: striped title bar, keycap buttons, dot-leader tables, ring dials. Pick **Sepia** or **Dark (Monokai Pro style)**, or follow the system, and choose a monospace font from the appearance menu in the header bar (IBM Plex Mono is the default; install `fonts-ibm-plex` for it). The window resizes from 480 px wide up, switching between side-by-side and stacked layouts.

## Install

**Easiest (Ubuntu/Debian):** download the `.deb` from the [latest release](https://github.com/ogden-marrow/powerlimit/releases/latest), then double-click it (Software / App Center) or run:

```bash
sudo apt install ./powerlimit_0.2.0_amd64.deb
```

Open **Power Limiter** from the app menu. Remove it with `sudo apt remove powerlimit`.

**From source** - needs Rust, GTK 4 dev files (`libgtk-4-dev`), `pkexec`/polkit; NVIDIA driver for GPU control:

```bash
make && sudo make install          # or: cmake -B build && cmake --build build && sudo cmake --install build
make deb                           # build your own .deb into dist/
```

Uninstall with `sudo make uninstall`.

## CLI

```
powerlimit status
sudo powerlimit set 35 45 30        # CPU PL1, PL2, GPU watts
sudo powerlimit reset
sudo powerlimit tune --game <name> --minutes 30 --cpu-max 85 --gpu-max 80
sudo powerlimit watch
```
Profiles are saved to `/etc/powerlimit/profiles.conf`.

## Testing the UI at different sizes

`POWERLIMIT_SHOT=<dir> POWERLIMIT_SHOT_SIZES=1280x900,480x600 POWERLIMIT_SHOT_THEMES=dark,sepia powerlimit gui` saves a PNG of every page for each theme and size, and logs when the window couldn't reach the requested size. `POWERLIMIT_DEMO=1` fills the Tune page with sample trials, and `POWERLIMIT_PROFILES=<file>` points the app at a test profiles file.
