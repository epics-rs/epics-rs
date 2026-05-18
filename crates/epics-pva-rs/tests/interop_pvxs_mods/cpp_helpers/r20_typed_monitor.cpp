// PVA-R20 interop harness: subscribe with the *typed-builder*
// pipeline option (Context::request().record("pipeline", true))
// against a target server, count events, exit 0 on success.
//
// This shape originates only from C++ code using the
// `RequestBuilder::record<bool>` template — pvxs's CLI parser
// stores `record[pipeline=true]` as a string. Pre-R20 the Rust
// server's `monitor_pipeline_options` only matched the string
// form and silently disabled flow control when this harness
// connected.
//
// Build with the bundled pvxs headers + libpvxs:
//
//   c++ -std=c++17 \
//       -I~/codes/pvxs/include -I~/epics/epics-base/include \
//       r20_typed_monitor.cpp \
//       -L~/codes/pvxs/lib/darwin-aarch64 -lpvxs \
//       -L~/epics/epics-base/lib/darwin-aarch64 -lCom \
//       -o r20_typed_monitor

#include <pvxs/client.h>
#include <pvxs/data.h>

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <iostream>
#include <string>
#include <thread>

using namespace pvxs;

static void usage(const char* argv0) {
    std::fprintf(stderr,
        "usage: %s --server host:port --pv NAME [--events N] [--timeout SEC]\n",
        argv0);
}

int main(int argc, char** argv) {
    std::string server;
    std::string pv;
    int expected_events = 2;
    double timeout_s = 5.0;

    for (int i = 1; i < argc; ++i) {
        std::string a = argv[i];
        if ((a == "--server") && (i + 1 < argc)) server = argv[++i];
        else if ((a == "--pv") && (i + 1 < argc)) pv = argv[++i];
        else if ((a == "--events") && (i + 1 < argc)) expected_events = std::atoi(argv[++i]);
        else if ((a == "--timeout") && (i + 1 < argc)) timeout_s = std::atof(argv[++i]);
        else if (a == "-h" || a == "--help") { usage(argv[0]); return 0; }
        else { usage(argv[0]); return 2; }
    }
    if (server.empty() || pv.empty()) { usage(argv[0]); return 2; }

    // Use TCP name-server discovery (deterministic — bypasses UDP
    // broadcast port conflicts on a shared host).
    client::Config cfg;
    cfg.nameServers = { server };
    cfg.addressList = {};
    cfg.autoAddrList = false;
    auto ctxt = cfg.build();

    std::atomic<int> got{0};

    auto sub = ctxt.monitor(pv)
        .record("pipeline", true)
        .record("queueSize", 4)
        .maskConnected(true)
        .maskDisconnected(true)
        .event([&](client::Subscription& s) {
            try {
                while (auto v = s.pop()) {
                    got.fetch_add(1, std::memory_order_relaxed);
                }
            } catch (std::exception& e) {
                std::fprintf(stderr, "pop exception: %s\n", e.what());
            }
        })
        .exec();

    auto deadline = std::chrono::steady_clock::now() +
        std::chrono::milliseconds(static_cast<int>(timeout_s * 1000));
    while (std::chrono::steady_clock::now() < deadline) {
        if (got.load(std::memory_order_relaxed) >= expected_events) {
            std::printf("OK got=%d expected=%d\n", got.load(), expected_events);
            return 0;
        }
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
    }
    std::fprintf(stderr, "FAIL: got=%d expected=%d (timeout %.1fs)\n",
                 got.load(), expected_events, timeout_s);
    return 1;
}
