// Sine test tone with a gain ramp. Header-only so tests can render it offline.
//
// Real-time safe: no allocation, locks or I/O. The control side writes the
// atomics; the audio callback reads them once per block.
#ifndef MUSIKORE_TONE_H
#define MUSIKORE_TONE_H

#include <atomic>
#include <cmath>
#include <cstddef>

namespace musikore {

class Tone {
 public:
  // Control side (any thread).
  void set(bool on, float freq_hz, float gain) noexcept {
    freq_.store(freq_hz, std::memory_order_relaxed);
    gain_.store(gain, std::memory_order_relaxed);
    on_.store(on, std::memory_order_release);
  }

  // Audio side (one thread). Call before the stream starts.
  void prepare(double sample_rate) noexcept {
    rate_ = sample_rate;
    // Full-scale ramp takes ~10 ms: long enough to avoid a click.
    step_ = static_cast<float>(1.0 / (sample_rate * kRampSeconds));
    phase_ = 0.0;
    level_ = 0.0f;
  }

  // Renders `frames` mono samples into `out`. Returns the block's peak.
  float render(float *out, std::size_t frames) noexcept {
    const bool on = on_.load(std::memory_order_acquire);
    const float target = on ? gain_.load(std::memory_order_relaxed) : 0.0f;
    const double inc = static_cast<double>(freq_.load(std::memory_order_relaxed)) /
                       rate_ * kTwoPi;
    float peak = 0.0f;
    for (std::size_t i = 0; i < frames; ++i) {
      if (level_ < target) {
        level_ = std::fmin(level_ + step_, target);
      } else if (level_ > target) {
        level_ = std::fmax(level_ - step_, target);
      }
      float s = 0.0f;
      if (level_ > 0.0f || on) {
        s = level_ * static_cast<float>(std::sin(phase_));
        phase_ += inc;
        if (phase_ >= kTwoPi) phase_ -= kTwoPi;
      }
      out[i] = s;
      peak = std::fmax(peak, std::fabs(s));
    }
    return peak;
  }

 private:
  static constexpr double kTwoPi = 6.283185307179586476925286766559;
  static constexpr double kRampSeconds = 0.010;

  std::atomic<bool> on_{false};
  std::atomic<float> freq_{440.0f};
  std::atomic<float> gain_{0.2f};
  double rate_ = 48000.0;
  double phase_ = 0.0;
  float level_ = 0.0f;
  float step_ = 0.0f;
};

}  // namespace musikore

#endif
