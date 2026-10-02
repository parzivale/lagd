# lagd

Deliberate latency injection for input, audio and video, dialled live and
independently per stage.

Three stages, three separate sets of knobs:

| stage | what it delays | how |
| --- | --- | --- |
| `input` | keyboards and pointers | grabs the evdev device, re-emits frames through a uinput twin |
| `audio` | what you hear | a PipeWire virtual sink with a ring buffer on the way to the real sink |
| `present` | Vulkan frames | a Vulkan layer that holds `vkQueuePresentKHR` |

The point of keeping them separate is A/B work: hold input latency fixed, drop
audio and video, and judge one thing at a time.

```console
$ lagd-ctl status
control plane /run/user/1000/lagd/state (version 1)
stage       delay   resume  path
input        40ms     40ms  active
audio        10ms     10ms  dropped
present      20ms     20ms  dropped
```

## Control

```
lagd-ctl status                 # all three stages, one line each
lagd-ctl set input 50           # absolute ms (clamped to 500)
lagd-ctl adj input -5           # relative, for sweeping
lagd-ctl drop audio present     # out of the path entirely, variadic
lagd-ctl restore audio          # back in, at the delay it still holds
lagd-ctl toggle present         # flip 0 <-> last non-zero value
lagd-ctl panic                  # everything to zero and out of the path
```

`drop` is not `set <stage> 0`, and the difference matters when you are
measuring:

| stage | `set 0` still costs | `drop` removes it by |
| --- | --- | --- |
| input | a grab + uinput round trip — measured at 0.6–1.3 ms, median 1.1 ms | releasing the `EVIOCGRAB`, so the real device delivers directly |
| audio | a PipeWire quantum | disconnecting both nodes, so clients move back to the real sink |
| present | nothing — the hook calls straight through | nothing to remove |

`toggle` exists for perceptual work: it remembers the value you turned off, so
you can flip a stage without retyping the delay you were testing.

## How the stages find their settings

There is no control socket. The present stage lives inside *every* Vulkan
client, so no single process could own one, and `vkQueuePresentKHR` must not
block on IPC. Instead every component maps one small `#[repr(C)]` struct out of
`$XDG_RUNTIME_DIR/lagd/state` (override with `LAGD_STATE`) and reads its own
stage's atomics; `lagd-ctl` writes them. Each stage has a fixed, independent
slot, so a write to one can never disturb another.

The hot paths read the delay directly. `bypass` is a state transition — the
input daemon has to ungrab, the audio daemon has to relink — so those two watch
it on a 20 Hz control tick instead. The present layer needs no thread: it reads
both fields inline.

## Caveats worth knowing before you trust a measurement

**The present delay costs throughput as well as latency.** Sleeping in present
pushes the frame back *and* pushes back the start of the next one, so framerate
falls to `1/(render + delay)`. Holding a frame without paying that would mean
owning the swapchain images, which a layer cannot do. A present delay is
therefore not the clean manipulation the input delay is.

**The present layer only sees Vulkan.** OpenGL needs a different hook
(`eglSwapBuffers` / `glXSwapBuffers` via `LD_PRELOAD`), and nothing running
inside a client can delay the compositor's own output. For a uniform delay on
everything you need a nested compositor.

**A delayed keyboard loses its LEDs.** Caps-lock and num-lock indicators flow
*into* a device rather than out of it, so forwarding them would need a second
pipe in the opposite direction. Not implemented.

**Changing the audio delay is a discontinuity.** It is crossfaded over
`--fade-ms` (10 ms by default) so it does not click. Shorter tracks the knob
more tightly and clicks more.

**Being in the `input` group means being able to read every keystroke on the
machine.** That is inherent to the feature, not incidental to this
implementation.

## Safety

The input stage grabs your keyboard, so it is built to fail open:

- the kernel releases a grab when the fd closes, so a crash or a `systemctl
  stop` always gives the device back;
- a *stalled* emitter would instead present as a live-but-silent keyboard, so a
  watchdog on the main thread drops the whole stage out of the path if a frame
  sits more than `--watchdog-grace-ms` past its deadline;
- `lagd-ctl panic` zeroes and drops all three stages;
- `systemctl --user stop lagd-input` is the blunt version, and needs no working
  keyboard beyond a TTY.

Frames, not events, are what get queued: a multitouch or absolute update split
across two delays would reach clients as a torn, self-contradictory state.

## Arch Linux

`packaging/arch/` holds a PKGBUILD. It is a VCS package (`lagd-git`) because
there is no tagged release yet to hash; once there is, it becomes `lagd` with a
versioned `source=` and a real checksum.

```console
$ cd packaging/arch && makepkg -si
```

All three stages install as **user** units, matching the NixOS module: the
control plane lives in `$XDG_RUNTIME_DIR`, which is where `lagd-ctl` and the
Vulkan layer inside your own processes look for it.

```console
$ sudo usermod -aG input "$USER"      # then log out and back in
$ systemctl --user enable --now lagd-input
```

Arguments live in `/etc/lagd/{input,audio,present}.conf`, read by the units as
`$LAGD_*_ARGS`, and are `backup=`-marked so pacman keeps your edits. Set
`audio.conf`'s `--target` before enabling `lagd-audio`.

The package uses the existing `input` group for `/dev/uinput` rather than adding
a second group, via a udev rule — a user who can already read every keyboard is
not meaningfully more privileged for being able to create the twins. It ships no
`lagd-probe`: that is the test harness, and it creates synthetic input devices.

## NixOS

```nix
{
  inputs.lagd.url = "github:parzivale/lagd";

  # in your configuration
  imports = [ inputs.lagd.nixosModules.default ];

  services.lagd = {
    enable = true;
    users = [ "bella" ];              # grants input + uinput group membership

    input.delayMs = 0;
    audio = {
      target = "alsa_output.pci-0000_00_1f.3.analog-stereo";
      delayMs = 0;
    };
    present.delayMs = 0;
  };
}
```

Find the audio target with `wpctl status` or `pw-cli ls Node`. Leave
`audio.target` unset and the delayed output autoconnects to the default sink —
which is a feedback loop the moment you make lagd itself the default. The module
warns about this.

All three run as **user** services: the audio stage has to live in your PipeWire
session and the Vulkan layer runs inside your own processes, so a system service
could not share one control plane with them.

The Vulkan layer is implicit but gated on `LAGD_PRESENT=1`, so installing it does
not put it in the path of every Vulkan process. Enable it per-process:

```console
$ LAGD_PRESENT=1 some-game
```

`services.lagd.present.enableForSession = true` sets it session-wide, which is
usually the wrong trade given the throughput cost above.

## Development

```console
$ nix develop          # cargo, clippy, evtest, wev, vulkaninfo, helvum
$ nix flake check      # clippy (pedantic, deny warnings), fmt, taplo, doc, nextest, audit, deny, VM
$ nix build            # all three binaries plus the layer, joined
$ lagd-input --list    # what autodetection would pick, and why not if nothing
```

### The VM test

Unit tests can cover the control plane and the ring buffer, but not a real
`EVIOCGRAB` or a real PipeWire graph. `checks.vm` boots a machine and checks the
things that only exist at runtime:

- all three stages start as **user** services in a session with no graphical
  login — the assumption most likely to be wrong in the module, since it needs
  `users.users.<name>.linger`;
- `lagd-ctl` and the daemons map the same `/run/user/1000/lagd/state`;
- the virtual sink appears in `pw-cli ls Node`, vanishes on `drop audio`, and
  comes back on `restore audio`;
- `vulkaninfo` succeeds under lavapipe with the layer active, which exercises the
  whole negotiate → `GetInstanceProcAddr` → `CreateInstance` → `CreateDevice` →
  `GetDeviceProcAddr` chain, and the layer library is *not* loaded without
  `LAGD_PRESENT=1`;
- the input stage really takes an exclusive grab, really releases it on `drop
  input`, and takes it back on `restore`;
- the delay it applies matches the delay it was told.

That last one is measured rather than asserted from the outside:
`crates/lagd-probe` creates a synthetic uinput keyboard, waits for `lagd-input`
to build its twin, then emits and times frames across the pair. One process owns
both ends on purpose — split in two it would be comparing clocks and racing the
device-creation order.

The timing assertion leans on the invariant that VM jitter cannot break: **a
frame can be late, never early.** So the lower bound is tight (`min >= delay`)
and the upper bound is deliberately loose. It also checks that 0/40/80 ms
produce *different* medians, because every per-row bound would pass for a stage
that ignored its configuration and delayed nothing.

What it measures, in a KVM-accelerated aarch64 VM:

| configured | min | median | max |
| --- | --- | --- | --- |
| 0 ms | 0.57 ms | 1.09 ms | 1.26 ms |
| 40 ms | 40.26 ms | 40.27 ms | 40.35 ms |
| 80 ms | 80.24 ms | 81.26 ms | 81.40 ms |

The 0 ms row is the stage's own overhead — the grab and uinput round trip you
cannot configure away, only `drop`. Expect it to be lower on hardware; it is
measured here, not promised.

The Vulkan half runs the probe's minimal client with and without
`LAGD_PRESENT=1` and requires the two reports to be identical, so the layer has
to be transparent rather than merely non-fatal.

```console
$ nix build .#checks.aarch64-linux.vm -L   # needs KVM
```

`nix flake check` caches it by derivation, so it re-runs when the code changes
and is instant when it has not.

Built with flake-parts, crane and rust-overlay.
