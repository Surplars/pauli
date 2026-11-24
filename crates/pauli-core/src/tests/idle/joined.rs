//! Native compare choices with stopped firmware and a waiting peer on one hart.
//! One manager drives the real create/stop/join paths; time advances only to the programmed compare.
use super::{Dispatcher, NOW, deadline, guests, guests_on, idle, park};
use pauli_control::ChildName;

#[test]
fn stopped_clones_do_not_insert_timer_wakes_before_the_live_compare() {
    let f = guests_on(1, false, pauli_config::CpuAffinity::Automatic, 1);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    for name in ["sensorA", "sensorB", "sensorC"] {
        let child = ChildName::new(name).unwrap().into();
        let slot = rt.manager.provision(rt.board, None, 1, child).unwrap();
        assert!(rt.manager.vm(slot).unwrap().lifecycle.joined());
    }

    deadline(&f, 1, Some(1800));
    NOW.with(|now| now.set(1000));
    let mut compares = std::vec::Vec::new();
    while !rt.dispatch(&mut local).unwrap() {
        let at = idle(&f, &mut local).expect("peer compare is still in the future");
        let prev = NOW.with(|now| now.replace(at));
        assert!(prev < at && at <= 1800);
        compares.push(at);
    }
    assert_eq!(compares, [1800]); // With only timer returns, no intermediary HS wake is needed.
}

#[test]
fn joined_console_tail_is_host_work_but_stopped_rx_is_not() {
    let f = guests(1, false);
    let rt = &f.runtime;
    let mut local = Dispatcher::new();
    park(&f, 0, &mut local);
    park(&f, 1, &mut local);
    NOW.with(|n| n.set(1000));
    deadline(&f, 1, Some(1800));
    let vm = rt.manager.vm(0).unwrap();
    {
        let mut io = vm.devices.io.lock();
        io.console.access(0, Some(b'!' as usize)).unwrap();
        io.console.access(2, Some(0x41)).unwrap(); // RX FIFO trigger: four bytes.
        io.console.access(1, Some(1)).unwrap(); // One unread byte arms the RX timeout.
        io.console.uart.advance(1000);
        assert!(io.console.uart.receive(b'x') && io.console.uart.rx_timeout_armed());
    }
    vm.host_stop();
    assert_eq!(idle(&f, &mut local), Some(1100)); // Retirement is not complete.
    rt.dispatch(&mut local).unwrap();
    rt.manager.service(rt.board);
    assert!(vm.lifecycle.joined());
    assert_eq!(idle(&f, &mut local), Some(1100)); // TX still belongs to the host.
    deadline(&f, 1, Some(1040));
    assert_eq!(idle(&f, &mut local), Some(1040)); // TX cannot postpone a real compare.
    let mut out = [0];
    assert_eq!(vm.devices.io.lock().console.drain(&mut out), 1);
    assert_eq!(&out, b"!");
    deadline(&f, 1, Some(1800));
    assert_eq!(idle(&f, &mut local), Some(1800)); // Unread RX cannot wake a joined vCPU.
}
