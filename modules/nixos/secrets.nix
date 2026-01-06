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
    toJSON
    ;
  inherit (lib.stringsWithDeps) stringAfter;
  inherit (lib.options) mkOption literalExpression;
  inherit (lib.lists) optional;
  inherit (lib.attrsets) mapAttrs;
  inherit (lib.modules) mkIf;
  inherit (lib.types)
    submodule
    str
    attrsOf
    nullOr
    unspecified
    uniq
    functionTo
    package
    ;
  inherit (fleetLib.strings) decodeRawSecret;

  sysConfig = config;
  secretPartType =
    secretName:
    submodule (
      { config, ... }:
      let
        partName = config._module.args.name;
      in
      {
        options = {
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
          raw = mkOption {
            type = str;
            description = "Raw (encoded/encrypted secret part data)";
          };
        };
        config = {
          hash = hashString "sha1" config.raw;
          data = decodeRawSecret config.raw;
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
        parts = mkOption {
          type = attrsOf (secretPartType secretName);
          description = "Definition of secret parts";
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
      };
      config = {
        parts = builtins.fleetEnsureHostSecret sysConfig.networking.hostName secretName config.generator;
      };
    }
  );
  secretsData = (mapAttrs (_: s: s.definition) config.secrets);
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
