// Copyright 2015 The Rust Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Transactional socket configuration.
//!
//! [`SocketConfigTxn`] applies a small set of configuration changes
//! (nonblocking mode and the read/write timeouts) to a *borrowed* socket as
//! an all-or-nothing transaction:
//!
//! * Before anything is changed the current value of every field in the plan
//!   is queried and saved. If the current state cannot be queried -- because
//!   the platform lacks the capability or the query itself fails -- the
//!   transaction is refused *before* any field is modified.
//! * The changes are then applied in a fixed order: nonblocking mode, read
//!   timeout, write timeout. If applying a change fails, only the fields that
//!   were already changed by this transaction are rolled back to their saved
//!   values; fields that were never touched are left alone.
//! * The socket is only ever borrowed, never owned, so it is never closed by
//!   the transaction.
//!
//! Only Unix platforms (using `fcntl`/`getsockopt`) are supported; on any
//! other platform [`SocketConfigTxn::commit`] explicitly refuses with
//! [`ConfigTxnError::Unsupported`].

use std::error::Error;
use std::fmt;
use std::io;
use std::time::Duration;

use crate::Socket;

#[cfg(all(unix, not(target_os = "vita")))]
use std::os::fd::AsRawFd;

/// Which socket timeout a [`ConfigBackend`] operation applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutKind {
    /// The read (`SO_RCVTIMEO`) timeout.
    Read,
    /// The write (`SO_SNDTIMEO`) timeout.
    Write,
}

impl TimeoutKind {
    #[cfg(all(unix, not(target_os = "vita")))]
    fn name(self) -> &'static str {
        match self {
            TimeoutKind::Read => "read_timeout",
            TimeoutKind::Write => "write_timeout",
        }
    }
}

/// Error returned by [`SocketConfigTxn::commit`].
#[derive(Debug)]
pub enum ConfigTxnError {
    /// The transaction was refused because the current socket state cannot be
    /// queried on this platform (or by this backend). Nothing was changed.
    Unsupported,
    /// The transaction was refused because saving the current value of a
    /// field failed. Nothing was changed.
    Query {
        /// Name of the field whose current value could not be queried.
        field: &'static str,
        /// The underlying query error.
        source: io::Error,
    },
    /// Applying a field failed. Every field already changed by this
    /// transaction was rolled back to its saved value; all other fields were
    /// left untouched.
    Apply {
        /// Name of the field that could not be applied.
        field: &'static str,
        /// The underlying setter error.
        source: io::Error,
    },
    /// Applying a field failed *and* rolling back an already-changed field
    /// failed too. The socket may be left in a partially configured state.
    Rollback {
        /// Name of the field that could not be applied.
        field: &'static str,
        /// The original setter error.
        apply: io::Error,
        /// The error encountered while restoring a changed field.
        rollback: io::Error,
    },
}

impl fmt::Display for ConfigTxnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigTxnError::Unsupported => f.write_str(
                "socket configuration transaction refused: \
                 querying the current socket state is not supported on this platform",
            ),
            ConfigTxnError::Query { field, source } => write!(
                f,
                "socket configuration transaction refused: \
                 failed to query the current `{field}` value: {source}"
            ),
            ConfigTxnError::Apply { field, source } => write!(
                f,
                "socket configuration transaction failed to set `{field}`: {source}; \
                 fields already changed were rolled back"
            ),
            ConfigTxnError::Rollback {
                field,
                apply,
                rollback,
            } => write!(
                f,
                "socket configuration transaction failed to set `{field}`: {apply}; \
                 rolling back an already-changed field also failed: {rollback}"
            ),
        }
    }
}

impl Error for ConfigTxnError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ConfigTxnError::Unsupported => None,
            ConfigTxnError::Query { source, .. } => Some(source),
            ConfigTxnError::Apply { source, .. } => Some(source),
            ConfigTxnError::Rollback { rollback, .. } => Some(rollback),
        }
    }
}

/// Backend used by [`SocketConfigTxn`] to query and apply socket settings.
///
/// The crate provides `UnixBackend` on supported Unix platforms; other
/// platforms get a backend that explicitly refuses every query. The trait is
/// public so tests can drive the transaction with a fake syscall fixture.
///
/// Implementations must only ever *borrow* the underlying socket and must
/// never close it.
pub trait ConfigBackend {
    /// Return the current nonblocking mode, or
    /// [`ConfigTxnError::Unsupported`] if it cannot be queried.
    fn query_nonblocking(&mut self) -> Result<bool, ConfigTxnError>;
    /// Set the nonblocking mode.
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()>;
    /// Return the current `kind` timeout, or
    /// [`ConfigTxnError::Unsupported`] if it cannot be queried.
    fn query_timeout(&mut self, kind: TimeoutKind) -> Result<Option<Duration>, ConfigTxnError>;
    /// Set the `kind` timeout.
    fn set_timeout(&mut self, kind: TimeoutKind, timeout: Option<Duration>) -> io::Result<()>;
}

impl<'b, B: ConfigBackend + ?Sized> ConfigBackend for &'b mut B {
    fn query_nonblocking(&mut self) -> Result<bool, ConfigTxnError> {
        (**self).query_nonblocking()
    }
    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        (**self).set_nonblocking(nonblocking)
    }
    fn query_timeout(&mut self, kind: TimeoutKind) -> Result<Option<Duration>, ConfigTxnError> {
        (**self).query_timeout(kind)
    }
    fn set_timeout(&mut self, kind: TimeoutKind, timeout: Option<Duration>) -> io::Result<()> {
        (**self).set_timeout(kind, timeout)
    }
}

/// A field changed by a transaction, together with its saved original value.
#[derive(Debug)]
enum AppliedField {
    Nonblocking(bool),
    Timeout(TimeoutKind, Option<Duration>),
}

/// A transaction that applies nonblocking mode and read/write timeout
/// changes to a borrowed socket, rolling back on failure.
///
/// See the [module documentation](self) for the exact semantics. The socket
/// is only borrowed for the lifetime of the transaction and is never closed.
///
/// # Examples
///
/// ```no_run
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// use std::time::Duration;
/// use socket2::{Domain, Socket, SocketConfigTxn, Type};
///
/// let socket = Socket::new(Domain::IPV4, Type::STREAM, None)?;
/// let mut txn = SocketConfigTxn::new(&socket);
/// txn.set_nonblocking(true)
///     .set_read_timeout(Some(Duration::from_secs(1)));
/// txn.commit()?;
/// // `socket` is still open and usable here.
/// # Ok(()) }
/// ```
#[derive(Debug)]
pub struct SocketConfigTxn<B> {
    backend: B,
    nonblocking: Option<bool>,
    read_timeout: Option<Option<Duration>>,
    write_timeout: Option<Option<Duration>>,
}

#[cfg(all(unix, not(target_os = "vita")))]
impl<'s> SocketConfigTxn<UnixBackend<'s>> {
    /// Create a transaction configuring the borrowed `socket`.
    ///
    /// The socket is only borrowed: it is never closed by the transaction.
    pub fn new(socket: &'s Socket) -> SocketConfigTxn<UnixBackend<'s>> {
        SocketConfigTxn::with_backend(UnixBackend { socket })
    }
}

#[cfg(not(all(unix, not(target_os = "vita"))))]
impl SocketConfigTxn<UnsupportedBackend> {
    /// Create a transaction configuring the borrowed `socket`.
    ///
    /// This platform cannot query the current socket state, so
    /// [`commit`](SocketConfigTxn::commit) will explicitly refuse with
    /// [`ConfigTxnError::Unsupported`] before changing anything. The socket
    /// is only borrowed and is never closed.
    pub fn new(socket: &Socket) -> SocketConfigTxn<UnsupportedBackend> {
        let _ = socket;
        SocketConfigTxn::with_backend(UnsupportedBackend { _private: () })
    }
}

impl<B: ConfigBackend> SocketConfigTxn<B> {
    /// Create a transaction using a custom `backend`.
    ///
    /// This exists so tests can drive the transaction with a fake syscall
    /// fixture instead of a real socket.
    pub fn with_backend(backend: B) -> SocketConfigTxn<B> {
        SocketConfigTxn {
            backend,
            nonblocking: None,
            read_timeout: None,
            write_timeout: None,
        }
    }

    /// Plan a change of the nonblocking mode.
    pub fn set_nonblocking(&mut self, nonblocking: bool) -> &mut Self {
        self.nonblocking = Some(nonblocking);
        self
    }

    /// Plan a change of the read (`SO_RCVTIMEO`) timeout.
    pub fn set_read_timeout(&mut self, timeout: Option<Duration>) -> &mut Self {
        self.read_timeout = Some(timeout);
        self
    }

    /// Plan a change of the write (`SO_SNDTIMEO`) timeout.
    pub fn set_write_timeout(&mut self, timeout: Option<Duration>) -> &mut Self {
        self.write_timeout = Some(timeout);
        self
    }

    /// Apply the planned changes as a transaction.
    ///
    /// First the current value of every planned field is queried and saved;
    /// if any query is unsupported or fails the transaction is refused
    /// before anything is changed. The changes are then applied in a fixed
    /// order (nonblocking, read timeout, write timeout). If applying a field
    /// fails, only the fields already changed by this call are rolled back
    /// to their saved values.
    pub fn commit(mut self) -> Result<(), ConfigTxnError> {
        // Phase 1: save the original state of every planned field. Refuse
        // before touching the socket if anything cannot be queried.
        let saved_nonblocking = match self.nonblocking {
            Some(_) => Some(self.backend.query_nonblocking()?),
            None => None,
        };
        let saved_read_timeout = match self.read_timeout {
            Some(_) => Some(self.backend.query_timeout(TimeoutKind::Read)?),
            None => None,
        };
        let saved_write_timeout = match self.write_timeout {
            Some(_) => Some(self.backend.query_timeout(TimeoutKind::Write)?),
            None => None,
        };

        // Phase 2: apply in a fixed order, remembering only the fields this
        // transaction actually changed so a failure rolls back exactly those.
        let mut applied: Vec<AppliedField> = Vec::new();

        if let (Some(target), Some(saved)) = (self.nonblocking, saved_nonblocking) {
            if let Err(source) = self.backend.set_nonblocking(target) {
                return Err(self.rollback("nonblocking", source, &applied));
            }
            applied.push(AppliedField::Nonblocking(saved));
        }
        if let (Some(target), Some(saved)) = (self.read_timeout, saved_read_timeout) {
            if let Err(source) = self.backend.set_timeout(TimeoutKind::Read, target) {
                return Err(self.rollback("read_timeout", source, &applied));
            }
            applied.push(AppliedField::Timeout(TimeoutKind::Read, saved));
        }
        if let (Some(target), Some(saved)) = (self.write_timeout, saved_write_timeout) {
            if let Err(source) = self.backend.set_timeout(TimeoutKind::Write, target) {
                return Err(self.rollback("write_timeout", source, &applied));
            }
            applied.push(AppliedField::Timeout(TimeoutKind::Write, saved));
        }
        Ok(())
    }

    /// Roll back the fields this transaction already changed, in reverse
    /// order of application, and build the error for the failed field.
    fn rollback(
        &mut self,
        failed: &'static str,
        apply: io::Error,
        applied: &[AppliedField],
    ) -> ConfigTxnError {
        for field in applied.iter().rev() {
            let result = match field {
                AppliedField::Nonblocking(saved) => self.backend.set_nonblocking(*saved),
                AppliedField::Timeout(kind, saved) => self.backend.set_timeout(*kind, *saved),
            };
            if let Err(rollback) = result {
                return ConfigTxnError::Rollback {
                    field: failed,
                    apply,
                    rollback,
                };
            }
        }
        ConfigTxnError::Apply {
            field: failed,
            source: apply,
        }
    }
}

/// [`ConfigBackend`] for Unix platforms, querying state with `fcntl` and
/// `getsockopt` and applying it through [`Socket`].
///
/// The socket is only ever borrowed; this backend never closes it.
#[cfg(all(unix, not(target_os = "vita")))]
#[derive(Debug)]
pub struct UnixBackend<'s> {
    socket: &'s Socket,
}

#[cfg(all(unix, not(target_os = "vita")))]
impl ConfigBackend for UnixBackend<'_> {
    fn query_nonblocking(&mut self) -> Result<bool, ConfigTxnError> {
        // NOTE: `Socket::nonblocking` is feature-gated, so query the file
        // status flags directly here.
        let flags = unsafe { libc::fcntl(self.socket.as_raw_fd(), libc::F_GETFL) };
        if flags == -1 {
            return Err(ConfigTxnError::Query {
                field: "nonblocking",
                source: io::Error::last_os_error(),
            });
        }
        Ok((flags & libc::O_NONBLOCK) != 0)
    }

    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        self.socket.set_nonblocking(nonblocking)
    }

    fn query_timeout(&mut self, kind: TimeoutKind) -> Result<Option<Duration>, ConfigTxnError> {
        let result = match kind {
            TimeoutKind::Read => self.socket.read_timeout(),
            TimeoutKind::Write => self.socket.write_timeout(),
        };
        result.map_err(|source| ConfigTxnError::Query {
            field: kind.name(),
            source,
        })
    }

    fn set_timeout(&mut self, kind: TimeoutKind, timeout: Option<Duration>) -> io::Result<()> {
        match kind {
            TimeoutKind::Read => self.socket.set_read_timeout(timeout),
            TimeoutKind::Write => self.socket.set_write_timeout(timeout),
        }
    }
}

/// [`ConfigBackend`] for platforms where the current socket state cannot be
/// queried. Every query explicitly refuses with
/// [`ConfigTxnError::Unsupported`], so a transaction using it never changes
/// the socket.
#[cfg(not(all(unix, not(target_os = "vita"))))]
#[derive(Debug)]
pub struct UnsupportedBackend {
    _private: (),
}

#[cfg(not(all(unix, not(target_os = "vita"))))]
impl ConfigBackend for UnsupportedBackend {
    fn query_nonblocking(&mut self) -> Result<bool, ConfigTxnError> {
        Err(ConfigTxnError::Unsupported)
    }

    fn set_nonblocking(&mut self, _nonblocking: bool) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "socket configuration transactions are not supported on this platform",
        ))
    }

    fn query_timeout(&mut self, _kind: TimeoutKind) -> Result<Option<Duration>, ConfigTxnError> {
        Err(ConfigTxnError::Unsupported)
    }

    fn set_timeout(&mut self, _kind: TimeoutKind, _timeout: Option<Duration>) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "socket configuration transactions are not supported on this platform",
        ))
    }
}
