// Keep the binary entry point small. The implementation is split into
// responsibility-focused source slices below; `include!` keeps the first
// refactor behavior-preserving while allowing the slices to share the
// existing private helpers without a premature public API.
include!("cli.rs");
include!("commands.rs");
include!("session_commands.rs");
include!("diagnostics.rs");
include!("rpc.rs");
include!("terminal.rs");
include!("scroll.rs");
include!("switching.rs");
include!("attach.rs");
include!("system.rs");

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod switching_tests {
    include!("../a_tests.rs");
}

pub(crate) fn entrypoint() {
    main();
}
