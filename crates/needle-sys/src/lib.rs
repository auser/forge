//! Raw, unsafe bindings to `libneedle` — the Cactus Needle 3 on-device
//! inference engine. The safe wrapper is
//! `forge_needle::ffi_backend::FfiBackend` (feature `ffi`).
//!
//! # Why these are hand-written
//!
//! The whole C API is six functions taking scalars and pointers, with no
//! structs, enums, unions, callbacks or typedefs — nothing that needs a code
//! generator to get right. Running `bindgen` here would make **libclang an
//! undocumented prerequisite of every default workspace build** (`needle-sys`
//! is a workspace member, so `cargo check --workspace` runs its build script,
//! and bindgen dynamically loads libclang and panics without it) and pull
//! `bindgen`/`clang-sys`/`prettyplease` into the default dependency graph for
//! six lines of output. So the declarations below are written by hand and
//! `build.rs` does nothing but resolve the library and emit link flags.
//!
//! `needle.h` is still committed next to this file, unmodified and
//! checksummed, as the **contract of record**: it is what these declarations
//! were transcribed from and the thing to `diff` when upgrading the engine.
//! [`tests::committed_header_still_matches_these_declarations`] fails if the
//! header ever stops agreeing with the code, which is the safety net a
//! generator would otherwise provide.
//!
//! # Contract (verified 2026-09-23 against `macos-arm64/libneedle.a`,
//! sha256 `60cc14f1a2eda8da72b75f8f228fb72cadc2850b38702370f43e9660b74e951a`)
//!
//! Everything here operates on **one process-global, non-thread-safe model**.
//! Callers must serialise every call — `forge-needle`'s engine does this by
//! owning the backend on a single dedicated thread — and only one live
//! binding may exist per process.
//!
//! Call order and return conventions, each confirmed by running the real
//! library (not inferred from the header):
//!
//! * [`needle_load`] takes the `.cact` archive bytes and returns `0` on
//!   success, `-1` on failure (e.g. garbage bytes → `needle_last_error()` =
//!   "failed to load .cact model"). Loading the same archive twice is fine.
//!   It **copies** the archive, so the caller's buffer can be freed
//!   immediately. The engine cannot *unload*: once weights are bound the
//!   process keeps them.
//! * [`needle_init`] installs the system-facts string and the tools JSON, and
//!   returns the tokenized static-prefix length (a *non-negative* count, e.g.
//!   63) or a negative error. Calling it again replaces the toolset, which is
//!   how one process serves several different tool surfaces.
//!   **It does not validate the tools JSON**: `"{not json"` and `"[]"` both
//!   return success, so callers must validate before calling.
//! * [`needle_complete`] returns the number of **tokens generated** (not
//!   bytes) or a negative error, and writes a NUL-terminated JSON envelope
//!   into `out`. It respects `out_capacity` (verified against a guard page)
//!   but **truncates silently** — a short buffer still returns success with
//!   invalid JSON in it, so callers must detect truncation themselves.
//! * [`needle_embed`] with a null `out` returns the embedding dimension
//!   (3072 for `needle3.cact`) without computing; with a real buffer it
//!   returns the number of floats written, which callers must check equals
//!   that dimension. A too-small capacity returns `-1` rather than
//!   overflowing. It needs [`needle_load`] but not [`needle_init`], and its
//!   result is independent of conversation state. Vectors come out
//!   L2-normalised.
//! * [`needle_reset`] clears the conversation, keeping the loaded weights and
//!   toolset.
//! * [`needle_last_error`] returns a runtime-owned C string that is only
//!   valid until the next API call — copy it out immediately. It points at an
//!   empty string (not null) when nothing has failed.

use std::os::raw::{c_char, c_int, c_uchar, c_ulonglong};

unsafe extern "C" {
    /// `int needle_init(const char*, const char*, const char*)` — installs the
    /// system-facts string and tools JSON. Returns the static-prefix token
    /// count, or negative on failure. `tool_index_path` may be null.
    pub fn needle_init(
        system_prompt: *const c_char,
        tools_json: *const c_char,
        tool_index_path: *const c_char,
    ) -> c_int;

    /// `const char* needle_last_error(void)` — runtime-owned, valid only until
    /// the next API call. Points at `""` rather than null when nothing failed.
    pub fn needle_last_error() -> *const c_char;

    /// `int needle_complete(const char*, int, char*, int)` — writes a
    /// NUL-terminated JSON envelope into `out`. Returns **tokens generated**
    /// (not bytes written), or negative on failure. Truncates silently.
    pub fn needle_complete(
        input: *const c_char,
        max_new_tokens: c_int,
        out: *mut c_char,
        out_capacity: c_int,
    ) -> c_int;

    /// `int needle_embed(const char*, float*, int)` — with `out` null and
    /// `out_capacity` 0, returns the embedding dimension without computing.
    /// Otherwise returns the number of floats written.
    pub fn needle_embed(input: *const c_char, out: *mut f32, out_capacity: c_int) -> c_int;

    /// `void needle_reset(void)` — clears the conversation, keeping the loaded
    /// weights and the installed toolset.
    pub fn needle_reset();

    /// `int needle_load(const unsigned char*, unsigned long long)` — loads a
    /// `.cact` archive. Returns 0 on success, negative on failure. Copies the
    /// bytes.
    pub fn needle_load(cact: *const c_uchar, n: c_ulonglong) -> c_int;
}

#[cfg(test)]
mod tests {
    /// The committed contract of record. Bundled into the test binary so this
    /// check needs no filesystem access.
    const HEADER: &str = include_str!("../needle.h");

    /// Collapse every whitespace run to a single space so the comparison is
    /// insensitive to the header's line breaking and indentation, but not to
    /// its types, order or spelling.
    fn normalized(source: &str) -> String {
        source.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Guards the hand-written `extern "C"` block above against the header it
    /// was transcribed from. If someone drops in a `needle.h` from a newer
    /// engine whose signatures changed, this fails loudly instead of leaving
    /// the declarations silently wrong — which, for an FFI boundary, would be
    /// undefined behaviour rather than a compile error.
    #[test]
    fn committed_header_still_matches_these_declarations() {
        let header = normalized(HEADER);
        for expected in [
            "int needle_init( const char* system_prompt, const char* tools_json, \
             const char* tool_index_path );",
            "const char* needle_last_error(void);",
            "int needle_complete( const char* input, int max_new_tokens, char* out, \
             int out_capacity );",
            "int needle_embed( const char* input, float* out, int out_capacity );",
            "void needle_reset(void);",
            "int needle_load( const unsigned char* cact, unsigned long long n );",
        ] {
            let expected = normalized(expected);
            assert!(
                header.contains(&expected),
                "crates/needle-sys/needle.h no longer declares `{expected}`.\n\
                 The hand-written bindings in src/lib.rs were transcribed from that header \
                 and must be updated to match before this crate can be trusted."
            );
        }
    }

    /// The API is exactly six functions. A seventh in a future header is not a
    /// problem in itself, but it must be a deliberate decision to ignore or
    /// bind it rather than something nobody noticed.
    #[test]
    fn header_declares_exactly_the_six_functions_we_bind() {
        // Declarations start a line with `NEEDLE_API`; the `#ifndef`/`#define
        // NEEDLE_API` preamble mentions the macro but does not start with it.
        let declarations = HEADER
            .lines()
            .filter(|line| line.starts_with("NEEDLE_API"))
            .count();
        assert_eq!(
            declarations, 6,
            "needle.h declares {declarations} NEEDLE_API functions, expected 6; \
             review src/lib.rs against the new header"
        );
    }
}
