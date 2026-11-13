//! What the kit says when there is no emulator to start. One test, because it changes the environment of the process.

use azemu_testkit::Emulator;

#[test]
fn a_missing_program_is_an_error_that_says_what_to_do() {
    std::env::set_var("AZEMU_BIN", "/nowhere/azemu-that-does-not-exist");
    let err = Emulator::try_start(&[]).unwrap_err();
    assert!(err.contains("AZEMU_BIN") && err.contains("does not exist") && err.contains("cargo build -p azemu"), "{err}");

    std::env::remove_var("AZEMU_BIN");
    std::env::set_var("PATH", "/nowhere");
    let err = Emulator::try_start(&[]).unwrap_err();
    assert!(
        err.contains("not found on the PATH")
            && err.contains("AZEMU_BIN")
            && err.contains("AZEMU_GATEWAY")
            && err.contains("cargo install"),
        "{err}"
    );

    // a program that is there but is not azemu: its own words are shown
    std::env::set_var("AZEMU_BIN", "/bin/false");
    let err = Emulator::try_start(&[]).unwrap_err();
    assert!(err.contains("did not become ready") && err.contains("/bin/false"), "{err}");
}
