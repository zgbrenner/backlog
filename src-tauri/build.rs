fn main() {
    // `cargo test` harness binaries link tao/wry the same way the app does, so
    // they inherit its static imports of `TaskDialogIndirect`,
    // `RemoveWindowSubclass` and `DefSubclassProc`. Those three are ComCtl32
    // **v6** exports: `C:\Windows\System32\comctl32.dll` is v5.82 and exports
    // only `SetWindowSubclass`, so v6 is reachable only through the
    // side-by-side `Microsoft.Windows.Common-Controls` assembly, which a
    // binary opts into with an application manifest.
    //
    // `tauri_build::build()` embeds that manifest via `cargo:rustc-link-arg-bins`
    // — bins only. A test harness gets no `.rsrc` section at all, binds to
    // v5.82, and dies with STATUS_ENTRYPOINT_NOT_FOUND (0xC0000139) before it
    // reaches `main`, which cargo reports as "test exited abnormally" with no
    // test ever having run. That is why `cargo test --workspace` could not
    // execute on Windows while `cargo test -p backlog-core` — which links none
    // of this — passed fine, and why `scripts/ci-local.sh` (Linux) never saw it.
    //
    // The scoped `rustc-link-arg-tests` covers only integration tests under
    // `tests/`, which this crate does not have — cargo rejects it outright with
    // "does not have a test target". The binary that actually fails is the
    // harness cargo builds from the *lib* target, and there is no build-script
    // instruction that scopes to that alone, so the dependency is declared
    // unscoped. It therefore also reaches the app binary, which is harmless:
    // `/MANIFESTDEPENDENCY` contributes to the linker-generated manifest that
    // tauri's embedded one already asks for, so the app's requirement is
    // unchanged and only the harness gains something it was missing.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!(
            "cargo:rustc-link-arg=/MANIFESTDEPENDENCY:type='win32' \
             name='Microsoft.Windows.Common-Controls' version='6.0.0.0' \
             processorArchitecture='*' publicKeyToken='6595b64144ccf1df' language='*'"
        );
    }

    tauri_build::build()
}
