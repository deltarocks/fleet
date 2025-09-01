{
  lib,
  craneLib,
  installShellFiles,
  inputs',
  pkg-config,
  rustPlatform,
}:
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

  buildInputs = [
    inputs'.nix.packages.nix-expr-c
    inputs'.nix.packages.nix-flake-c
    inputs'.nix.packages.nix-fetchers-c
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
