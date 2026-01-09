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
  inherit (lib.types) deferredModule unspecified uniq str;
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
    hosts = mkHostsOption (hostArgs: let
      hostName = hostArgs.config._module.args.name;
    in {
      inherit _file;
      options = {
        name = mkOption {
          description = ''
            Host name (alias)
          '';
          type = uniq str;
          default = hostName;
        };
        nixos = mkOption {
          description = ''
            Nixos configuration for the current host.
          '';
          type = deferredModule;
          apply =
            module:
            let
              modulesPath = "${config.nixpkgs.buildUsing}/nixos/modules";
            in
            config.nixpkgs.buildUsing.lib.evalModules {
              class = "nixos";
              prefix = [
                "fleetConfiguration"
                "hosts"
                hostName
                "nixos"
              ];
              modules = (import "${modulesPath}/module-list.nix") ++ [
                (module // { key = "attr<host.nixos>"; })
                (config.nixos // { key = "attr<fleet.nixos>"; })
              ];
              specialArgs = {
                inherit
                  fleetLib
                  inputs
                  self
                  modulesPath
                  ;
              };
            };
        };
        nixos_unchecked = mkOption {
          type = unspecified;
        };
      };
      config = {
        nixos =
          let
            inherit (hostArgs.config) system;
          in
          {
            _module.args = {
              nixosHosts = mapAttrs (_: value: value.nixos_unchecked.config) config.hosts;
              hosts = config.hosts;
              host = hostArgs.config;
              fleetConfiguration = config;

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
            nixpkgs.hostPlatform = system;
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
  config.nixos.imports = import ./nixos/module-list.nix;
}
