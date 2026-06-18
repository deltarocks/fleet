{
  lib,
  craneLib,
  installShellFiles,
  inputs,
  remowt-agents-bundle,

  stdenv,
  pkg-config,
  rustPlatform,
  rofi,
}:
let
  system = stdenv.hostPlatform.system;
in
craneLib.buildPackage rec {
  pname = "fleet";
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

  REMOWT_AGENTS_DIR = "${remowt-agents-bundle}";
  # TODO: built-in fleet prompter should be a prodash widget, or it should require
  # tty remowt prompter running on host machine idk.
  ROFI = "${rofi}/bin/rofi";

  buildInputs = [
    inputs.nix.packages.${system}.nix-expr-c
    inputs.nix.packages.${system}.nix-flake-c
    inputs.nix.packages.${system}.nix-fetchers-c
  ];
  nativeBuildInputs = [
    installShellFiles
    pkg-config
    rustPlatform.bindgenHook
  ];

  postInstall = ''
    for shell in bash fish zsh; do
      installShellCompletion --cmd fleet \
        --$shell <($out/bin/fleet complete --shell $shell)
    done
  '';
}
