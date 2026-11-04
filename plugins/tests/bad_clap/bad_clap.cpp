// A CLAP plugin that misbehaves, for the plugin host's tests. Built by the tests with the system
// C++ compiler, one binary per mode (-DMODE=N):
//
//   0  clap_plugin.init throws a C++ exception
//   1  clap_entry.init aborts the process
//   2  clap_entry.init never returns
//   3  clap_entry.init throws a C++ exception
//   4  an instrument with a stereo output whose process throws
//
// CLAP is a C ABI; the few structs it needs are declared here, as CLAP's headers lay them out
// (CLAP 1.2), so the test needs no headers. Never shipped.

#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <stdexcept>
#include <unistd.h>

#ifndef MODE
#define MODE 0
#endif

extern "C" {

struct clap_version {
    uint32_t major, minor, revision;
};

struct clap_plugin_descriptor {
    clap_version version;
    const char *id, *name, *vendor, *url, *manual_url, *support_url, *version_text, *description;
    const char* const* features;
};

struct clap_host;
struct clap_process;

struct clap_plugin {
    const clap_plugin_descriptor* desc;
    void* plugin_data;
    bool (*init)(const clap_plugin*);
    void (*destroy)(const clap_plugin*);
    bool (*activate)(const clap_plugin*, double, uint32_t, uint32_t);
    void (*deactivate)(const clap_plugin*);
    bool (*start_processing)(const clap_plugin*);
    void (*stop_processing)(const clap_plugin*);
    void (*reset)(const clap_plugin*);
    int32_t (*process)(const clap_plugin*, const clap_process*);
    const void* (*get_extension)(const clap_plugin*, const char*);
    void (*on_main_thread)(const clap_plugin*);
};

struct clap_plugin_factory {
    uint32_t (*get_plugin_count)(const clap_plugin_factory*);
    const clap_plugin_descriptor* (*get_plugin_descriptor)(const clap_plugin_factory*, uint32_t);
    const clap_plugin* (*create_plugin)(const clap_plugin_factory*, const clap_host*, const char*);
};

struct clap_plugin_entry {
    clap_version version;
    bool (*init)(const char*);
    void (*deinit)();
    const void* (*get_factory)(const char*);
};

struct clap_audio_port_info {
    uint32_t id;
    char name[256];
    uint32_t flags;
    uint32_t channel_count;
    const char* port_type;
    uint32_t in_place_pair;
};

struct clap_plugin_audio_ports {
    uint32_t (*count)(const clap_plugin*, bool);
    bool (*get)(const clap_plugin*, uint32_t, bool, clap_audio_port_info*);
};

struct clap_note_port_info {
    uint32_t id;
    uint32_t supported_dialects;
    uint32_t preferred_dialect;
    char name[256];
};

struct clap_plugin_note_ports {
    uint32_t (*count)(const clap_plugin*, bool);
    bool (*get)(const clap_plugin*, uint32_t, bool, clap_note_port_info*);
};

}  // extern "C"

namespace {

const char* const kFeatures[] = {"instrument", nullptr};
const clap_plugin_descriptor kDesc = {{1, 2, 2},       "dev.tunekore.bad", "Bad CLAP Plugin", "tuneKore tests", "", "", "",
                                      "0.0.1",         "misbehaves",       kFeatures};

bool plugin_init(const clap_plugin*) {
#if MODE == 0
    throw std::runtime_error("boom from init");
#endif
    return true;
}
void plugin_destroy(const clap_plugin*) {}
bool plugin_activate(const clap_plugin*, double, uint32_t, uint32_t) { return true; }
void plugin_nothing(const clap_plugin*) {}
bool plugin_start(const clap_plugin*) { return true; }
int32_t plugin_process(const clap_plugin*, const clap_process*) {
#if MODE == 4
    throw std::runtime_error("boom from process");
#endif
    return 1;
}

uint32_t ports_count(const clap_plugin*, bool is_input) { return is_input ? 0 : 1; }
bool ports_get(const clap_plugin*, uint32_t index, bool is_input, clap_audio_port_info* info) {
    if (is_input || index != 0) return false;
    std::memset(info, 0, sizeof *info);
    std::strcpy(info->name, "Out");
    info->flags = 1;  // main
    info->channel_count = 2;
    info->port_type = "stereo";
    info->in_place_pair = UINT32_MAX;
    return true;
}
const clap_plugin_audio_ports kPorts = {ports_count, ports_get};

uint32_t notes_count(const clap_plugin*, bool is_input) { return is_input ? 1 : 0; }
bool notes_get(const clap_plugin*, uint32_t index, bool is_input, clap_note_port_info* info) {
    if (!is_input || index != 0) return false;
    std::memset(info, 0, sizeof *info);
    info->supported_dialects = 1;  // CLAP
    info->preferred_dialect = 1;
    std::strcpy(info->name, "Notes");
    return true;
}
const clap_plugin_note_ports kNotes = {notes_count, notes_get};

const void* plugin_extension(const clap_plugin*, const char* id) {
    if (std::strcmp(id, "clap.audio-ports") == 0) return &kPorts;
    if (std::strcmp(id, "clap.note-ports") == 0) return &kNotes;
    return nullptr;
}

clap_plugin g_plugin = {&kDesc,          nullptr,      plugin_init,    plugin_destroy, plugin_activate,  plugin_nothing,
                        plugin_start,    plugin_nothing, plugin_nothing, plugin_process, plugin_extension, plugin_nothing};

uint32_t factory_count(const clap_plugin_factory*) { return 1; }
const clap_plugin_descriptor* factory_desc(const clap_plugin_factory*, uint32_t i) { return i == 0 ? &kDesc : nullptr; }
const clap_plugin* factory_create(const clap_plugin_factory*, const clap_host*, const char* id) {
    return std::strcmp(id, kDesc.id) == 0 ? &g_plugin : nullptr;
}
const clap_plugin_factory kFactory = {factory_count, factory_desc, factory_create};

bool entry_init(const char*) {
#if MODE == 1
    std::abort();
#elif MODE == 2
    for (;;) sleep(1);
#elif MODE == 3
    throw std::runtime_error("boom from clap_entry.init");
#endif
    return true;
}
void entry_deinit() {}
const void* entry_factory(const char* id) { return std::strcmp(id, "clap.plugin-factory") == 0 ? &kFactory : nullptr; }

}  // namespace

extern "C" __attribute__((visibility("default"))) const clap_plugin_entry clap_entry = {{1, 2, 2}, entry_init, entry_deinit, entry_factory};
