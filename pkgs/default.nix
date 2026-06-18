{
  callPackage,
  craneLib,
  inputs,
}:
let
  remowt-agents-bundle = callPackage ./remowt-agents-bundle.nix { inherit craneLib; };
in
{
  fleet = callPackage ./fleet.nix { inherit craneLib inputs; };
  fleet-install-secrets = callPackage ./fleet-install-secrets.nix { inherit craneLib; };
  fleet-generator-helper = callPackage ./fleet-generator-helper.nix { inherit craneLib; };

  inherit remowt-agents-bundle;
  remowt-plugin-fleet = callPackage ./remowt-plugin-fleet.nix {
    inherit craneLib inputs remowt-agents-bundle;
  };
  remowt-ssh = callPackage ./remowt-ssh.nix { inherit craneLib remowt-agents-bundle; };
}
