// Offline test of the tone generator: no audio device needed.
#include <cmath>
#include <cstdio>
#include <vector>

#include "tone.h"

static int failures = 0;
#define CHECK(cond)                                                  \
  do {                                                               \
    if (!(cond)) {                                                   \
      std::printf("FAIL %s:%d  %s\n", __FILE__, __LINE__, #cond);    \
      ++failures;                                                    \
    }                                                                \
  } while (0)

int main() {
  const double rate = 48000.0;
  musikore::Tone tone;
  tone.prepare(rate);
  std::vector<float> buf(4800);

  // Off: silence.
  CHECK(tone.render(buf.data(), buf.size()) == 0.0f);

  // On: ramps up over ~10 ms with no jump, then reaches the target gain.
  tone.set(true, 440.0f, 0.5f);
  float peak = tone.render(buf.data(), buf.size());
  CHECK(std::fabs(buf[0]) < 0.01f);  // starts near zero, no click
  float max_step = 0.0f;
  for (std::size_t i = 1; i < buf.size(); ++i)
    max_step = std::fmax(max_step, std::fabs(buf[i] - buf[i - 1]));
  CHECK(max_step < 0.1f);  // 440 Hz at gain 0.5 moves at most ~0.03 per sample
  CHECK(peak > 0.49f && peak <= 0.5f);

  // Off again: ramps to silence, then exactly zero.
  tone.set(false, 440.0f, 0.5f);
  tone.render(buf.data(), buf.size());
  CHECK(tone.render(buf.data(), buf.size()) == 0.0f);

  // Frequency: count zero crossings of 1 second of 1 kHz.
  tone.set(true, 1000.0f, 0.5f);
  std::vector<float> sec(48000);
  tone.render(sec.data(), sec.size());
  int crossings = 0;
  for (std::size_t i = 1; i < sec.size(); ++i)
    if ((sec[i - 1] < 0.0f) != (sec[i] < 0.0f)) ++crossings;
  CHECK(crossings >= 1995 && crossings <= 2005);

  if (failures == 0) std::printf("tone_test: ok\n");
  return failures ? 1 : 0;
}
