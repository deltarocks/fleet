{
  lib,
  config,
  ...
}:
let
  inherit (lib.options) mkOption;
  inherit (lib.types)
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
  inherit (lib.lists) elem filter;
  inherit (lib.attrsets) attrNames;

  sharedSecret =
    { config, ... }:
    {
      options = {
        expectedOwners = mkOption {
          type = listOf str;
          description = ''
            Specifies the list of hosts authorized to decrypt and access this shared secret.
          '';
        };
        regenerateOnOwnerAdded = mkOption {
          type = bool;
          description = ''
            Whether the secret prefers to be rotated when new owners are added.

            Note that this is only a security measure, if the secret needs to be regenerated due to e.g X.509 SANs
            changes - then you most likely want to use generationData for that instead.
          '';
          default = false;
        };
        regenerateOnOwnerRemoved = mkOption {
          type = bool;
          description = ''
            Whether the secret prefers to be rotated when the owners are removed, so the encrypted data
            stored in fleet state can't be decrypted by those. Note that the secrets are still present in encrypted
            form on those hosts until gc happens.
          '';
          default = false;
        };
        allowDifferent = mkOption {
          type = bool;
          description = ''
            When adding owner, do not update secret value for other owners, instead creating a new distribution.

            Defaults to true, since all secrets might differ on hosts on some point of deployment process.

            Secret generator might also have opinion on this, like it makes little sense for askPass/synchronizing
            generators to keep old data.
          '';
          default = true;
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
      };
    };
in
{
  options = {
    secrets = mkOption {
      type = attrsOf (submodule sharedSecret);
      default = { };
      description = "Collection of secrets shared across multiple hosts with configurable ownership";
    };
  };
  config = {
    nixos = {host, ...}: {
      _providedSharedSecrets = filter (name: elem host.name config.secrets.${name}.expectedOwners) (attrNames config.secrets);
    };
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
                generationData ? null,
                allowDifferent ? true,
                parts,
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
                    inherit
                      impureOn
                      parts
                      generationData
                      allowDifferent
                      ;
                    generatorKind = "impure";
                  };
                });
            # Pure generators are disabled for now
            mkSecretGenerator = { script, parts }: mkImpureSecretGenerator { inherit script parts; };

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
