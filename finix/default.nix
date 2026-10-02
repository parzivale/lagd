# lagd on finix.
#
# The shape differs from the NixOS module in one structural way, and it is the better shape:
# there, all three stages are systemd user services sharing a control plane in
# `$XDG_RUNTIME_DIR`. Here the stages are split by what they actually need.
#
# The input stage needs devices, not a session - `/dev/input/event*` and `/dev/uinput` and
# nothing else - so it is a system unit, and works on every backend. The audio stage needs the
# session, because the PipeWire it talks to is the user's own, so it is a unit in that user's
# own tree. Those two cannot share `$XDG_RUNTIME_DIR`, so the control plane is pinned to a
# fixed path owned by a `lagd` group instead, and both reach the same file.
#
# `LAGD_STATE` exists for exactly this.
{ self }:
{
  config,
  pkgs,
  lib,
  ...
}:
let
  cfg = config.services.lagd;

  # Every component maps this one file: the two daemons, `lagd-ctl`, and the Vulkan layer
  # inside each of the user's own processes. Hence the group rather than a per-user path.
  stateDir = "/run/lagd";
  statePath = "${stateDir}/state";

  # Whether the configured init can be one user's own supervisor. finit cannot today, which is
  # a "not yet" rather than a "never"; dinit can.
  hasUserScope = config.providers.services.user.manager ? supervisor;
in
{
  imports = [ ./providers.services.nix ];

  options.services.lagd = {
    enable = lib.mkEnableOption "the lagd latency injectors";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "lagd";
      description = "The lagd package providing the three binaries and the Vulkan layer.";
    };

    users = lib.mkOption {
      type = with lib.types; listOf str;
      default = [ ];
      example = [ "bella" ];
      description = ''
        Users who may dial the stages, and whose session runs the audio stage.

        Each is added to `input` and `uinput` - the input stage's own needs, for the
        {command}`lagd-input --list` case and for a user-run instance - and to `lagd`, which
        is what grants write access to the control plane.

        Listing a user here lets them read every keystroke on the machine. That is inherent to
        the feature rather than incidental to this module.
      '';
    };

    stateDir = lib.mkOption {
      type = lib.types.str;
      default = stateDir;
      description = ''
        Directory holding the shared control plane.

        One fixed path, not `$XDG_RUNTIME_DIR`: the input stage is a system unit and the audio
        stage is a unit in a user's session tree, and a per-user path could not be reached by
        both.
      '';
    };

    input = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = "Run the evdev/uinput input delay stage, as a system unit.";
      };

      devices = lib.mkOption {
        type = with lib.types; listOf str;
        default = [ ];
        example = [ "/dev/input/by-id/usb-Logitech_G502-event-mouse" ];
        description = ''
          Device nodes to delay. Empty autodetects every keyboard and pointer.

          Prefer {file}`/dev/input/by-id` paths: `eventN` numbers are assigned in probe order
          and move between boots.
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
          Start with the stage out of the signal path, so no device is grabbed until
          {command}`lagd-ctl restore input` says so.
        '';
      };

      watchdogGraceMs = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 250;
        description = ''
          How far past its release deadline a queued event frame may sit before the watchdog
          drops the stage out of the path.

          This is the fail-open that keeps a stalled emitter from presenting as a dead
          keyboard. Raising it past a second or so defeats the purpose.
        '';
      };
    };

    audio = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Run the PipeWire virtual sink that delays audio output, in each listed user's session
          tree.

          Off by default, unlike the other two: it needs a session-scoped supervisor, which not
          every init finix supports can be, and it needs a PipeWire that finix does not itself
          ship a module for. Both are checked rather than assumed.
        '';
      };

      target = lib.mkOption {
        type = with lib.types; nullOr str;
        default = null;
        example = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        description = ''
          Node name of the real sink to forward delayed audio to. Find it with
          {command}`wpctl status` or {command}`pw-cli ls Node`.

          Leaving this null autoconnects to the default sink, which becomes a feedback loop the
          moment lagd itself is made the default. Set it.
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
          Crossfade applied when the delay changes, in milliseconds. A delay change moves the
          read head, which is a step in the signal; this is what keeps it from clicking.
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
          Install the Vulkan layer that delays {function}`vkQueuePresentKHR`, and seed its
          delay at boot.

          The layer is implicit but gated on `LAGD_PRESENT=1`, so installing it does not put it
          in the path of every Vulkan process.
        '';
      };

      delayMs = lib.mkOption {
        type = lib.types.ints.unsigned;
        default = 0;
        description = ''
          Delay to seed the present stage with at startup, in milliseconds.

          Nothing owns this stage - the layer lives inside each Vulkan client - so a oneshot
          writes it into the control plane once the directory exists.
        '';
      };

      enableForSession = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Set `LAGD_PRESENT=1` for each listed user's session, so every Vulkan client they
          start loads the layer.

          Off by default: a present delay costs throughput as well as latency, so it is usually
          better to enable it per-process with {command}`LAGD_PRESENT=1 some-game`.
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];

    # So an interactive `lagd-ctl` finds the plane without being told where it is.
    environment.variables.LAGD_STATE = statePath;

    hardware.uinput.enable = lib.mkIf cfg.input.enable true;

    # Write access to the control plane is group-granted, because the processes that need it do
    # not share a user: a root system unit, a unit in a user's session, and the Vulkan layer
    # inside whatever that user runs.
    users.groups.lagd = { };

    users.users = lib.genAttrs cfg.users (_: {
      extraGroups = [
        "input"
        config.hardware.uinput.group
        "lagd"
      ];
    });

    assertions = [
      {
        assertion = (cfg.audio.enable || cfg.present.enableForSession) -> hasUserScope;
        message = ''
          services.lagd ${
            if cfg.audio.enable then "audio.enable" else "present.enableForSession"
          } needs an init that can supervise a user's own tree, and
          providers.services.user.manager is `none` for the one configured.

          The audio stage talks to the user's PipeWire, so it has to start with their session
          and stop with it - a system unit merely running as that user has no session to find.
          A session variable is the same scope for the same reason: what has to see it is the
          session's children.

          Use an init with a per-user mode (dinit today; finit has no per-user instance yet),
          or leave both off. The input and present stages work without one, and the present
          layer can always be enabled per-process with `LAGD_PRESENT=1 some-game`.
        '';
      }
      {
        assertion = cfg.audio.enable -> cfg.users != [ ];
        message = ''
          services.lagd.audio.enable is set but services.lagd.users is empty, so there is no
          session tree for the virtual sink to live in.
        '';
      }
    ];

    warnings =
      lib.optional (cfg.input.enable && cfg.users == [ ]) ''
        services.lagd.input is enabled but services.lagd.users is empty. The stage itself will
        run - it is a system unit - but nobody can dial it, because write access to the control
        plane is granted through the `lagd` group.
      ''
      ++ lib.optional (cfg.audio.enable && cfg.audio.target == null) ''
        services.lagd.audio.target is unset: the delayed output will autoconnect to the default
        sink. If you make lagd the default sink that is a feedback loop - set it to the real
        device's node name.
      ''
      ++ lib.optional cfg.audio.enable ''
        services.lagd.audio.enable requires a running PipeWire, which finix ships no module
        for. The unit will start and fail until something provides one.
      '';
  };
}
