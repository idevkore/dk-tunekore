// A VST3 "plugin" that misbehaves, for the scanner and exception-shim tests. No SDK headers: the
// COM tables are written out by hand, just enough for the host to find one audio class.
//
// Built by tests/common/mod.rs with -DMODE=n:
//   0  initialize() throws a C++ exception (what Analog Lab V did in the hosting spike)
//   1  GetPluginFactory() aborts the process
//   2  GetPluginFactory() never returns
//   3  GetPluginFactory() throws a C++ exception

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <stdexcept>
#include <unistd.h>

#ifndef MODE
#define MODE 0
#endif

typedef int32_t tresult;
static const tresult kOk = 0, kNoInterface = -1;

struct Com { void** vtbl; };

// ---- component ----
static tresult c_query(void* self, const char*, void** obj) { *obj = self; return kOk; }
static uint32_t c_ref(void*) { return 1; }
static tresult c_initialize(void*, void*) { throw std::runtime_error("boom from initialize"); }
static tresult c_terminate(void*) { return kOk; }
static tresult c_unused(void*) { return kNoInterface; }
static void* g_component_vtbl[] = {
    (void*)c_query, (void*)c_ref, (void*)c_ref,           // FUnknown
    (void*)c_initialize, (void*)c_terminate,              // IPluginBase
    (void*)c_unused, (void*)c_unused, (void*)c_unused, (void*)c_unused, (void*)c_unused,
    (void*)c_unused, (void*)c_unused, (void*)c_unused, (void*)c_unused,  // IComponent (never reached)
};
static Com g_component = { g_component_vtbl };

// ---- factory ----
static tresult f_query(void*, const char*, void** obj) { *obj = nullptr; return kNoInterface; }
static uint32_t f_ref(void*) { return 1; }
static tresult f_info(void*, void* info) {
    char* p = static_cast<char*>(info);
    std::memset(p, 0, 64 + 256 + 128 + 4);
    std::strcpy(p, "tuneKore tests");
    return kOk;
}
static int32_t f_count(void*) { return 1; }
static tresult f_class(void*, int32_t i, void* info) {
    if (i != 0) return 2;
    char* p = static_cast<char*>(info);
    std::memset(p, 0, 16 + 4 + 32 + 64);
    for (int k = 0; k < 16; ++k) p[k] = static_cast<char>(0x40 + k);
    int32_t cardinality = 0x7FFFFFFF;
    std::memcpy(p + 16, &cardinality, 4);
    std::strcpy(p + 20, "Audio Module Class");
    std::strcpy(p + 52, "Bad Plugin");
    return kOk;
}
static tresult f_create(void*, const char*, const char*, void** obj) { *obj = &g_component; return kOk; }
static void* g_factory_vtbl[] = { (void*)f_query, (void*)f_ref, (void*)f_ref, (void*)f_info, (void*)f_count, (void*)f_class, (void*)f_create };
static Com g_factory = { g_factory_vtbl };

extern "C" {
__attribute__((visibility("default"))) bool bundleEntry(void*) { return true; }
__attribute__((visibility("default"))) bool bundleExit() { return true; }
__attribute__((visibility("default"))) void* GetPluginFactory() {
#if MODE == 1
    std::abort();
#elif MODE == 2
    for (;;) sleep(1000);
#elif MODE == 3
    throw std::runtime_error("boom from GetPluginFactory");
#endif
    return &g_factory;
}
}
