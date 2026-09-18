//! Services: ring-3 programs the kernel supervises. This is the
//! "self-healing" half of the design — a crashed service is torn down
//! (address space, stack, capabilities) and re-instantiated from its image.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::VirtAddr;

use crate::arch::x86_64::{context, gdt, interrupts as irq};
use crate::ipc::{self, Endpoint};
use crate::klog;
use crate::loader;
use crate::mm::vmm::{AddressSpace, Flags, USER_STACK_TOP};
use crate::obj::{Capability, Object, Rights};
use crate::sched::{self, UserEntry, task::Task};

const USER_STACK_PAGES: usize = 16;
const MAX_RESTARTS_BEFORE_BACKOFF: u32 = 3;

pub struct Grant {
    pub endpoint: Arc<Endpoint>,
    pub rights: Rights,
}

pub struct ServiceSpec {
    pub name: &'static str,
    pub image: &'static [u8],
    pub grants: Vec<Grant>,
}

pub struct ServiceState {
    pub spec: ServiceSpec,
    pub task: Option<sched::task::TaskId>,
    pub restarts: u32,
    pub last_exit: Option<i64>,
}

pub static SERVICES: Mutex<Vec<ServiceState>> = Mutex::new(Vec::new());

macro_rules! image {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".elf"))
    };
}

static HELLO: &[u8] = image!("hello");
static FLAKY: &[u8] = image!("flaky");
static PING: &[u8] = image!("ping");
static PONG: &[u8] = image!("pong");
static KBD: &[u8] = image!("kbd");

pub fn register(spec: ServiceSpec) -> usize {
    let mut s = SERVICES.lock();
    s.push(ServiceState {
        spec,
        task: None,
        restarts: 0,
        last_exit: None,
    });
    s.len() - 1
}

/// Register the built-in services and wire their IPC capabilities.
pub fn init_builtin() {
    let req = Endpoint::new();
    let rep = Endpoint::new();

    register(ServiceSpec {
        name: "kbd",
        image: KBD,
        grants: alloc::vec![Grant {
            endpoint: ipc::keyboard_endpoint(),
            rights: Rights::RECV,
        }],
    });
    register(ServiceSpec {
        name: "hello",
        image: HELLO,
        grants: Vec::new(),
    });
    register(ServiceSpec {
        name: "flaky",
        image: FLAKY,
        grants: Vec::new(),
    });
    register(ServiceSpec {
        name: "pong",
        image: PONG,
        grants: alloc::vec![
            Grant {
                endpoint: req.clone(),
                rights: Rights::RECV,
            },
            Grant {
                endpoint: rep.clone(),
                rights: Rights::SEND,
            },
        ],
    });
    register(ServiceSpec {
        name: "ping",
        image: PING,
        grants: alloc::vec![
            Grant {
                endpoint: req,
                rights: Rights::SEND,
            },
            Grant {
                endpoint: rep,
                rights: Rights::RECV,
            },
        ],
    });
}

/// Build a fresh ring-3 task for service `idx` and make it runnable.
pub fn spawn(idx: usize) -> Option<sched::task::TaskId> {
    let (name, image, grants) = {
        let s = SERVICES.lock();
        let st = s.get(idx)?;
        let grants: Vec<Capability> = st
            .spec
            .grants
            .iter()
            .map(|g| Capability {
                object: Object::Endpoint(g.endpoint.clone()),
                rights: g.rights,
            })
            .collect();
        (st.spec.name, st.spec.image, grants)
    };

    let mut asp = AddressSpace::new()?;
    let loaded = match loader::load(&mut asp, image) {
        Ok(l) => l,
        Err(e) => {
            klog!(
                "superv",
                "service '{}': cannot load ELF image: {:?}",
                name,
                e
            );
            return None;
        }
    };
    let stack_bottom = USER_STACK_TOP - (USER_STACK_PAGES as u64) * 4096;
    asp.map_user_range(
        VirtAddr::new(stack_bottom),
        USER_STACK_PAGES,
        Flags::WRITABLE | Flags::NO_EXECUTE,
    )
    .ok()?;

    let mut t: Box<Task> = Task::new_kernel(0, name, user_task_entry, 0);
    t.addr_space = Some(asp);
    t.user = Some(UserEntry {
        rip: loaded.entry,
        rsp: USER_STACK_TOP - 16,
    });
    t.service = Some(idx);
    for cap in grants {
        t.caps.insert(cap);
    }
    let id = sched::add_task(t);
    SERVICES.lock()[idx].task = Some(id);
    Some(id)
}

extern "C" fn user_task_entry(_: u64) {
    let entry = sched::with_current(|t| t.user.expect("user task without entry"));
    let sel = gdt::selectors();
    unsafe {
        context::enter_user(
            entry.rip,
            entry.rsp,
            sel.user_code.0 as u64,
            sel.user_data.0 as u64,
        )
    }
}

pub fn start_all() {
    let n = SERVICES.lock().len();
    for i in 0..n {
        let (name, size) = {
            let s = SERVICES.lock();
            (s[i].spec.name, s[i].spec.image.len())
        };
        match spawn(i) {
            Some(id) => klog!(
                "superv",
                "started service '{}' as task {} ({} KiB ELF)",
                name,
                id,
                size / 1024
            ),
            None => klog!("superv", "failed to start service '{}'", name),
        }
    }
}

/// The supervisor thread: reaps dead tasks and restarts services.
pub extern "C" fn supervisor_main(_: u64) {
    klog!(
        "superv",
        "supervisor running as task {}",
        sched::current_id()
    );
    loop {
        while let Some(dead) = sched::take_dead() {
            let name = dead.name;
            let code = dead.exit_code.unwrap_or(0);
            let svc = dead.service;
            drop(dead);

            let Some(idx) = svc else {
                klog!("superv", "reaped kernel thread '{}' (exit {})", name, code);
                continue;
            };

            let (restarts, delay_ms) = {
                let mut s = SERVICES.lock();
                let st = &mut s[idx];
                st.task = None;
                st.last_exit = Some(code);
                st.restarts += 1;
                let backoff = if st.restarts > MAX_RESTARTS_BEFORE_BACKOFF {
                    (200u64 * (st.restarts - MAX_RESTARTS_BEFORE_BACKOFF) as u64).min(3000)
                } else {
                    0
                };
                (st.restarts, backoff)
            };
            let reason = if code < 0 { "crashed" } else { "exited" };
            klog!(
                "superv",
                "service '{}' {} (code {}) -> restarting (restart #{}{})",
                name,
                reason,
                code,
                restarts,
                if delay_ms > 0 {
                    alloc::format!(", backoff {} ms", delay_ms)
                } else {
                    alloc::string::String::new()
                }
            );
            if delay_ms > 0 {
                sched::sleep_ms(delay_ms);
            }
            match spawn(idx) {
                Some(id) => klog!(
                    "superv",
                    "service '{}' back up as task {} at {} ms",
                    name,
                    id,
                    irq::uptime_ms()
                ),
                None => klog!("superv", "service '{}' failed to respawn", name),
            }
        }
        sched::sleep_ms(50);
    }
}

pub fn dump() {
    let s = SERVICES.lock();
    for st in s.iter() {
        klog!(
            "superv",
            "  {:<8} task={:<4} restarts={} last_exit={:?}",
            st.spec.name,
            st.task.map(|t| t as i64).unwrap_or(-1),
            st.restarts,
            st.last_exit
        );
    }
}
