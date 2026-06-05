{
  lib,
  craneLib,
  inputs,

  stdenv,
  pkg-config,
  rustPlatform,
}:
let
  system = stdenv.hostPlatform.system;
in
craneLib.buildPackage rec {
  pname = "remowt-plugin-fleet";
  src = lib.cleanSourceWith {
    src = ../.;
    filter =
      path: type:
      (lib.hasSuffix "\.cc" path)
      || (lib.hasSuffix "\.hh" path)
      || (craneLib.filterCargoSources path type);
  };
  strictDeps = true;

  cargoExtraArgs = "--locked -p ${pname}";

  buildInputs = [
    inputs.nix.packages.${system}.nix-expr-c
    inputs.nix.packages.${system}.nix-flake-c
    inputs.nix.packages.${system}.nix-fetchers-c
  ];
  nativeBuildInputs = [
    pkg-config
    rustPlatform.bindgenHook
  ];
}
