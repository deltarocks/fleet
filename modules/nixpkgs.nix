{
  lib,
  fleetLib,
  config,
  ...
}:
let
  inherit (lib.options) mkOption literalExpression;
  inherit (lib.types) path;
  inherit (fleetLib.options) mkHostsOption;
  inherit (fleetLib.types) listOfOverlay;

  _file = ./nixpkgs.lib;
in
{
  options = {
    nixpkgs = {
      buildUsing = mkOption {
        description = ''
          Default nixpkgs to use for building the systems.
        '';
        type = path;
      };
      overlays = mkOption {
        description = ''
          Package overlays to apply for all the hosts, gets propagated into
          `hosts.*.nixosModules.nixpkgs.overlays`.
        '';
        type = listOfOverlay;
      };
    };
    hosts = mkHostsOption {
      inherit _file;
      options.nixpkgs.buildUsing = mkOption {
        description = ''
          Nixpkgs to use for building the system.

          Note that this option is defined at the host level, not the nixosModules level,
          nixosModules will be evaluated using this flake input.
        '';
        type = path;
        default = config.nixpkgs.buildUsing;
        defaultText = literalExpression "config.nixpkgs.buildUsing";
      };
      config.nixos = {
        inherit _file;
        nixpkgs.overlays = config.nixpkgs.overlays;
      };
    };
  };
}
