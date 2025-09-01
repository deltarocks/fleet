#include "nix-eval/src/logging.rs"
#include "logging.hh"
#include <nix/util/logging.hh>

using namespace nix;

struct TracingLogger : Logger {
  TracingLogger() {}

  bool isVerbose() override { return true; }
  // void addFields(nlohmann::json & json, const Fields & fields)
  //    {
  //        if (fields.empty())
  //            return;
  //        auto & arr = json["fields"] = nlohmann::json::array();
  //        for (auto & f : fields)
  //            if (f.type == Logger::Field::tInt)
  //                arr.push_back(f.i);
  //            else if (f.type == Logger::Field::tString)
  //                arr.push_back(f.s);
  //            else
  //                unreachable();
  //    }
  void log(Verbosity lvl, std::string_view s) override {
    rust::Str str(s.data(), s.size());
    emit_log(lvl, str);
  }
  void logEI(const ErrorInfo &ei) override { emit_log(ei.level, ei.msg.str()); }

  void startActivity(ActivityId act, Verbosity lvl, ActivityType type,
                     const std::string &s, const Fields &fields,
                     ActivityId parent) override {
    auto b = new_start_activity(act, lvl, type);
    for (auto &f : fields) {
      if (f.type == Logger::Field::tInt) {
        b->add_int_field(f.i);
      } else if (f.type == Logger::Field::tString) {
        b->add_string_field(f.s);
      } else {
        unreachable();
      }
    }
    b->emit(parent, s);
  };

  void stopActivity(ActivityId act) override { emit_stop(act); };

  void result(ActivityId act, ResultType type, const Fields &fields) override {
    auto b = new_start_activity(act, 0, type);
    for (auto &f : fields) {
      if (f.type == Logger::Field::tInt) {
        b->add_int_field(f.i);
      } else if (f.type == Logger::Field::tString) {
        b->add_string_field(f.s);
      } else {
        unreachable();
      }
    }
    b->emit_result(type);
  };

  void writeToStdout(std::string_view s) override {
    printf("writeToStdout() called\n");
  }
  void warn(const std::string &msg) override { emit_warn(msg); }

  virtual std::optional<char> ask(std::string_view s) {
    printf("ask() called\n");
    return {};
  }
};

extern "C" {
void apply_tracing_logger() { logger = std::make_unique<TracingLogger>(); }
}
