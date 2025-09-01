#pragma once
#include <nix_api_fetchers.h>

extern "C" {
void set_fetcher_setting(nix_fetchers_settings *settings, const char *setting,
                         const char *value);
}
