# Shared functions for fleet configuration, available as `fleet` module argument
{ lib }:
let
  inherit (lib.trivial) isFunction functionArgs;
  inherit (lib.options) mkOption mergeOneOption;
  inherit (lib.modules) mkOverride;
  inherit (lib.types)
    listOf
    submodule
    attrsOf
    mkOptionType
    ;
  inherit (lib.strings) optionalString hasPrefix removePrefix;
in
rec {
  types = {
    overlay = mkOptionType {
      name = "nixpkgs-overlay";
      description = "nixpkgs overlay";
      check = {
        __functor = _self: isFunction;
        isV2MergeCoherent = true;
      };
      merge = mergeOneOption;
    };
    listOfOverlay = listOf types.overlay;

    mkHostsType = module: attrsOf (submodule module);
    mkDataType = module: submodule module;
  };

  options = {
    mkHostsOption =
      module:
      mkOption {
        type = types.mkHostsType module;
      };
    mkDataOption =
      module:
      mkOption {
        type = types.mkDataType module;
      };
  };

  inherit (options) mkHostsOption;

  modules = {
    /**
      Use in places, where fleet might know better than nixpkgs defaults to
    */
    mkFleetDefault = mkOverride 999;
    /**
      Some generators use mkDefault, but optionDefault is set by nixpkgs.
    */
    mkFleetGeneratorDefault = mkOverride 1001;
  };

  inherit (modules) mkFleetDefault mkFleetGeneratorDefault;

  secrets = {

    /**
      Generate a random secret password, 32 ascii characters by default

      Options:
        size: generated password length in ascii characters (bytes).
        noSymbols: by default, character set includes various special characters ($ , ! + * : ~), and might
                   not be accepted in some contexts, this option switches charset to just [A-Za-z0-9].

      Output:
        Resulting secret has only part: secret, which contains encrypted password.
    */
    mkPassword =
      {
        size ? 32,
      }:
      (
        {
          coreutils,
          mkSecretGenerator,
        }:
        mkSecretGenerator {
          script = ''
            mkdir $out
            gh generate password -o $out/secret --size ${toString size}
          '';
          parts.secret.encrypted = true;
        }
      );

    /**
      Generate a random ed25519 keypair

      Options:
        noEmbedPublic: By default, secret key also embeds public key in itself ("extended" format, 64 bytes)
                       When noEmbedPublis is enabled - only the private scalar is included.
        encoding: Encoring of public and secret parts, can be "raw" (default), "base64" or "hex".

      Output:
        Resulting secret has two parts: public and secret, where the secret part is encrypted.

      This secret format is used by e.g Garage S3 server
    */
    mkEd25519 =
      {
        noEmbedPublic ? false,
        encoding ? null,
      }:
      (
        { mkSecretGenerator }:
        mkSecretGenerator {
          script = ''
            mkdir $out
            gh generate ed25519 -p $out/public -s $out/secret \
              ${optionalString noEmbedPublic "--no-embed-public"} \
              ${optionalString (encoding != null) "--encoding=${encoding}"}
          '';
          parts.secret.encrypted = true;
          parts.public.encrypted = false;
        }
      );

    /**
      Generate a random x25519 keypair

      Options:
        encoding: Encoring of public and secret parts, can be "raw" (default), "base64" or "hex".

      Output:
        Resulting secret has two parts: public and secret, where the secret part is encrypted.

      This secret format is used by e.g Wireguard VPN for peers (base64-encoded)
    */
    mkX25519 =
      {
        encoding ? null,
      }:
      (
        { mkSecretGenerator }:
        mkSecretGenerator {
          script = ''
            mkdir $out
            gh generate x25519 -p $out/public -s $out/secret \
              ${optionalString (encoding != null) "--encoding=${encoding}"}
          '';

          parts.secret.encrypted = true;
          parts.public.encrypted = false;
        }
      );

    /**
      Generate a random RSA keypair

      Options:
        size: RSA key size, 4096 by default

      Output:
        Resulting secret has two parts: public and secret, where the secret part is encrypted.
        Both parts are PEM encoded.
    */
    mkRsa =
      {
        size ? 4096,
      }:
      (
        {
          openssl,
          mkSecretGenerator,
        }:
        mkSecretGenerator {
          script = ''
            mkdir $out

            ${openssl}/bin/openssl genrsa -out rsa_private.key ${toString size}
            ${openssl}/bin/openssl rsa -in rsa_private.key -pubout -out rsa_public.key

            cat rsa_private.key | gh private -o $out/secret
            cat rsa_public.key | gh public -o $out/public
          '';

          parts.secret.encrypted = true;
          parts.public.encrypted = false;
        }
      );

    /**
      Generate a random byte sequence

      Options:
        size: generated password length in bytes, 32 by default.
        encoding: how the generated bytes should be encoded, "raw" (default), "hex" or "base64"
        noNuls: prevent output byte sequence from containing internal \0, useful for some C applications
                that can't handle their strings properly.

      Output:
        Resulting secret has only part: secret, which contains encrypted bytes.

      Might be used for e.g. Wireguard VPN PSK keys (base64-encoded)
    */
    mkBytes =
      {
        count ? 32,
        encoding,
        noNuls ? false,
      }:
      (
        { mkSecretGenerator }:
        mkSecretGenerator {
          script = ''
            mkdir $out
            gh generate bytes --count=${toString count} --encoding=${encoding} -o $out/secret \
              ${optionalString noNuls "--no-nuls"}
          '';
          parts.secret.encrypted = true;
        }
      );
    /**
      Shorthand for `mkBytes`, which defaults to "hex" encoding
    */
    mkHexBytes =
      {
        count ? 32,
      }:
      mkBytes {
        inherit count;
        encoding = "hex";
      };
    /**
      Shorthand for `mkBytes`, which defaults to "base64" encoding
    */
    mkBase64Bytes =
      {
        count ? 32,
      }:
      mkBytes {
        inherit count;
        encoding = "base64";
      };

    # Wireguard
    # mkWireguard = {}: mkX25519 {encoding = "base64";};
    # mkWireguardPsk = {}: mkBase64Bytes {count = 32;};
  };

  inherit (secrets)
    mkPassword
    mkEd25519
    mkX25519
    mkRsa
    mkBytes
    mkHexBytes
    mkBase64Bytes
    ;

  strings =
    let
      plaintextPrefix = "<PLAINTEXT>";
      plaintextNewlinePrefix = "<PLAINTEXT-NL>";
    in
    {
      /**
        Decode public secret part into string
      */
      decodeRawSecret =
        raw:
        if hasPrefix plaintextPrefix raw then
          removePrefix plaintextPrefix raw
        else if hasPrefix plaintextNewlinePrefix raw then
          removePrefix plaintextNewlinePrefix raw
        else
          throw "decodeRawSecret only works with plaintext-encoded secret public parts, got ${raw}";
    };

  inherit (strings) decodeRawSecret;
}
