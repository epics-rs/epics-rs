// PVA-batch-4: pvxs server hosting a subset of complex_pv_matrix
// with `Config::overrideSendBE(true)` so all outbound frames are
// big-endian on the wire. Used by `be_byte_order.rs::interop_be_b`
// to prove the Rust client wire decoder accepts BE.
//
// Requires `-DPVXS_ENABLE_EXPERT_API` at compile time (the
// override is gated behind that macro in pvxs's public headers).

#define PVXS_ENABLE_EXPERT_API 1

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

using namespace pvxs;

static std::atomic<bool> stop_flag{false};
static void on_sig(int) { stop_flag.store(true); }

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
    if (port <= 0) {
        std::fprintf(stderr, "usage: %s --port N [--ready FILE]\n", argv[0]);
        return 2;
    }

    server::Config cfg;
    cfg.tcp_port = static_cast<unsigned short>(port);
    cfg.interfaces = {"127.0.0.1"};
    cfg.beaconDestinations = {};
    cfg.auto_beacon = false;
    cfg.overrideSendBE(true);  // <-- the test's whole point
    auto srv = cfg.build();

    auto p_str = server::SharedPV::buildReadonly();
    p_str.open(nt::NTScalar{TypeCode::String}.create()
        .update("value", std::string{"hello world"}));
    auto p_long = server::SharedPV::buildReadonly();
    p_long.open(nt::NTScalar{TypeCode::Int64}.create()
        .update("value", int64_t{9'000'000'000LL}));
    auto p_dbl = server::SharedPV::buildReadonly();
    p_dbl.open(nt::NTScalar{TypeCode::Float64}.create()
        .update("value", double{123.456789}));

    srv.addPV("T:STR", p_str);
    srv.addPV("T:LONG", p_long);
    srv.addPV("T:DBL", p_dbl);

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
