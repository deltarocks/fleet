#include "nix-eval/src/lib.rs"
#include "lib.hh"
#include <nix/fetchers/fetch-settings.hh>
#include <nix/util/ref.hh>
#include <nix_api_fetchers.h>

struct nix_fetchers_settings {
  nix::ref<nix::fetchers::Settings> settings;
};

extern "C" {
void set_fetcher_setting(nix_fetchers_settings *settings_struct,
                         const char *setting, const char *value) {
  auto &settings_ref = settings_struct->settings;
  bool result = settings_ref->set(setting, value);
}
}
