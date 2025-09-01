{
  callPackage,
  craneLib,
  inputs',
}:
{
  fleet = callPackage ./fleet.nix { inherit craneLib inputs'; };
  fleet-install-secrets = callPackage ./fleet-install-secrets.nix { inherit craneLib; };
  fleet-generator-helper = callPackage ./fleet-generator-helper.nix { inherit craneLib; };
}
