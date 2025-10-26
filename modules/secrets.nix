{
  lib,
  config,
  ...
}:
let
  inherit (lib.options) mkOption literalExpression;
  inherit (lib.types)
    unspecified
    nullOr
    listOf
    str
    bool
    attrsOf
    submodule
    functionTo
    package
    uniq
    ;
  inherit (lib.strings) concatStringsSep;
  inherit (lib.attrsets) mapAttrs;

  sharedSecret =
    { config, ... }:
    {
      options = {
        expectedOwners = mkOption {
          type = nullOr (listOf str);
          description = ''
            Specifies the list of hosts authorized to decrypt and access this shared secret.

            When null, secret ownership is managed manually via fleet.nix and CLI.
            Decrypted secrets will be stored at /run/secrets/$\{name} on authorized hosts.
          '';
          default = null;
        };
        regenerateOnOwnerAdded = mkOption {
          type = bool;
          description = ''
            Controls whether the secret must be regenerated when new owners are added.

            Set to true when the secret contains owner-specific references (e.g., X.509 Subject Alternative Names).
            When true, adding a new owner will trigger secret regeneration instead of simple re-encryption.
          '';
        };
        regenerateOnOwnerRemoved = mkOption {
          default = config.regenerateOnOwnerAdded;
          defaultText = literalExpression "regenerateOnOwnerAdded";
          type = bool;
          description = ''
            Determines secret behavior when owners are removed from the configuration.

            Typically mirrors regenerateOnOwnerAdded. Override cautiously.
            Set to false if host permissions are revoked through alternative mechanisms like firewall rules.
          '';
        };
        generator = mkOption {
          type = uniq (nullOr (functionTo package));
          description = ''
            Function evaluating to nix derivation responsible for (re)generating the secret's content.

            An input to this function - `pkgs` of a generator host with implementation-defined representation of extra encryption data,
            use `mkSecretGenerator` helpers to implement own generators.
          '';
          default = null;
        };
        expectedGenerationData = mkOption {
          type = unspecified;
          description = "Contextual metadata embedded within the secret part value";
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
    };
in
{
  options = {
    sharedSecrets = mkOption {
      type = attrsOf (submodule sharedSecret);
      default = { };
      description = "Collection of secrets shared across multiple hosts with configurable ownership";
    };
  };
  config = {
    hosts = mapAttrs (
      _: secretMap:
      let
        partsOf =
          s:
          removeAttrs s [
            "createdAt"
            "expiresAt"
            "generationData"
          ];

      in
      {
        nixos.data.secrets = mapAttrs (_: s: partsOf s) secretMap;
        # nixos.secrets = mapAttrs (
        #   _: s: mapAttrs (_: _: {}) (partsOf s)
        # ) secretMap;
      }
    ) config.data.hostSecrets;
    nixpkgs.overlays = [
      (final: prev: {
        mkSecretGenerators =
          { recipients }:
          rec {
            # TODO: Merge both generators to one with consistent options syntax?
            # Impure generator is built on local machine, then built closure is copied to remote machine,
            # and then it is ran in inpure context, so that this generator may access HSMs and other things.
            mkImpureSecretGenerator =
              {
                script,
                # If set - script will be run on remote machine, otherwise it will be run with fleet project in CWD
                # (Some secrets-encryption-in-git/managed PKI solution is expected)
                impureOn ? null,
              }:
              (prev.writeShellScript "impureGenerator.sh" ''
                #!/bin/sh
                set -eu

                export GENERATOR_HELPER_IDENTITIES="${concatStringsSep "\n" recipients}";
                export PATH=${final.fleet-generator-helper}/bin:$PATH

                # TODO: Provide tempdir from outside, to make it securely erasurable as needed?
                tmp=$(mktemp -d)
                cd $tmp
                # cd /var/empty

                created_at=$(date -u +"%Y-%m-%dT%H:%M:%S.%NZ")

                ${script}

                if ! test -d $out; then
                  echo "impure generator script did not produce expected \$out output"
                  exit 1
                fi

                echo -n $created_at > $out/created_at
                echo -n SUCCESS > $out/marker
              '').overrideAttrs
                (old: {
                  passthru = {
                    inherit impureOn;
                    generatorKind = "impure";
                  };
                });
            # Pure generators are disabled for now
            mkSecretGenerator = { script }: mkImpureSecretGenerator { inherit script; };

            # TODO: Implement consistent naming
            # Pure secret generator is supposed to be run entirely by nix, using `__impure` derivation type...
            # But for now, it is ran the same way as `impureSecretGenerator`, but on the local machine.
            # mkSecretGenerator = {script}:
            #   (prev.writeShellScript "generator.sh" ''
            #     #!/bin/sh
            #     set -eu
            #     # TODO: make nix daemon build secret, not just the script.
            #     cd /var/empty
            #
            #     created_at=$(date -u +"%Y-%m-%dT%H:%M:%S.%NZ")
            #
            #     ${script}
            #     if ! test -d $out; then
            #       echo "impure generator script did not produce expected \$out output"
            #       exit 1
            #     fi
            #
            #     echo -n $created_at > $out/created_at
            #     echo -n SUCCESS > $out/marker
            #   '')
            #   .overrideAttrs (old: {
            #     passthru = {
            #       generatorKind = "pure";
            #     };
            #     # TODO: make nix daemon build secret, not just the script.
            #     # __impure = true;
            #   });
          };
      })
    ];
  };
}
