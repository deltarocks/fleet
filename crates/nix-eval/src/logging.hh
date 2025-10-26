#pragma once
#include "nix-eval/src/logging.rs"
#include "rust/cxx.h"
#include <nix_api_util.h>
#include <nix_api_util_internal.h>

struct ErrorInfoBuilder;

extern "C" {
void apply_tracing_logger();
rust::Box<ErrorInfoBuilder> extract_error_info(const nix_c_context *ctx);
}
