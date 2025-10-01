{
  lib,
  inputs',
  pkgs,
  config,
  ...
}:
let
  inherit (lib.options) mkOption mkPackageOption;
  inherit (lib.types) listOf package functionTo;
in
{
  options = {
    tf.package = mkPackageOption pkgs "terraform" {
      extraDescription = "Terraform package to use";
    };
    tf.providers = mkOption {
      description = "List of used terraform providers";
      type = functionTo (listOf package);
      default = _: [ ];
    };
    tf.finalPackage = mkOption {
      description = "Terraform package with all providers";
      type = package;
    };
  };
  config = {
    tf.finalPackage = inputs'.fleet-tf.packages.terraform-locked.override {
      inherit (config.tf) providers;
      terraform = config.tf.package;
    };
    shelly.shells.default = {
      packages = [ config.tf.finalPackage ];
    };
    packages.terraform = config.tf.finalPackage;
  };
}
