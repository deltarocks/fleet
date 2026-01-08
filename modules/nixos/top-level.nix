{
  pkgs,
  config,
  lib,
}:
let
  inherit (lib.strings) optionalString;
  # FIXME: Should not be copy-pasted, instead nixpkgs should export systemBuilder directly
  systemBuilder = ''
    mkdir $out

    ${
      if config.boot.initrd.enable && config.boot.initrd.systemd.enable then
        ''
          # This must not be a symlink or the abs_path of the grub builder for the tests
          # will resolve the symlink and we end up with a path that doesn't point to a
          # system closure.
          cp "$systemd/lib/systemd/systemd" $out/init

          ${lib.optionalString (!config.system.nixos-init.enable) ''
            cp ${config.system.build.bootStage2} $out/prepare-root
            substituteInPlace $out/prepare-root --subst-var-by systemConfig $out
          ''}
        ''
      else
        ''
          cp ${config.system.build.bootStage2} $out/init
          substituteInPlace $out/init --subst-var-by systemConfig $out
        ''
    }

    ln -s ${config.system.build.etc}/etc $out/etc

    ${lib.optionalString config.system.etc.overlay.enable ''
      ln -s ${config.system.build.etcMetadataImage} $out/etc-metadata-image
      ln -s ${config.system.build.etcBasedir} $out/etc-basedir
    ''}

    ln -s ${config.system.path} $out/sw
    ln -s "$systemd" $out/systemd

    echo -n "systemd ${toString config.systemd.package.interfaceVersion}" > $out/init-interface-version
    echo -n "$nixosLabel" > $out/nixos-version
    echo -n "${config.boot.kernelPackages.stdenv.hostPlatform.system}" > $out/system

    ${config.system.systemBuilderCommands}

    cp "$extraDependenciesPath" "$out/extra-dependencies"

      ${config.boot.bootspec.writer}
      ${optionalString config.boot.bootspec.enableValidation ''${config.boot.bootspec.validator} "$out/${config.boot.bootspec.filename}"''}
  '';
in
{
  system.build.toplevel-fleet = pkgs.stdenvNoCC.mkDerivation (
    {
      name = "nixos-system-${config.system.name}-${config.system.nixos.label}";
      preferLocalBuild = true;
      allowSubstitutes = false;
      passAsFile = [ "extraDependencies" ];
      buildCommand = systemBuilder;

      systemd = config.systemd.package;

      nixosLabel = config.system.nixos.label;

      inherit (config.system) extraDependencies;
    }
    // config.system.systemBuilderArgs
  );
}
