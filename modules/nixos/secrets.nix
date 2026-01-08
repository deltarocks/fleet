{
  lib,
  fleetLib,
  config,
  pkgs,
  host,
  fleetConfiguration,
  ...
}:
let
  inherit (builtins)
    hashString
    toJSON
    ;
  inherit (lib.stringsWithDeps) stringAfter;
  inherit (lib.options) mkOption literalExpression;
  inherit (lib.lists) optional elem;
  inherit (lib.attrsets) mapAttrs mapAttrsToList;
  inherit (lib.modules) mkIf;
  inherit (lib.types)
    submodule
    str
    attrsOf
    unspecified
    uniq
    functionTo
    package
    bool
    enum
    either
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
      literal = l: enum [l];
    in
    {
      options = {
        parts = mkOption {
          type = uniq (attrsOf (secretPartType secretName));
          description = "Definition of secret parts";
        };
        generator = mkOption {
          type = either (functionTo package) (literal "shared");
          description = "Derivation to evaluate for secret generation";
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
        # C api is broken in regard to thunks
        # https://github.com/NixOS/nix/issues/12800
        parts = let 
          hostName = host._module.args.name;
          generator = config.generator;
        in builtins.deepSeq [
          hostName
          secretName
          generator
        ] (builtins.fleetEnsureHostSecret
          hostName
          secretName
          generator);
      };
    }
  );
  secretsFile = pkgs.writeTextFile {
    name = "secrets.json";
    text = toJSON config.system.secretsData;
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
      apply = mapAttrs (_: secret: secret.parts // {definition = secret;});
      description = "Host-local secrets";
    };
    system.secretsData = mkOption {
      type = unspecified;
      default = mapAttrs (_: s:
        (removeAttrs s.definition ["generator"]) // {
          parts = mapAttrs (_: part: removeAttrs part ["data"]) s.definition.parts;
        }
      ) config.secrets;
      description = "secrets.json contents";
    };
  };
  config = {
    environment.systemPackages = [ pkgs.fleet-install-secrets ];

    assertions = mapAttrsToList (name: secret: let
      hasSharedDefinition = fleetConfiguration.secrets ? name;
    in {
      assertion = (secret.definition.generator == "shared") == hasSharedDefinition && hasSharedDefinition -> (elem host._module.args.name fleetConfiguration.secrets.${name}.expectedOwners);
      message = if hasSharedDefinition then"secret ${name} has host-specific secret generator, secrets with host-specific generators can not have shared generator in fleet configuration"
      else "secret ${name} is declared as shared, for shared secret fleet configuration should include shared secret generator, and expectedOwners should contain this host";
    }) config.secrets;

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
