{
  lib,
  fleetLib,
  config,
  pkgs,
  ...
}:
let
  inherit (builtins)
    hashString
    elemAt
    length
    toJSON
    filter
    ;
  inherit (lib.stringsWithDeps) stringAfter;
  inherit (lib.options) mkOption literalExpression;
  inherit (lib.lists) optional;
  inherit (lib.attrsets) mapAttrs mapAttrsToList;
  inherit (lib.modules) mkIf mkMerge;
  inherit (lib.types)
    submodule
    str
    attrsOf
    nullOr
    unspecified
    lazyAttrsOf
    uniq
    functionTo
    package
    listOf
    bool
    ;
  inherit (fleetLib.strings) decodeRawSecret;

  sysConfig = config;
  secretPartDataType = submodule {
    options = {
      raw = mkOption {
        type = str;
        internal = true;
        description = "Encoded & Encrypted secret part data, passed from fleet.nix";
      };
    };
  };
  secretDataType = submodule {
    freeformType = lazyAttrsOf secretPartDataType;
    options = {
      shared = mkOption {
        description = "Is this secret owned by this machine, or propagated from shared secrets";
        default = false;
      };
    };
  };
  secretPartType =
    secretName:
    submodule (
      { config, ... }:
      let
        partName = config._module.args.name;
      in
      {
        options = {
          encrypted = mkOption {
            type = bool;
            description = "Is this secret part supposed to be encrypted?";
          };

          hash = mkOption {
            type = str;
            description = "Hash of secret in encoded format";
          };
          path = mkOption {
            type = str;
            description = "Path to secret part, incorporating data hash (thus it will be updated on secret change)";
          };
          stablePath = mkOption {
            type = str;
            description = "Path to secret part, stable path (users are expected to watch for file changes/re-read secret on demand)";
          };
          data = mkOption {
            type = str;
            description = "Secret public data (only available for plaintext)";
          };
        };
        config =
          let
            raw = sysConfig.data.secrets.${secretName}.${partName}.raw;
          in
          {
            hash = hashString "sha1" raw;
            data = decodeRawSecret raw;
            path = "/run/secrets/${secretName}/${config.hash}-${partName}";
            stablePath = "/run/secrets/${secretName}/${partName}";
          };
      }
    );
  secretType = submodule (
    {
      config,
      ...
    }:
    let
      secretName = config._module.args.name;
    in
    {
      options = {
        shared = mkOption {
          type = bool;
          description = "Was this secret propagated from a shared secret?";
        };
        parts = mkOption {
          type = lazyAttrsOf (secretPartType secretName);
          description = "Definition of secret parts";
          default = { };
        };
        generator = mkOption {
          type = uniq (nullOr (functionTo package));
          description = "Derivation to evaluate for secret generation";
          default = null;
        };
        mode = mkOption {
          type = str;
          description = "Secret mode";
          default = "0440";
        };
        owner = mkOption {
          type = str;
          description = "Owner of the secret";
          default = "root";
        };
        group = mkOption {
          type = str;
          description = "Group of the secret";
          default = sysConfig.users.users.${config.owner}.group;
          defaultText = literalExpression "config.users.users.$${owner}.group";
        };
        expectedGenerationData = mkOption {
          type = unspecified;
          description = "Data that gets embedded into secret part";
          default = null;
        };
      };
      config = {
        shared = (sysConfig.data.secrets.${secretName} or { shared = false; }).shared;
        parts = mkMerge [
          (mkIf (config.generator != null)
            (
              # Get fake derivation body, in future it should be implemented the same way as in Rust.
              lib.callPackageWith (
                pkgs
                // {
                  mkSecretGenerator = pkgs.stdenv.mkDerivation;
                  mkImpureSecretGenerator = pkgs.stdenv.mkDerivation;
                }
              ) config.generator { }
            ).parts
          )
          (mapAttrs (_: _: { }) (
            removeAttrs (sysConfig.data.secrets.${secretName} or { }) [
              "shared"
              "managed"
            ]
          ))
        ];
      };
    }
  );
  processPart = secretName: partName: part: {
    inherit (part) path stablePath;
    raw = config.data.secrets.${secretName}.${partName}.raw;
  };
  processSecret = secretName: secret: {
    inherit (secret.definition) group mode owner;
    parts = (mapAttrs (processPart secretName) (secret.definition.parts));
  };
  secretsData = (mapAttrs (processSecret) config.secrets);
  secretsFile = pkgs.writeTextFile {
    name = "secrets.json";
    text = toJSON secretsData;
  };
  useSysusers =
    (config.systemd ? sysusers && config.systemd.sysusers.enable)
    || (config ? userborn && config.userborn.enable);
in
{
  options = {
    data.secrets = mkOption {
      type = attrsOf secretDataType;
      default = { };
      description = "Host-local secret data";
    };
    secrets = mkOption {
      type = attrsOf secretType;
      default = { };
      apply = v: (mapAttrs (_: secret: secret.parts // { definition = secret; }) v);
      description = "Host-local secrets";
    };
    system.secretsData = mkOption {
      type = unspecified;
      default = { };
      description = "secrets.json contents";
    };
  };
  config = {
    system = { inherit secretsData; };
    environment.systemPackages = [ pkgs.fleet-install-secrets ];

    systemd.services.fleet-install-secrets = mkIf useSysusers {
      wantedBy = [ "sysinit.target" ];
      after = [ "systemd-sysusers.service" ];
      restartTriggers = [
        secretsFile
      ];
      aliases = [
        "sops-install-secrets"
        "agenix-install-secrets"
      ];

      unitConfig.DefaultDependencies = false;

      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${pkgs.fleet-install-secrets}/bin/fleet-install-secrets install ${secretsFile}";
      };
    };
    system.activationScripts.decryptSecrets = mkIf (!useSysusers) (
      stringAfter
        (
          [
            # secrets are owned by user/group, thus we need to refer to those
            "users"
            "groups"
            "specialfs"
          ]
          # nixos-impermanence compatibility: secrets are encrypted by host-key,
          # but with impermanence we expect that the host-key is installed by
          # persist-file activation script.
          ++ (optional (config.system.activationScripts ? "persist-files") "persist-files")
        )
        ''
          1>&2 echo "setting up secrets"
          ${pkgs.fleet-install-secrets}/bin/fleet-install-secrets install ${secretsFile}
        ''
    );
  };
}
