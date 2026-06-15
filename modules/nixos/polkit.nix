{ ... }:
{
  users.groups.fleet-pusher = { };

  security.polkit.extraConfig = ''
    polkit.addRule(function(action, subject) {
      if (action.id == "org.freedesktop.systemd1.manage-units" && subject.isInGroup("fleet-pusher")) {
        const unit = action.lookup("unit");
        if (unit && unit.indexOf("remowt-fleet-") == 0) {
          return polkit.Result.YES;
        }
      }
    });
  '';
}
