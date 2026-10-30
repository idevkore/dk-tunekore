// tuneKore plugin shim.
//
// Rust cannot catch a C++ exception, and a plugin that throws one into Rust aborts the whole
// process (Analog Lab V does this from IComponent::initialize on a Mac where its licence check
// fails). So no Rust frame ever calls a plugin directly. Every call goes through one of the
// functions below, which call the plugin's function pointer from C++ inside try/catch and report
// "it threw" as a status code, with the exception's type and message kept for the caller.
//
// The calls are generic over the function's signature, not over the interface: Rust takes the
// function pointer out of the plugin object's vtable and passes it here, so the shim needs no
// VST3 (or CLAP) headers. The name of each entry says its signature:
//
//     tk_<return>_<arguments>      p pointer   i int32   u uint32   h int16   b uint8   d double
//
// Every plugin method takes the object as its first argument, so that is the first `p`. A
// function that returns bool uses `bool` as its return type (`tk_b_*`).
//
// On the normal path a call costs one indirect call and no allocation, lock or thread-local
// access: the try block is free until something throws. Only the catch path touches the
// thread-local message buffer.
//
// Not caught: a crash (SIGSEGV, abort in the plugin, std::terminate from a throwing destructor).
// The scanner runs plugins in a child process for those.

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <exception>
#include <stdexcept>
#include <typeinfo>

#if defined(__GNUC__) || defined(__clang__)
#include <cxxabi.h>
#define TK_HAVE_CXXABI 1
#endif

namespace {

enum { TK_OK = 0, TK_EXCEPTION = 1 };

constexpr size_t kMessageBytes = 512;
thread_local char g_message[kMessageBytes];

void set_message(const char* type, const char* what) {
    std::snprintf(g_message, kMessageBytes, "%s%s%s", type ? type : "unknown C++ exception",
                  (what && *what) ? ": " : "", what ? what : "");
}

// Called only from a catch block. Records what was thrown; allocation here is fine.
void record_current_exception() noexcept {
    try {
        throw;
    } catch (const std::exception& e) {
        const char* type = typeid(e).name();
#ifdef TK_HAVE_CXXABI
        int status = 0;
        char* demangled = abi::__cxa_demangle(type, nullptr, nullptr, &status);
        set_message(status == 0 && demangled ? demangled : type, e.what());
        std::free(demangled);
#else
        set_message(type, e.what());
#endif
    } catch (const char* s) {
        set_message("const char*", s);
    } catch (int v) {
        char buf[32];
        std::snprintf(buf, sizeof buf, "%d", v);
        set_message("int", buf);
    } catch (...) {
        const char* type = nullptr;
#ifdef TK_HAVE_CXXABI
        // Covers Objective-C exceptions too (NSException is thrown as a C++-compatible object
        // on 64-bit Apple platforms).
        if (const std::type_info* t = abi::__cxa_current_exception_type()) type = t->name();
#endif
        set_message(type, nullptr);
    }
}

template <typename R, typename... A>
inline int invoke(const void* fn, R* out, A... a) noexcept {
    try {
        using F = R (*)(A...);
        *out = reinterpret_cast<F>(const_cast<void*>(fn))(a...);
        return TK_OK;
    } catch (...) {
        record_current_exception();
        return TK_EXCEPTION;
    }
}

}  // namespace

#define TK0(name, R) \
    extern "C" int name(const void* f, R* out) { return invoke<R>(f, out); }
#define TK1(name, R, T1) \
    extern "C" int name(const void* f, R* out, T1 a1) { return invoke<R, T1>(f, out, a1); }
#define TK2(name, R, T1, T2) \
    extern "C" int name(const void* f, R* out, T1 a1, T2 a2) { return invoke<R, T1, T2>(f, out, a1, a2); }
#define TK3(name, R, T1, T2, T3) \
    extern "C" int name(const void* f, R* out, T1 a1, T2 a2, T3 a3) { return invoke<R, T1, T2, T3>(f, out, a1, a2, a3); }
#define TK4(name, R, T1, T2, T3, T4)                                                       \
    extern "C" int name(const void* f, R* out, T1 a1, T2 a2, T3 a3, T4 a4) {              \
        return invoke<R, T1, T2, T3, T4>(f, out, a1, a2, a3, a4);                          \
    }
#define TK5(name, R, T1, T2, T3, T4, T5)                                                   \
    extern "C" int name(const void* f, R* out, T1 a1, T2 a2, T3 a3, T4 a4, T5 a5) {       \
        return invoke<R, T1, T2, T3, T4, T5>(f, out, a1, a2, a3, a4, a5);                  \
    }

typedef void* P;
typedef int32_t I;
typedef uint32_t U;
typedef int16_t H;
typedef uint8_t B;
typedef double D;

// Plugin methods: the first P is the object.
TK1(tk_i_p, I, P)
TK1(tk_u_p, U, P)
TK2(tk_i_pp, I, P, P)
TK2(tk_i_pi, I, P, I)
TK2(tk_i_pb, I, P, B)
TK2(tk_d_pu, D, P, U)
TK2(tk_p_pp, P, P, P)
TK3(tk_i_ppp, I, P, P, P)
TK3(tk_i_pip, I, P, I, P)
TK3(tk_i_pii, I, P, I, I)
TK3(tk_i_pud, I, P, U, D)
TK3(tk_d_pud, D, P, U, D)
TK4(tk_i_pppp, I, P, P, P, P)
TK4(tk_i_piip, I, P, I, I, P)
TK4(tk_i_pudp, I, P, U, D, P)
TK4(tk_i_pupp, I, P, U, P, P)
TK5(tk_i_piiip, I, P, I, I, I, P)
TK5(tk_i_piiib, I, P, I, I, I, B)
TK5(tk_i_ppipi, I, P, P, I, P, I)
TK5(tk_i_pihhp, I, P, I, H, H, P)

// Module entry points, which have no object: bundleEntry(CFBundleRef), bundleExit(),
// GetPluginFactory().
TK1(tk_b_p, bool, P)
TK0(tk_b_, bool)
TK0(tk_p_, P)

// What the last call that returned TK_EXCEPTION on this thread threw, as "type: message".
// Valid until the next exception on the same thread; copy it straight away.
extern "C" const char* tk_last_exception(void) { return g_message; }

// Functions the unit tests hand to the shim in place of a plugin method.
extern "C" I tk_selftest_ok(P, P) { return 42; }
extern "C" I tk_selftest_throws_std(P, P) { throw std::runtime_error("boom from a plugin"); }
extern "C" I tk_selftest_throws_int(P, P) { throw 7; }
namespace {
struct Odd {
    int unused = 0;
};
}  // namespace
extern "C" I tk_selftest_throws_other(P, P) { throw Odd{}; }
