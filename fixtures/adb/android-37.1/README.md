# adb fixtures (Android 37.1 image)

Raw bytes of the adb server protocol, captured from a real emulator, used as input to the agent's
device parser and fake-adb tests (`crates/fleet-agent/src/adb.rs`, `crates/fleet-agent/src/device.rs`).
**Never edit by hand and never invent one** (PLAN §0.6): capture, then commit the bytes unchanged.

| File | Request(s) on the adb server | What it holds |
|---|---|---|
| `track-devices.raw` | `host:track-devices` | `OKAY`, then the device list: `002a` length, then `serial<TAB>state` lines |
| `probe.raw` | `host:transport:emulator-5584`, then `shell:<PROBE>` | the probe's output (`boot=1`, `/proc/stat` line, `/proc/meminfo`) |
| `probe-2.raw` | the same, a few seconds later | a second probe, to compute guest CPU between two samples |
| `probe-unset-prop.raw` | `host:transport:emulator-5584`, then `shell:<PROBE with an unset property>` | `boot=` (empty: the property is unset) |
| `missing-device.raw` | `host:transport:emulator-5999` | `FAIL0020device 'emulator-5999' not found` (adb's refusal) |

`<PROBE>` is the shell command the agent runs (`crates/fleet-agent/src/device.rs`):

```sh
echo "boot=$(getprop sys.boot_completed)"; head -n 1 /proc/stat; cat /proc/meminfo
```

Each `*.raw` is what the server sent on the socket, in order. Every request is answered with a four-byte
status (`OKAY` or `FAIL`), so a shell reply starts with the transport's `OKAY` and then the shell's
`OKAY`, and the output follows with no framing. The shell service (not `shell,v2`) does not translate
line endings: the output has LF only. The first eight bytes of `probe.raw` are therefore the two `OKAY`s.

## Provenance

- **Captured:** 2026-11-07, on macOS 26 (Darwin 25.6.0, Apple Silicon), against the host's adb server
  (platform-tools 37.0.0).
- **Device:** a throwaway AVD on the `system-images;android-37.1;google_apis_playstore_ps16k;arm64-v8a`
  image (`ro.build.version.sdk=37`), `hw.gpu.enabled=yes`, `hw.gpu.mode=auto`, booted headless with
  `-memory 2048 -cores 2 -no-snapshot -no-audio -no-boot-anim -no-metrics` on console port 5584
  (serial `emulator-5584`). The AVD lived in a temporary `ANDROID_AVD_HOME` and was deleted afterwards.
- **Not touched:** the owner's emulator (`emulator-5554`) appears in `track-devices.raw` only because
  it was in the device list. No command was sent to it.
- **How:** a 40-line socket client written for the capture (not committed) sent each request
  length-prefixed (four hex digits) to `127.0.0.1:5037`, then read the reply until the server closed
  the connection or a second went by. The commands are the ones listed in the table above.
