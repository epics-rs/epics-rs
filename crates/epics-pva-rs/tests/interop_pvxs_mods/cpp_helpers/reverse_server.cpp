// PVA-batch-1: pvxs server hosting the same matrix of complex
// shapes as the Rust forward-direction test. Used to verify the
// Rust *client* (decoder) accepts what pvxs encodes — symmetric
// to complex_types.rs which verifies pvxs accepts what Rust
// encodes.
//
// Args:
//   --port N        TCP port to bind (PVA only; we never advertise
//                   via UDP, the Rust client connects with
//                   `EPICS_PVA_NAME_SERVERS`).
//   --ready FILE    Optional. After the server has bound, touch
//                   the named file. Test uses this as a readiness
//                   gate (more deterministic than sleep).
//
// PVs hosted:
//   T:STR  NTScalar<string>  = "hello world"
//   T:INT  NTScalar<int32>   = -12345
//   T:LONG NTScalar<int64>   = 9_000_000_000
//   T:DBL  NTScalar<double>  = 123.456789
//   T:WF:DBL NTScalarArray<double> = [1.5, 2.5, 3.5]
//   T:WF:INT NTScalarArray<int32>  = [7, 8, 9, 10]
//   T:WF:STR NTScalarArray<string> = ["alpha", "beta", "gamma"]
//   T:ENUM NTEnum               index=2, choices=["OFF","ON","AUTO"]
//   T:TBL  NTTable               xs,ys (double), name (string), 3 rows
//
// (T:NEST is omitted — generic deeply-nested struct has no
//  canonical pvxs example. The encoder-side golden already
//  covers it; the symmetric reverse-direction check uses a
//  Rust↔Rust decoder unit test where the wire bytes come from
//  the captured forward fixture.)

#include <pvxs/server.h>
#include <pvxs/sharedpv.h>
#include <pvxs/nt.h>
#include <pvxs/data.h>

#include <atomic>
#include <chrono>
#include <csignal>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <string>
#include <thread>
#include <vector>

using namespace pvxs;

static std::atomic<bool> stop_flag{false};
static void on_sig(int) { stop_flag.store(true); }

static server::SharedPV make_string(const std::string& v) {
    auto pv = server::SharedPV::buildReadonly();
    auto value = nt::NTScalar{TypeCode::String}.create();
    value["value"] = v;
    pv.open(value);
    return pv;
}

static server::SharedPV make_int32(int32_t v) {
    auto pv = server::SharedPV::buildReadonly();
    auto value = nt::NTScalar{TypeCode::Int32}.create();
    value["value"] = v;
    pv.open(value);
    return pv;
}

static server::SharedPV make_int64(int64_t v) {
    auto pv = server::SharedPV::buildReadonly();
    auto value = nt::NTScalar{TypeCode::Int64}.create();
    value["value"] = v;
    pv.open(value);
    return pv;
}

static server::SharedPV make_double(double v) {
    auto pv = server::SharedPV::buildReadonly();
    auto value = nt::NTScalar{TypeCode::Float64}.create();
    value["value"] = v;
    pv.open(value);
    return pv;
}

template <class T, TypeCode::code_t Code>
static server::SharedPV make_array(const std::vector<T>& xs) {
    auto pv = server::SharedPV::buildReadonly();
    auto value = nt::NTScalar{TypeCode{Code}}.create();
    shared_array<T> arr(xs.begin(), xs.end());
    value["value"] = arr.freeze();
    pv.open(value);
    return pv;
}

static server::SharedPV make_string_array(const std::vector<std::string>& xs) {
    auto pv = server::SharedPV::buildReadonly();
    auto value = nt::NTScalar{TypeCode::StringA}.create();
    shared_array<std::string> arr(xs.begin(), xs.end());
    value["value"] = arr.freeze();
    pv.open(value);
    return pv;
}

static server::SharedPV make_enum(int32_t idx, const std::vector<std::string>& choices) {
    auto pv = server::SharedPV::buildReadonly();
    auto value = nt::NTEnum{}.create();
    value["value.index"] = idx;
    shared_array<std::string> arr(choices.begin(), choices.end());
    value["value.choices"] = arr.freeze();
    pv.open(value);
    return pv;
}

static server::SharedPV make_table() {
    auto pv = server::SharedPV::buildReadonly();
    auto def = nt::NTTable{}
                   .add_column(TypeCode::Float64, "xs", "X axis")
                   .add_column(TypeCode::Float64, "ys", "Y axis")
                   .add_column(TypeCode::String,  "name", "Name");
    auto value = def.create();
    shared_array<double> xs({1.0, 2.0, 3.0});
    shared_array<double> ys({10.0, 20.0, 30.0});
    shared_array<std::string> names({"a", "b", "c"});
    value["value.xs"]   = xs.freeze();
    value["value.ys"]   = ys.freeze();
    value["value.name"] = names.freeze();
    value["descriptor"] = "table";
    pv.open(value);
    return pv;
}

int main(int argc, char** argv) {
    std::signal(SIGINT, on_sig);
    std::signal(SIGTERM, on_sig);

    int port = 0;
    std::string ready_path;
    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        if (a == "--port" && i + 1 < argc) port = std::atoi(argv[++i]);
        else if (a == "--ready" && i + 1 < argc) ready_path = argv[++i];
        else { std::fprintf(stderr, "unknown arg: %s\n", a.c_str()); return 2; }
    }
    if (port <= 0) { std::fprintf(stderr, "usage: %s --port N [--ready FILE]\n", argv[0]); return 2; }

    server::Config cfg;
    cfg.tcp_port = static_cast<unsigned short>(port);
    cfg.interfaces = {"127.0.0.1"};
    cfg.beaconDestinations = {};
    cfg.auto_beacon = false;
    auto srv = cfg.build();

    srv.addPV("T:STR",    make_string("hello world"));
    srv.addPV("T:INT",    make_int32(-12345));
    srv.addPV("T:LONG",   make_int64(9'000'000'000LL));
    srv.addPV("T:DBL",    make_double(123.456789));
    srv.addPV("T:WF:DBL", make_array<double, TypeCode::Float64A>({1.5, 2.5, 3.5}));
    srv.addPV("T:WF:INT", make_array<int32_t, TypeCode::Int32A>({7, 8, 9, 10}));
    srv.addPV("T:WF:STR", make_string_array({"alpha", "beta", "gamma"}));
    srv.addPV("T:ENUM",   make_enum(2, {"OFF", "ON", "AUTO"}));
    srv.addPV("T:TBL",    make_table());

    srv.start();
    if (!ready_path.empty()) {
        std::ofstream(ready_path) << "ready\n";
    }
    while (!stop_flag.load()) {
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
    }
    srv.stop();
    return 0;
}
