// Copyright 2026 The Rust Project Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Transactional socket configuration.
//!
//! This module provides [`SocketConfig`], a builder that describes a set of
//! socket options to be applied together, and the [`Socket::configure`] method
//! which applies them as a transaction: the original state of every field is
//! queried first, and if a later setter fails only the fields that were
//! already changed are rolled back to their original values.
//!
//! The transaction is only supported on platforms where the original state of
//! every configured field can be queried. On platforms where a field cannot be
//! queried (e.g. nonblocking mode on Windows) the transaction is not
//! available.

use std::io;
use std::time::Duration;

use crate::sys;
use crate::sys::c_int;

/// A set of socket options to be applied transactionally via
/// [`Socket::configure`].
///
/// `SocketConfig` is a builder: start with [`SocketConfig::new`] and call the
/// `with_*` methods to select which options to change. Options that are not
/// set are left untouched by the transaction.
///
/// [`Socket::configure`]: crate::Socket::configure
///
/// # Examples
///
/// ```
/// use std::time::Duration;
/// use socket2::SocketConfig;
///
/// let _config = SocketConfig::new()
///     .with_nonblocking(true)
///     .with_read_timeout(Some(Duration::from_secs(1)));
/// ```
#[derive(Debug, Clone, Default)]
pub struct SocketConfig {
    nonblocking: Option<bool>,
    read_timeout: Option<Option<Duration>>,
    write_timeout: Option<Option<Duration>>,
}

impl SocketConfig {
    /// Creates an empty configuration that changes no options.
    pub fn new() -> SocketConfig {
        SocketConfig::default()
    }

    /// Sets the nonblocking mode of the socket.
    ///
    /// See [`Socket::set_nonblocking`].
    ///
    /// [`Socket::set_nonblocking`]: crate::Socket::set_nonblocking
    pub fn with_nonblocking(mut self, nonblocking: bool) -> Self {
        self.nonblocking = Some(nonblocking);
        self
    }

    /// Sets the read timeout of the socket.
    ///
    /// See [`Socket::set_read_timeout`].
    ///
    /// [`Socket::set_read_timeout`]: crate::Socket::set_read_timeout
    pub fn with_read_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.read_timeout = Some(timeout);
        self
    }

    /// Sets the write timeout of the socket.
    ///
    /// See [`Socket::set_write_timeout`].
    ///
    /// [`Socket::set_write_timeout`]: crate::Socket::set_write_timeout
    pub fn with_write_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.write_timeout = Some(timeout);
        self
    }
}

/// The syscall operations needed to apply a [`SocketConfig`] transactionally.
///
/// This trait abstracts over the real platform syscalls (see [`RealOps`]) so
/// that the transaction logic can be tested with a fake syscall fixture.
// Used on Unix by `Socket::configure`; on other platforms only the tests use
// it, so allow dead code there.
#[allow(dead_code)]
pub(crate) trait ConfigOps {
    /// Returns the current nonblocking mode of the socket.
    fn nonblocking(&self, fd: sys::RawSocket) -> io::Result<bool>;

    /// Sets the nonblocking mode of the socket.
    fn set_nonblocking(&self, fd: sys::RawSocket, nonblocking: bool) -> io::Result<()>;

    /// Returns the current value of the given timeout option.
    fn timeout(&self, fd: sys::RawSocket, opt: c_int) -> io::Result<Option<Duration>>;

    /// Sets the value of the given timeout option.
    fn set_timeout(
        &self,
        fd: sys::RawSocket,
        opt: c_int,
        timeout: Option<Duration>,
    ) -> io::Result<()>;
}

/// The real syscall implementation of [`ConfigOps`] for Unix platforms.
///
/// This is the only supported platform for the transaction because it is the
/// only one where the original nonblocking mode can be queried, which is
/// required to be able to roll back a failed transaction.
#[cfg(unix)]
pub(crate) struct RealOps;

#[cfg(unix)]
impl ConfigOps for RealOps {
    fn nonblocking(&self, fd: sys::RawSocket) -> io::Result<bool> {
        sys::nonblocking(fd)
    }

    fn set_nonblocking(&self, fd: sys::RawSocket, nonblocking: bool) -> io::Result<()> {
        sys::set_nonblocking(fd, nonblocking)
    }

    fn timeout(&self, fd: sys::RawSocket, opt: c_int) -> io::Result<Option<Duration>> {
        sys::timeout_opt(fd, sys::SOL_SOCKET, opt)
    }

    fn set_timeout(
        &self,
        fd: sys::RawSocket,
        opt: c_int,
        timeout: Option<Duration>,
    ) -> io::Result<()> {
        sys::set_timeout_opt(fd, sys::SOL_SOCKET, opt, timeout)
    }
}

/// Applies `config` to `fd` transactionally using the given syscall
/// implementation.
///
/// The original state of every configured field is queried before any setter
/// is called. If a setter fails, only the fields that were already changed are
/// rolled back to their original values; fields that were not changed are left
/// untouched. The borrowed socket is never closed.
///
/// If querying the original state of any configured field fails, the
/// transaction is refused and no setter is called.
// Used on Unix by `Socket::configure`; on other platforms only the tests use
// it, so allow dead code there.
#[allow(dead_code)]
pub(crate) fn configure_with<O: ConfigOps>(
    fd: sys::RawSocket,
    config: &SocketConfig,
    ops: &O,
) -> io::Result<()> {
    // 1. Query the original state of every field we are about to change. If
    //    any query fails we refuse the transaction without touching the
    //    socket, because we would not be able to roll back.
    let mut original_nonblocking = None;
    let mut original_read_timeout = None;
    let mut original_write_timeout = None;

    if config.nonblocking.is_some() {
        original_nonblocking = Some(ops.nonblocking(fd)?);
    }
    if config.read_timeout.is_some() {
        original_read_timeout = Some(ops.timeout(fd, sys::SO_RCVTIMEO)?);
    }
    if config.write_timeout.is_some() {
        original_write_timeout = Some(ops.timeout(fd, sys::SO_SNDTIMEO)?);
    }

    // 2. Apply the setters, tracking which fields were actually changed.
    let mut changed_nonblocking = false;
    let mut changed_read_timeout = false;
    let mut changed_write_timeout = false;

    let result = (|| {
        if let Some(nonblocking) = config.nonblocking {
            ops.set_nonblocking(fd, nonblocking)?;
            changed_nonblocking = true;
        }
        if let Some(timeout) = config.read_timeout {
            ops.set_timeout(fd, sys::SO_RCVTIMEO, timeout)?;
            changed_read_timeout = true;
        }
        if let Some(timeout) = config.write_timeout {
            ops.set_timeout(fd, sys::SO_SNDTIMEO, timeout)?;
            changed_write_timeout = true;
        }
        Ok(())
    })();

    // 3. On failure, roll back only the fields that were changed in step 2.
    if let Err(error) = result {
        if changed_nonblocking {
            if let Some(original) = original_nonblocking {
                // Best-effort rollback: ignore errors so the original error is
                // returned to the caller.
                let _ = ops.set_nonblocking(fd, original);
            }
        }
        if changed_read_timeout {
            if let Some(original) = original_read_timeout {
                let _ = ops.set_timeout(fd, sys::SO_RCVTIMEO, original);
            }
        }
        if changed_write_timeout {
            if let Some(original) = original_write_timeout {
                let _ = ops.set_timeout(fd, sys::SO_SNDTIMEO, original);
            }
        }
        return Err(error);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A recorded syscall operation.
    #[derive(Debug, Clone, PartialEq)]
    enum Op {
        GetNonblocking,
        SetNonblocking(bool),
        GetTimeout(c_int),
        SetTimeout(c_int, Option<Duration>),
    }

    /// A fake syscall implementation that records every operation and can be
    /// programmed to fail the nth setter call.
    ///
    /// The fake uses a dummy file descriptor and never closes it: the
    /// [`ConfigOps`] trait has no close operation, so the transaction can
    /// never close the borrowed socket.
    #[derive(Debug)]
    struct FakeSyscall {
        ops: Mutex<Vec<Op>>,
        nonblocking: Mutex<bool>,
        read_timeout: Mutex<Option<Duration>>,
        write_timeout: Mutex<Option<Duration>>,
        /// Fail the nth setter call (1-indexed). `0` means never fail.
        fail_setter_nth: Mutex<usize>,
        /// Number of setter calls made so far.
        setter_count: Mutex<usize>,
        /// Set to `true` if the socket is closed. The transaction never
        /// closes it, so this always stays `false`.
        closed: bool,
    }

    impl FakeSyscall {
        fn new(
            nonblocking: bool,
            read_timeout: Option<Duration>,
            write_timeout: Option<Duration>,
        ) -> Self {
            FakeSyscall {
                ops: Mutex::new(Vec::new()),
                nonblocking: Mutex::new(nonblocking),
                read_timeout: Mutex::new(read_timeout),
                write_timeout: Mutex::new(write_timeout),
                fail_setter_nth: Mutex::new(0),
                setter_count: Mutex::new(0),
                closed: false,
            }
        }

        fn fail_setter_nth(&mut self, n: usize) {
            *self.fail_setter_nth.get_mut().unwrap() = n;
        }

        fn ops(&self) -> Vec<Op> {
            self.ops.lock().unwrap().clone()
        }

        fn nonblocking(&self) -> bool {
            *self.nonblocking.lock().unwrap()
        }

        fn read_timeout(&self) -> Option<Duration> {
            *self.read_timeout.lock().unwrap()
        }

        fn write_timeout(&self) -> Option<Duration> {
            *self.write_timeout.lock().unwrap()
        }
    }

    impl ConfigOps for FakeSyscall {
        fn nonblocking(&self, _fd: sys::RawSocket) -> io::Result<bool> {
            self.ops.lock().unwrap().push(Op::GetNonblocking);
            Ok(*self.nonblocking.lock().unwrap())
        }

        fn set_nonblocking(&self, _fd: sys::RawSocket, nonblocking: bool) -> io::Result<()> {
            self.ops.lock().unwrap().push(Op::SetNonblocking(nonblocking));
            let mut count = self.setter_count.lock().unwrap();
            *count += 1;
            if *count == *self.fail_setter_nth.lock().unwrap() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected setter failure",
                ));
            }
            *self.nonblocking.lock().unwrap() = nonblocking;
            Ok(())
        }

        fn timeout(&self, _fd: sys::RawSocket, opt: c_int) -> io::Result<Option<Duration>> {
            self.ops.lock().unwrap().push(Op::GetTimeout(opt));
            if opt == sys::SO_RCVTIMEO {
                Ok(*self.read_timeout.lock().unwrap())
            } else if opt == sys::SO_SNDTIMEO {
                Ok(*self.write_timeout.lock().unwrap())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "unknown timeout option",
                ))
            }
        }

        fn set_timeout(
            &self,
            _fd: sys::RawSocket,
            opt: c_int,
            timeout: Option<Duration>,
        ) -> io::Result<()> {
            self.ops
                .lock()
                .unwrap()
                .push(Op::SetTimeout(opt, timeout));
            let mut count = self.setter_count.lock().unwrap();
            *count += 1;
            if *count == *self.fail_setter_nth.lock().unwrap() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected setter failure",
                ));
            }
            if opt == sys::SO_RCVTIMEO {
                *self.read_timeout.lock().unwrap() = timeout;
            } else if opt == sys::SO_SNDTIMEO {
                *self.write_timeout.lock().unwrap() = timeout;
            }
            Ok(())
        }
    }

    /// Dummy file descriptor used by the fake; the fake never touches a real
    /// socket.
    const FAKE_FD: sys::RawSocket = 0;

    #[test]
    fn configure_succeeds() {
        let fake = FakeSyscall::new(false, None, None);
        let config = SocketConfig::new()
            .with_nonblocking(true)
            .with_read_timeout(Some(Duration::from_secs(1)))
            .with_write_timeout(Some(Duration::from_secs(2)));

        configure_with(FAKE_FD, &config, &fake).expect("configuration should succeed");

        // All fields are set to the requested values.
        assert_eq!(fake.nonblocking(), true);
        assert_eq!(fake.read_timeout(), Some(Duration::from_secs(1)));
        assert_eq!(fake.write_timeout(), Some(Duration::from_secs(2)));
        // The socket was not closed.
        assert_eq!(fake.closed, false);
        // No rollback happened: every setter is called exactly once.
        assert_eq!(
            fake.ops(),
            vec![
                Op::GetNonblocking,
                Op::GetTimeout(sys::SO_RCVTIMEO),
                Op::GetTimeout(sys::SO_SNDTIMEO),
                Op::SetNonblocking(true),
                Op::SetTimeout(sys::SO_RCVTIMEO, Some(Duration::from_secs(1))),
                Op::SetTimeout(sys::SO_SNDTIMEO, Some(Duration::from_secs(2))),
            ]
        );
    }

    #[test]
    fn configure_rolls_back_first_field_on_second_setter_failure() {
        let mut fake = FakeSyscall::new(false, None, None);
        // Fail the second setter call (the read timeout setter).
        fake.fail_setter_nth(2);
        let config = SocketConfig::new()
            .with_nonblocking(true)
            .with_read_timeout(Some(Duration::from_secs(1)))
            .with_write_timeout(Some(Duration::from_secs(2)));

        let error = configure_with(FAKE_FD, &config, &fake)
            .expect_err("configuration should fail");
        assert_eq!(error.kind(), io::ErrorKind::Other);

        // The first field (nonblocking) is restored to its original value.
        assert_eq!(fake.nonblocking(), false);
        // The unchanged fields are untouched: the read timeout setter failed
        // before applying, and the write timeout setter was never reached.
        assert_eq!(fake.read_timeout(), None);
        assert_eq!(fake.write_timeout(), None);
        // The socket was not closed.
        assert_eq!(fake.closed, false);
        // The write timeout setter was never called (only changed fields are
        // rolled back, and the write timeout was never changed).
        assert_eq!(
            fake.ops(),
            vec![
                Op::GetNonblocking,
                Op::GetTimeout(sys::SO_RCVTIMEO),
                Op::GetTimeout(sys::SO_SNDTIMEO),
                Op::SetNonblocking(true),
                Op::SetTimeout(sys::SO_RCVTIMEO, Some(Duration::from_secs(1))),
                // Rollback of the first field only.
                Op::SetNonblocking(false),
            ]
        );
    }

    #[test]
    fn configure_refuses_when_query_fails() {
        /// A fake whose nonblocking query always fails.
        struct FailingQueryOps;

        impl ConfigOps for FailingQueryOps {
            fn nonblocking(&self, _fd: sys::RawSocket) -> io::Result<bool> {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "query capability insufficient",
                ))
            }

            fn set_nonblocking(&self, _fd: sys::RawSocket, _v: bool) -> io::Result<()> {
                panic!("setter must not be called when query fails");
            }

            fn timeout(&self, _fd: sys::RawSocket, _o: c_int) -> io::Result<Option<Duration>> {
                panic!("setter must not be called when query fails");
            }

            fn set_timeout(
                &self,
                _fd: sys::RawSocket,
                _o: c_int,
                _t: Option<Duration>,
            ) -> io::Result<()> {
                panic!("setter must not be called when query fails");
            }
        }

        let config = SocketConfig::new().with_nonblocking(true);
        let error = configure_with(FAKE_FD, &config, &FailingQueryOps)
            .expect_err("configuration should be refused");
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn configure_only_changes_configured_fields() {
        let fake = FakeSyscall::new(false, None, None);
        // Only configure nonblocking; the timeouts must not be touched.
        let config = SocketConfig::new().with_nonblocking(true);

        configure_with(FAKE_FD, &config, &fake).expect("configuration should succeed");

        assert_eq!(fake.nonblocking(), true);
        assert_eq!(fake.read_timeout(), None);
        assert_eq!(fake.write_timeout(), None);
        assert_eq!(
            fake.ops(),
            vec![Op::GetNonblocking, Op::SetNonblocking(true),]
        );
    }

    /// End-to-end test of the public [`Socket::configure`] API on a real
    /// socket. Only runs on Unix, where the transaction is supported.
    #[cfg(unix)]
    #[test]
    fn configure_real_socket() {
        use crate::{Domain, Socket, Type};

        let socket = Socket::new(Domain::IPV4, Type::STREAM, None).unwrap();
        let config = SocketConfig::new()
            .with_nonblocking(true)
            .with_read_timeout(Some(Duration::from_secs(1)));

        socket.configure(&config).expect("configuration should succeed");

        assert_eq!(socket.nonblocking().unwrap(), true);
        assert_eq!(
            socket.read_timeout().unwrap(),
            Some(Duration::from_secs(1))
        );
        // The write timeout was not configured and must remain unchanged.
        assert_eq!(socket.write_timeout().unwrap(), None);
    }
}