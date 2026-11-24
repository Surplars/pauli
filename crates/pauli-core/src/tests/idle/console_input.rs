//! Exercise the monitor's bounded UART consumer and manager-side WFI decision.
//! The test owns the simulated RX ring; only Monitor::poll may remove a byte.
use super::{Dispatcher, NOW, deadline, guests, idle, park};
use crate::monitor::Monitor;

#[test]
fn input_left_by_a_bounded_monitor_pass_skips_wfi_without_consuming_the_byte() {
    let f = guests(1, false);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    let mut monitor = Monitor::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|now| now.set(1000));

    rt.board.input.lock().extend([b'x'; 32]);
    rt.board.input.lock().push_back(b'y');
    monitor.poll(rt);
    assert_eq!(rt.board.input.lock().front(), Some(&b'y'));
    assert_eq!(idle(&f, &mut local), None); // No guest deadline.
    deadline(&f, 0, Some(1500));
    assert_eq!(idle(&f, &mut local), None); // A future guest timer cannot hide queued RX.
    assert_eq!(rt.board.input.lock().len(), 1);

    monitor.poll(rt);
    let mut line = [0; 160];
    assert_eq!(monitor.terminal.line(&mut line), Some(33));
    assert_eq!(&line[..32], &[b'x'; 32]);
    assert_eq!(line[32], b'y');
    assert!(rt.board.input.lock().is_empty());
    assert_eq!(idle(&f, &mut local), Some(1500));
}

#[test]
fn without_a_deadline_only_unfinished_work_keeps_the_short_poll() {
    let f = guests(1, false);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|now| now.set(1000));

    let first = rt.manager.vm(0).unwrap();
    first.host_stop();
    assert_eq!(idle(&f, &mut local), Some(1100)); // Not yet retired.
    rt.dispatch(&mut local).unwrap();
    rt.manager.service(rt.board);
    assert!(first.lifecycle.joined());
    assert_eq!(idle(&f, &mut local), Some(2000));
    let second = rt.manager.vm(1).unwrap();
    second
        .devices
        .io
        .lock()
        .console
        .access(0, Some(65))
        .unwrap();
    assert_eq!(idle(&f, &mut local), Some(1100)); // Guest UART output pending.
    let mut monitor = Monitor::new();
    monitor.poll(rt);
    second.host_stop();
    rt.dispatch(&mut local).unwrap();
    rt.manager.service(rt.board);
    assert!(second.lifecycle.joined());
    assert_eq!(idle(&f, &mut local), Some(2000)); // Entire host quiescent, timer still armed.
}
