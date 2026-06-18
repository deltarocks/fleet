{
  craneLib,
  remowt-agents-bundle,
  rofi,
}:
let
  pname = "remowt-ssh";
in
craneLib.buildPackage {
  inherit pname;
  src = craneLib.cleanCargoSource ../.;

  cargoExtraArgs = "--locked -p ${pname}";

  REMOWT_AGENTS_DIR = "${remowt-agents-bundle}";
  ROFI = "${rofi}/bin/rofi";
}
