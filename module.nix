{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:

let
  cfg = config.services.lagd;

  # All three stages run as *user* services, not system ones. The audio stage
  # has to live in the user's PipeWire session, and the Vulkan layer runs inside
  # the user's own processes — so putting the input stage anywhere else would
  # mean the control plane could not be one file in $XDG_RUNTIME_DIR that all
  # three agree on.
  userUnit = description: {
    inherit description;
    wantedBy = [ "default.target" ];
    serviceConfig = {
      Restart = "on-failure";
      RestartSec = 2;

      # Deliberately absent: PrivateDevices. The input stage needs
      # /dev/input/event* and /dev/uinput, and PrivateDevices hides exactly
      # those.
      LockPersonality = true;
      MemoryDenyWriteExecute = true;
      NoNewPrivileges = true;
      ProtectClock = true;
      ProtectHostname = true;
      ProtectKernelLogs = true;
      ProtectKernelTunables = true;
      RestrictNamespaces = true;
      RestrictRealtime = false; # the audio stage wants RT scheduling
      RestrictSUIDSGID = true;
      SystemCallArchitectures = "native";
    };
  };

  flag = name: value: lib.optionalString (value != null) "--${name} ${toString value}";
in
{
  options.services.lagd = {
    enable = lib.mkEnableOption "the lagd latency injectors";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "lagd";
      description = "The lagd package providing the three binaries and the Vulkan layer.";
    };

    users = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "bella" ];
      description = ''
        Users to add to the `input` and `uinput` groups.

        The input stage reads {file}`/dev/input/event*` and opens
        {file}`/dev/uinput`; as a user service it needs this membership. Listing
        a user here grants them the ability to read every keystroke on the
        machine, which is inherent to the feature, not incidental.
      '';
    };

    input = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Run the evdev/uinput input delay stage.";
      };

      devices = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        example = [ "/dev/input/by-id/usb-Logitech_G502-event-mouse" ];
        description = ''
          Device nodes to delay. Empty autodetects every keyboard and pointer.

          Prefer {file}`/dev/input/by-id` paths: `eventN` numbers are assigned
          in probe order and move between boots.
        '';
      };

      delayMs = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Delay to seed the input stage with at startup, in milliseconds.";
      };

      startDropped = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Start with the stage out of the signal path, so devices are not
          grabbed until {command}`lagd-ctl restore input` says so.
        '';
      };

      watchdogGraceMs = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 250;
        description = ''
          How far past its release deadline a queued event frame may sit before
          the watchdog drops the stage out of the path.

          This is the fail-open that keeps a stalled emitter from presenting as
          a dead keyboard. Raising it past a second or so defeats the purpose.
        '';
      };
    };

    audio = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Run the PipeWire virtual sink that delays audio output.";
      };

      target = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        description = ''
          Node name of the real sink to forward delayed audio to. Find it with
          {command}`wpctl status` or {command}`pw-cli ls Node`.

          Leaving this null autoconnects to the default sink, which becomes a
          feedback loop the moment lagd itself is made the default. Set it.
        '';
      };

      delayMs = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = "Delay to seed the audio stage with at startup, in milliseconds.";
      };

      rate = lib.mkOption {
        type = lib.types.ints.positive;
        default = 48000;
        description = "Sample rate the virtual sink offers.";
      };

      channels = lib.mkOption {
        type = lib.types.ints.positive;
        default = 2;
        description = "Channel count the virtual sink offers.";
      };

      fadeMs = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 10;
        description = ''
          Crossfade applied when the delay changes, in milliseconds. A delay
          change is a discontinuity in the signal; this is what keeps it from
          clicking. Shorter tracks the knob more tightly and clicks more.
        '';
      };

      startDropped = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = "Start with the virtual sink out of the graph entirely.";
      };
    };

    present = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Install the Vulkan layer that delays {function}`vkQueuePresentKHR`.

          The layer is implicit but gated on `LAGD_PRESENT=1`, so installing it
          does not put it in the path of every Vulkan process.
        '';
      };

      delayMs = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = ''
          Delay to seed the present stage with at startup, in milliseconds.

          Unlike the other two stages nothing owns this one — the layer lives
          inside each Vulkan client — so a one-shot unit seeds it at login.
        '';
      };

      enableForSession = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Set `LAGD_PRESENT=1` for the whole session, so every Vulkan client
          loads the layer.

          Off by default: a present delay costs throughput as well as latency
          (see the layer's documentation), so it is usually better to enable it
          per-process with {command}`LAGD_PRESENT=1 some-game`.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # Loads the uinput module and creates the `uinput` group the virtual devices
    # are built through.
    hardware.uinput.enable = lib.mkIf cfg.input.enable true;

    users.users = lib.genAttrs cfg.users (_: {
      extraGroups = [
        "input"
        "uinput"
      ];
    });

    warnings =
      lib.optional (cfg.enable && cfg.input.enable && cfg.users == [ ]) ''
        services.lagd.input is enabled but services.lagd.users is empty, so no
        user can open /dev/uinput and the input stage will exit at startup.
      ''
      ++ lib.optional (cfg.audio.enable && cfg.audio.target == null) ''
        services.lagd.audio.target is unset: the delayed output will autoconnect
        to the default sink. If you make lagd the default sink that is a feedback
        loop — set it to the real device's node name.
      '';

    systemd.user.services.lagd-input = lib.mkIf cfg.input.enable (
      lib.recursiveUpdate (userUnit "lagd input latency stage") {
        serviceConfig.ExecStart = lib.concatStringsSep " " (
          [
            "${lib.getExe' cfg.package "lagd-input"}"
            (flag "delay-ms" cfg.input.delayMs)
            (flag "watchdog-grace-ms" cfg.input.watchdogGraceMs)
          ]
          ++ lib.optional cfg.input.startDropped "--start-dropped"
          ++ map (device: "--device ${lib.escapeShellArg device}") cfg.input.devices
        );
      }
    );

    systemd.user.services.lagd-audio = lib.mkIf cfg.audio.enable (
      lib.recursiveUpdate (userUnit "lagd audio latency stage") {
        after = [ "pipewire.service" ];
        wants = [ "pipewire.service" ];
        serviceConfig.ExecStart = lib.concatStringsSep " " (
          [
            "${lib.getExe' cfg.package "lagd-audio"}"
            (flag "delay-ms" cfg.audio.delayMs)
            (flag "rate" cfg.audio.rate)
            (flag "channels" cfg.audio.channels)
            (flag "fade-ms" cfg.audio.fadeMs)
          ]
          ++ lib.optional cfg.audio.startDropped "--start-dropped"
          ++ lib.optional (cfg.audio.target != null) "--target ${lib.escapeShellArg cfg.audio.target}"
        );
      }
    );

    # Nothing is resident for the present stage, so its starting value has to be
    # written into the control plane by something. A one-shot at login is the
    # smallest thing that does it.
    systemd.user.services.lagd-present-seed = lib.mkIf cfg.present.enable {
      description = "Seed the lagd present latency stage";
      wantedBy = [ "default.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${lib.getExe' cfg.package "lagd-ctl"} set present ${toString cfg.present.delayMs}";
      };
    };

    environment.sessionVariables = lib.mkIf cfg.present.enableForSession {
      LAGD_PRESENT = "1";
    };
  };
}
