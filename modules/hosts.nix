{
  lib,
  fleetLib,
  config,
  ...
}:
let
  inherit (fleetLib.modules) mkFleetGeneratorDefault;
  inherit (fleetLib.types) mkHostsType mkDataType;
  inherit (lib.options) mkOption;
  inherit (lib.types)
    str
    listOf
    attrsOf
    submodule
    ;
  inherit (lib.attrsets) mapAttrsToList mapAttrs;
  inherit (lib.lists) flatten groupBy;
in
{
  # Fleet Meta Configuration Module

  options = {
    data = mkOption {
      type = mkDataType {
        options = {
          version = mkOption {
            type = str;
            internal = true;
            description = "Internal version identifier for saved fleet state";
          };

          gcRootPrefix = mkOption {
            type = str;
            internal = true;
            description = "Prefix for fleet-generated gc garbage collection roots";
          };

          hosts = mkOption {
            type = attrsOf (submodule {
              options.encryptionKey = mkOption {
                type = str;
                description = "Rage SSH encryption key for host-bound secrets";
              };
            });
          };
        };
      };
      description = ''
        Persistent configuration data for fleet management.
        Typically used to maintain state between fleet configuration runs.
      '';
    };

    taggedWith = mkOption {
      type = attrsOf (listOf str);
      internal = true;
      description = "Mapping of hosts grouped by tags, used by fleet CLI";
    };

    hosts = mkOption {
      type = mkHostsType (
        { config, ... }:
        {
          options = {
            system = mkOption {
              description = "System architecture and platform identifier";
              type = str;
              example = "x86_64-linux";
            };

            tags = mkOption {
              description = ''
                Tags for host classification.
                Used for host selection via @tag syntax in CLI tools.
              '';
              type = listOf str;
            };

            # Network configuration details
            network = mkOption {
              type = submodule {
                options = {
                  internalIps = mkOption {
                    description = "List of internal IP addresses for the host";
                    type = listOf str;
                    default = [ ];
                  };

                  externalIps = mkOption {
                    description = "List of external IP addresses for the host";
                    type = listOf str;
                    default = [ ];
                  };
                };
              };
            };
          };
          config = {
            # Default hostname generation
            nixos.networking.hostName = mkFleetGeneratorDefault config._module.args.name;
            # Default 'all' tag for every host
            tags = [
              "all"
              "system/${config.system}"
            ];
          };
          _file = ./meta.nix;
        }
      );
      default = { };
    };
  };

  # Generate a mapping of hosts indexed by their tags
  config.taggedWith =
    let
      # Flatten host tags into a list of {hostname, tag} pairs
      hostTagList = flatten (
        mapAttrsToList (hostname: host: map (tag: { inherit hostname tag; }) host.tags) config.hosts
      );
      # Group hostnames by their tags
      grouped = mapAttrs (_: hosts: lib.map (pair: pair.hostname) hosts) (
        groupBy (elem: elem.tag) hostTagList
      );
    in
    grouped;

  # Source file reference
  _file = ./meta.nix;
}
