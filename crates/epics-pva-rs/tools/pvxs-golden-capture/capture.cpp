// Captures pvxs's actual `to_wire` byte output for every fixture
// asserted by the Rust wire-golden suite under
// `crates/epics-pva-rs/tests/pva_cases/`.
//
// The point: a "golden" derived by reading pvxs source can share
// blind spots with the encoder under test. Bytes that came out of
// pvxs's own encoder at run time can't share that mistake — they
// either match the Rust output (encoder agrees with pvxs) or they
// don't (real divergence to fix).
//
// Build: `./build.sh` (writes `./capture`).
// Run:   `./capture` — emits one `key=hex` line per fixture on stdout.
//
// Pinned to pvxs `f8d6192` (the commit the golden suite tracks).
// When pvxs ships a new wire shape, rebuild and diff the output.

#include <cstdio>
#include <cstdint>
#include <vector>
#include <string>
#include <utility>

#include <pvxs/sharedArray.h>
#include <pvxs/data.h>
#include <pvxs/nt.h>

#include "dataimpl.h"
#include "pvaproto.h"

using namespace pvxs;
using namespace pvxs::impl;

// ── helpers ───────────────────────────────────────────────────────

template <typename Fn>
static std::vector<uint8_t> capture(bool be, Fn&& fn) {
    std::vector<uint8_t> buf;
    VectorOutBuf S(be, buf);
    fn(S);
    buf.resize(buf.size() - S.size());
    return buf;
}

static void emit(const char* key, const std::vector<uint8_t>& bytes) {
    std::printf("%s=", key);
    for (auto b : bytes) std::printf("%02x", b);
    std::printf("\n");
}

// to_wire_valid emits: Size(bitset_bytes) + bitset_bytes + marked_values.
// The Rust `encode_pv_field` family produces only the trailing
// "marked_values" portion (no Size, no bitset). Skip those leading
// bytes here so the captured hex compares apples-to-apples.
static std::vector<uint8_t> strip_bitset(const std::vector<uint8_t>& in) {
    if (in.empty()) return in;
    size_t off = 0;
    uint8_t lead = in[off++];
    size_t blen;
    if (lead == 254) {
        // 5-byte extended Size form (BE-decoded here; we only ever
        // capture with be=true for whole-Value fixtures).
        blen = (uint32_t(in[off]) << 24)
             | (uint32_t(in[off+1]) << 16)
             | (uint32_t(in[off+2]) << 8)
             | uint32_t(in[off+3]);
        off += 4;
    } else if (lead == 255) {
        // Null sentinel: should not occur for `to_wire_valid`.
        std::fprintf(stderr, "warn: unexpected 0xFF size lead\n");
        return std::vector<uint8_t>(in.begin() + off, in.end());
    } else {
        blen = lead;
    }
    off += blen;
    return std::vector<uint8_t>(in.begin() + off, in.end());
}

// ── fixtures ──────────────────────────────────────────────────────

int main() {
    // ---- scalars (encode_pv_field on a `Scalar` → raw value bytes) ----
    emit("scalar_int_be", capture(true, [](Buffer& b){
        int32_t v = 0x01234567; to_wire(b, v);
    }));
    emit("scalar_int_le", capture(false, [](Buffer& b){
        int32_t v = 0x01234567; to_wire(b, v);
    }));
    emit("scalar_double_be", capture(true, [](Buffer& b){
        double v = 1.0; to_wire(b, v);
    }));
    emit("scalar_double_le", capture(false, [](Buffer& b){
        double v = 1.0; to_wire(b, v);
    }));
    emit("scalar_string_hi_be", capture(true, [](Buffer& b){
        std::string s = "hi"; to_wire(b, s);
    }));
    emit("scalar_string_empty", capture(true, [](Buffer& b){
        std::string s; to_wire(b, s);
    }));
    emit("scalar_string_253", capture(true, [](Buffer& b){
        std::string s(253, 'x'); to_wire(b, s);
    }));
    emit("scalar_string_254_be", capture(true, [](Buffer& b){
        std::string s(254, 'x'); to_wire(b, s);
    }));
    emit("scalar_bool_true", capture(true, [](Buffer& b){
        uint8_t v = 1; to_wire(b, v);
    }));
    emit("scalar_bool_false", capture(true, [](Buffer& b){
        uint8_t v = 0; to_wire(b, v);
    }));
    emit("scalar_byte_neg1", capture(true, [](Buffer& b){
        int8_t v = -1; to_wire(b, v);
    }));
    emit("scalar_ubyte_0xab", capture(true, [](Buffer& b){
        uint8_t v = 0xAB; to_wire(b, v);
    }));
    emit("scalar_short_be", capture(true, [](Buffer& b){
        int16_t v = 0x1234; to_wire(b, v);
    }));
    emit("scalar_short_le", capture(false, [](Buffer& b){
        int16_t v = 0x1234; to_wire(b, v);
    }));
    emit("scalar_ushort_be", capture(true, [](Buffer& b){
        uint16_t v = 0xABCD; to_wire(b, v);
    }));
    emit("scalar_uint_be", capture(true, [](Buffer& b){
        uint32_t v = 0xDEADBEEF; to_wire(b, v);
    }));
    emit("scalar_uint_le", capture(false, [](Buffer& b){
        uint32_t v = 0xDEADBEEF; to_wire(b, v);
    }));
    emit("scalar_long_be", capture(true, [](Buffer& b){
        int64_t v = 0x0102030405060708LL; to_wire(b, v);
    }));
    emit("scalar_long_le", capture(false, [](Buffer& b){
        int64_t v = 0x0102030405060708LL; to_wire(b, v);
    }));
    emit("scalar_ulong_be", capture(true, [](Buffer& b){
        uint64_t v = 0xFFEEDDCCBBAA9988ULL; to_wire(b, v);
    }));
    emit("scalar_float_be", capture(true, [](Buffer& b){
        float v = 1.0f; to_wire(b, v);
    }));
    emit("scalar_float_le", capture(false, [](Buffer& b){
        float v = 1.0f; to_wire(b, v);
    }));

    // ---- Size encoding boundaries (pvaproto.h to_wire(Size)) ----
    emit("size_0_be",        capture(true,  [](Buffer& b){ to_wire(b, Size{0}); }));
    emit("size_0_le",        capture(false, [](Buffer& b){ to_wire(b, Size{0}); }));
    emit("size_253_be",      capture(true,  [](Buffer& b){ to_wire(b, Size{253}); }));
    emit("size_253_le",      capture(false, [](Buffer& b){ to_wire(b, Size{253}); }));
    emit("size_254_be",      capture(true,  [](Buffer& b){ to_wire(b, Size{254}); }));
    emit("size_254_le",      capture(false, [](Buffer& b){ to_wire(b, Size{254}); }));
    emit("size_65535_be",    capture(true,  [](Buffer& b){ to_wire(b, Size{65535}); }));
    emit("size_65536_be",    capture(true,  [](Buffer& b){ to_wire(b, Size{65536}); }));
    emit("size_max_u31_be",  capture(true,  [](Buffer& b){ to_wire(b, Size{0x7FFFFFFF}); }));

    // ---- scalar arrays — pvaproto.h:477 to_wire(shared_array) ----
    // Layout: Size(N) + N elements in negotiated byte order.
    emit("scalar_array_empty_int", capture(true, [](Buffer& b){
        shared_array<const int32_t> arr;
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_bool", capture(true, [](Buffer& b){
        // Boolean array: each element is a single byte (0|1).
        shared_array<const uint8_t> arr({1u, 0u});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_byte", capture(true, [](Buffer& b){
        shared_array<const int8_t> arr({-1, 0, 1});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_ubyte", capture(true, [](Buffer& b){
        shared_array<const uint8_t> arr({0xAAu, 0xBBu});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_short_be", capture(true, [](Buffer& b){
        shared_array<const int16_t> arr({int16_t(0x1234), int16_t(0x5678)});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_short_le", capture(false, [](Buffer& b){
        shared_array<const int16_t> arr({int16_t(0x1234), int16_t(0x5678)});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_ushort_be", capture(true, [](Buffer& b){
        shared_array<const uint16_t> arr({uint16_t(0xABCD)});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_int_be", capture(true, [](Buffer& b){
        shared_array<const int32_t> arr({1, 2});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_int_le", capture(false, [](Buffer& b){
        shared_array<const int32_t> arr({1, 2});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_uint_be", capture(true, [](Buffer& b){
        shared_array<const uint32_t> arr({0xDEADBEEFu});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_long_be", capture(true, [](Buffer& b){
        shared_array<const int64_t> arr({0x0102030405060708LL});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_ulong_be", capture(true, [](Buffer& b){
        shared_array<const uint64_t> arr({0xFFEEDDCCBBAA9988ULL});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_float_be", capture(true, [](Buffer& b){
        shared_array<const float> arr({1.0f});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_double_be", capture(true, [](Buffer& b){
        shared_array<const double> arr({1.0});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));
    emit("scalar_array_string", capture(true, [](Buffer& b){
        shared_array<const std::string> arr({"hi", "world"});
        to_wire<typename decltype(arr)::value_type>(b, arr.castTo<const void>());
    }));

    // ---- compound: sub-Structure / Union / Variant (extracted from
    //      a marked-everything Value to skip the leading bitset) ----
    {
        // structure { int32 a; float64 b } with a=0x01020304, b=1.0
        TypeDef td(TypeCode::Struct, {
            Member(TypeCode::Int32, "a"),
            Member(TypeCode::Float64, "b"),
        });
        auto val = td.create();
        val["a"] = int32_t(0x01020304);
        val["b"] = double(1.0);
        auto raw = capture(true, [&val](Buffer& b){ to_wire_valid(b, val); });
        emit("sub_structure_two_fields_be", strip_bitset(raw));
    }
    {
        // union { int i; float64 f }, selector=0 (i=7)
        TypeDef td(TypeCode::Union, {
            Member(TypeCode::Int32, "i"),
            Member(TypeCode::Float64, "f"),
        });
        auto holder = TypeDef(TypeCode::Struct, {
            Member(TypeCode::Union, "u", "", {
                Member(TypeCode::Int32, "i"),
                Member(TypeCode::Float64, "f"),
            }),
        }).create();
        holder["u->i"] = int32_t(7);
        auto raw = capture(true, [&holder](Buffer& b){ to_wire_valid(b, holder); });
        emit("union_present_int_selector_be", strip_bitset(raw));
    }
    {
        // Same Union shape, selector = null (no variant chosen).
        auto holder = TypeDef(TypeCode::Struct, {
            Member(TypeCode::Union, "u", "", {
                Member(TypeCode::Int32, "i"),
            }),
        }).create();
        holder["u"].mark(); // mark without selecting → null selector
        auto raw = capture(true, [&holder](Buffer& b){ to_wire_valid(b, holder); });
        emit("union_null_selector", strip_bitset(raw));
    }
    {
        // Variant carrying int=9
        auto holder = TypeDef(TypeCode::Struct, {
            Member(TypeCode::Any, "v"),
        }).create();
        auto inner = TypeDef(TypeCode::Int32).create();
        inner = int32_t(9);
        holder["v"].from(inner);
        auto raw = capture(true, [&holder](Buffer& b){ to_wire_valid(b, holder); });
        emit("variant_present_int_be", strip_bitset(raw));
    }
    {
        // Variant with no descriptor (marked but empty)
        auto holder = TypeDef(TypeCode::Struct, {
            Member(TypeCode::Any, "v"),
        }).create();
        holder["v"].mark();
        auto raw = capture(true, [&holder](Buffer& b){ to_wire_valid(b, holder); });
        emit("variant_null_descriptor", strip_bitset(raw));
    }

    // ---- compound arrays (struct[] / union[] / any[]) ----
    {
        // 2-element struct[] of `structure { int32 v }`.
        auto holder = TypeDef(TypeCode::Struct, {
            Member(TypeCode::StructA, "s", "", {
                Member(TypeCode::Int32, "v"),
            }),
        }).create();
        auto fld = holder["s"];
        shared_array<Value> arr(2);
        arr[0] = fld.allocMember();
        arr[1] = fld.allocMember();
        arr[0]["v"] = int32_t(0x01020304);
        arr[1]["v"] = int32_t(0x05060708);
        fld = arr.freeze().castTo<const void>();
        auto raw = capture(true, [&holder](Buffer& b){ to_wire_valid(b, holder); });
        emit("struct_array_two_present", strip_bitset(raw));
    }
    {
        // 1-element union[] [int, float64], selector=0, int=7.
        auto holder = TypeDef(TypeCode::Struct, {
            Member(TypeCode::UnionA, "u", "", {
                Member(TypeCode::Int32, "i"),
                Member(TypeCode::Float64, "f"),
            }),
        }).create();
        auto fld = holder["u"];
        shared_array<Value> arr(1);
        arr[0] = fld.allocMember();
        arr[0]["->i"] = int32_t(7);
        fld = arr.freeze().castTo<const void>();
        auto raw = capture(true, [&holder](Buffer& b){ to_wire_valid(b, holder); });
        emit("union_array_present_int_selector", strip_bitset(raw));
    }
    {
        // 1-element any[] carrying int=9.
        auto holder = TypeDef(TypeCode::Struct, {
            Member(TypeCode::AnyA, "a"),
        }).create();
        auto fld = holder["a"];
        shared_array<Value> arr(1);
        arr[0] = TypeDef(TypeCode::Int32).create();
        arr[0] = int32_t(9);
        fld = arr.freeze().castTo<const void>();
        auto raw = capture(true, [&holder](Buffer& b){ to_wire_valid(b, holder); });
        emit("variant_array_present_int", strip_bitset(raw));
    }

    return 0;
}
