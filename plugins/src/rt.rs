//! Pieces the VST3 and CLAP hosts share: the failure message the audio thread writes without
//! allocating, and the calling thread's OS id.

use crate::vst3::shim;
use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

/// Bytes kept of a failure message written on the audio thread.
const RT_MESSAGE: usize = 512;

/// A failure message written on the audio thread without allocating, read on the main thread.
pub(crate) struct RtMessage {
    /// 0 empty, 1 being written, 2 written, 3 taken.
    state: AtomicU8,
    len: AtomicUsize,
    buf: UnsafeCell<[u8; RT_MESSAGE]>,
}

// SAFETY: the buffer is written by one thread only (the compare-exchange in `write` elects it) and
// read only after the writer publishes state 2.
unsafe impl Sync for RtMessage {}

impl RtMessage {
    pub fn new() -> RtMessage {
        RtMessage { state: AtomicU8::new(0), len: AtomicUsize::new(0), buf: UnsafeCell::new([0; RT_MESSAGE]) }
    }

    /// Records `during` and this thread's last C++ exception, the first time only. No allocation.
    pub fn write(&self, during: &'static str) {
        self.write_with(during, shim::copy_last_exception);
    }

    /// Records `during` and `why`, the first time only. No allocation.
    pub fn write_str(&self, during: &'static str, why: &'static str) {
        self.write_with(during, |buf| {
            let n = why.len().min(buf.len());
            buf[..n].copy_from_slice(&why.as_bytes()[..n]);
            n
        });
    }

    fn write_with(&self, during: &'static str, tail: impl FnOnce(&mut [u8]) -> usize) {
        if self.state.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return;
        }
        // SAFETY: the compare-exchange above makes this the only writer, and readers wait for 2.
        let buf = unsafe { &mut *self.buf.get() };
        let mut n = 0;
        for part in [during.as_bytes(), b": "] {
            let k = part.len().min(RT_MESSAGE - n);
            buf[n..n + k].copy_from_slice(&part[..k]);
            n += k;
        }
        n += tail(&mut buf[n..]);
        self.len.store(n, Ordering::Relaxed);
        self.state.store(2, Ordering::Release);
    }

    /// The message, once. Allocates: main thread.
    pub fn take(&self) -> Option<String> {
        if self.state.compare_exchange(2, 3, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return None;
        }
        // SAFETY: state 2 means the writer is done; nobody writes again.
        let buf = unsafe { &*self.buf.get() };
        let raw = String::from_utf8_lossy(&buf[..self.len.load(Ordering::Relaxed)]).into_owned();
        Some(match raw.split_once(": ") {
            Some((during, rest)) => format!("{during}: {}", shim::readable(rest)),
            None => raw,
        })
    }
}

/// The OS id of the calling thread. Unlike `std::thread::current`, it never allocates (fit for
/// the audio thread) and works in exit handlers.
pub(crate) fn os_thread() -> usize {
    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn pthread_self() -> usize;
        }
        // SAFETY: `pthread_self` takes no arguments and cannot fail; `pthread_t` is an integer or
        // a pointer of pointer size on the Unix systems we build for.
        unsafe { pthread_self() }
    }
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentThreadId() -> u32;
        }
        // SAFETY: `GetCurrentThreadId` takes no arguments and cannot fail.
        unsafe { GetCurrentThreadId() as usize }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_message_from_the_audio_thread_is_kept_once() {
        let m = RtMessage::new();
        assert_eq!(m.take(), None);
        m.write("IAudioProcessor::process");
        m.write("ignored: only the first failure is kept");
        let s = m.take().unwrap();
        assert!(s.starts_with("IAudioProcessor::process: "), "{s}");
        assert_eq!(m.take(), None, "taken once");
        let m = RtMessage::new();
        m.write_str("clap_plugin.start_processing", "it refused");
        assert_eq!(m.take().as_deref(), Some("clap_plugin.start_processing: it refused"));
    }

    #[test]
    fn thread_ids_differ_between_threads_and_stay_put_within_one() {
        let here = os_thread();
        assert_eq!(here, os_thread());
        assert_ne!(std::thread::spawn(os_thread).join().unwrap(), here);
    }
}
