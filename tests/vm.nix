# Runtime test for the parts that only exist on a booted machine: the user
# services actually starting in a real session, a real `EVIOCGRAB` being taken
# and released, the delay the input stage actually applies, the Vulkan layer
# loading without breaking Vulkan, and the virtual sink entering and leaving the
# PipeWire graph.
#
# `nix flake check` caches this by derivation, so it re-runs when the code
# changes and is instant when it has not.
{ self, pkgs }:
let
  system = pkgs.stdenv.hostPlatform.system;

  # Software Vulkan, so the layer's dispatch chain can be exercised on a machine
  # with no GPU.
  lvpIcd = "${pkgs.mesa}/share/vulkan/icd.d/lvp_icd.${pkgs.stdenv.hostPlatform.uname.processor}.json";

  testerUid = 1000;
in
pkgs.testers.runNixOSTest {
  name = "lagd";

  nodes.machine = {
    imports = [ self.nixosModules.default ];

    services.lagd = {
      enable = true;
      users = [ "tester" ];
      # Zero everywhere: this phase is about the units starting and sharing one
      # control plane, not about what the delays are. The measured delays are
      # driven from the test script against a separate control plane.
      input.delayMs = 0;
      audio.delayMs = 0;
      present.delayMs = 0;
      # Deliberately left null, so the delayed output autoconnects to the dummy
      # sink. The module warns about this, which is correct advice in general
      # and the right configuration here: lagd is not the default sink, so
      # there is no loop to walk into.
      audio.target = null;
    };

    users.users.tester = {
      isNormalUser = true;
      uid = testerUid;
      # Without lingering there is no `systemd --user` and no XDG_RUNTIME_DIR,
      # because a VM test never logs anyone in. Since the module ships all three
      # stages as user services, this is the assumption most likely to be wrong
      # in the module itself.
      linger = true;
    };

    # The `virt` machine type has no PS/2 controller, so without an explicit
    # keyboard there is no input device for the autodetecting unit to find and
    # it exits before the test can look at it.
    virtualisation.qemu.options = [ "-device virtio-keyboard-pci" ];
    virtualisation.memorySize = 2048;

    # snd-dummy gives PipeWire a real sink to forward to, so the audio stage is
    # exercised against a graph rather than against nothing.
    boot.kernelModules = [ "snd-dummy" ];
    services.pipewire = {
      enable = true;
      alsa.enable = true;
    };

    environment.systemPackages = [
      self.packages.${system}.lagd-probe
      pkgs.pipewire
      pkgs.vulkan-loader
    ];
  };

  testScript = ''
    import json

    # Commands as the session user, which is where the control plane lives.
    def as_tester(cmd):
        return "su tester -s /bin/sh -c 'XDG_RUNTIME_DIR=/run/user/1000 " + cmd + "'"

    def ctl(args):
        return as_tester("lagd-ctl " + args)

    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("user@1000.service")

    with subtest("all three stages come up as user services in a real session"):
        machine.wait_for_unit("lagd-input.service", user="tester")
        machine.wait_for_unit("lagd-audio.service", user="tester")
        # The present stage has no daemon; a one-shot seeds its delay.
        machine.wait_for_unit("lagd-present-seed.service", user="tester")

    with subtest("lagd-ctl reaches the same control plane the daemons mapped"):
        status = machine.succeed(ctl("status"))
        assert "/run/user/1000/lagd/state" in status, status
        machine.succeed(ctl("set input 55"))
        again = machine.succeed(ctl("status"))
        assert "55ms" in again, again
        machine.succeed(ctl("set input 0"))

    with subtest("the virtual sink enters and leaves the PipeWire graph"):
        machine.wait_until_succeeds(as_tester("pw-cli ls Node | grep -q lagd"), timeout=90)
        machine.succeed(ctl("drop audio"))
        machine.wait_until_fails(as_tester("pw-cli ls Node | grep -q lagd"), timeout=90)
        machine.succeed(ctl("restore audio"))
        machine.wait_until_succeeds(as_tester("pw-cli ls Node | grep -q lagd"), timeout=90)

    with subtest("the Vulkan layer is transparent to a real Vulkan client"):
        machine.succeed("mkdir -p /run/lagd-test")

        # Not vulkaninfo: it builds an Xlib window unconditionally and segfaults
        # destroying it on a machine with no X server, before flushing any
        # output, so it can neither report a device nor fail cleanly. The probe
        # does the minimum that exercises the layer — create an instance,
        # enumerate, create a device, fetch a queue — and exits non-zero if any
        # of it fails.
        #
        # That chain is the whole point: negotiate -> GetInstanceProcAddr ->
        # CreateInstance -> CreateDevice -> GetDeviceProcAddr -> GetDeviceQueue
        # all pass through us, and that is where a layer bug lives.
        vk = (
            "LD_LIBRARY_PATH=${pkgs.vulkan-loader}/lib"
            " VK_ICD_FILENAMES=${lvpIcd}"
            " VK_LOADER_DEBUG=layer"
        )
        off = json.loads(
            machine.succeed(vk + " lagd-probe vulkan 2>/run/lagd-test/loader-off.log")
        )
        on = json.loads(
            machine.succeed(
                vk + " LAGD_PRESENT=1 lagd-probe vulkan 2>/run/lagd-test/loader-on.log"
            )
        )
        print("without the layer:", off)
        print("with the layer:   ", on)

        assert off["physical_devices"] > 0, (
            "lavapipe enumerated no device even without the layer, so this "
            "subtest would prove nothing either way: " + str(off)
        )
        assert off["created_device"] and off["got_queue"], off
        assert on == off, (
            "the layer changed what a Vulkan client sees: "
            + str(on) + " with it, " + str(off) + " without"
        )

        # And it has to be gated: implicit means loaded into everything
        # otherwise.
        on_log = machine.succeed("cat /run/lagd-test/loader-on.log")
        off_log = machine.succeed("cat /run/lagd-test/loader-off.log")
        assert "liblagd_present.so" in on_log, (
            "LAGD_PRESENT=1 did not load the layer library:\n" + on_log[-3000:]
        )
        assert "liblagd_present.so" not in off_log, (
            "the layer library loaded without LAGD_PRESENT=1:\n" + off_log[-3000:]
        )

    with subtest("the input stage grabs the device and applies the delay it is told"):
        # Stop the autodetecting unit so only the instance under test holds the
        # synthetic device.
        machine.systemctl("stop lagd-input.service", user="tester")

        # A control plane of its own, so nothing here disturbs the session's.
        env = "--setenv=LAGD_STATE=/run/lagd-test/state"

        # The probe owns both ends of the measurement: it creates the synthetic
        # source, so it is the only process that can emit on it, and it reads the
        # twin. It writes the source's path out and then waits for the twin to
        # appear, which is how it hands over to lagd-input.
        machine.succeed(
            "systemd-run --unit=probe " + env
            + " --property=StandardOutput=file:/run/lagd-test/result.json"
            + " --property=StandardError=file:/run/lagd-test/probe.log"
            + " lagd-probe latency --path-file /run/lagd-test/source --delays 0,40,80 --count 12"
        )
        machine.wait_for_file("/run/lagd-test/source")
        source = machine.succeed("cat /run/lagd-test/source").strip()

        machine.succeed(
            "systemd-run --unit=delayer " + env
            + " --property=StandardOutput=file:/run/lagd-test/input.log"
            + " --property=StandardError=append:/run/lagd-test/input.log"
            + " lagd-input --device " + source
        )

        try:
            machine.wait_until_succeeds(
                "grep -q measurements /run/lagd-test/result.json", timeout=240
            )
        except Exception:
            machine.execute("cat /run/lagd-test/probe.log >&2")
            machine.execute("cat /run/lagd-test/input.log >&2")
            raise

        result = json.loads(machine.succeed("cat /run/lagd-test/result.json"))
        print(json.dumps(result, indent=2))

        # `drop` has to release the grab, not merely stop delaying — that
        # distinction is the whole reason it exists alongside `set 0`.
        assert result["grabbed_when_active"], "lagd-input never took an exclusive grab"
        assert not result["grabbed_when_dropped"], "`drop input` left the device grabbed"
        assert result["grabbed_after_restore"], "`restore input` did not take the grab back"

        # QEMU timing is noisy, so lean on the invariant noise cannot break: a
        # frame can be late, never early. The upper bound stays loose on purpose;
        # a tight one here would be a flaky test, which is worse than no test.
        slack_us = 40000
        for m in result["measurements"]:
            want = m["delay_ms"] * 1000
            assert m["samples"] >= 8, m
            assert m["min_us"] >= want, (
                "delay " + str(m["delay_ms"]) + "ms: fastest frame arrived in "
                + str(m["min_us"]) + "us, earlier than the " + str(want)
                + "us it was supposed to be held"
            )
            assert m["median_us"] < want + slack_us, (
                "delay " + str(m["delay_ms"]) + "ms: median " + str(m["median_us"])
                + "us is more than " + str(slack_us) + "us over target"
            )

        # The per-row bounds above would all pass for a stage that ignored its
        # configuration and delayed nothing, so check the delays differ too.
        medians = [m["median_us"] for m in result["measurements"]]
        assert medians == sorted(medians), medians
        assert medians[-1] - medians[0] > 60000, (
            "configured delays 0/40/80ms produced medians " + str(medians)
            + ", which is not a delay actually being applied"
        )
  '';
}
