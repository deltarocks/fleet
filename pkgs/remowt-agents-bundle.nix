{
  craneLib,
  lib,
  pkgs,
  runCommandLocal,
}:
let
  crateName = "remowt-agent";

  buildFor =
    {
      target,
      crossPkgs,
    }:
    let
      cc = crossPkgs.stdenv.cc;
      ccBin = "${cc}/bin/${cc.targetPrefix}";
      ut = builtins.replaceStrings [ "-" ] [ "_" ] target;
      linkerEnv = "CARGO_TARGET_${lib.toUpper ut}_LINKER";
    in
    craneLib.buildPackage (
      {
        src = craneLib.cleanCargoSource ../.;
        pname = "${crateName}-${target}";

        cargoExtraArgs = "--locked -p ${crateName}";

        CARGO_BUILD_TARGET = target;
        CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";

        depsBuildBuild = [ cc ];
        doCheck = false;
      }
      // {
        ${linkerEnv} = "${ccBin}cc";
        "CC_${ut}" = "${ccBin}cc";
        "CXX_${ut}" = "${ccBin}c++";
        "AR_${ut}" = "${ccBin}ar";
      }
    );
  x86_64 = buildFor {
    target = "x86_64-unknown-linux-musl";
    crossPkgs = pkgs;
  };
  aarch64 = buildFor {
    target = "aarch64-unknown-linux-musl";
    crossPkgs = pkgs.pkgsCross.aarch64-multiplatform-musl;
  };
  armv7l = buildFor {
    target = "armv7-unknown-linux-musleabihf";
    crossPkgs = pkgs.pkgsCross.armv7l-hf-multiplatform.pkgsMusl;
  };
in
runCommandLocal "remowt-agents-bundle"
  {
    passthru = {
      perArch = {
        inherit x86_64 aarch64 armv7l;
      };
    };
  }
  ''
    mkdir -p $out
    cp ${x86_64}/bin/remowt-agent  $out/remowt-agent-x86_64
    cp ${aarch64}/bin/remowt-agent $out/remowt-agent-aarch64
    cp ${armv7l}/bin/remowt-agent  $out/remowt-agent-armv7l
    chmod +w $out/remowt-agent-*

    for arch in x86_64 aarch64 armv7l; do
      hash=$(sha256sum "$out/remowt-agent-$arch" | cut -d' ' -f1)
      printf '%s %s\n' "$arch" "$hash" >> $out/hashes
    done
  ''
