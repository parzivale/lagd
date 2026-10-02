{
  description = "lagd — deliberate input, audio and present latency, dialled live";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    crane.url = "github:ipetkov/crane";

    flake-parts.url = "github:hercules-ci/flake-parts";

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } (
      { self, ... }:
      {

        systems = [
          "x86_64-linux"
          "aarch64-linux"
        ];

        flake.nixosModules.default = import ./module.nix { inherit self; };

        perSystem =
          {
            pkgs,
            lib,
            system,
            self',
            ...
          }:
          let
            rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
            craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

            # `cleanCargoSource` would strip the Vulkan layer manifest, which has
            # to be installed alongside the shared object it points at.
            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                (craneLib.fileset.commonCargoSources ./.)
                ./crates/lagd-present/lagd_present.json
              ];
            };

            commonArgs = {
              inherit src;
              strictDeps = true;

              # This is a virtual workspace, so there is no root Cargo.toml for
              # crane to read a name and version out of. Both are set explicitly
              # rather than left to be guessed.
              pname = "lagd";
              version = "0.1.0";

              nativeBuildInputs = [
                pkgs.pkg-config
                # pipewire-sys runs bindgen; the hook sets LIBCLANG_PATH and the
                # include flags it needs.
                pkgs.rustPlatform.bindgenHook
              ];

              buildInputs = [
                pkgs.pipewire
              ];
            };

            # Built once and reused by every package and check below.
            cargoArtifacts = craneLib.buildDepsOnly commonArgs;

            crate =
              pname:
              craneLib.buildPackage (
                commonArgs
                // {
                  inherit cargoArtifacts pname;
                  cargoExtraArgs = "-p ${pname}";
                  # The workspace's tests are run once, by the nextest check.
                  doCheck = false;
                }
              );

            lagd-input = crate "lagd-input";
            lagd-audio = crate "lagd-audio";
            lagd-ctl = crate "lagd-ctl";

            # A Vulkan layer is a plain shared object plus a manifest telling the
            # loader where to find it and which environment variable turns it on,
            # so neither of crane's binary-install conventions applies.
            lagd-present = craneLib.buildPackage (
              commonArgs
              // {
                inherit cargoArtifacts;
                pname = "lagd-present";
                cargoExtraArgs = "-p lagd-present";
                doCheck = false;

                doNotPostBuildInstallCargoBinaries = true;
                installPhaseCommand = ''
                  so=$(find target -name liblagd_present.so -not -path '*/deps/*' | head -1)
                  if [ -z "$so" ]; then
                    echo "ERROR: cargo produced no liblagd_present.so" >&2
                    exit 1
                  fi
                  install -Dm755 "$so" "$out/lib/liblagd_present.so"

                  manifest=$out/share/vulkan/implicit_layer.d/lagd_present.json
                  install -Dm644 crates/lagd-present/lagd_present.json "$manifest"
                  # The loader needs an absolute path, which only exists now.
                  substituteInPlace "$manifest" --replace-fail '@out@' "$out"
                '';

                meta = {
                  description = "Vulkan layer that holds vkQueuePresentKHR to inject frame latency";
                  platforms = lib.platforms.linux;
                };
              }
            );
          in
          {
            _module.args.pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [ inputs.rust-overlay.overlays.default ];
            };

            packages = {
              inherit
                lagd-input
                lagd-audio
                lagd-ctl
                lagd-present
                ;

              # One path to put on PATH and in XDG_DATA_DIRS: the three binaries
              # plus the layer and its manifest.
              default = pkgs.symlinkJoin {
                name = "lagd";
                paths = [
                  lagd-input
                  lagd-audio
                  lagd-ctl
                  lagd-present
                ];
                meta = {
                  description = "Deliberate input, audio and present latency, dialled live";
                  mainProgram = "lagd-ctl";
                  platforms = lib.platforms.linux;
                };
              };
            };

            apps.default = {
              type = "app";
              program = lib.getExe' lagd-ctl "lagd-ctl";
              meta.description = "Dial the lagd latency stages up and down at runtime";
            };

            checks = {
              inherit
                lagd-input
                lagd-audio
                lagd-ctl
                lagd-present
                ;

              lagd-clippy = craneLib.cargoClippy (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  cargoClippyExtraArgs = "--all-targets -- --deny warnings";
                }
              );

              lagd-doc = craneLib.cargoDoc (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  env.RUSTDOCFLAGS = "--deny warnings";
                }
              );

              lagd-fmt = craneLib.cargoFmt { inherit src; };

              lagd-toml-fmt = craneLib.taploFmt {
                src = lib.sources.sourceFilesBySuffices src [ ".toml" ];
              };

              lagd-audit = craneLib.cargoAudit {
                inherit src;
                inherit (inputs) advisory-db;
              };

              lagd-deny = craneLib.cargoDeny { inherit src; };

              lagd-nextest = craneLib.cargoNextest (
                commonArgs
                // {
                  inherit cargoArtifacts;
                  partitions = 1;
                  partitionType = "count";
                  cargoNextestPartitionsExtraArgs = "--no-tests=pass";
                }
              );
            };

            devShells.default = craneLib.devShell {
              inherit (self') checks;

              packages = [
                rustToolchain
                pkgs.pkg-config
                pkgs.pipewire
                # evtest and wev for watching what the twins actually emit;
                # vulkaninfo for checking the layer is being picked up.
                pkgs.evtest
                pkgs.wev
                pkgs.vulkan-tools
                pkgs.helvum
              ];

              # bindgenHook is a build-time hook; a dev shell needs the same
              # environment set by hand.
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            };

            formatter = pkgs.nixfmt-tree;
          };
      }
    );
}
