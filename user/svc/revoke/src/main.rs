//! revoke — what happens to authority that is taken back.
//!
//! Delegating a capability is easy; being able to withdraw it is the part that
//! matters, and it has a sharp edge worth testing from ring 3: a revoked slot is
//! a tombstone, not a free slot. A program that kept the number must not find a
//! different object there next time, because "the call failed" and "the call
//! reached something else" are very different failures.
#![no_std]
#![no_main]

use k1k_rt::{
    Cap, Error, WaitMode, cap_drop, cap_revoke, exit, log, notify_create, notify_signal,
    notify_wait,
};

/// Slot 0: the kernel's notification, signal and wait.
const SHARED: Cap = Cap(0);

fn main() -> ! {
    // Take a capability this task owns back from itself.
    let n = notify_create().unwrap_or_else(|e| die("notify_create", e));
    check("a fresh capability works", notify_signal(n, 1).is_ok());
    check("revoking our own slot works", cap_revoke(me(), n).is_ok());
    check(
        "a revoked capability does not work",
        matches!(notify_signal(n, 1), Err(Error::Perm)),
    );
    check(
        "nor does it look like it is waiting for anything",
        matches!(notify_wait(n, WaitMode::Poll), Err(Error::Perm)),
    );
    check(
        "revoking it twice says there is nothing there",
        matches!(cap_revoke(me(), n), Err(Error::Inval)),
    );

    // The slot must stay dead: a new capability has to go somewhere else.
    let m = notify_create().unwrap_or_else(|e| die("notify_create", e));
    check("a new capability does not land in a revoked slot", m != n);
    check("and the new one works", notify_signal(m, 1).is_ok());

    // Dropping is the other half: the slot is free again, so the next
    // capability reuses it. Revoking twice is refused; dropping twice is not,
    // because a dropped slot is simply empty.
    check("dropping works", cap_drop(m).is_ok());
    let again = notify_create().unwrap_or_else(|e| die("notify_create", e));
    check("a dropped slot is reused", again == m);
    check("and the reused slot works", notify_signal(again, 2).is_ok());

    // Revoking a capability this task does not own, and one that is not there.
    // Authority is checked before the task is even looked up, so a task without
    // `Control` cannot find out which tasks exist by watching error codes.
    check(
        "revoking another task's capability is refused without authority",
        matches!(cap_revoke(me() + 1000, SHARED), Err(Error::Perm)),
    );
    check(
        "and a task that does not exist gives the same answer",
        matches!(cap_revoke(0, SHARED), Err(Error::Perm)),
    );
    check(
        "revoking a slot that was never used is refused",
        matches!(cap_revoke(me(), Cap(200)), Err(Error::Inval)),
    );

    // The kernel-granted capability is still ours to hand back, and once it is
    // gone it is gone: the shared notification must be empty afterwards.
    check(
        "revoking the shared slot works",
        cap_revoke(me(), SHARED).is_ok(),
    );
    check(
        "and the shared capability is dead",
        matches!(notify_wait(SHARED, WaitMode::Poll), Err(Error::Perm)),
    );

    log!("capability revocation ok: withdrawn, tombstoned, drop is not revoke");
    loop {
        k1k_rt::sleep_ms(500);
    }
}

/// This task's id, from the boot info the supervisor wrote.
fn me() -> u32 {
    k1k_rt::info().task_id
}

fn check(what: &str, ok: bool) {
    if ok {
        log!("ok: {}", what);
    } else {
        log!("FAILED: {}", what);
        exit(3);
    }
}

fn die(what: &str, e: Error) -> ! {
    log!("{} failed: {:?}", what, e);
    exit(2)
}

k1k_rt::main!(main);
