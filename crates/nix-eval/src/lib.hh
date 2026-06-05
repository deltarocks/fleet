#pragma once
#include "rust/cxx.h"
#include <nix_api_fetchers.h>

struct Store;

struct AddFileToStoreResult;
struct CxxBuildResult;

extern "C" {
void set_fetcher_setting(nix_fetchers_settings *settings, const char *setting,
                         const char *value);
}

rust::String switch_profile(Store *store, rust::Str profile,
                            rust::Str store_path);

rust::String sign_closure(Store *store, rust::Str store_path,
                          rust::Str key_file);

struct CxxListGenerationsResult;
CxxListGenerationsResult list_generations(rust::Str profile_path);

AddFileToStoreResult add_file_to_store(Store *store, rust::Str name,
                                       rust::Str path);

CxxBuildResult build_drv_outputs(Store *store, rust::Str drv_path,
                                 rust::Str output_names_joined);

CxxBuildResult substitute_paths(Store *store, rust::Str paths_joined);

bool is_valid_path(Store *store, rust::Str path);
