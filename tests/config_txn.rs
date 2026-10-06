//! Targeted tests for transactional socket configuration
//! (`socket2::SocketConfigTxn`), driven by a fake syscall fixture so setter
//! failures can be injected deterministically.

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::time::Duration;

use socket2::{ConfigBackend, ConfigTxnError, SocketConfigTxn, TimeoutKind};

/// Fake syscall fixture: an in-memory "socket" plus a syscall log and
/// failure injection. `open` models the borrowed file descriptor; the
/// transaction must never close it.
#[derive(Debug)]
struct FakeSocket {
    // Current "kernel" state of the fake socket.
    nonblocking: bool,
    read_timeout: Option<Duration>,
    write_timeout: Option<Duration>,
    /// Whether the borrowed file descriptor is still open.
    open: bool,
    /// Log of every syscall the transaction made, in order.
    calls: Vec<String>,
    /// Syscalls to fail, as `(name, 1-based occurrence)` pairs.
    fail_on: Vec<(String, usize)>,
}

impl FakeSocket {
    fn new() -> Rc<RefCell<FakeSocket>> {
        Rc::new(RefCell::new(FakeSocket {
            nonblocking: false,
            read_timeout: Some(Duration::from_secs(1)),
            write_timeout: Some(Duration::from_secs(2)),
            open: true,
            calls: Vec::new(),
            fail_on: Vec::new(),
        }))
    }
}

/// Backend handing out fake syscalls against the shared `FakeSocket` state.
#[derive(Debug, Clone)]
struct FakeBackend {
    socket: Rc<RefCell<FakeSocket>>,
}

impl FakeBackend {
    fn enter(&self, name: &str) -> io::Result<()> {
        let mut socket = self.socket.borrow_mut();
        assert!(socket.open, "syscall `{name}` on a closed fd");
        socket.calls.push(name.to_string());
        let occurrence = socket.calls.iter().filter(|call| *call == name).count();
        if socket
            .fail_on
            .iter()
            .any(|(fail_name, fail_occ)| fail_name == name && *fail_occ == occurrence)
        {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("injected failure in `{name}`"),
            ));
        }
        Ok(())
    }
}

impl ConfigBackend for FakeBackend {
    fn query_nonblocking(&mut self) -> Result<bool, ConfigTxnError> {
        match self.enter("query_nonblocking") {
            Ok(()) => Ok(self.socket.borrow().nonblocking),
            Err(source) => Err(ConfigTxnError::Query {
                field: "nonblocking",
                source,
            }),
        }
    }

    fn set_nonblocking(&mut self, nonblocking: bool) -> io::Result<()> {
        self.enter("set_nonblocking")?;
        self.socket.borrow_mut().nonblocking = nonblocking;
        Ok(())
    }

    fn query_timeout(&mut self, kind: TimeoutKind) -> Result<Option<Duration>, ConfigTxnError> {
        let (name, field) = match kind {
            TimeoutKind::Read => ("query_read_timeout", "read_timeout"),
            TimeoutKind::Write => ("query_write_timeout", "write_timeout"),
        };
        match self.enter(name) {
            Ok(()) => {
                let socket = self.socket.borrow();
                Ok(match kind {
                    TimeoutKind::Read => socket.read_timeout,
                    TimeoutKind::Write => socket.write_timeout,
                })
            }
            Err(source) => Err(ConfigTxnError::Query { field, source }),
        }
    }

    fn set_timeout(&mut self, kind: TimeoutKind, timeout: Option<Duration>) -> io::Result<()> {
        let name = match kind {
            TimeoutKind::Read => "set_read_timeout",
            TimeoutKind::Write => "set_write_timeout",
        };
        self.enter(name)?;
        let mut socket = self.socket.borrow_mut();
        match kind {
            TimeoutKind::Read => socket.read_timeout = timeout,
            TimeoutKind::Write => socket.write_timeout = timeout,
        }
        Ok(())
    }
}

fn calls(socket: &Rc<RefCell<FakeSocket>>) -> Vec<String> {
    socket.borrow().calls.clone()
}

/// Inject a failure in the second setter (`set_read_timeout`): the first
/// field must be rolled back, the failed field and the never-reached field
/// must keep their original values, and the fd must stay open.
#[test]
fn second_setter_failure_rolls_back_only_changed_fields() {
    let socket = FakeSocket::new();
    socket
        .borrow_mut()
        .fail_on
        .push(("set_read_timeout".to_string(), 1));

    let backend = FakeBackend {
        socket: Rc::clone(&socket),
    };
    let mut txn = SocketConfigTxn::with_backend(backend);
    txn.set_nonblocking(true)
        .set_read_timeout(Some(Duration::from_secs(5)))
        .set_write_timeout(Some(Duration::from_secs(9)));
    let err = txn.commit().expect_err("commit must fail");

    match err {
        ConfigTxnError::Apply { field, .. } => assert_eq!(field, "read_timeout"),
        other => panic!("expected Apply error, got: {other:?}"),
    }

    let state = socket.borrow();
    // First field was applied, then rolled back to its original value.
    assert!(!state.nonblocking, "nonblocking must be restored");
    // The failed setter never took effect; the original value stays.
    assert_eq!(state.read_timeout, Some(Duration::from_secs(1)));
    // The third field was never attempted.
    assert_eq!(state.write_timeout, Some(Duration::from_secs(2)));
    // The borrowed fd is never closed.
    assert!(state.open, "borrowed fd must not be closed");
    drop(state);

    assert_eq!(
        calls(&socket),
        vec![
            "query_nonblocking",
            "query_read_timeout",
            "query_write_timeout",
            "set_nonblocking",  // apply first field
            "set_read_timeout", // second setter fails here
            "set_nonblocking",  // rollback of the first field only
        ],
        "only the field changed by this transaction may be rolled back",
    );
}

/// A successful commit applies every planned field exactly once, performs no
/// rollback, and leaves the borrowed fd open.
#[test]
fn commit_success_applies_all_fields() {
    let socket = FakeSocket::new();
    let backend = FakeBackend {
        socket: Rc::clone(&socket),
    };
    let mut txn = SocketConfigTxn::with_backend(backend);
    txn.set_nonblocking(true)
        .set_read_timeout(Some(Duration::from_secs(5)))
        .set_write_timeout(None);
    txn.commit().expect("commit must succeed");

    let state = socket.borrow();
    assert!(state.nonblocking);
    assert_eq!(state.read_timeout, Some(Duration::from_secs(5)));
    assert_eq!(state.write_timeout, None);
    assert!(state.open, "borrowed fd must not be closed");
    drop(state);

    assert_eq!(
        calls(&socket),
        vec![
            "query_nonblocking",
            "query_read_timeout",
            "query_write_timeout",
            "set_nonblocking",
            "set_read_timeout",
            "set_write_timeout",
        ],
        "each field is queried once and set once, with no rollback",
    );
}

/// If saving the original state fails, the transaction is refused before any
/// setter runs.
#[test]
fn query_failure_refuses_before_any_change() {
    let socket = FakeSocket::new();
    socket
        .borrow_mut()
        .fail_on
        .push(("query_nonblocking".to_string(), 1));

    let backend = FakeBackend {
        socket: Rc::clone(&socket),
    };
    let mut txn = SocketConfigTxn::with_backend(backend);
    txn.set_nonblocking(true)
        .set_read_timeout(Some(Duration::from_secs(5)));
    let err = txn.commit().expect_err("commit must be refused");

    match err {
        ConfigTxnError::Query { field, .. } => assert_eq!(field, "nonblocking"),
        other => panic!("expected Query error, got: {other:?}"),
    }

    let state = socket.borrow();
    assert!(!state.nonblocking);
    assert_eq!(state.read_timeout, Some(Duration::from_secs(1)));
    assert!(state.open);
    drop(state);
    assert_eq!(
        calls(&socket),
        vec!["query_nonblocking"],
        "no setter may run when a query fails",
    );
}

/// A backend that cannot query the current state explicitly refuses with
/// `ConfigTxnError::Unsupported` and changes nothing.
#[test]
fn unsupported_query_capability_refuses() {
    #[derive(Debug)]
    struct IncapableBackend;
    impl ConfigBackend for IncapableBackend {
        fn query_nonblocking(&mut self) -> Result<bool, ConfigTxnError> {
            Err(ConfigTxnError::Unsupported)
        }
        fn set_nonblocking(&mut self, _: bool) -> io::Result<()> {
            panic!("setter must never run when the query is unsupported")
        }
        fn query_timeout(&mut self, _: TimeoutKind) -> Result<Option<Duration>, ConfigTxnError> {
            Err(ConfigTxnError::Unsupported)
        }
        fn set_timeout(&mut self, _: TimeoutKind, _: Option<Duration>) -> io::Result<()> {
            panic!("setter must never run when the query is unsupported")
        }
    }

    let mut txn = SocketConfigTxn::with_backend(IncapableBackend);
    txn.set_nonblocking(true);
    match txn.commit() {
        Err(ConfigTxnError::Unsupported) => {}
        other => panic!("expected Unsupported error, got: {other:?}"),
    }
}

/// If rolling back a changed field also fails, the error reports both the
/// apply failure and the rollback failure.
#[test]
fn rollback_failure_is_reported() {
    let socket = FakeSocket::new();
    {
        let mut state = socket.borrow_mut();
        state.fail_on.push(("set_read_timeout".to_string(), 1));
        // The rollback of `nonblocking` is the second `set_nonblocking` call.
        state.fail_on.push(("set_nonblocking".to_string(), 2));
    }

    let backend = FakeBackend {
        socket: Rc::clone(&socket),
    };
    let mut txn = SocketConfigTxn::with_backend(backend);
    txn.set_nonblocking(true)
        .set_read_timeout(Some(Duration::from_secs(5)));
    let err = txn.commit().expect_err("commit must fail");

    match err {
        ConfigTxnError::Rollback { field, .. } => assert_eq!(field, "read_timeout"),
        other => panic!("expected Rollback error, got: {other:?}"),
    }

    let state = socket.borrow();
    // The rollback of `nonblocking` failed, so it keeps the applied value.
    assert!(state.nonblocking);
    assert_eq!(state.read_timeout, Some(Duration::from_secs(1)));
    assert!(state.open, "borrowed fd must not be closed");
}

/// An empty plan is a no-op that touches nothing.
#[test]
fn empty_plan_is_noop() {
    let socket = FakeSocket::new();
    let backend = FakeBackend {
        socket: Rc::clone(&socket),
    };
    SocketConfigTxn::with_backend(backend)
        .commit()
        .expect("empty commit must succeed");
    assert!(calls(&socket).is_empty());
    assert!(socket.borrow().open);
}

/// On Unix (the one supported platform) a real borrowed socket is configured
/// transactionally and stays open and usable afterwards.
#[cfg(all(unix, not(target_os = "vita")))]
#[test]
fn unix_real_socket_commit_and_borrowed_fd_stays_open() {
    use socket2::{Domain, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
    let original_read_timeout = socket.read_timeout().unwrap();

    let mut txn = SocketConfigTxn::new(&socket);
    txn.set_nonblocking(true)
        .set_read_timeout(Some(Duration::from_millis(250)));
    txn.commit().expect("commit must succeed");

    // The borrowed socket was never closed and reflects the new settings.
    assert_eq!(
        socket.read_timeout().unwrap(),
        Some(Duration::from_millis(250))
    );
    #[cfg(feature = "all")]
    assert!(socket.nonblocking().unwrap());
    // Still fully usable: another transaction can restore the originals.
    let mut txn = SocketConfigTxn::new(&socket);
    txn.set_nonblocking(false)
        .set_read_timeout(original_read_timeout);
    txn.commit().expect("restore must succeed");
    assert_eq!(socket.read_timeout().unwrap(), original_read_timeout);
}

/// On platforms without the query capability the transaction explicitly
/// refuses and the borrowed socket is untouched and still open.
#[cfg(not(all(unix, not(target_os = "vita"))))]
#[test]
fn unsupported_platform_refuses_and_keeps_socket_open() {
    use socket2::{Domain, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, None).unwrap();
    let before = socket.read_timeout().unwrap();

    let mut txn = SocketConfigTxn::new(&socket);
    txn.set_nonblocking(true)
        .set_read_timeout(Some(Duration::from_millis(250)));
    match txn.commit() {
        Err(ConfigTxnError::Unsupported) => {}
        other => panic!("expected Unsupported error, got: {other:?}"),
    }

    // Nothing changed and the borrowed socket was never closed.
    assert_eq!(socket.read_timeout().unwrap(), before);
    socket
        .set_read_timeout(Some(Duration::from_millis(10)))
        .expect("socket must still be usable");
    assert_eq!(
        socket.read_timeout().unwrap(),
        Some(Duration::from_millis(10))
    );
}
