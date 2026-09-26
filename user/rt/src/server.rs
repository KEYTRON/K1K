//! Server side of a capability service: one endpoint that many clients call.
//!
//! A client says hello by sending the endpoint its replies should go to
//! ([`crate::fsproto::CONNECT`], usually together with a shared buffer). The
//! server remembers that pair per client task, which is what lets a single
//! listener serve many clients without giving any of them ambient authority:
//! a client can only be replied to on the endpoint it handed over itself.
//!
//! The buffer bookkeeping (mapping a client's memory object, bounds-checking
//! offsets into it) lives here so every service that speaks this shape gets it
//! right instead of rolling its own.

use crate::fsproto::*;
use crate::{Cap, Error, Result, mem_map, recv, send};

/// What the server knows about one connected client.
pub struct Client {
    /// Task that sent us the messages; also the key we reply to.
    pub task: u32,
    /// Where replies go — the client's own endpoint, once it has sent it.
    reply: Option<Cap>,
    /// The client's shared buffer, if it registered one.
    pub buf: Option<Cap>,
    /// Our mapping of that buffer.
    map: Option<*mut u8>,
    pub map_len: usize,
}

impl Client {
    fn new(task: u32) -> Self {
        Self {
            task,
            reply: None,
            buf: None,
            map: None,
            map_len: 0,
        }
    }

    /// Whether the client has told us where to send its replies.
    pub fn has_reply(&self) -> bool {
        self.reply.is_some()
    }
}

impl Client {
    /// The client's buffer as a slice, or empty when it never registered one.
    pub fn bytes(&self) -> &[u8] {
        match self.map {
            Some(p) if self.map_len > 0 => unsafe {
                core::slice::from_raw_parts(p as *const u8, self.map_len)
            },
            _ => &[],
        }
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        match self.map {
            Some(p) if self.map_len > 0 => unsafe {
                core::slice::from_raw_parts_mut(p, self.map_len)
            },
            _ => &mut [],
        }
    }
}

/// A request taken off the wire, with the client it came from.
pub struct Request {
    pub op: u64,
    /// Bytes the client says it wrote into the request block.
    pub req_len: u64,
    /// Bytes it wants reserved for the result.
    pub want: u64,
    pub client: u32,
    /// A capability that arrived with the request, if any.
    pub cap: Option<Cap>,
}

pub struct Server {
    listen: Cap,
    clients: [Option<Client>; MAX_CLIENTS],
    cursor: usize,
}

impl Server {
    pub fn new(listen: Cap) -> Self {
        Self {
            listen,
            clients: core::array::from_fn(|_| None),
            cursor: 0,
        }
    }

    /// Clients currently connected.
    pub fn client_count(&self) -> usize {
        self.clients.iter().filter(|c| c.is_some()).count()
    }

    fn slot_of(&self, task: u32) -> Option<usize> {
        self.clients
            .iter()
            .position(|c| c.as_ref().is_some_and(|c| c.task == task))
    }

    /// The client's slot, created on first contact. The two handshakes —
    /// `CONNECT` and `REGISTER_BUF` — are independent, so either may arrive
    /// first and a client may even re-send one to change it.
    fn slot_of_or_new(&mut self, task: u32) -> Option<&mut Client> {
        if self.slot_of(task).is_none() {
            for _ in 0..MAX_CLIENTS {
                self.cursor = (self.cursor + 1) % MAX_CLIENTS;
                if self.clients[self.cursor].is_none() {
                    self.clients[self.cursor] = Some(Client::new(task));
                    break;
                }
            }
        }
        let i = self.slot_of(task)?;
        self.clients[i].as_mut()
    }

    /// The client's state, if it has connected.
    pub fn client(&self, task: u32) -> Option<&Client> {
        self.slot_of(task).and_then(|i| self.clients[i].as_ref())
    }

    pub fn client_mut(&mut self, task: u32) -> Option<&mut Client> {
        let i = self.slot_of(task)?;
        self.clients[i].as_mut()
    }

    /// Block until a request arrives, handling the connection handshakes and
    /// `BYE` on the way. Returns `None` only if our own endpoint is gone.
    pub fn recv(&mut self) -> Option<Request> {
        loop {
            let m = recv(self.listen).ok()?;
            // Opcodes live in the low 16 bits; `register` carries its page
            // count above them, every other request keeps w0 clean.
            let raw = m.words[0];
            let op = raw & 0xFFFF;
            let task = m.sender;
            match op {
                CONNECT => {
                    let Some(reply) = m.cap else { continue };
                    self.connect(task, reply);
                }
                REGISTER_BUF => {
                    let Some(buf) = m.cap else { continue };
                    self.register(task, buf, (raw >> 32) as usize);
                }
                BYE => {
                    self.drop_client(task);
                }
                _ => {
                    return Some(Request {
                        op,
                        req_len: m.words[1],
                        want: m.words[2],
                        client: task,
                        cap: m.cap,
                    });
                }
            }
        }
    }

    /// Remember (or replace) the endpoint a client's replies go to.
    pub fn connect(&mut self, task: u32, reply: Cap) -> bool {
        match self.slot_of_or_new(task) {
            Some(c) => {
                c.reply = Some(reply);
                true
            }
            None => false,
        }
    }

    /// Map a client's shared buffer, replacing any previous one.
    pub fn register(&mut self, task: u32, buf: Cap, pages: usize) -> bool {
        let Some(c) = self.slot_of_or_new(task) else {
            return false;
        };
        let pages = pages.clamp(1, 4096);
        match mem_map(buf, true) {
            Ok(p) => {
                c.buf = Some(buf);
                c.map = Some(p);
                c.map_len = pages * 4096;
            }
            Err(_) => {
                c.buf = None;
                c.map = None;
                c.map_len = 0;
            }
        }
        true
    }

    pub fn drop_client(&mut self, task: u32) {
        if let Some(i) = self.slot_of(task) {
            self.clients[i] = None;
        }
    }

    /// The client's buffer as raw parts. A handler needs this (rather than
    /// `client_mut`) so it can work on the bytes and still reply afterwards.
    pub fn client_buffer(&self, task: u32) -> Option<(*mut u8, usize)> {
        let c = self.client(task)?;
        match c.map {
            Some(p) if c.map_len > 0 => Some((p, c.map_len)),
            _ => None,
        }
    }

    /// Send a reply to the client that made `req`. A client that never sent
    /// its reply endpoint gets an error rather than silence, so the caller can
    /// say so instead of waiting forever.
    pub fn reply(&self, req: &Request, status: u64, arg0: u64, arg1: u64) -> Result<()> {
        let Some(c) = self.client(req.client) else {
            return Err(Error::Inval);
        };
        let Some(reply) = c.reply else {
            return Err(Error::Inval);
        };
        send(reply, status, arg0, arg1)
    }

    /// Tell a client something went wrong, and log nothing: the client sees
    /// the status in its own reply.
    pub fn error(&self, req: &Request, status: u64) {
        let _ = self.reply(req, status, 0, 0);
    }
}
