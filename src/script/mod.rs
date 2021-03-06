// Copyright (c) 2017, All Contributors (see CONTRIBUTORS file)
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! # PumpkinScript
//!
//! PumpkinScript is a minimalistic concatenative, stack-based language inspired
//! by Forth.
//!
//! It is used in PumpkinDB to operate a low-level database "virtual machine" —
//! to manipulate, record and retrieve data.
//!
//! This is an ultimate gateway to flexibility in how PumpkinDB can operate, what
//! formats can it support, etc.
//!
//! # Reasoning
//!
//! Why is it important?
//!
//! In previous incarnations (or, rather, inspirations) of PumpkinDB much more rigid structures,
//! formats and encoding were established as a prerequisite for using it, unnecessarily limiting
//! the applicability and appeal of the technology and ideas behind it. For example, one had to buy
//! into [ELF](https://rfc.eventsourcing.com/spec:1/ELF), UUID-based event identification and
//! [HLC-based](https://rfc.eventsourcing.com/spec:6/HLC) timestamps.
//!
//! So it was deemed to be important to lift this kind of restrictions in PumpkinDB. But how do we
//! support all the formats without knowing what they are?
//!
//! What if there was a way to describe how data should be processed, for example,
//! for indexing — in a compact, unambiguous and composable form? Or even for recording data
//! itself?
//! Well, that's where the idea to use something like a Forth-like script was born.
//!
//! Instead of devising custom protocols for talking to PumpkinDB, the protocol of communication has
//! become a pipeline to a script executor.
//!
//! So, for example, a command/events set can be recorded with something like this (not an actual
//! script, below is pseudocode):
//!
//! ```forth
//! <command id> <command payload> JOURNAL <event id> <event payload> JOURNAL
//! ```
//!
//! This offers us enormous extension and flexibility capabilities. To name a few:
//!
//! * Low-level imperative querying (as a foundation for declarative queries)
//! * Indexing filters
//! * Subscription filters
//!
//! # Features
//!
//! * Binary and text (human readable & writable) forms
//! * No types, just byte arrays
//! * Dynamic code evaluation
//! * Zero-copy interpretation (where feasible; currently does not apply to the most
//!   important part, the storage itself as transactional model of LMDB precludes us
//!   from carrying these references outside of the scope of the transaction)
//!


use alloc::heap;

use num_bigint::BigUint;
use std::cmp;

/// `word!` macro is used to define a built-in word, its signature (if applicable)
/// and representation
macro_rules! word {
    ($name : ident,
    ($($input : ident),* => $($output : ident),*),
    $ident : expr) =>
    (
     word!($name, $ident);
    );
    ($name : ident,
    $ident : expr) =>
    (
     const $name : &'static[u8] = $ident;
    )
}

// Built-in words
// TODO: the list of built-in words is far from completion

// How to write a new built-in word:
// 1. Add `word!(...)` to define a constant
// 2. Document it
// 3. Write a test in mod tests
// 4. Add `handle_word` function in VM and list it in `match_words!()` macro
//    invocation in VM::pass

// Category: Stack

word!(DROP, (a => ), b"\x84DROP");
word!(DUP, (a => a, a), b"\x83DUP");
word!(SWAP, (a, b => b, a), b"\x84SWAP");
word!(ROT, (a, b, c  => b, c, a), b"\x83ROT");
word!(OVER, (a, b => a, b, a), b"\x84OVER");
word!(DEPTH, b"\x85DEPTH");

// Category: Byte arrays

word!(CONCAT, (a, b => c), b"\x86CONCAT");

// Category: Control flow
word!(EVAL, b"\x84EVAL");

// Category: Storage
word!(WRITE, b"\x85WRITE");
word!(WRITE_END, b"\x80\x85WRITE"); // internal word

word!(READ, b"\x84READ");
word!(READ_END, b"\x80\x84READ"); // internal word

word!(ASSOC, b"\x85ASSOC");
word!(ASSOCQ, b"\x86ASSOC?");
word!(RETR, b"\x84RETR");
word!(COMMIT, b"\x86COMMIT");

/// # Data Representation
///
/// In an effort to keep PumpkinScript dead simple, we are not introducing enums
/// or structures to represent instructions (although some argued that we rather should).
/// Instead, their binary form is kept.
///
/// Data push instructions:
///
/// * `<len @ 0..120u8> [_;len]` — byte arrays of up to 120 bytes can have their size indicated
/// in the first byte, followed by that size's number of bytes
/// * `<121u8> <len u8> [_; len]` — byte array from 121 to 255 bytes can have their size indicated
/// in the second byte, followed by that size's number of bytes, with `121u8` as the first byte
/// * `<122u8> <len u16> [_; len]` — byte array from 256 to 65535 bytes can have their size
/// indicated in the second and third bytes (u16), followed by that size's number of bytes,
/// with `122u8` as the first byte
/// * `<123u8> <len u32> [_; len]` — byte array from 65536 to 4294967296 bytes can have their
/// size indicated in the second, third, fourth and fifth bytes (u32), followed by that size's
/// number of bytes, with `123u8` as the first byte
///
/// Word:
///
/// * `<len @ 129u8..255u8> [_; len ^ 128u8]` — if `len` is greater than `128u8`, the following
/// byte array of `len & 128u8` length (len without the highest bit set) is considered a word.
/// Length must be greater than zero.
///
/// `128u8` is reserved as a prefix to be followed by an internal VM's word (not to be accessible
/// to the end users).
///
/// The rest of tags (`124u8` to `127u8`) are reserved for future use.
///
pub type Program = Vec<u8>;

/// `Error` represents an enumeration of possible `Executor` errors.
#[derive(Debug, PartialEq, Clone)]
pub enum Error {
    /// An attempt to get a value off the top of the stack was made,
    /// but the stack was empty.
    EmptyStack,
    /// Word is unknown
    UnknownWord,
    /// Binary format decoding failed
    DecodingError,
    /// Database Error
    DatabaseError(lmdb::Error),
    /// Duplicate key
    DuplicateKey,
    /// Key not found
    UnknownKey,
    /// No active transaction
    NoTransaction,
    /// An internal scheduler's error to indicate that currently
    /// executed environment should be rescheduled from the same point
    Reschedule,
}
/// Parse-related error
#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// Incomplete input
    Incomplete,
    /// Error with a code
    Err(u32),
    /// Unknown error
    UnknownErr,
}

pub mod binparser;
pub use self::binparser::parse as parse_bin;

mod textparser;
pub use self::textparser::parse;

/// Initial stack size
pub const STACK_SIZE: usize = 32_768;
/// Initial heap size
pub const HEAP_SIZE: usize = 32_768;

/// Env is a representation of a stack and the heap.
///
/// Doesn't need to be used directly as it's primarily
/// used by [`VM`](struct.VM.html)
pub struct Env<'a> {
    stack: Vec<&'a [u8]>,
    stack_size: usize,
    heap: *mut u8,
    heap_size: usize,
    heap_align: usize,
    heap_ptr: usize,
}

impl<'a> std::fmt::Debug for Env<'a> {
    fn fmt(&self, fmt: &mut std::fmt::Formatter) -> std::fmt::Result {
        fmt.write_str("Env()")
    }
}

unsafe impl<'a> Send for Env<'a> {}

const _EMPTY: &'static [u8] = b"";

use std::slice;
use std::mem;

impl<'a> Env<'a> {
    /// Creates an environment with [an empty stack of default size](constant.STACK_SIZE.html)
    pub fn new() -> Self {
        Env::new_with_stack_size(STACK_SIZE)
    }

    /// Creates an environment with an empty stack of specific size
    pub fn new_with_stack_size(size: usize) -> Self {
        Env::new_with_stack(vec![_EMPTY; size], 0)
    }

    /// Creates an environment with an existing stack and a pointer to the
    /// topmost element (stack_size)
    ///
    /// This function is useful for working with result stacks received from
    /// [VM](struct.VM.html)
    pub fn new_with_stack(stack: Vec<&'a [u8]>, stack_size: usize) -> Self {
        Env {
            stack: stack,
            stack_size: stack_size,
            heap: unsafe { heap::allocate(HEAP_SIZE, mem::align_of::<u8>()) },
            heap_size: HEAP_SIZE,
            heap_align: mem::align_of::<u8>(),
            heap_ptr: 0,
        }
    }

    /// Returns the entire stack
    #[inline]
    pub fn stack(&self) -> &[&'a [u8]] {
        &self.stack.as_slice()[0..self.stack_size as usize]
    }

    /// Returns top of the stack without removing it
    #[inline]
    pub fn stack_top(&self) -> Option<&'a [u8]> {
        if self.stack_size == 0 {
            None
        } else {
            Some(self.stack.as_slice()[self.stack_size as usize - 1])
        }
    }

    /// Removes the top of the stack and returns it
    #[inline]
    pub fn pop(&mut self) -> Option<&'a [u8]> {
        if self.stack_size == 0 {
            None
        } else {
            let val = Some(self.stack.as_slice()[self.stack_size as usize - 1]);
            self.stack.as_mut_slice()[self.stack_size as usize - 1] = _EMPTY;
            self.stack_size -= 1;
            val
        }
    }

    /// Pushes value on top of the stack
    #[inline]
    pub fn push(&mut self, data: &'a [u8]) {
        // check if we are at capacity
        if self.stack_size == self.stack.len() {
            let mut vec = vec![_EMPTY; STACK_SIZE];
            self.stack.append(&mut vec);
        }
        self.stack.as_mut_slice()[self.stack_size] = data;
        self.stack_size += 1;
    }

    /// Allocates a slice off the Env-specific heap. Will be collected
    /// once this Env is dropped.
    pub fn alloc(&mut self, len: usize) -> &'a mut [u8] {
        if self.heap_ptr + len >= self.heap_size {
            let increase = cmp::max(len, HEAP_SIZE);
            unsafe {
                self.heap = heap::reallocate(self.heap,
                                 self.heap_size,
                                 self.heap_size + increase,
                                 self.heap_align);
            }
            self.heap_size += increase;
        }
        let mut space = unsafe { slice::from_raw_parts_mut(self.heap, self.heap_size) };
        let slice = &mut space[self.heap_ptr..self.heap_ptr + len];
        self.heap_ptr += len;
        slice
    }
}

impl<'a> Drop for Env<'a> {
    fn drop(&mut self) {
        unsafe {
            heap::deallocate(self.heap, self.heap_size, self.heap_align);
        }
    }
}

use nom;

#[inline]
pub fn offset_by_size(size: usize) -> usize {
    match size {
        0...120 => 1,
        120...255 => 2,
        255...65535 => 3,
        65536...4294967296 => 5,
        _ => unreachable!(),
    }
}

macro_rules! write_size_into_slice {
    ($size:expr, $slice: expr) => {
     match $size {
        0...120 => {
            $slice[0] = $size as u8;
            1
        }
        121...255 => {
            $slice[0] = 121u8;
            $slice[1] = $size as u8;
            2
        }
        256...65535 => {
            $slice[0] = 122u8;
            $slice[1] = ($size >> 8) as u8;
            $slice[2] = $size as u8;
            3
        }
        65536...4294967296 => {
            $slice[0] = 123u8;
            $slice[1] = ($size >> 24) as u8;
            $slice[2] = ($size >> 16) as u8;
            $slice[3] = ($size >> 8) as u8;
            $slice[4] = $size as u8;
            5
        }
        _ => unreachable!(),
    }
    };
}

macro_rules! data {
    ($ptr:expr) => {
        {
          let (_, size) = binparser::data_size($ptr).unwrap();
          (&$ptr[offset_by_size(size)..$ptr.len()], size)
        }
    };
}

macro_rules! handle_words {
    ($me: expr, $env: expr, $program: expr, $word: expr, $res: ident,
     $pid: ident, [ $($name: ident),* ], $block: expr) => {
    {
      let mut env = $env;
      $(
       env =
        match $me.$name(env, $word, $pid) {
          Err((env, Error::Reschedule)) => return Ok((env, Some($program.clone()))),
          Err((env, Error::UnknownWord)) => env,
          Err((env, err)) => return Err((env, err)),
          Ok($res) => $block
        };
      )*
      return Err((env, Error::UnknownWord))
    }
    };
}

macro_rules! validate_lockout {
    ($env: expr, $name: expr, $pid: expr) => {
        if let Some((pid_, _)) = $name {
            if pid_ != $pid {
                return Err(($env, Error::Reschedule))
            }
        }
    };
}

use std::sync::mpsc;
use snowflake::ProcessUniqueId;
use std;

pub type EnvId = ProcessUniqueId;

pub type Sender<T> = mpsc::Sender<T>;
pub type Receiver<T> = mpsc::Receiver<T>;

/// Communication messages used to talk with the [VM](struct.VM.html) thread.
#[derive(Debug)]
pub enum RequestMessage<'a> {
    /// Requests scheduling a new environment with a given
    /// id and a program.
    ScheduleEnv(EnvId, Vec<u8>, Sender<ResponseMessage<'a>>),
    /// An internal message that schedules an execution of
    /// the next instruction in an identified environment on
    /// the next 'tick'
    RescheduleEnv(EnvId, Vec<u8>, Env<'a>, Sender<ResponseMessage<'a>>),
    /// Requests VM shutdown
    Shutdown,
}

/// Messages received from the [VM](struct.VM.html) thread.
#[derive(Debug)]
pub enum ResponseMessage<'a> {
    /// Notifies of successful environment termination with
    /// an id, stack and top of the stack pointer.
    EnvTerminated(EnvId, Vec<&'a [u8]>, usize),
    /// Notifies of abnormal environment termination with
    /// an id, error, stack and top of the stack pointer.
    EnvFailed(EnvId, Error, Vec<&'a [u8]>, usize),
}

pub type TrySendError<T> = std::sync::mpsc::TrySendError<T>;

use lmdb;

/// VM is a PumpkinScript scheduler and interpreter. This is the
/// most central part of this module.
///
/// # Example
///
/// ```no_run
/// let mut vm = VM::new(&env, &db); // lmdb comes from outside
///
/// let sender = vm.sender();
/// let handle = thread::spawn(move || {
///     vm.run();
/// });
/// let script = parse($script).unwrap();
/// let (callback, receiver) = mpsc::channel::<ResponseMessage>();
/// let _ = sender.send(RequestMessage::ScheduleEnv(EnvId::new(), script.clone(), callback));
/// match receiver.recv() {
///     Ok(ResponseMessage::EnvTerminated(_, stack, stack_size)) => {
///         let _ = sender.send(RequestMessage::Shutdown);
///         // success
///         // ...
///     }
///     Ok(ResponseMessage::EnvFailed(_, err, stack, stack_size)) => {
///         let _ = sender.send(RequestMessage::Shutdown);
///         // failure
///         // ...
///     }
///     Err(err) => {
///         panic!("recv error: {:?}", err);
///     }
/// }
/// ```
pub struct VM<'a> {
    inbox: Receiver<RequestMessage<'a>>,
    sender: Sender<RequestMessage<'a>>,
    loopback: Sender<RequestMessage<'a>>,
    db: &'a lmdb::Database<'a>,
    db_env: &'a lmdb::Environment,
    db_write_txn: Option<(EnvId, lmdb::WriteTransaction<'a>)>,
    db_read_txn: Option<(EnvId, lmdb::ReadTransaction<'a>)>,
}

unsafe impl<'a> Send for VM<'a> {}

type PassResult<'a> = Result<(Env<'a>, Option<Vec<u8>>), (Env<'a>, Error)>;

const TRUE: &'static [u8] = b"\x01\x01";
const FALSE: &'static [u8] = b"\x01\x00";

use lmdb::traits::LmdbResultExt;

impl<'a> VM<'a> {
    /// Creates an instance of VM with three communication channels:
    ///
    /// * Response sender
    /// * Internal sender
    /// * Request receiver
    pub fn new(db_env: &'a lmdb::Environment, db: &'a lmdb::Database<'a>) -> Self {
        let (sender, receiver) = mpsc::channel::<RequestMessage<'a>>();
        VM {
            inbox: receiver,
            sender: sender.clone(),
            loopback: sender.clone(),
            db_env: db_env,
            db: db,
            db_write_txn: None,
            db_read_txn: None,
        }
    }

    pub fn sender(&self) -> Sender<RequestMessage<'a>> {
        self.sender.clone()
    }

    /// Scheduler thread. It is supposed to be running in a separate thread
    ///
    /// The scheduler handles all incoming and internal messages. Once at least one
    /// program is scheduled (`ScheduleEnv`), it will create an [Env](struct.Env.html) for
    /// it and reschedule for execution (`RescheduleEnv`), at which time it will execute
    /// one instruction. This way it can execute multiple scripts at the same time.
    ///
    /// Once an environment execution has been terminated, a message will be sent,
    /// depending on the result (`EnvTerminated` or `EnvFailed`)
    pub fn run(&mut self) {
        loop {
            match self.inbox.recv() {
                Err(err) => panic!("error receiving: {:?}", err),
                Ok(RequestMessage::Shutdown) => break,
                Ok(RequestMessage::ScheduleEnv(pid, program, chan)) => {
                    let env = Env::new();
                    let _ = self.loopback
                        .send(RequestMessage::RescheduleEnv(pid, program, env, chan));
                }
                Ok(RequestMessage::RescheduleEnv(pid, mut program, env, chan)) => {
                    match self.pass(env, &mut program, pid.clone()) {
                        Err((env, err)) => {
                            let _ = chan.send(ResponseMessage::EnvFailed(pid,
                                                                         err,
                                                                         Vec::from(env.stack()),
                                                                         env.stack_size));
                        }
                        Ok((env, Some(program))) => {
                            let _ = self.loopback
                                .send(RequestMessage::RescheduleEnv(pid, program, env, chan));
                        }
                        Ok((env, None)) => {
                            let _ = chan.send(ResponseMessage::EnvTerminated(pid,
                                                                     Vec::from(env.stack()),
                                                                     env.stack_size));
                        }
                    };
                }
            }
        }
    }

    fn pass(&mut self, mut env: Env<'a>, program: &mut Vec<u8>, pid: EnvId) -> PassResult<'a> {
        let mut slice = env.alloc(program.len());
        for i in 0..program.len() {
            slice[i] = program[i];
        }
        if let nom::IResult::Done(_, data) = binparser::data(slice) {
            env.push(data);
            let rest = program.split_off(data.len());
            return Ok(match rest.len() {
                0 => (env, None),
                _ => (env, Some(rest)),
            });
        } else if let nom::IResult::Done(_, word) = binparser::word_or_internal_word(slice) {
            handle_words!(self,
                          env,
                          program,
                          word,
                          res,
                          pid,
                          [handle_drop,
                           handle_dup,
                           handle_swap,
                           handle_rot,
                           handle_over,
                           handle_depth,
                           handle_concat,
                           handle_eval,
                           // storage
                           handle_write,
                           handle_read,
                           handle_assoc,
                           handle_assocq,
                           handle_retr,
                           handle_commit],
                          {
                              let (env_, rest) = match res {
                                  (env_, Some(code_injection)) => {
                                      let mut vec = Vec::from(code_injection);
                                      let mut rest_0 = program.split_off(word.len());
                                      vec.append(&mut rest_0);
                                      (env_, vec)
                                  }
                                  (env_, None) => (env_, program.split_off(word.len())),
                              };
                              return Ok(match rest.len() {
                                  0 => (env_, None),
                                  _ => (env_, Some(rest)),
                              });
                          });
        } else {
            return Err((env, Error::DecodingError));
        }
    }

    #[inline]
    fn handle_dup(&mut self, mut env: Env<'a>, word: &'a [u8], _: EnvId) -> PassResult<'a> {
        if word == DUP {
            match env.pop() {
                None => return Err((env, Error::EmptyStack)),
                Some(v) => {
                    env.push(v);
                    env.push(v);
                    Ok((env, None))
                }
            }
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_swap(&mut self, mut env: Env<'a>, word: &'a [u8], _: EnvId) -> PassResult<'a> {
        if word == SWAP {
            let a = env.pop();
            let b = env.pop();

            if a.is_none() || b.is_none() {
                return Err((env, Error::EmptyStack));
            }

            env.push(a.unwrap());
            env.push(b.unwrap());

            Ok((env, None))
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_over(&mut self, mut env: Env<'a>, word: &'a [u8], _: EnvId) -> PassResult<'a> {
        if word == OVER {
            let a = env.pop();
            let b = env.pop();

            if a.is_none() || b.is_none() {
                return Err((env, Error::EmptyStack));
            }

            env.push(b.unwrap());
            env.push(a.unwrap());
            env.push(b.unwrap());

            Ok((env, None))
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_rot(&mut self, mut env: Env<'a>, word: &'a [u8], _: EnvId) -> PassResult<'a> {
        if word == ROT {
            let a = env.pop();
            let b = env.pop();
            let c = env.pop();

            if a.is_none() || b.is_none() || c.is_none() {
                return Err((env, Error::EmptyStack));
            }

            env.push(b.unwrap());
            env.push(a.unwrap());
            env.push(c.unwrap());

            Ok((env, None))
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_drop(&mut self, mut env: Env<'a>, word: &'a [u8], _: EnvId) -> PassResult<'a> {
        if word == DROP {
            match env.pop() {
                None => return Err((env, Error::EmptyStack)),
                _ => Ok((env, None)),
            }
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_depth(&mut self, mut env: Env<'a>, word: &'a [u8], _: EnvId) -> PassResult<'a> {
        if word == DEPTH {
            let bytes = BigUint::from(env.stack_size).to_bytes_le();
            let offset = offset_by_size(bytes.len());
            let slice = env.alloc(bytes.len() + offset);
            write_size_into_slice!(bytes.len(), slice);
            let mut i = offset;
            for byte in bytes {
                slice[i] = byte;
                i += 1;
            }
            env.push(slice);
            Ok((env, None))
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_concat(&mut self, mut env: Env<'a>, word: &'a [u8], _: EnvId) -> PassResult<'a> {
        if word == CONCAT {
            let a = env.pop();
            let b = env.pop();

            if a.is_none() || b.is_none() {
                return Err((env, Error::EmptyStack));
            }

            let a1 = a.unwrap();
            let b1 = b.unwrap();

            let (a1_, size_a) = data!(a1);
            let (b1_, size_b) = data!(b1);

            let size = a1_.len() + b1_.len();

            let mut slice = env.alloc(size + offset_by_size(size_a + size_b));
            let mut offset = write_size_into_slice!(size, slice);

            for byte in b1_ {
                slice[offset] = *byte;
                offset += 1
            }

            for byte in a1_ {
                slice[offset] = *byte;
                offset += 1
            }

            env.push(slice);

            Ok((env, None))
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_eval(&mut self, mut env: Env<'a>, word: &'a [u8], _: EnvId) -> PassResult<'a> {
        if word == EVAL {
            match env.pop() {
                None => return Err((env, Error::EmptyStack)),
                Some(v) => {
                    let (code, _) = data!(v);
                    Ok((env, Some(Vec::from(code))))
                }
            }
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_write(&mut self, mut env: Env<'a>, word: &'a [u8], pid: EnvId) -> PassResult<'a> {
        match word {
            WRITE => {
                match env.pop() {
                    None => return Err((env, Error::EmptyStack)),
                    Some(v) => {
                        validate_lockout!(env, self.db_write_txn, pid);
                        let (code, _) = data!(v);
                        let mut vec = Vec::from(code);
                        vec.extend_from_slice(WRITE_END); // transaction end marker
                        // prepare transaction
                        match lmdb::WriteTransaction::new(self.db_env) {
                            Err(e) => Err((env, Error::DatabaseError(e))),
                            Ok(txn) => {
                                self.db_write_txn = Some((pid, txn));
                                Ok((env, Some(vec)))
                            }
                        }
                    }
                }
            }
            WRITE_END => {
                validate_lockout!(env, self.db_write_txn, pid);
                self.db_write_txn = None;
                Ok((env, None))
            }
            _ => Err((env, Error::UnknownWord)),
        }
    }

    #[inline]
    fn handle_read(&mut self, mut env: Env<'a>, word: &'a [u8], pid: EnvId) -> PassResult<'a> {
        match word {
            READ => {
                match env.pop() {
                    None => return Err((env, Error::EmptyStack)),
                    Some(v) => {
                        validate_lockout!(env, self.db_read_txn, pid);
                        validate_lockout!(env, self.db_write_txn, pid);
                        let (code, _) = data!(v);
                        let mut vec = Vec::from(code);
                        vec.extend_from_slice(READ_END); // transaction end marker
                        // prepare transaction
                        match lmdb::ReadTransaction::new(self.db_env) {
                            Err(e) => Err((env, Error::DatabaseError(e))),
                            Ok(txn) => {
                                self.db_read_txn = Some((pid, txn));
                                Ok((env, Some(vec)))
                            }
                        }
                    }
                }
            }
            READ_END => {
                validate_lockout!(env, self.db_read_txn, pid);
                validate_lockout!(env, self.db_write_txn, pid);
                self.db_read_txn = None;
                Ok((env, None))
            }
            _ => Err((env, Error::UnknownWord)),
        }
    }

    #[inline]
    fn handle_assoc(&mut self, mut env: Env<'a>, word: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if word == ASSOC {
            validate_lockout!(env, self.db_write_txn, pid);
            if let Some((_, ref txn)) = self.db_write_txn {
                let value = env.pop();
                let key = env.pop();

                if value.is_none() || key.is_none() {
                    return Err((env, Error::EmptyStack));
                }

                let value1 = value.unwrap();
                let key1 = key.unwrap();

                let mut access = txn.access();

                match access.put(self.db, key1, value1, lmdb::put::NOOVERWRITE) {
                    Ok(_) => Ok((env, None)),
                    Err(lmdb::Error::ValRejected(_)) => Err((env, Error::DuplicateKey)),
                    Err(err) => Err((env, Error::DatabaseError(err))),
                }
            } else {
                Err((env, Error::NoTransaction))
            }
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_commit(&mut self, env: Env<'a>, word: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if word == COMMIT {
            validate_lockout!(env, self.db_write_txn, pid);
            if let Some((_, txn)) = mem::replace(&mut self.db_write_txn, None) {
                let _ = txn.commit();
                Ok((env, None))
            } else {
                Err((env, Error::NoTransaction))
            }
        } else {
            Err((env, Error::UnknownWord))
        }
    }


    #[inline]
    fn handle_retr(&mut self, mut env: Env<'a>, word: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if word == RETR {
            validate_lockout!(env, self.db_write_txn, pid);
            let key = env.pop();
            if key.is_none() {
                return Err((env, Error::EmptyStack));
            }
            let key1 = key.unwrap();
            if let Some((_, ref txn)) = self.db_write_txn {
                let access = txn.access();

                match access.get::<[u8], [u8]>(self.db, key1).to_opt() {
                    Ok(Some(val)) => {
                        let slice = env.alloc(val.len());
                        for i in 0..val.len() {
                            slice[i] = val[i];
                        }
                        env.push(slice);
                        Ok((env, None))
                    }
                    Ok(None) => Err((env, Error::UnknownKey)),
                    Err(err) => Err((env, Error::DatabaseError(err))),
                }
            } else if let Some((_, ref txn)) = self.db_read_txn {
                let access = txn.access();

                match access.get::<[u8], [u8]>(self.db, key1).to_opt() {
                    Ok(Some(val)) => {
                        let slice = env.alloc(val.len());
                        for i in 0..val.len() {
                            slice[i] = val[i];
                        }
                        env.push(slice);
                        Ok((env, None))
                    }
                    Ok(None) => Err((env, Error::UnknownKey)),
                    Err(err) => Err((env, Error::DatabaseError(err))),
                }
            } else {
                Err((env, Error::NoTransaction))
            }
        } else {
            Err((env, Error::UnknownWord))
        }
    }

    #[inline]
    fn handle_assocq(&mut self, mut env: Env<'a>, word: &'a [u8], pid: EnvId) -> PassResult<'a> {
        if word == ASSOCQ {
            validate_lockout!(env, self.db_write_txn, pid);
            let key = env.pop();
            if key.is_none() {
                return Err((env, Error::EmptyStack));
            }
            let key1 = key.unwrap();
            if let Some((_, ref txn)) = self.db_write_txn {
                let access = txn.access();

                match access.get::<[u8], [u8]>(self.db, key1).to_opt() {
                    Ok(Some(_)) => {
                        env.push(TRUE);
                        Ok((env, None))
                    }
                    Ok(None) => {
                        env.push(FALSE);
                        Ok((env, None))
                    }
                    Err(err) => Err((env, Error::DatabaseError(err))),
                }
            } else if let Some((_, ref txn)) = self.db_read_txn {
                let access = txn.access();

                match access.get::<[u8], [u8]>(self.db, key1).to_opt() {
                    Ok(Some(_)) => {
                        env.push(TRUE);
                        Ok((env, None))
                    }
                    Ok(None) => {
                        env.push(FALSE);
                        Ok((env, None))
                    }
                    Err(err) => Err((env, Error::DatabaseError(err))),
                }
            } else {
                Err((env, Error::NoTransaction))
            }
        } else {
            Err((env, Error::UnknownWord))
        }
    }
}


#[cfg(test)]
#[allow(unused_variables, unused_must_use, unused_mut)]
mod tests {

    use script::{Env, VM, Error, RequestMessage, ResponseMessage, EnvId, parse};
    use std::sync::mpsc;
    use std::fs;
    use tempdir::TempDir;
    use lmdb;
    use crossbeam;

    const _EMPTY: &'static [u8] = b"";

    #[test]
    fn env_stack_growth() {
        let mut env = Env::new();
        let target = env.stack.len() * 100;
        for i in 1..target {
            env.push(_EMPTY);
        }
        assert!(env.stack.len() >= target);
    }

    #[test]
    fn env_heap_growth() {
        let mut env = Env::new();
        let sz = env.heap_size;
        for i in 1..100 {
            env.alloc(sz);
        }
        assert!(env.heap_size >= sz * 100);
    }

    macro_rules! eval {
        ($script: expr, $env: ident, $expr: expr) => {
           eval!($script, $env, _result, $expr);
        };
        ($script: expr, $env: ident, $result: pat, $expr: expr) => {
          {
            let dir = TempDir::new("pumpkindb").unwrap();
            let path = dir.path().to_str().unwrap();
            fs::create_dir_all(path).expect("can't create directory");
            let env = unsafe {
                lmdb::EnvBuilder::new()
                    .expect("can't create env builder")
                    .open(path, lmdb::open::Flags::empty(), 0o600)
                    .expect("can't open env")
            };

            let db = lmdb::Database::open(&env,
                                 None,
                                 &lmdb::DatabaseOptions::new(lmdb::db::CREATE))
                                 .expect("can't open database");
            crossbeam::scope(|scope| {
                let mut vm = VM::new(&env, &db);
                let sender = vm.sender();
                let handle = scope.spawn(move || {
                    vm.run();
                });
                let script = parse($script).unwrap();
                let (callback, receiver) = mpsc::channel::<ResponseMessage>();
                let _ = sender.send(RequestMessage::ScheduleEnv(EnvId::new(),
                                    script.clone(), callback));
                match receiver.recv() {
                   Ok(ResponseMessage::EnvTerminated(_, stack, stack_size)) => {
                      let _ = sender.send(RequestMessage::Shutdown);
                      let $result = Ok::<(), Error>(());
                      let mut $env = Env::new_with_stack(stack, stack_size);
                      $expr;
                   }
                   Ok(ResponseMessage::EnvFailed(_, err, stack, stack_size)) => {
                      let _ = sender.send(RequestMessage::Shutdown);
                      let $result = Err::<(), Error>(err);
                      let mut $env = Env::new_with_stack(stack, stack_size);
                      $expr;
                   }
                   Err(err) => {
                      panic!("recv error: {:?}", err);
                   }
                }
                let _ = handle.join();
          });
        };
      }
    }

    #[test]
    fn drop() {
        eval!("0x010203 DROP", env, {
            assert_eq!(env.pop(), None);
        });

        eval!("DROP", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

    }

    #[test]
    fn dup() {
        eval!("0x010203 DUP", env, {
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x010203").unwrap());
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x010203").unwrap());
            assert_eq!(env.pop(), None);
        });

        eval!("DUP", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });
    }

    #[test]
    fn swap() {
        eval!("0x010203 0x030201 SWAP", env, {
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x010203").unwrap());
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x030201").unwrap());
            assert_eq!(env.pop(), None);
        });

        eval!("SWAP", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

        eval!("0x10 SWAP", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

    }


    #[test]
    fn rot() {
        eval!("0x010203 0x030201 0x00 ROT", env, {
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x010203").unwrap());
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x00").unwrap());
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x030201").unwrap());
            assert_eq!(env.pop(), None);
        });

        eval!("0x010203 0x030201 ROT", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

        eval!("0x010203 ROT", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

        eval!("ROT", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

    }

    #[test]
    fn over() {
        eval!("0x010203 0x00 OVER", env, {
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x010203").unwrap());
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x00").unwrap());
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x010203").unwrap());
        });

        eval!("0x00 OVER", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

        eval!("OVER", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

    }

    #[test]
    fn depth() {
        eval!("0x010203 0x00 \"Hello\" DEPTH", env, {
            assert_eq!(Vec::from(env.pop().unwrap()), parse("3").unwrap());
        });
    }

    #[test]
    fn concat() {
        eval!("0x10 0x20 CONCAT", env, {
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x1020").unwrap());
            assert_eq!(env.pop(), None);
        });

        eval!("0x20 CONCAT", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });

        eval!("CONCAT", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });
    }

    #[test]
    fn eval() {
        eval!("[0x01 DUP [DUP] EVAL] EVAL DROP", env, {
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x01").unwrap());
            assert_eq!(Vec::from(env.pop().unwrap()), parse("0x01").unwrap());
            assert_eq!(env.pop(), None);
        });

        eval!("EVAL", env, result, {
            assert!(matches!(result.err(), Some(Error::EmptyStack)));
        });
    }

    #[test]
    fn invalid_eval() {
        eval!("0x10 EVAL", env, result, {
            assert!(result.is_err());
            assert!(matches!(result.err(), Some(Error::DecodingError)));
        });
    }

    #[test]
    fn write() {
        eval!("[\"Hello\" \"world\" ASSOC COMMIT] WRITE [\"Hello\" RETR] READ",
              env,
              result,
              {
                  assert!(!result.is_err());
                  assert_eq!(Vec::from(env.pop().unwrap()), parse("\"world\"").unwrap());
              });

        // overwrite
        eval!("[\"Hello\" \"world\" ASSOC \"Hello\" \"world\" ASSOC COMMIT] WRITE",
              env,
              result,
              {
                  assert!(result.is_err());
              });

        // missing key
        eval!("[\"Hello\" \"world\" ASSOC COMMIT] WRITE [\"world\" RETR] READ",
              env,
              result,
              {
                  assert!(result.is_err());
              });

    }

    #[test]
    fn assocq() {
        eval!("[\"Hello\" \"world\" ASSOC COMMIT] WRITE [\"Hello\" ASSOC? \"Bye\" ASSOC?] READ",
              env,
              result,
              {
                  assert!(!result.is_err());
                  assert_eq!(Vec::from(env.pop().unwrap()), parse("0x00").unwrap());
                  assert_eq!(Vec::from(env.pop().unwrap()), parse("0x01").unwrap());
              });
    }

    #[test]
    fn commit() {
        eval!("[\"Hey\" \"everybody\" ASSOC] WRITE [\"Hey\" RETR] READ",
              env,
              result,
              {
                  assert!(result.is_err());
              });
    }

}
