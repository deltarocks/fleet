{
  description = "NixOS cluster configuration management";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/release-25.05";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    shelly.url = "github:CertainLach/shelly";
    fleet-tf = {
      url = "github:CertainLach/fleet-tf";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.shelly.follows = "shelly";
      inputs.flake-parts.follows = "flake-parts";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    # DeterminateSystem's nix fork is controversial, but I don't mind it,
    # and it has lazy-trees support which is useful for fleet.
    nix.url = "github:deltarocks/nix/fleet";
  };
  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake
      {
        inherit inputs;
      }
      {
        imports = [ inputs.shelly.flakeModule ];
        flake = rec {
          lib =
            (import ./lib {
              inherit (inputs.nixpkgs) lib;
            })
            // {
              fleetConfiguration = throw "function-based interface is deprecated, use flake-parts syntax instead";
            };
          flakeModules.default = import ./lib/flakePart.nix {
            inherit (inputs) crane;
          };
          flakeModule = flakeModules.default;

          flakeModules.fleet-tf = ./modules/extras/tf.nix;

          # Used to test nix-eval bindings
          testData = {
            testObj = {
              v = "Hello";
            };
            testString = "hello";
          };

          # To be used with https://github.com/NixOS/nix/pull/8892
          schemas =
            let
              inherit (inputs.nixpkgs.lib) mapAttrs;
            in
            {
              fleetConfigurations = {
                version = 1;
                doc = ''
                  The `fleetConfigurations` flake output defines fleet cluster configurations.
                '';
                inventory = output: {
                  children = mapAttrs (configName: cluster: {
                    what = "fleet cluster configuration";

                    children = mapAttrs (hostName: host: {
                      what = "host [${host.system}]";
                    }) cluster.config.hosts;
                    # It is possible to implement this inventory right now, but I want to
                    # get rid of `fleet.nix` file in the future.
                    # children.secrets = { };
                  }) output;
                };
              };
            };
        };
        # Supported and tested list of deployment targets.
        systems = [
          "x86_64-linux"
          "aarch64-linux"
          "armv7l-linux"
          "armv6l-linux"
        ];
        perSystem =
          {
            config,
            system,
            pkgs,
            self,
            inputs',
            ...
          }:
          let
            inherit (lib.attrsets) mapAttrs';
            inherit (lib.lists) elem;
            # Can also be built for darwin, through it is not usual to deploy nixos systems from macos machines.
            # I have no hardware for such testing, thus only adding machines I actually have and use.
            #
            # It is not possible to deploy any host from armv6/armv7 hardware, and I don't think it even makes sense.
            deployerSystems = [
              "aarch64-linux"
              "x86_64-linux"
            ];
            deployerSystem = elem system deployerSystems;
            lib = pkgs.lib;
            rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
            craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rust;
            treefmt = (inputs.treefmt-nix.lib.evalModule pkgs ./treefmt.nix).config.build;
          in
          {
            _module.args.pkgs = import inputs.nixpkgs {
              inherit system;
              overlays = [
                (inputs.rust-overlay.overlays.default)
                (final: prev: {
                  boehmgc = prev.boehmgc.overrideAttrs (prevAttrs: {
                    configureFlags = prevAttrs.configureFlags ++ [
                      "--enable-gc-assertions"
                    ];
                  });
                })
              ];
            };
            # Reference fleet package should be built with nightly rust, specified in rust-toolchain.toml.
            packages = lib.mkIf deployerSystem (
              let
                packages = pkgs.callPackages ./pkgs {
                  inherit craneLib inputs';
                };
              in
              packages // { default = packages.fleet; }
            );
            # fleet-install-secrets will not be built normally, because they are not ran directly by user most of the time.
            # checks there build packages for default nixpkgs rustPlatform packages.
            checks =
              let
                nixpkgsCraneLib = inputs.crane.mkLib pkgs;
                packages = pkgs.callPackages ./pkgs {
                  craneLib = nixpkgsCraneLib;
                  inherit inputs;
                };
                prefixAttrs =
                  prefix: attrs:
                  mapAttrs' (name: value: {
                    name = "${prefix}${name}";
                    value = value.overrideAttrs (prev: {
                      pname = "${prefix}${prev.pname}";
                    });
                  }) attrs;
              in
              # fleet-install-secrets is installed to remote systems, thus needs to work
              # with rust in nixpkgs.
              (prefixAttrs "nixpkgs-" {
                inherit (packages) fleet-install-secrets;
              })
              // {
                formatting = treefmt.check self;
              };
            # TODO: It should be possible to move lib.mkIf to default attribute, instead of disabling the whole
            # devShells block, yet nix flake check fails here, due to no default shell found. It is nix or flake-parts bug?
            shelly.shells.default = lib.mkIf deployerSystem {
              factory = craneLib.devShell;
              packages = with pkgs; [
                rust
                cargo-edit
                cargo-udeps
                cargo-fuzz
                cargo-watch
                cargo-outdated

                pkg-config
                openssl
                rustPlatform.bindgenHook
                inputs'.nix.packages.nix-expr-c
                inputs'.nix.packages.nix-flake-c
                inputs'.nix.packages.nix-fetchers-c
              ];
              environment.PROTOC = "${pkgs.protobuf}/bin/protoc";
            };
            formatter = treefmt.wrapper;
          };
      };
}
