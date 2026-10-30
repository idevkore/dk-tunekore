// A VST3 "plugin" that misbehaves, for the scanner and exception-shim tests. No SDK headers: the
// COM tables are written out by hand, just enough for the host to find one audio class.
//
// Built by tests/common/mod.rs with -DMODE=n:
//   0  initialize() throws a C++ exception (what Analog Lab V did in the hosting spike)
//   1  GetPluginFactory() aborts the process
//   2  GetPluginFactory() never returns
//   3  GetPluginFactory() throws a C++ exception
//   4  loads, activates and processes like an instrument, until process() throws

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

// ---- component (IComponent) and its audio processor (IAudioProcessor) ----
// One object with two vtables, as a C++ class with two bases would be laid out.
static const unsigned char kIComponent[16] = {0xE8, 0x31, 0xFF, 0x31, 0xF2, 0xD5, 0x43, 0x01, 0x92, 0x8E, 0xBB, 0xEE, 0x25, 0x69, 0x78, 0x02};
static const unsigned char kIAudioProcessor[16] = {0x42, 0x04, 0x3F, 0x99, 0xB7, 0xDA, 0x45, 0x3C, 0xA5, 0x69, 0xE7, 0x9D, 0x9A, 0xAE, 0xC3, 0x3D};
static const unsigned char kFUnknown[16] = {0, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46};
static const tresult kNotImplemented = 3;

struct Plugin { void** component; void** processor; };
static Plugin g_plugin;

static tresult query(const char* iid, void** obj) {
    if (std::memcmp(iid, kIComponent, 16) == 0 || std::memcmp(iid, kFUnknown, 16) == 0) { *obj = &g_plugin.component; return kOk; }
    if (std::memcmp(iid, kIAudioProcessor, 16) == 0) { *obj = &g_plugin.processor; return kOk; }
    *obj = nullptr;
    return kNoInterface;
}
static tresult c_query(void*, const char* iid, void** obj) { return query(iid, obj); }
static uint32_t c_ref(void*) { return 1; }
static tresult c_initialize(void*, void*) {
#if MODE == 0
    throw std::runtime_error("boom from initialize");
#endif
    return kOk;
}
static tresult c_ok(void*) { return kOk; }
static tresult c_not_implemented(void*) { return kNotImplemented; }
static tresult c_controller_class(void*, void*) { return kNotImplemented; }
static tresult c_io_mode(void*, int32_t) { return kOk; }
// One stereo audio output and one event input.
static int32_t c_bus_count(void*, int32_t media, int32_t dir) { return (media == 0 && dir == 1) || (media == 1 && dir == 0) ? 1 : 0; }
static tresult c_bus_info(void*, int32_t media, int32_t dir, int32_t index, void* info) {
    if (index != 0) return 2;
    char* p = static_cast<char*>(info);
    std::memset(p, 0, 4 * 3 + 256 + 4 * 2);
    int32_t channels = media == 0 ? 2 : 16, flags = 1;
    std::memcpy(p, &media, 4);
    std::memcpy(p + 4, &dir, 4);
    std::memcpy(p + 8, &channels, 4);
    std::memcpy(p + 12 + 256 + 4, &flags, 4);
    return kOk;
}
static tresult c_routing(void*, void*, void*) { return kNotImplemented; }
static tresult c_activate_bus(void*, int32_t, int32_t, int32_t, uint8_t) { return kOk; }
static tresult c_set_active(void*, uint8_t) { return kOk; }
static tresult c_state(void*, void*) { return kOk; }
static void* g_component_vtbl[] = {
    (void*)c_query, (void*)c_ref, (void*)c_ref,                                        // FUnknown
    (void*)c_initialize, (void*)c_ok,                                                  // IPluginBase
    (void*)c_controller_class, (void*)c_io_mode, (void*)c_bus_count, (void*)c_bus_info, // IComponent
    (void*)c_routing, (void*)c_activate_bus, (void*)c_set_active, (void*)c_state, (void*)c_state,
};

static tresult p_arrangements(void*, void*, int32_t, void*, int32_t) { return kOk; }
static tresult p_arrangement(void*, int32_t, int32_t, uint64_t* arr) { *arr = 3; return kOk; }  // stereo
static tresult p_sample_size(void*, int32_t size) { return size == 0 ? kOk : kNotImplemented; }
static uint32_t p_zero(void*) { return 0; }
static tresult p_setup(void*, void*) { return kOk; }
static tresult p_set_processing(void*, uint8_t) { return kOk; }
static tresult p_process(void*, void*) {
#if MODE == 4
    throw std::runtime_error("boom from process");
#endif
    return kOk;
}
static void* g_processor_vtbl[] = {
    (void*)c_query, (void*)c_ref, (void*)c_ref,                                          // FUnknown
    (void*)p_arrangements, (void*)p_arrangement, (void*)p_sample_size, (void*)p_zero,     // IAudioProcessor
    (void*)p_setup, (void*)p_set_processing, (void*)p_process, (void*)p_zero,
};

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
    (void)c_not_implemented;
    return kOk;
}
static tresult f_create(void*, const char*, const char* iid, void** obj) {
    g_plugin.component = g_component_vtbl;
    g_plugin.processor = g_processor_vtbl;
    return query(iid, obj);
}
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
