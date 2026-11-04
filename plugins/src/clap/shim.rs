//! Calls into CLAP plugin code, through the same exception-catching C++ shim as VST3
//! (`shim/tk_shim.cpp`, `crate::vst3::shim`).
//!
//! CLAP is a C API, but most plugins are C++, and an exception that escapes one (it is undefined
//! behaviour in C++, and happens) would abort the process the moment it reaches a Rust frame. So
//! no Rust code calls a CLAP function pointer directly: each wrapper here takes the pointer with its
//! exact `clap-sys` type and hands it to the shim entry of the same shape. A missing (null)
//! function pointer is never called; the wrappers take the `Option` CLAP structs hold and answer
//! `None` for it.
//!
//! The `_rt` variants are for the audio thread: a throw comes back as [`Thrown`], with nothing
//! allocated.

use crate::vst3::shim::{finish, Guarded, RawPtr, Thrown};
use std::ffi::c_void;

unsafe extern "C" {
    fn tk_v_(f: *const c_void) -> i32;
    fn tk_v_p(f: *const c_void, a1: *mut c_void) -> i32;
    fn tk_v_pu(f: *const c_void, a1: *mut c_void, a2: u32) -> i32;
    fn tk_v_ppp(f: *const c_void, a1: *mut c_void, a2: *mut c_void, a3: *mut c_void) -> i32;
    fn tk_b_p(f: *const c_void, out: *mut bool, a1: *mut c_void) -> i32;
    fn tk_u_p(f: *const c_void, out: *mut u32, a1: *mut c_void) -> i32;
    fn tk_i_pp(f: *const c_void, out: *mut i32, a1: *mut c_void, a2: *mut c_void) -> i32;
    fn tk_p_p(f: *const c_void, out: *mut *mut c_void, a1: *mut c_void) -> i32;
    fn tk_p_pp(f: *const c_void, out: *mut *mut c_void, a1: *mut c_void, a2: *mut c_void) -> i32;
    fn tk_p_pu(f: *const c_void, out: *mut *mut c_void, a1: *mut c_void, a2: u32) -> i32;
    fn tk_p_ppp(f: *const c_void, out: *mut *mut c_void, a1: *mut c_void, a2: *mut c_void, a3: *mut c_void) -> i32;
    fn tk_b_pp(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: *mut c_void) -> i32;
    fn tk_b_pi(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: i32) -> i32;
    fn tk_u_pz(f: *const c_void, out: *mut u32, a1: *mut c_void, a2: bool) -> i32;
    fn tk_b_pup(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: u32, a3: *mut c_void) -> i32;
    fn tk_b_ppz(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: *mut c_void, a3: bool) -> i32;
    fn tk_b_ppp(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: *mut c_void, a3: *mut c_void) -> i32;
    fn tk_b_puu(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: u32, a3: u32) -> i32;
    fn tk_b_puzp(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: u32, a3: bool, a4: *mut c_void) -> i32;
    fn tk_b_pduu(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: f64, a3: u32, a4: u32) -> i32;
    fn tk_b_pudpu(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: u32, a3: f64, a4: *mut c_void, a5: u32) -> i32;
    fn tk_b_pzuzu(f: *const c_void, out: *mut bool, a1: *mut c_void, a2: bool, a3: u32, a4: bool, a5: u32) -> i32;

    #[cfg(test)]
    fn tk_selftest_void_throws(a: *mut c_void);
    #[cfg(test)]
    fn tk_selftest_count(a: *mut c_void, is_input: bool) -> u32;
}

/// A guarded call's result: `None` if the plugin has no such function, otherwise what it returned
/// or the exception it threw.
pub type Call<T> = Option<Guarded<T>>;

// Callers of every wrapper must pass an object the function pointer belongs to and arguments the
// plugin function accepts, as for any CLAP call, and call it on the thread CLAP says.

/// `fn()`: `clap_plugin_entry.deinit`.
#[inline]
pub unsafe fn v_(f: Option<unsafe extern "C" fn()>) -> Call<()> {
    let f = f?;
    Some(finish(unsafe { tk_v_(f as *const c_void) }, ()))
}
/// `fn(object)`: `destroy`, `deactivate`, `stop_processing`, `on_main_thread`, `gui.destroy`.
#[inline]
pub unsafe fn v_p<T>(f: Option<unsafe extern "C" fn(*const T)>, this: *const T) -> Call<()> {
    let f = f?;
    Some(finish(unsafe { tk_v_p(f as *const c_void, this.raw()) }, ()))
}
/// `timer_support.on_timer`.
#[inline]
pub unsafe fn v_pu<T>(f: Option<unsafe extern "C" fn(*const T, u32)>, this: *const T, a: u32) -> Call<()> {
    let f = f?;
    Some(finish(unsafe { tk_v_pu(f as *const c_void, this.raw(), a) }, ()))
}
/// `params.flush(plugin, in, out)`.
#[inline]
pub unsafe fn v_ppp<T, A: RawPtr, B: RawPtr>(f: Option<unsafe extern "C" fn(*const T, A, B)>, this: *const T, a: A, b: B) -> Call<()> {
    let f = f?;
    Some(finish(unsafe { tk_v_ppp(f as *const c_void, this.raw(), a.raw(), b.raw()) }, ()))
}
/// [`v_ppp`] for the audio thread.
#[inline]
pub unsafe fn v_ppp_rt<T, A: RawPtr, B: RawPtr>(f: Option<unsafe extern "C" fn(*const T, A, B)>, this: *const T, a: A, b: B) -> Option<Result<(), Thrown>> {
    let f = f?;
    Some(if unsafe { tk_v_ppp(f as *const c_void, this.raw(), a.raw(), b.raw()) } == 0 { Ok(()) } else { Err(Thrown) })
}
/// `fn(ptr) -> bool`: `clap_entry.init(path)`, `plugin.init`, `can_resize`, `show`, `hide`.
#[inline]
pub unsafe fn b_p<A: RawPtr>(f: Option<unsafe extern "C" fn(A) -> bool>, a: A) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_p(f as *const c_void, &mut out, a.raw()) }, out))
}
/// [`b_p`] for the audio thread (`start_processing`).
#[inline]
pub unsafe fn b_p_rt<A: RawPtr>(f: Option<unsafe extern "C" fn(A) -> bool>, a: A) -> Option<Result<bool, Thrown>> {
    let f = f?;
    let mut out = false;
    Some(if unsafe { tk_b_p(f as *const c_void, &mut out, a.raw()) } == 0 { Ok(out) } else { Err(Thrown) })
}
/// `fn(object) -> u32`: `get_plugin_count`, `params.count`, `latency.get`, `tail.get`.
#[inline]
pub unsafe fn u_p<T>(f: Option<unsafe extern "C" fn(*const T) -> u32>, this: *const T) -> Call<u32> {
    let f = f?;
    let mut out = 0;
    Some(finish(unsafe { tk_u_p(f as *const c_void, &mut out, this.raw()) }, out))
}
/// `plugin.process`, on the audio thread.
#[inline]
pub unsafe fn i_pp_rt<T, A: RawPtr>(f: Option<unsafe extern "C" fn(*const T, A) -> i32>, this: *const T, a: A) -> Option<Result<i32, Thrown>> {
    let f = f?;
    let mut out = 0;
    Some(if unsafe { tk_i_pp(f as *const c_void, &mut out, this.raw(), a.raw()) } == 0 { Ok(out) } else { Err(Thrown) })
}
/// `clap_entry.get_factory(id)`.
#[inline]
pub unsafe fn p_p<R, A: RawPtr>(f: Option<unsafe extern "C" fn(A) -> *const R>, a: A) -> Call<*const R> {
    let f = f?;
    let mut out: *mut c_void = std::ptr::null_mut();
    Some(finish(unsafe { tk_p_p(f as *const c_void, &mut out, a.raw()) }, out.cast_const().cast()))
}
/// `plugin.get_extension(plugin, id)`.
#[inline]
pub unsafe fn p_pp<T, R, A: RawPtr>(f: Option<unsafe extern "C" fn(*const T, A) -> *const R>, this: *const T, a: A) -> Call<*const R> {
    let f = f?;
    let mut out: *mut c_void = std::ptr::null_mut();
    Some(finish(unsafe { tk_p_pp(f as *const c_void, &mut out, this.raw(), a.raw()) }, out.cast_const().cast()))
}
/// `factory.get_plugin_descriptor(factory, index)`.
#[inline]
pub unsafe fn p_pu<T, R>(f: Option<unsafe extern "C" fn(*const T, u32) -> *const R>, this: *const T, a: u32) -> Call<*const R> {
    let f = f?;
    let mut out: *mut c_void = std::ptr::null_mut();
    Some(finish(unsafe { tk_p_pu(f as *const c_void, &mut out, this.raw(), a) }, out.cast_const().cast()))
}
/// `factory.create_plugin(factory, host, id)`.
#[inline]
pub unsafe fn p_ppp<T, R, A: RawPtr, B: RawPtr>(f: Option<unsafe extern "C" fn(*const T, A, B) -> *const R>, this: *const T, a: A, b: B) -> Call<*const R> {
    let f = f?;
    let mut out: *mut c_void = std::ptr::null_mut();
    Some(finish(unsafe { tk_p_ppp(f as *const c_void, &mut out, this.raw(), a.raw(), b.raw()) }, out.cast_const().cast()))
}
/// `fn(object, ptr) -> bool`: `state.save`, `state.load`, `gui.set_parent`.
#[inline]
pub unsafe fn b_pp<T, A: RawPtr>(f: Option<unsafe extern "C" fn(*const T, A) -> bool>, this: *const T, a: A) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_pp(f as *const c_void, &mut out, this.raw(), a.raw()) }, out))
}
/// `render.set(plugin, mode)`.
#[inline]
pub unsafe fn b_pi<T>(f: Option<unsafe extern "C" fn(*const T, i32) -> bool>, this: *const T, a: i32) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_pi(f as *const c_void, &mut out, this.raw(), a) }, out))
}
/// `audio_ports.count(plugin, is_input)`, `note_ports.count`.
#[inline]
pub unsafe fn u_pz<T>(f: Option<unsafe extern "C" fn(*const T, bool) -> u32>, this: *const T, a: bool) -> Call<u32> {
    let f = f?;
    let mut out = 0;
    Some(finish(unsafe { tk_u_pz(f as *const c_void, &mut out, this.raw(), a) }, out))
}
/// `params.get_info(plugin, index, info)`, `params.get_value(plugin, id, value)`.
#[inline]
pub unsafe fn b_pup<T, A: RawPtr>(f: Option<unsafe extern "C" fn(*const T, u32, A) -> bool>, this: *const T, a: u32, b: A) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_pup(f as *const c_void, &mut out, this.raw(), a, b.raw()) }, out))
}
/// `gui.is_api_supported(plugin, api, floating)`, `gui.create`.
#[inline]
pub unsafe fn b_ppz<T, A: RawPtr>(f: Option<unsafe extern "C" fn(*const T, A, bool) -> bool>, this: *const T, a: A, b: bool) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_ppz(f as *const c_void, &mut out, this.raw(), a.raw(), b) }, out))
}
/// `gui.get_size(plugin, &w, &h)`, `gui.adjust_size`.
#[inline]
pub unsafe fn b_ppp<T, A: RawPtr, B: RawPtr>(f: Option<unsafe extern "C" fn(*const T, A, B) -> bool>, this: *const T, a: A, b: B) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_ppp(f as *const c_void, &mut out, this.raw(), a.raw(), b.raw()) }, out))
}
/// `gui.set_size(plugin, w, h)`.
#[inline]
pub unsafe fn b_puu<T>(f: Option<unsafe extern "C" fn(*const T, u32, u32) -> bool>, this: *const T, a: u32, b: u32) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_puu(f as *const c_void, &mut out, this.raw(), a, b) }, out))
}
/// `audio_ports.get(plugin, index, is_input, info)`, `note_ports.get`.
#[inline]
pub unsafe fn b_puzp<T, A: RawPtr>(f: Option<unsafe extern "C" fn(*const T, u32, bool, A) -> bool>, this: *const T, a: u32, b: bool, c: A) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_puzp(f as *const c_void, &mut out, this.raw(), a, b, c.raw()) }, out))
}
/// `plugin.activate(plugin, sample_rate, min_frames, max_frames)`.
#[inline]
pub unsafe fn b_pduu<T>(f: Option<unsafe extern "C" fn(*const T, f64, u32, u32) -> bool>, this: *const T, a: f64, b: u32, c: u32) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_pduu(f as *const c_void, &mut out, this.raw(), a, b, c) }, out))
}
/// `params.value_to_text(plugin, id, value, buffer, capacity)`.
#[inline]
pub unsafe fn b_pudpu<T, A: RawPtr>(f: Option<unsafe extern "C" fn(*const T, u32, f64, A, u32) -> bool>, this: *const T, a: u32, b: f64, c: A, d: u32) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_pudpu(f as *const c_void, &mut out, this.raw(), a, b, c.raw(), d) }, out))
}
/// `audio_ports_activation.set_active(plugin, is_input, index, active, sample_size)`.
#[inline]
pub unsafe fn b_pzuzu<T>(f: Option<unsafe extern "C" fn(*const T, bool, u32, bool, u32) -> bool>, this: *const T, a: bool, b: u32, c: bool, d: u32) -> Call<bool> {
    let f = f?;
    let mut out = false;
    Some(finish(unsafe { tk_b_pzuzu(f as *const c_void, &mut out, this.raw(), a, b, c, d) }, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_function_is_not_called() {
        // SAFETY: nothing is called.
        assert!(unsafe { v_p::<c_void>(None, std::ptr::null()) }.is_none());
        assert!(unsafe { u_pz::<c_void>(None, std::ptr::null(), true) }.is_none());
    }

    #[test]
    fn a_void_function_that_throws_comes_back_as_an_error() {
        // SAFETY: the test double ignores its argument.
        let f: unsafe extern "C" fn(*const c_void) = unsafe { std::mem::transmute(tk_selftest_void_throws as unsafe extern "C" fn(*mut c_void)) };
        let e = unsafe { v_p(Some(f), std::ptr::null()) }.unwrap().unwrap_err();
        assert!(e.0.contains("runtime_error") && e.0.contains("boom from a void call"), "{e}");
    }

    #[test]
    fn a_bool_argument_reaches_the_plugin_as_a_c_bool() {
        // SAFETY: the test double ignores its object.
        let f: unsafe extern "C" fn(*const c_void, bool) -> u32 = unsafe { std::mem::transmute(tk_selftest_count as unsafe extern "C" fn(*mut c_void, bool) -> u32) };
        assert_eq!(unsafe { u_pz(Some(f), std::ptr::null(), true) }, Some(Ok(2)));
        assert_eq!(unsafe { u_pz(Some(f), std::ptr::null(), false) }, Some(Ok(1)));
    }
}
