/* musiKore engine: the C interface the app shell calls.
 *
 * Control calls come from one thread at a time (the caller serializes them).
 * mk_engine_take_peak and mk_engine_set_tone only touch atomics and are safe
 * from any thread. The audio callback runs on the device's own thread and
 * only reads atomics set by these calls. */
#ifndef MUSIKORE_ENGINE_H
#define MUSIKORE_ENGINE_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct mk_engine mk_engine;

/* Creates the engine. Returns NULL only if memory runs out. */
mk_engine *mk_engine_create(void);
void mk_engine_destroy(mk_engine *engine);

/* Engine version, e.g. "0.1.0". */
const char *mk_engine_version(void);

/* Writes the output devices as a JSON array into buf (NUL-terminated) and
 * returns the full length the JSON needs, not counting the NUL. If the return
 * value is >= cap, call again with a bigger buffer. Each entry is
 * {"id":N,"name":"...","channels":N,"sampleRates":[...],"preferredRate":N,"isDefault":bool}. */
size_t mk_engine_list_outputs_json(mk_engine *engine, char *buf, size_t cap);

/* Opens an output device and starts the stream. device_id 0 means the
 * system default. Returns 0 on success, -1 on failure (see last_error). */
int mk_engine_open_output(mk_engine *engine, unsigned device_id,
                          unsigned sample_rate, unsigned buffer_frames);
void mk_engine_close_output(mk_engine *engine);

/* Turns the test tone on or off. Safe to call while the stream runs. */
void mk_engine_set_tone(mk_engine *engine, int on, float freq_hz, float gain);

/* Peak output level since the last call, 0..1. */
float mk_engine_take_peak(mk_engine *engine);

/* Last error message, or "" if none. Valid until the next call. */
const char *mk_engine_last_error(mk_engine *engine);

#ifdef __cplusplus
}
#endif

#endif
