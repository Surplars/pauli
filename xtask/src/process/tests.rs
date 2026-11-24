//! Bound the real smoke process even when the controller cannot poll its clock.
//! Tests own local copies of this test binary, never QEMU or an external fixture.
use super::{Output, Session};
use std::{
    env,
    process::Command,
    thread,
    time::{Duration, Instant},
};

const CHILD_TEST: &str = "process::tests::unread_stdin_child";
const READY: &str = "child holding stdin open";

#[test]
#[ignore = "private subprocess entry for the watchdog test"]
fn unread_stdin_child() {
    if env::var_os("PAULI_TEST_HOLD_STDIN").is_some() {
        println!("{READY}");
        // A broken watchdog must fail its test, not hang the host test runner.
        thread::sleep(Duration::from_secs(5));
    }
}

#[test]
fn deadline_terminates_child_while_serial_write_is_blocked() {
    let mut cmd = Command::new(env::current_exe().unwrap());
    cmd.args(["--ignored", "--exact", CHILD_TEST, "--nocapture"])
        .env("PAULI_TEST_HOLD_STDIN", "1");
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut session = Session::start_until(&mut cmd, deadline).unwrap();
    let mut output = Vec::new();
    while !output.windows(READY.len()).any(|s| s == READY.as_bytes()) {
        match session
            .data(deadline.saturating_duration_since(Instant::now()))
            .unwrap()
        {
            Output::Data(bytes) => output.extend(bytes),
            _ => panic!("fixture never reached its unread stdin"),
        }
    }
    // Exceeds a normal anonymous pipe. No controller poll, close_input or Drop
    // can help this write: the watchdog must terminate the holder of the read end.
    assert!(session.send(&vec![0; 2 * 1024 * 1024]).is_err());
    let status = session
        .child
        .lock()
        .unwrap()
        .try_wait()
        .unwrap()
        .expect("blocked writer returned while child was still running");
    assert!(
        !status.success(),
        "fixture returned normally instead of being killed"
    );
    assert!(
        Instant::now() >= deadline,
        "watchdog fired before the deadline"
    );
}

#[test]
fn clean_exit_cancels_watchdog_without_waiting_for_deadline() {
    let mut cmd = Command::new(env::current_exe().unwrap());
    cmd.args(["--list", "--exact", CHILD_TEST]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut session = Session::start_until(&mut cmd, deadline).unwrap();
    session.finish(Some(deadline)).unwrap();
    drop(session);
    assert!(
        Instant::now() < deadline,
        "cleanup waited out an unused watchdog"
    );
}
