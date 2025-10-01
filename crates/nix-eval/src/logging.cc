#include "nix-eval/src/logging.rs"
#include "logging.hh"
#include <nix/util/logging.hh>

using namespace nix;

struct TracingLogger : Logger {
  TracingLogger() {}

  bool isVerbose() override { return true; }
  void log(Verbosity lvl, std::string_view s) override {
    rust::Slice<const unsigned char> str(
        reinterpret_cast<const unsigned char *>(s.data()), s.size());
    emit_log(lvl, str);
  }
  void logEI(const ErrorInfo &ei) override {
    auto s = ei.msg.str();
    rust::Slice<const unsigned char> str(
        reinterpret_cast<const unsigned char *>(s.data()), s.size());
    emit_log(ei.level, str);
  }

  void startActivity(ActivityId act, Verbosity lvl, ActivityType type,
                     const std::string &s, const Fields &fields,
                     ActivityId parent) override {
    auto b = new_start_activity(act, lvl, type);
    for (auto &f : fields) {
      if (f.type == Logger::Field::tInt) {
        b->add_int_field(f.i);
      } else if (f.type == Logger::Field::tString) {
        auto s = &f.s;
        rust::Slice<const unsigned char> str(
            reinterpret_cast<const unsigned char *>(s->data()), s->size());
        b->add_string_field(str);
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
        auto s = &f.s;
        rust::Slice<const unsigned char> str(
            reinterpret_cast<const unsigned char *>(s->data()), s->size());
        b->add_string_field(str);
      } else {
        unreachable();
      }
    }
    b->emit_result(type);
  };

  void writeToStdout(std::string_view s) override {
    emit_warn("writeToStdout() called, but unsupported");
  }
  void warn(const std::string &msg) override { emit_warn(msg); }

  virtual std::optional<char> ask(std::string_view s) {
    emit_warn("ask() called, but unsupported");
    return {};
  }
};

extern "C" {
void apply_tracing_logger() {
  logger = std::make_unique<TracingLogger>();
  // verbosity = lvlVomit;
}
}
