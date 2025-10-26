{
  lib,
  fleetLib,
  config,
  ...
}:
let
  inherit (fleetLib.options) mkDataOption;
  inherit (lib.options) mkOption;
  inherit (lib.types)
    nullOr
    listOf
    str
    attrsOf
    submodule
    bool
    unspecified
    ;
  inherit (lib.attrsets)
    mapAttrsToList
    mapAttrs
    filterAttrs
    genAttrs
    ;
  inherit (lib.lists) sort unique concatLists;
  inherit (lib.strings) toJSON;

  secretDataValue = {
    options = {
      raw = mkOption {
        type = nullOr str;
        description = "Raw secret data in unspecified encoded and optionally encrypted format.";
        default = null;
      };
    };
  };

  sharedSecretData = {
    freeformType = attrsOf (submodule secretDataValue);
    options = {
      createdAt = mkOption {
        type = str;
        description = "Timestamp of secret generation/last rotation.";
        default = null;
      };
      expiresAt = mkOption {
        type = nullOr str;
        description = "Expiration timestamp triggering mandatory secret rotation.";
        default = null;
      };

      owners = mkOption {
        type = listOf str;
        description = ''
          List of hosts currently authorized to decrypt this shared secret.

          If owners differ from expected owners, the secret is considered outdated
          and requires regeneration or re-encryption.
        '';
        default = [ ];
      };
      generationData = mkOption {
        type = unspecified;
        description = "Contextual metadata associated with secret part.";
        default = null;
      };
    };
    config = { };
  };

  hostSecretData = {
    freeformType = attrsOf (submodule secretDataValue);
    options = {
      createdAt = mkOption {
        type = str;
        description = "Timestamp of secret generation/last rotation.";
        default = null;
      };
      expiresAt = mkOption {
        type = nullOr str;
        description = "Expiration timestamp triggering mandatory secret rotation.";
        default = null;
      };
      shared = mkOption {
        type = bool;
        description = "Indicates if secret is a shared secret, so other hosts might have the same piece of secret data.";
        default = false;
      };
      generationData = mkOption {
        type = unspecified;
        description = "Contextual metadata associated with secret part.";
        default = null;
      };
    };
    config = { };
  };
  managerKey = {
    options = {
      name = mkOption {
        type = str;
        description = "Who does this manager key belongs to.";
      };
      key = mkOption {
        type = str;
        description = "Age-compatible key";
      };
    };
    config = { };
  };
in
{
  options.data = mkDataOption (
    { config, ... }:
    {
      options = {
        managerKeys = mkOption {
          type = listOf (submodule managerKey);
        };
        sharedSecrets = mkOption {
          type = attrsOf (submodule sharedSecretData);
          default = { };
          description = "Shared secret data.";
        };
        hostSecrets = mkOption {
          type = attrsOf (attrsOf (submodule hostSecretData));
          default = { };
          description = "Host-specific secrets.";
          internal = true;
        };
      };
      config.hostSecrets =
        let
          hostsWithSharedSecrets = unique (
            concatLists (mapAttrsToList (_: s: s.owners) config.sharedSecrets)
          );
          secretsHavingHost = host: filterAttrs (_: secret: lib.elem host secret.owners) config.sharedSecrets;
          toHostSecret = _: secret: (removeAttrs secret [ "owners" ]) // { shared = true; };
        in
        genAttrs hostsWithSharedSecrets (host: mapAttrs toHostSecret (secretsHavingHost host));
    }
  );
  config = {
    assertions =
      (mapAttrsToList (name: secret: {
        assertion =
          secret.expectedOwners == null
          ||
            sort (a: b: a < b) (config.data.sharedSecrets.${name} or { owners = [ ]; }).owners
            == sort (a: b: a < b) secret.expectedOwners;
        message = "Shared secret ${name} is expected to be encrypted for ${toJSON secret.expectedOwners}, but it is encrypted for ${
          toJSON (config.data.sharedSecrets.${name} or { owners = [ ]; }).owners
        }. Run fleet secrets regenerate to fix";
      }) config.sharedSecrets)

      ++ (mapAttrsToList (name: secret: {
        # TODO: Same assertion should be in host secrets
        assertion =
          (config.data.sharedSecrets.${name} or { generationData = null; }).generationData
          == secret.expectedGenerationData;
        message = "Shared secret ${name} has unexpected generation data ${toJSON secret.expectedGenerationData} != ${
          toJSON (config.data.sharedSecrets.${name} or { generationData = null; }).generationData
        }. Run fleet secrets regenerate to fix";
      }) config.sharedSecrets);
  };
}
