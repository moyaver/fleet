# Emulator process fixtures (macOS 26.6, Apple Silicon)

`ps-qemu-system.txt` is one line of `ps -axww -o pid=,ppid=,command=` for an Android emulator's
`qemu-system-*` process. It is what the agent's emulator mapping parses
(`crates/fleet-agent/src/emulator.rs`): the console port (`-port 5590`) gives the serial
`emulator-5590`, and `-avd` gives the AVD name.

**Never edit by hand and never invent one** (PLAN §0.6). The arguments contain no spaces, so
splitting the line on whitespace is exact for this capture.

## Provenance

- **Captured:** 2026-11-07 by starting the owner's existing AVD `Pixel_10a_atd` headless and
  read-only (`-read-only`, so its state was not written) on console port 5590, with
  `-no-window -no-snapshot -no-audio -no-boot-anim -no-metrics -memory 2048 -cores 2`. The process
  was then stopped. The line is the `qemu-system-aarch64-headless` process the emulator launcher
  started, with the arguments it passed on.
