#include "musikore/engine.h"

#include <RtAudio.h>

#include <algorithm>
#include <atomic>
#include <cstdio>
#include <cstring>
#include <exception>
#include <mutex>
#include <string>
#include <vector>

#include "tone.h"

#ifndef MUSIKORE_ENGINE_VERSION
#define MUSIKORE_ENGINE_VERSION "0.0.0"
#endif

struct mk_engine {
  mk_engine()
      : rt(RtAudio::UNSPECIFIED, [this](RtAudioErrorType type, const std::string &text) {
          if (type != RTAUDIO_NO_ERROR) set_error(text);
        }) {}

  void set_error(const std::string &text) {
    std::lock_guard<std::mutex> lock(error_mutex);
    error = text;
  }

  RtAudio rt;
  musikore::Tone tone;
  std::atomic<float> peak{0.0f};
  unsigned channels = 0;

  // Audio-thread scratch, sized in open_output before the stream starts.
  std::vector<float> mono;

  std::mutex error_mutex;  // main-thread error text only; never taken by the audio callback
  std::string error;
  std::string last_error_copy;  // what last_error() hands out
  std::string json;
};

namespace {

// RtAudio callback. Runs on the device thread: no allocation, locks or I/O.
int audio_callback(void *output, void * /*input*/, unsigned frames, double /*time*/,
                   RtAudioStreamStatus /*status*/, void *user) {
  auto *e = static_cast<mk_engine *>(user);
  auto *out = static_cast<float *>(output);
  float *mono = e->mono.data();
  const unsigned n = std::min<unsigned>(frames, static_cast<unsigned>(e->mono.size()));

  const float block_peak = e->tone.render(mono, n);
  const unsigned ch = e->channels;
  for (unsigned i = 0; i < n; ++i) {
    for (unsigned c = 0; c < ch; ++c) out[i * ch + c] = mono[i];
  }
  for (unsigned i = n * ch; i < frames * ch; ++i) out[i] = 0.0f;

  // Raise the shared peak if this block is louder.
  float seen = e->peak.load(std::memory_order_relaxed);
  while (block_peak > seen &&
         !e->peak.compare_exchange_weak(seen, block_peak, std::memory_order_relaxed)) {
  }
  return 0;
}

void append_escaped(std::string &out, const std::string &s) {
  out += '"';
  for (unsigned char c : s) {
    switch (c) {
      case '"': out += "\\\""; break;
      case '\\': out += "\\\\"; break;
      case '\n': out += "\\n"; break;
      case '\r': out += "\\r"; break;
      case '\t': out += "\\t"; break;
      default:
        if (c < 0x20) {
          char buf[8];
          std::snprintf(buf, sizeof buf, "\\u%04x", c);
          out += buf;
        } else {
          out += static_cast<char>(c);
        }
    }
  }
  out += '"';
}

}  // namespace

extern "C" {

mk_engine *mk_engine_create(void) {
  try {
    return new mk_engine();
  } catch (...) {
    return nullptr;
  }
}

void mk_engine_destroy(mk_engine *engine) {
  if (!engine) return;
  mk_engine_close_output(engine);
  delete engine;
}

const char *mk_engine_version(void) { return MUSIKORE_ENGINE_VERSION; }

size_t mk_engine_list_outputs_json(mk_engine *engine, char *buf, size_t cap) {
  std::string &json = engine->json;
  json = "[";
  try {
    const unsigned default_id = engine->rt.getDefaultOutputDevice();
    bool first = true;
    for (unsigned id : engine->rt.getDeviceIds()) {
      const RtAudio::DeviceInfo info = engine->rt.getDeviceInfo(id);
      if (info.outputChannels == 0) continue;
      if (!first) json += ',';
      first = false;
      json += "{\"id\":" + std::to_string(id) + ",\"name\":";
      append_escaped(json, info.name);
      json += ",\"channels\":" + std::to_string(info.outputChannels) + ",\"sampleRates\":[";
      for (std::size_t i = 0; i < info.sampleRates.size(); ++i) {
        if (i) json += ',';
        json += std::to_string(info.sampleRates[i]);
      }
      json += "],\"preferredRate\":" + std::to_string(info.preferredSampleRate);
      json += ",\"isDefault\":";
      json += (info.isDefaultOutput || id == default_id) ? "true" : "false";
      json += '}';
    }
  } catch (const std::exception &ex) {
    engine->set_error(ex.what());
  }
  json += ']';

  if (buf && cap > 0) {
    const std::size_t n = std::min(json.size(), cap - 1);
    std::memcpy(buf, json.data(), n);
    buf[n] = '\0';
  }
  return json.size();
}

int mk_engine_open_output(mk_engine *engine, unsigned device_id, unsigned sample_rate,
                          unsigned buffer_frames) {
  mk_engine_close_output(engine);
  try {
    const unsigned id = device_id ? device_id : engine->rt.getDefaultOutputDevice();
    if (id == 0) {
      engine->set_error("No audio output device found");
      return -1;
    }
    const RtAudio::DeviceInfo info = engine->rt.getDeviceInfo(id);
    if (info.outputChannels == 0) {
      engine->set_error("That device has no outputs");
      return -1;
    }
    const unsigned rate =
        sample_rate ? sample_rate
                    : (info.preferredSampleRate ? info.preferredSampleRate : 48000u);
    unsigned frames = buffer_frames ? buffer_frames : 512u;

    RtAudio::StreamParameters params;
    params.deviceId = id;
    params.nChannels = std::min(info.outputChannels, 2u);
    params.firstChannel = 0;

    engine->channels = params.nChannels;
    engine->mono.assign(4096, 0.0f);  // allocated here, never on the audio thread
    engine->tone.prepare(rate);
    engine->peak.store(0.0f);

    if (engine->rt.openStream(&params, nullptr, RTAUDIO_FLOAT32, rate, &frames,
                              &audio_callback, engine) != RTAUDIO_NO_ERROR) {
      return -1;
    }
    if (frames > engine->mono.size()) {
      // The device chose a bigger block than the scratch holds.
      engine->rt.closeStream();
      engine->set_error("The device's buffer is larger than the engine supports");
      return -1;
    }
    if (engine->rt.startStream() != RTAUDIO_NO_ERROR) {
      engine->rt.closeStream();
      return -1;
    }
    return 0;
  } catch (const std::exception &ex) {
    engine->set_error(ex.what());
    return -1;
  }
}

void mk_engine_close_output(mk_engine *engine) {
  try {
    if (engine->rt.isStreamOpen()) {
      if (engine->rt.isStreamRunning()) engine->rt.stopStream();
      engine->rt.closeStream();
    }
  } catch (...) {
  }
}

void mk_engine_set_tone(mk_engine *engine, int on, float freq_hz, float gain) {
  freq_hz = std::clamp(freq_hz, 20.0f, 20000.0f);
  gain = std::clamp(gain, 0.0f, 1.0f);
  engine->tone.set(on != 0, freq_hz, gain);
}

float mk_engine_take_peak(mk_engine *engine) {
  return engine->peak.exchange(0.0f, std::memory_order_relaxed);
}

const char *mk_engine_last_error(mk_engine *engine) {
  std::lock_guard<std::mutex> lock(engine->error_mutex);
  engine->last_error_copy = engine->error;
  return engine->last_error_copy.c_str();
}

}  // extern "C"
