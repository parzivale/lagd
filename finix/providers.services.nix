# how services.lagd runs, as providers.services units
#
# Separated from the module's own options the same way finix's own service modules are, so what
# this module asks of the service contract is in one place.
{
  config,
  pkgs,
  lib,
  ...
}:
let
  cfg = config.services.lagd;

  statePath = "${cfg.stateDir}/state";

  # Both of these live in a user's own session tree: the audio unit because the PipeWire it
  # talks to is the user's, and the session variable because what has to see it is the
  # session's children.
  needsUserTree = cfg.audio.enable || cfg.present.enableForSession;

  # The plane and its lock are pre-created with the right ownership rather than left to
  # whichever process maps them first. Otherwise the root system unit creates them under its own
  # umask - 0644 root:root - and a user in the `lagd` group can open neither for writing, which
  # is what `flock` on the lock file needs.
  stateRules = [
    {
      path = cfg.stateDir;
      type.directory = {
        mode = "0770";
        group = "lagd";
      };
    }
    {
      path = statePath;
      type.file = {
        mode = "0660";
        group = "lagd";
      };
    }
    {
      path = "${cfg.stateDir}/.lock";
      type.file = {
        mode = "0660";
        group = "lagd";
      };
    }
  ];

  flag = name: value: lib.optionalString (value != null) "--${name} ${toString value}";

  inputCommand = lib.concatStringsSep " " (
    [
      (lib.getExe' cfg.package "lagd-input")
      (flag "delay-ms" cfg.input.delayMs)
      (flag "watchdog-grace-ms" cfg.input.watchdogGraceMs)
    ]
    ++ lib.optional cfg.input.startDropped "--start-dropped"
    ++ map (device: "--device ${lib.escapeShellArg device}") cfg.input.devices
  );

  audioCommand = lib.concatStringsSep " " (
    [
      (lib.getExe' cfg.package "lagd-audio")
      (flag "delay-ms" cfg.audio.delayMs)
      (flag "rate" cfg.audio.rate)
      (flag "channels" cfg.audio.channels)
      (flag "fade-ms" cfg.audio.fadeMs)
    ]
    ++ lib.optional cfg.audio.startDropped "--start-dropped"
    ++ lib.optional (cfg.audio.target != null) "--target ${lib.escapeShellArg cfg.audio.target}"
  );
in
{
  config = lib.mkIf cfg.enable {
    providers.services.tmpfiles.rules = stateRules;

    providers.services.units = lib.mkMerge [
      (lib.mkIf cfg.input.enable {
        lagd-input = {
          description = "lagd input latency stage";

          # The device manager's settle, named outright for the same reason keyd names it:
          # lagd-input enumerates /dev/input once at startup and grabs what it finds, which is a
          # look at a set rather than a wait for one device - so there is no path to wait for,
          # and a keyboard whose driver probes late is simply one it never grabbed.
          requires = [
            "basic"
          ]
          ++ lib.optional config.services.udev.enable "udev-settle"
          ++ lib.optional config.services.mdevd.enable "coldplug"
          ++ lib.optional config.services.gardendevd.enable "gardendevd-settle";

          # Runs as root, like keyd: it needs `/dev/input/event*` and `/dev/uinput`, and a
          # dedicated user would need both groups plus `lagd` while still being a process that
          # reads every keystroke. The isolation would be nominal.
          type.service.command = inputCommand;

          environment.LAGD_STATE = statePath;
        };
      })

      (lib.mkIf cfg.present.enable {
        lagd-present-seed = {
          description = "seed the lagd present latency stage";

          # Nothing is resident for the present stage - the layer lives inside each Vulkan
          # client - so its starting delay has to be written into the plane by something.
          type.oneshot.command = "${lib.getExe' cfg.package "lagd-ctl"} set present ${toString cfg.present.delayMs}";

          environment.LAGD_STATE = statePath;
        };
      })
    ];

    # The audio stage belongs to the session, not to the system: the PipeWire it connects to is
    # the user's own, started by their login and gone when it ends. A system unit running as
    # that user would have no session to find.
    #
    # Gated on something actually needing a tree, rather than declared for every listed user:
    # `providers.services.users` being non-empty is itself what requires a session-scoped
    # supervisor, so naming a user here with nothing in their tree would make the input stage
    # alone unusable on an init that has none.
    providers.services.users = lib.mkIf needsUserTree (
      lib.genAttrs cfg.users (_: {
        units = lib.mkIf cfg.audio.enable {
          lagd-audio = {
            description = "lagd audio latency stage";
            type.service.command = audioCommand;
            environment.LAGD_STATE = statePath;
          };
        };

        # Set here rather than in `environment.variables` so it reaches the session's children -
        # which is what a Vulkan client is - and only for the users who asked for it.
        sessionVariables = lib.mkIf cfg.present.enableForSession { LAGD_PRESENT = "1"; };
      })
    );
  };
}
