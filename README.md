# powerlimit

Native GTK4 app (Rust) to cap CPU/GPU power on a Dell XPS 15 9510 (i7-11800H + RTX 3050 Ti)
and auto-tune the limits per game so the laptop stops overheating.

- **Presets & sliders** for CPU sustained (PL1), CPU burst (PL2) and GPU watts, with a live temperature/power graph.
- **Auto-tune**: start a game, press *Start tuning*. For ~30 min it climbs a ladder of power levels,
  tracks peak CPU/GPU temperature and GPU thermal-throttle flags, and saves the highest level that stays under
  your temperature caps (default 85 °C CPU / 80 °C GPU). There is no FPS reading, so "best" means
  *most power that stays cool*, not measured frame rate.
- **`powerlimit watch`** re-applies a game's saved profile whenever that game runs.
- Limits live in RAPL (`/sys/class/powercap`) and `nvidia-smi -pl`; they reset on reboot.
  Some laptops lock the GPU power limit, in which case only the CPU limit is applied.

Other Intel laptops with RAPL may work, but this is only tested on the XPS 15 9510. Use at your own risk.

## Build & install

Needs Rust, GTK 4 dev files (`libgtk-4-dev`), and `pkexec`/polkit. NVIDIA driver for GPU control.

```bash
make && sudo make install          # or:
cmake -B build && cmake --build build && sudo cmake --install build
```

Uninstall with `sudo make uninstall`. Then launch **Power Limiter** from the app menu, or run `powerlimit`.

## CLI

```
powerlimit status
sudo powerlimit set 35 45 30        # CPU PL1, PL2, GPU watts
sudo powerlimit reset
sudo powerlimit tune --game <name> --minutes 30 --cpu-max 85 --gpu-max 80
sudo powerlimit watch
```
Profiles are saved to `/etc/powerlimit/profiles.conf`.
