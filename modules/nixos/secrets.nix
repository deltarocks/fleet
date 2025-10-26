{
  lib,
  fleetLib,
  config,
  pkgs,
  ...
}:
let
  inherit (builtins) hashString elemAt length toJSON filter;
  inherit (lib.stringsWithDeps) stringAfter;
  inherit (lib.options) mkOption literalExpression;
  inherit (lib.lists) optional;
  inherit (lib.attrsets) mapAttrs mapAttrsToList;
  inherit (lib.modules) mkIf;
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
      loc,
      options,
      ...
    }:
    let
      secretName =
        # Due to config definition for freeformType, we can't just use _module.args due to infinite recursion, instead
        # extract the secret name the ugly way...
        let
          saLoc = options._module.specialArgs.loc;
          comp = elemAt saLoc;
        in
        assert
          (length saLoc == 2 ||
          length saLoc == 4 &&
          comp 0 == "secrets" && comp 2 == "_module" && comp 3 == "specialArgs") ||
          throw "Unexpected module structure ${toJSON saLoc}";
        if length saLoc == 2 then "documentation generator stub" else comp 1;
    in
    {
      freeformType = lazyAttrsOf (secretPartType secretName);
      options = {
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
        expectedPrivateParts = mkOption {
          type = listOf str;
          default = [ ];
          description = "List of parts that are expected to be encrypted";
        };
        expectedPublicParts = mkOption {
          type = listOf str;
          default = [ ];
          description = "List of parts that are expected to be public";
        };
      };
      config = mapAttrs (_: _: { }) (removeAttrs (sysConfig.data.secrets.${secretName} or {}) [ "shared" ]);
    }
  );
  processPart = secretName: partName: part: {
    inherit (part) path stablePath;
    raw = config.data.secrets.${secretName}.${partName}.raw;
  };
  processSecret =
    secretName: secret:
    {
      inherit (secret) group mode owner;
    }
    // (mapAttrs (processPart secretName) (
      removeAttrs secret [
        "shared"
        "generator"
        "mode"
        "group"
        "owner"
        "expectedGenerationData"
        "expectedPrivateParts"
        "expectedPublicParts"
      ]
    ));
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
      description = "Host-local secrets";
    };
    system.secretsData = mkOption {
      type = unspecified;
      default = {};
      description = "secrets.json contents";
    };
  };
  config = {
    system = {inherit secretsData;};
    environment.systemPackages = [ pkgs.fleet-install-secrets ];

    warnings = filter (v: v!=null) (mapAttrsToList (
      name: secret:
      if
        secret.expectedPrivateParts == [ ]
        && secret.expectedPublicParts == [ ]
        && !(config.data.secrets.${name} or { shared = false; }).shared
      then
        "Secret ${name} has no expected parts defined, this is deprecated for better visibility"
      else
        null
    ) config.secrets);

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
