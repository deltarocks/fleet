{
  lib,
  fleetLib,
  inputs,
  self,
  config,
  _fleetFlakeRootConfig,
  ...
}:
let
  inherit (lib.attrsets) mapAttrs;
  inherit (lib.options) mkOption;
  inherit (lib.types) deferredModule unspecified;
  inherit (lib.modules) mkRemovedOptionModule;
  inherit (lib.strings) escapeNixIdentifier;
  inherit (fleetLib.options) mkHostsOption;

  _file = ./nixos.nix;
in
{
  options = {
    nixos = mkOption {
      description = ''
        Shared nixos configuration module for all hosts.
      '';
      type = deferredModule;
    };
    hosts = mkHostsOption (hostArgs: {
      inherit _file;
      options = {
        nixos = mkOption {
          description = ''
            Nixos configuration for the current host.
          '';
          type = deferredModule;
          apply =
            module:
            let
              inherit (hostArgs.config) system;
            in
            config.nixpkgs.buildUsing.lib.nixosSystem {
              inherit system;
              modules = [
                (module // { key = "attr<host.nixos>"; })
                (config.nixos // { key = "attr<fleet.nixos>"; })
              ];
              specialArgs = {
                inherit fleetLib inputs self;
                inputs' = mapAttrs (
                  inputName: input:
                  builtins.addErrorContext
                    "while retrieving system-dependent attributes for input ${escapeNixIdentifier inputName}"
                    (
                      if input._type or null == "flake" then
                        _fleetFlakeRootConfig.perInput system input
                      else
                        "input is not a flake, perhaps flake = false was added to te input declaration?"
                    )
                ) inputs;
                self' = builtins.addErrorContext "while retrieving system-dependent attributes for a flake's own outputs" (
                  _fleetFlakeRootConfig.perInput system self
                );
              };
            };
        };
        nixos_unchecked = mkOption {
          type = unspecified;
        };
      };
      config = {
        # imports = [
        #   (mkRemovedOptionModule ["nixosModules"] "replaced with hosts.*.nixos.imports.")
        # ];
        nixos = {
          config._module.args = {
            nixosHosts = mapAttrs (_: value: value.nixos_unchecked.config) config.hosts;
            hosts = config.hosts;
            host = hostArgs.config;
            fleetConfiguration = config;
          };
        };
        nixos_unchecked = hostArgs.config.nixos.extendModules {
          modules = [
            {
              _module.check = false;
            }
          ];
        };
      };
    });
  };
  imports = [
    (mkRemovedOptionModule [ "nixosModules" ] "replaced with nixos.imports.")
  ];
  config.nixos.imports = import ./nixos/module-list.nix;
}
