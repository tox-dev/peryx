use std::collections::BTreeSet;
use std::io::ErrorKind;
use std::net::TcpListener;
#[cfg(unix)]
use std::process::Command;
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::time::Duration;

use crate::{
    CLAIM_PORT_OFFSET, FIXTURE_PORT_BASE, FIXTURE_PORT_COUNT, ListenerReservation, claim_port, fixture_port_candidates,
    free_port, startup_log,
};
#[cfg(unix)]
use crate::{StartupSignal, wait_for_startup};

fn band() -> std::ops::Range<u16> {
    FIXTURE_PORT_BASE..FIXTURE_PORT_BASE + FIXTURE_PORT_COUNT
}

#[test]
fn startup_log_keeps_the_failure_and_backtrace_tail() {
    let lines: Vec<_> = (0..80).map(|index| format!("line {index}")).collect();
    let excerpt = startup_log(&lines.join("\n"));
    assert!(excerpt.starts_with("line 0\n"));
    assert!(excerpt.contains("... 20 lines omitted ..."));
    assert!(excerpt.ends_with("line 79"));
    assert_eq!(startup_log("short\nlog"), "short\nlog");
}

#[test]
fn reservation_holds_a_claim_on_a_port_below_the_ephemeral_range() {
    let reservation = ListenerReservation::ephemeral().expect("claim a fixture port");
    assert!(band().contains(&reservation.port));
    assert_eq!(
        TcpListener::bind(("127.0.0.1", claim_port(reservation.port)))
            .map_err(|error| error.kind())
            .unwrap_err(),
        ErrorKind::AddrInUse,
    );
}

#[test]
fn reservation_walks_past_a_candidate_another_draw_claimed() {
    let held = ListenerReservation::ephemeral().expect("claim a fixture port");
    let reservation = ListenerReservation::claimed(std::iter::once(held.port).chain(fixture_port_candidates()))
        .expect("draw past the held claim");
    assert_ne!(reservation.port, held.port);
}

#[test]
fn reservation_walks_past_a_candidate_whose_number_is_bound() {
    // `held` listens on its own claim number, which no draw ever claims, so that candidate fails on
    // the number rather than on the claim.
    let held = ListenerReservation::ephemeral().expect("claim a fixture port");
    let bound = claim_port(held.port);
    let reservation = ListenerReservation::claimed(std::iter::once(bound).chain(fixture_port_candidates()))
        .expect("draw past the bound number");
    assert_ne!(reservation.port, bound);
}

#[test]
fn reservation_reports_a_band_with_nothing_left() {
    let held = ListenerReservation::ephemeral().expect("claim a fixture port");
    assert_eq!(
        ListenerReservation::claimed(std::iter::once(held.port))
            .map_err(|error| error.kind())
            .unwrap_err(),
        ErrorKind::AddrInUse,
    );
}

#[test]
fn reservation_rebinds_the_same_number_for_a_restart() {
    let mut reservation = ListenerReservation::ephemeral().expect("claim a fixture port");
    let port = reservation.port;
    reservation.rebind().expect("take the number back");
    assert_eq!(reservation.port, port);
    assert_eq!(
        TcpListener::bind(("127.0.0.1", claim_port(port)))
            .map_err(|error| error.kind())
            .unwrap_err(),
        ErrorKind::AddrInUse,
    );
}

#[test]
fn reservation_rebind_leaves_an_unused_control_number_alone() {
    let mut reservation = ListenerReservation::released(0);
    reservation.rebind().expect("skip the unused number");
    assert_eq!(reservation.port, 0);
}

#[test]
fn free_port_keeps_the_claim_and_leaves_the_number_bindable() {
    let reservation = free_port();
    let bound = TcpListener::bind(("127.0.0.1", reservation.port)).expect("bind the freed number");
    assert_eq!(bound.local_addr().expect("bound address").port(), reservation.port,);
    assert_eq!(
        TcpListener::bind(("127.0.0.1", claim_port(reservation.port)))
            .map_err(|error| error.kind())
            .unwrap_err(),
        ErrorKind::AddrInUse,
    );
}

#[test]
fn candidates_cover_the_whole_band_from_a_moving_start() {
    let first: Vec<_> = fixture_port_candidates().collect();
    let second: Vec<_> = fixture_port_candidates().collect();
    assert_eq!(first.len(), usize::from(FIXTURE_PORT_COUNT));
    assert_eq!(first.iter().copied().collect::<BTreeSet<_>>().len(), first.len());
    assert!(first.iter().all(|port| band().contains(port)));
    assert_ne!(first[0], second[0]);
}

#[test]
fn claim_numbers_sit_above_the_band_and_below_the_ephemeral_range() {
    assert_eq!(claim_port(FIXTURE_PORT_BASE), FIXTURE_PORT_BASE + CLAIM_PORT_OFFSET);
    assert!(claim_port(band().end - 1) < 32_768);
}

#[test]
#[cfg(unix)]
fn node_timeout_reports_a_reaped_child_while_the_event_channel_is_open() {
    let mut child = Command::new("true").spawn().expect("start child");
    child.wait().expect("reap child");
    let (event_sender, process_events) = mpsc::channel();
    let signal = wait_for_startup(&mut child, &process_events, Duration::ZERO, &mut |_| false)
        .expect("classify startup timeout");
    drop(event_sender);
    assert!(matches!(signal, StartupSignal::Exited(_)));
}
