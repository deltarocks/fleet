{
  config,
  lib,
  ...
}:
let
  inherit (lib.options) mkOption;
  inherit (lib.modules) mkIf mkDefault;
  inherit (lib.types)
    attrsOf
    str
    submodule
    either
    listOf
    lines
    bool
    ;
  inherit (lib.attrsets) mapAttrs;
  inherit (lib.trivial) isString;
in
{
  options.system.onlineActivationScripts = mkOption {
    default = { };
    type = attrsOf (
      either str (submodule {
        options = {
          deps = mkOption {
            type = listOf str;
            default = [ ];
          };
          text = mkOption {
            type = lines;
          };
          supportsDryActivation = mkOption {
            type = bool;
            default = false;
          };
        };
      })
    );
    description = ''
      Same as activation scripts, but only ran on online activation (i.e when operator is actively running fleet deploy, and not on system restart)

      Can be used to apply configuration such as ceph monitor maps, which is required to be up-to-date to correctly function,
      we should not apply outdated ceph monmap.
    '';

    apply =
      set:
      mapAttrs (
        name: value:
        if isString value then
          {
            text = ''
              if [ ! -z ''${FLEET_ONLINE_ACTIVATION+x} ]; then
                ${value}
              fi
            '';
            deps = [ "onlineActivation" ];
          }
        else
          value
          // {
            deps = [ "onlineActivation" ] ++ value.deps;
            text = ''
              if [ ! -z ''${FLEET_ONLINE_ACTIVATION+x} ]; then
                ${value.text}
              fi
            '';
          }
      ) set;
  };

  config.system.activationScripts = {
    onlineActivation = {
      text = ''
        if [ ! -z ''${FLEET_ONLINE_ACTIVATION+x} ]; then
          1>&2 echo "online activation; hello, fleet!"
        fi
      '';
      supportsDryActivation = true;
    };
  }
  // config.system.onlineActivationScripts;

  config.systemd.services = mkIf config.networking.networkmanager.enable {
    # If machine is managed by fleet, we should not restart NetworkManager during activation,
    # as it will disrupt the activation process. Furthermore, NetworkManager is not declarative,
    # so even if user wants to update his network settings - disabled NetworkManager restart
    # will not affect that.
    NetworkManager.restartIfChanged = mkDefault false;
  };
}
