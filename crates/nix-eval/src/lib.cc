#include "nix-eval/src/lib.rs"
#include "lib.hh"
#include <nix/fetchers/fetch-settings.hh>
#include <nix/store/build-result.hh>
#include <nix/store/content-address.hh>
#include <nix/store/derived-path.hh>
#include <nix/store/local-fs-store.hh>
#include <nix/store/outputs-spec.hh>
#include <nix/store/path-info.hh>
#include <nix/store/profiles.hh>
#include <nix/store/realisation.hh>
#include <nix/store/store-api.hh>
#include <nix/util/file-system.hh>
#include <nix/util/hash.hh>
#include <nix/util/posix-source-accessor.hh>
#include <nix/util/ref.hh>
#include <nix/util/signature/local-keys.hh>
#include <nix/util/signature/signer.hh>
#include <nix/util/source-path.hh>
#include <nix_api_fetchers.h>
#include <nix_api_store_internal.h>
#include <sstream>

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

rust::String switch_profile(Store *store, rust::Str profile,
                            rust::Str store_path) {
  try {
    auto nixStore = store->ptr;
    auto *lfs = dynamic_cast<nix::LocalFSStore *>(&*nixStore);
    if (!lfs)
      return rust::String("destination is not a local-fs store");
    auto path = nixStore->parseStorePath(std::string(store_path));
    std::filesystem::path prof = std::string(profile);
    auto gen = nix::createGeneration(*lfs, prof, path);
    nix::switchLink(prof, gen);
    return rust::String();
  } catch (const std::exception &e) {
    return rust::String(e.what());
  }
}

rust::String sign_closure(Store *store, rust::Str store_path,
                          rust::Str key_file) {
  try {
    auto nixStore = store->ptr;
    nix::LocalSigner signer(
        nix::SecretKey::parse(nix::readFile(std::string(key_file))));
    auto root = nixStore->parseStorePath(std::string(store_path));
    nix::StorePathSet closure;
    nixStore->computeFSClosure(root, closure);
    for (auto &p : closure) {
      auto info = nixStore->queryPathInfo(p);
      nix::ValidPathInfo info2(*info);
      info2.sign(*nixStore, signer);
      nixStore->addSignatures(p, info2.sigs);
    }
    return rust::String();
  } catch (const std::exception &e) {
    return rust::String(e.what());
  }
}

CxxListGenerationsResult list_generations(rust::Str profile_path) {
  CxxListGenerationsResult out{rust::String(), {}};
  try {
    auto [gens, current] =
        nix::findGenerations(std::filesystem::path(std::string(profile_path)));
    for (auto &g : gens) {
      CxxProfileGeneration cg{};
      cg.id = g.number;
      cg.store_path = rust::String(g.path.string());
      cg.creation_time_unix = static_cast<int64_t>(g.creationTime);
      cg.current = current.has_value() && *current == g.number;
      out.generations.push_back(cg);
    }
  } catch (const std::exception &e) {
    out.error = rust::String(e.what());
  }
  return out;
}

static std::vector<std::string> split_lines(rust::Str joined) {
  std::vector<std::string> out;
  std::string buf;
  std::istringstream iss((std::string(joined)));
  while (std::getline(iss, buf)) {
    if (!buf.empty())
      out.push_back(buf);
  }
  return out;
}

CxxBuildResult build_drv_outputs(Store *store, rust::Str drv_path,
                                 // TODO: Vec<str>
                                 rust::Str output_names_joined) {
  CxxBuildResult res{rust::String(), {}};
  try {
    auto nixStore = store->ptr;
    auto sp = nixStore->parseStorePath(std::string(drv_path));

    nix::DerivedPath::Built built{
        .drvPath = nix::makeConstantStorePathRef(sp),
        .outputs = nix::OutputsSpec{nix::OutputsSpec::All{}},
    };
    auto names = split_lines(output_names_joined);
    if (!names.empty()) {
      std::set<nix::OutputName, std::less<>> nameSet;
      for (auto &n : names)
        nameSet.insert(n);
      built.outputs =
          nix::OutputsSpec{nix::OutputsSpec::Names{std::move(nameSet)}};
    }

    std::vector<nix::DerivedPath> reqs;
    reqs.push_back(built);

    auto results = nixStore->buildPathsWithResults(reqs);
    for (auto &r : results) {
      if (auto *failure = r.tryGetFailure()) {
        res.error = rust::String(failure->what());
        return res;
      }
    }
    for (auto &r : results) {
      if (auto *success = r.tryGetSuccess()) {
        for (auto &[name, real] : success->builtOutputs) {
          res.outputs.push_back(
              rust::String(nixStore->printStorePath(real.outPath)));
        }
      }
    }
  } catch (const std::exception &e) {
    res.error = rust::String(e.what());
  }
  return res;
}

CxxBuildResult substitute_paths(Store *store, rust::Str paths_joined) {
  CxxBuildResult res{rust::String(), {}};
  try {
    auto nixStore = store->ptr;
    auto paths = split_lines(paths_joined);

    std::vector<nix::DerivedPath> reqs;
    reqs.reserve(paths.size());
    for (auto &p : paths) {
      reqs.push_back(nix::DerivedPath::Opaque{nixStore->parseStorePath(p)});
    }

    // Substituter miss will cause scheduler to trigger build
    auto results = nixStore->buildPathsWithResults(reqs);
    for (auto &r : results) {
      if (r.tryGetSuccess() == nullptr)
        continue;
      if (auto *opaque = std::get_if<nix::DerivedPath::Opaque>(&r.path.raw())) {
        res.outputs.push_back(
            rust::String(nixStore->printStorePath(opaque->path)));
      }
    }
  } catch (const std::exception &e) {
    res.error = rust::String(e.what());
  }
  return res;
}

bool is_valid_path(Store *store, rust::Str path) {
  try {
    auto nixStore = store->ptr;
    auto sp = nixStore->parseStorePath(std::string(path));
    return nixStore->isValidPath(sp);
  } catch (const std::exception &) {
    return false;
  }
}

AddFileToStoreResult add_file_to_store(Store *store, rust::Str name,
                                       rust::Str path) {
  AddFileToStoreResult out{rust::String(), rust::String(), rust::String()};
  try {
    auto nixStore = store->ptr;
    auto src = nix::PosixSourceAccessor::createAtRoot(
        std::filesystem::path(std::string(path)));
    auto info = nixStore->addToStoreSlow(std::string(name), src,
                                         nix::ContentAddressMethod::Raw::Flat,
                                         nix::HashAlgorithm::SHA256);
    out.store_path = rust::String(nixStore->printStorePath(info.path));
    if (info.ca.has_value()) {
      out.hash =
          rust::String(info.ca->hash.to_string(nix::HashFormat::SRI, true));
    }
  } catch (const std::exception &e) {
    out.error = rust::String(e.what());
  }
  return out;
}
