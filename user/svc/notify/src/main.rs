//! notify — the semantics of the asynchronous notification object, checked from
//! ring 3.
//!
//! The kernel hands this service two capabilities on the *same* notification:
//! slot 0 may signal and wait, slot 1 may only wait. That is the point of the
//! object, so it is the first thing checked — authority over a notification is
//! two separate rights, and the weaker one is useless for anything but
//! receiving.
//!
//! The rest of the script runs against a notification the service creates for
//! itself, and checks what a message queue cannot do: a signal that arrives
//! before anybody is waiting is still there afterwards, a driver can take every
//! pending signal in one go, and a poll that finds nothing says so instead of
//! sleeping.
#![no_std]
#![no_main]

use k1k_rt::{Cap, WaitMode, exit, log, notify_create, notify_signal, notify_wait};

/// Slot 0: the kernel's notification, signal and wait.
const SHARED: Cap = Cap(0);
/// Slot 1: the same notification, waiting only.
const WAIT_ONLY: Cap = Cap(1);

fn main() -> ! {
    rights_are_separate();
    signals_are_remembered();
    log!("notification semantics ok: remembered, drainable, rights split");
    // Stay alive: a restart would show up in the log, and a service that exits
    // with 0 is never restarted.
    loop {
        k1k_rt::sleep_ms(500);
    }
}

/// The waiting half of a relationship cannot become the signalling half.
fn rights_are_separate() {
    check(
        "signal on the full capability",
        notify_signal(SHARED, 1).is_ok(),
    );
    check(
        "wait-only capability can take the signal",
        matches!(notify_wait(WAIT_ONLY, WaitMode::One), Ok(1)),
    );
    check(
        "wait-only capability cannot signal",
        matches!(notify_signal(WAIT_ONLY, 1), Err(k1k_rt::Error::Perm)),
    );
    // Leave the shared notification empty for the next run.
    check(
        "shared notification is empty again",
        matches!(notify_wait(SHARED, WaitMode::Poll), Err(e) if e == k1k_rt::Error::Again),
    );
}

fn signals_are_remembered() {
    let n = notify_create().unwrap_or_else(|e| die("notify_create", e));

    // Nothing has happened yet, so a poll must not sleep.
    check(
        "poll on an empty notification",
        matches!(notify_wait(n, WaitMode::Poll), Err(e) if e == k1k_rt::Error::Again),
    );

    // Three signals, then one wait that takes them all. A signal recorded
    // before the wait is not lost: that is the difference between a
    // notification and a queue that drops what it cannot hold.
    check("signal three", notify_signal(n, 3).is_ok());
    check(
        "take all three at once",
        matches!(notify_wait(n, WaitMode::All), Ok(3)),
    );
    check(
        "poll after draining",
        matches!(notify_wait(n, WaitMode::Poll), Err(e) if e == k1k_rt::Error::Again),
    );

    // Signals nobody takes stay counted, however long that takes — and a poll
    // takes exactly one, so the count only goes down by what is asked for here.
    check("signal seven", notify_signal(n, 7).is_ok());
    check(
        "a poll takes one",
        matches!(notify_wait(n, WaitMode::Poll), Ok(1)),
    );
    for round in 1..=2 {
        k1k_rt::sleep_ms(20);
        check(
            "still counted after a pause",
            matches!(notify_wait(n, WaitMode::Poll), Ok(1)),
        );
        let _ = round;
    }
    check(
        "the other four are still there",
        matches!(notify_wait(n, WaitMode::All), Ok(4)),
    );
    check(
        "one at a time now",
        matches!(notify_signal(n, 2), Ok(0))
            && matches!(notify_wait(n, WaitMode::One), Ok(1))
            && matches!(notify_wait(n, WaitMode::One), Ok(1)),
    );
}

fn check(what: &str, ok: bool) {
    if ok {
        log!("ok: {}", what);
    } else {
        log!("FAILED: {}", what);
        exit(3);
    }
}

fn die(what: &str, e: k1k_rt::Error) -> ! {
    log!("{} failed: {:?}", what, e);
    exit(2)
}

k1k_rt::main!(main);
