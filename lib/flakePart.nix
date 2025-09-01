{ crane }:
{
  fleetLib,
  lib,
  config,
  inputs,
  self,
  ...
}:
let
  inherit (lib.options) mkOption mkEnableOption;
  inherit (lib.attrsets) mapAttrs;
  inherit (lib.types)
    lazyAttrsOf
    deferredModule
    unspecified
    str
    ;
  inherit (lib.strings) isPath;
  inherit (lib.modules) mkIf mkOptionDefault;
in
{
  options.fleetModules = mkOption {
    type = lazyAttrsOf unspecified;
    default = { };
  };
  options.fleetNixosConfigurationsCompat = {
    enable = mkEnableOption "Create nixosConfiguration output based on fleetConfiguration";
    configuration = mkOption {
      type = str;
      description = "Which fleetConfiguration to use for compatibility";
      default = "default";
    };
    data = mkOption {
      type = unspecified;
      description = "Imported fleet.nix file for fleet";
    };
  };
  options.fleetConfigurations = mkOption {
    type = lazyAttrsOf deferredModule;
    apply =
      nameToModule:
      mapAttrs (
        name: module: data:
        let
          # To use user-provided nixpkgs, we first need to extract wanted nixpkgs attribute,
          # to do that, evaluate all the modules with only needed option declared.
          bootstrapEval = lib.evalModules {
            modules = [
              module
              {
                options.nixpkgs.buildUsing = mkOption {
                  description = ''
                    Nixpkgs to use for fleetConfiguration evaluation.
                  '';
                };
                config = {
                  _module.check = false;
                  nixpkgs.buildUsing = mkOptionDefault inputs.nixpkgs;
                };
              }
            ];
          };
          bootstrapNixpkgs = bootstrapEval.config.nixpkgs.buildUsing;
          normalEval = bootstrapNixpkgs.lib.evalModules {
            modules = (import ../modules/module-list.nix) ++ [
              module
              ({inputs', ...}: {
                config = {
                  data = if isPath data then import data else data;
                  nixpkgs.buildUsing = mkOptionDefault bootstrapNixpkgs;
                  nixpkgs.overlays = [
                    (final: prev: {
                      inherit
                        (import ../pkgs {
                          inherit (prev) callPackage;
                          inherit inputs';
                          craneLib = crane.mkLib prev;
                        })
                        fleet-install-secrets
                        fleet-generator-helper
                        ;
                    })
                  ];
                };
              })
            ];
            specialArgs = {
              inherit inputs self;
              fleetLib = import ../lib {
                inherit (bootstrapNixpkgs) lib;
              };
              _fleetFlakeRootConfig = config;
            };
          };
        in
        normalEval
      ) nameToModule;
  };
  config = {
    _module.args.fleetLib = import ../lib { inherit lib; };
    flake.fleetConfigurations = config.fleetConfigurations;
    flake.nixosConfigurations =
      let
        cfg = config.fleetNixosConfigurationsCompat;
      in
      mkIf cfg.enable (
        mapAttrs (name: host: host.nixos)
          (config.fleetConfigurations.${cfg.configuration} cfg.data).config.hosts
      );
    flake.fleetModules = config.fleetModules;
  };

  _file = ./flakePart.nix;
}
