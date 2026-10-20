<p align="center">
  <img src="https://gamemax-br.com/img/produtos/FOTO%20PARA%20SITE-Photoroom(2).png" alt="GAMEMAX Sigma 520 n2 CPU Cooler" width="600">
</p>

# CPU Cooler Temperature Display via Argus Monitor

When I saw a CPU Cooler with a temperature display for sale I thought: Cool! They've managed to put a thermal probe into the cooler.

Silly me! These cheap Chinese CPU Coolers with seven segment displays depend on USB and crappy proprietary software to work.

Given that I already run Argus Monitor on my computer I took advantage of its data API to update the temperature on the CPU Cooler's display, all while being faster and lighter than the original software.

This program is for the GAMEMAX Sigma 520 n2. Change `VENDOR_ID` and `PRODUCT_ID` in `src/main.rs` to match another USB display. It uses the CPU0 temperature, which is the package temperature Argus Monitor reports for AMD.

The process sleeps for two seconds between samples, reads Argus shared memory in place, and sends a USB report only when the rounded temperature changes or 15 seconds have passed since the last write.

## Efficiency

Version 2.0.0 is a Rust rewrite of the Python/PyInstaller 1.x service. Same behavior, a fraction of the resources:

| | 1.x (Python) | 2.0.0 (Rust) | Improvement |
|---|---:|---:|---:|
| Idle CPU (% of 1 core) | 0.022% | 0.0019% | **11× less** |
| Startup CPU time | 199 ms | 4.3 ms | **46× less** |
| Private memory | 15.4 MiB | 648 KiB | **24× less** |
| Working set (RAM in use) | 32.4 MiB | 260 KiB | **127× less** |
| Executable size | 8.15 MiB | 322 KiB | **26× smaller** |
| Processes / threads | 2 / 11 | 1 / 1 | |
| Handles | 229 | 55 | **4× fewer** |

Measured on the same machine with Argus Monitor and the display connected, each version running alone. Memory is the median over 30 s of steady state, taken 15 s after launch. CPU % is process cycle time over those 30 s divided by one core's capacity (6.54 ms vs 0.58 ms of CPU time); Task Manager rounds both to 0%.

What makes the difference:

- One native process instead of the PyInstaller bootloader plus a Python interpreter.
- The Argus layout is probed once, then each cycle reads only the CPU temperature straight from the shared mapping. Nothing is copied.
- The display is found through `cfgmgr32` instead of SetupAPI, so the crypto and RPC libraries SetupAPI pulls in never load.
- Startup-only pages are released to Windows after the first cycle, and the threads Windows spawns while loading DLLs exit after about 30 s.

## Building

The build uses the Rust GNU toolchain (`x86_64-pc-windows-gnu`) and MinGW-w64 `windres`, which embeds `exhaust-fan.ico` and the version info. One way to install both:

```bash
winget install Rustlang.Rust.GNU
winget install BrechtSanders.WinLibs.POSIX.UCRT
```

Then:

```bash
cargo build --release
```

The result is a single file, `target/release/argus-cpu-display-service-r.exe` (version 2.0.0). It links only Windows system libraries.

If rustup defaults to MSVC, add the GNU target and build for it. The executable then lands in `target/x86_64-pc-windows-gnu/release/`.

```bash
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

Started from Explorer or at login, it runs with no console window. Started from a terminal, it logs there; Ctrl+C stops it. Otherwise end the process in Task Manager.

```bash
argus-cpu-display-service-r.exe --debug
argus-cpu-display-service-r.exe --extract-sensors
```

