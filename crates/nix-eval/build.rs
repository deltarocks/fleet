use bindgen::{
	RustEdition,
	callbacks::{ItemInfo, ParseCallbacks},
};
use std::path::PathBuf;

#[derive(Debug)]
struct StripPrefix;
impl ParseCallbacks for StripPrefix {
	fn item_name(&self, name: ItemInfo<'_>) -> Option<String> {
		name.name.strip_prefix("nix_").map(ToOwned::to_owned)
	}
}

fn main() {
	// Link nix C++ libraries for cxx
	for lib in &[
		"nix-util",
		"nix-store",
		"nix-expr",
		"nix-flake",
		"nix-fetchers",
		"bdw-gc",
	] {
		if let Ok(library) = pkg_config::probe_library(lib) {
			for lib_path in library.libs {
				println!("cargo:rustc-link-lib={lib_path}");
			}
			for search_path in library.link_paths {
				println!("cargo:rustc-link-search=native={}", search_path.display());
			}
		}
	}

	cxx_build::bridge("src/logging.rs")
		.file("src/logging.cc")
		.std("c++20")
		.shared_flag(true)
		.compile("nix-eval-logging");
	cxx_build::bridge("src/lib.rs")
		.file("src/lib.cc")
		.std("c++20")
		.shared_flag(true)
		.compile("nix-eval");

	println!("cargo:rerun-if-changed=src/lib.cc");
	println!("cargo:rerun-if-changed=src/lib.hh");
	println!("cargo:rerun-if-changed=src/logging.cc");
	println!("cargo:rerun-if-changed=src/logging.hh");

	//
	let mut libnix = bindgen::builder()
		.rust_edition(RustEdition::Edition2024)
		.header_contents(
			"nix.h",
			"
				#define GC_THREADS
				#include <gc/gc.h>
				#include <nix_api_expr.h>
				#include <nix_api_store.h>
				#include <nix_api_flake.h>
				#include <nix_api_fetchers.h>
				#include <nix_api_util.h>
				#include <nix_api_value.h>
			",
		)
		.parse_callbacks(Box::new(StripPrefix));

	for header in pkg_config::probe_library("nix-expr-c")
		.expect("nix-expr-c")
		.include_paths
		.into_iter()
		.chain(
			pkg_config::probe_library("nix-flake-c")
				.expect("nix-flake-c")
				.include_paths
				.into_iter(),
		)
		.chain(
			pkg_config::probe_library("nix-fetchers-c")
				.expect("nix-fetchers-c")
				.include_paths
				.into_iter(),
		)
		.chain(
			pkg_config::probe_library("bdw-gc")
				.expect("bdw-gc")
				.include_paths
				.into_iter(),
		) {
		libnix = libnix.clang_arg(format!("-I{}", header.to_str().expect("path is utf-8")));
	}

	let mut out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
	out.push("bindings.rs");
	libnix
		.generate()
		.expect("generate bindings")
		.write_to_file(out)
		.expect("write bindings");
}
